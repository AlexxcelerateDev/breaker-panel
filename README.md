# breaker-panel

Kill switches jerárquicos embebidos para Rust: un archivo TOML local, hot-reload y cascada por
prefijo de key, sin servicio externo.

Son *ops toggles* en la taxonomía de Pete Hodgson: apagar en caliente un proveedor caído o una
operación pausada, sin redeploy. No son release toggles, experimentos ni permisos por usuario.

```toml
[flags]
"payments"                       = { enabled = true }
"payments.methods.paypal"        = { enabled = true, meta = { display_name = "PayPal" } }
"payments.methods.paypal.refund" = { enabled = false, reason = "PayPal no procesa reembolsos hoy" }
"payments.methods.stripe"        = { enabled = true, meta = { display_name = "Stripe" } }
"payments.methods.stripe.refund" = { enabled = true }
"payments.ops.charge"            = { enabled = true }
"payments.ops.refund"            = { enabled = true }
```

```rust
use breaker_panel::{Flags, segment};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
struct Meta {
    display_name: String,
}

# fn main() -> Result<(), Box<dyn std::error::Error>> {
# let toml = r#"
# [flags]
# "payments"                       = { enabled = true }
# "payments.methods.paypal"        = { enabled = true, meta = { display_name = "PayPal" } }
# "payments.methods.paypal.refund" = { enabled = false, reason = "PayPal no procesa reembolsos hoy" }
# "payments.methods.stripe"        = { enabled = true, meta = { display_name = "Stripe" } }
# "payments.methods.stripe.refund" = { enabled = true }
# "payments.ops.charge"            = { enabled = true }
# "payments.ops.refund"            = { enabled = true }
# "#;
// En producción: `let (flags, _watcher) = Flags::<Meta>::watch_file("flags.toml")?;`
let flags: Flags<Meta> = Flags::from_toml_str(toml)?;

// GET /payment-methods: varias consultas sobre el mismo snapshot son consistentes.
let snap = flags.snapshot();
for (key, r) in snap.children("payments.methods") {
    println!("{key}: {} activo={} {:?}", r.meta.display_name, r.enabled, r.reason);
}

// POST /refunds: un `require` por dimensión. El método llega del usuario: se valida antes.
let m = segment("paypal")?;
let err = flags.require(format!("payments.methods.{m}.refund")).unwrap_err();
assert_eq!(err.to_string(), r#""payments.methods.paypal.refund" está apagado: PayPal no procesa reembolsos hoy"#);
flags.require("payments.ops.refund")?;
# Ok(())
# }
```

Un servidor completo con axum —los tres endpoints, la traducción de errores a HTTP y la recarga
en caliente— está en [`examples/axum.rs`](examples/axum.rs):

```text
cargo run --example axum
curl localhost:3000/payment-methods
curl -X POST localhost:3000/refunds -H 'content-type: application/json' -d '{"method":"paypal"}'
```

## Semántica

- Cada entrada lleva `enabled`; `reason` es obligatorio si está apagada (se le muestra al usuario
  final); `meta` es opcional y lo deserializa tu tipo `M`. Cualquier otro campo es un error.
- **Cascada sin override**: una key queda encendida solo si ella y todos sus ancestros
  declarados lo están. `disabled_by` es el ancestro apagado más cercano a la raíz, y el `reason`
  expuesto es el suyo.
- Un segmento intermedio no declarado (`payments.methods`) es neutro: no apaga nada y no se puede
  consultar.
- Consultar una key no declarada da `Unknown`, nunca un `false` silencioso.
- El archivo se valida entero al cargar y al recargar. Si una recarga falla, sigue vigente el
  snapshot anterior y no se avisa a `on_change`. Al arrancar no hay defaults: archivo ausente o
  inválido es un error.
- Las keys van **entre comillas**: sin ellas, TOML lee `payments.methods` como tablas anidadas
  (y la carga falla).

## Guía de modelado

Formato de key: `<dominio>.<dimensión>.<valor>[.<subswitch>]`. La librería hace cumplir el
formato (`^[a-z0-9_]+(\.[a-z0-9_]+)*$`) y el `reason`; el resto son reglas de diseño.

1. **Una raíz por dominio.** Todo lo que deba morir con el dominio va debajo de él.
2. **Cada concepto vive en un solo lugar.** Un proveedor existe solo bajo `payments.methods`.
   Dos ramas son dos fuentes de verdad.
3. **Hermanos del mismo tipo.** Los hijos de un nodo son todos métodos o todos operaciones,
   nunca mezclados, para que `children()` tenga sentido.
4. **Prueba del padre.** "Si apago el padre, ¿el hijo puede seguir vivo?" Si la respuesta es
   "nunca", es hijo. Si tiene dos padres naturales (`paypal` y `refund`), es una combinación: se
   ubica según la regla 5 y el otro padre se comprueba con un segundo `require`.
5. **Las combinaciones cuelgan del eje que más cambia.** Los métodos se añaden y se quitan; las
   operaciones casi no cambian. Un método nuevo es un bloque autocontenido, y dar de baja un
   proveedor es borrar su bloque.
6. **Subswitches solo cuando hacen falta, y en todos los hermanos.** Si existe `.refund` en un
   método, debe existir en todos: si falta en uno, la key es desconocida y falla cerrado.
7. **Nombres.** Dimensión en plural (`methods`, `ops`), valores en singular (`paypal`,
   `refund`), `snake_case`.
8. **Todo lo apagado lleva `reason`.**

## Errores y HTTP

La traducción la decide la app. Una sugerencia:

| Error | Respuesta |
|---|---|
| `InvalidSegment` | 400 |
| `Unknown` en una key armada con input del usuario | 400 (método inexistente) |
| `Unknown` en cualquier otra key | 500 (bug de configuración) |
| `Disabled` | 503 (o 409/422) con el `reason` |

## Recarga en caliente (feature `watch`)

`Flags::watch_file` vigila el **directorio** del archivo, así que ve los guardados atómicos de
los editores (temp + rename) y el cambio de symlink de un `ConfigMap` de Kubernetes. Para los bind
mounts de Docker en Mac y Windows, que no propagan eventos, `Flags::poll_file` mira el archivo
cada cierto intervalo. `Watcher::reload` fuerza la recarga (SIGHUP, endpoint admin); soltar el
`Watcher` deja de vigilar. El watcher corre en su propio hilo: no hace falta runtime async.

Cada réplica recarga por su cuenta y durante la propagación pueden diferir: el listado (`GET`) es
informativo y el `require` de la operación (`POST`), la autoridad.

## Keys registradas (feature `registry`)

```rust,standalone_crate
use breaker_panel::{Flags, flag_key};

flag_key!(CHARGE = "payments.ops.charge");

# fn main() -> Result<(), Box<dyn std::error::Error>> {
let flags: Flags = Flags::from_toml_str("[flags]\n\"payments.ops.charge\" = { enabled = true }")?;
flags.require(CHARGE)?;
# Ok(())
# }
```

Toda key declarada con `flag_key!` tiene que estar en el archivo: si falta, falla el arranque, y
una recarga que la quite se rechaza. Una key mal formada (`"payments.Methods"`) ni siquiera
compila. Las keys dinámicas (`format!`) no se validan al arrancar: si no existen, dan `Unknown`
en runtime.

## Cuándo usar esto y cuándo no

- **flagd en modo archivo** (`open-feature-flagd`) evalúa en local, recarga el archivo y trae
  targeting con JSONLogic y rollouts por porcentaje. Si necesitas eso, úsalo.
- **LaunchDarkly, Unleash**: si necesitas el plano de control —UI, auditoría, permisos,
  analítica—.
- **breaker-panel**: si lo que quieres es jerarquía con cascada y `disabled_by`, keys validadas
  al arrancar, una API mínima sin estado global y un archivo TOML revisado por PR. La auditoría,
  el versionado y el rollback son los de Git; `on_change` avisa de cada cambio aplicado.
