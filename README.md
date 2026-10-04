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
| `Disabled` | 503 con el `reason` |

Para `Disabled`, 503 y no 409 ni 422: un cliente que reintenta trata esos dos como permanentes y
abandona, cuando un kill switch es por definición temporal.

## Recarga en caliente (feature `watch`)

`Flags::watch_file` vigila el **directorio** del archivo, así que ve los guardados atómicos de
los editores (temp + rename) y el cambio de symlink de un `ConfigMap` de Kubernetes.
`Watcher::reload` fuerza la recarga (SIGHUP, endpoint admin); soltar el `Watcher` deja de vigilar.
El watcher corre en su propio hilo: no hace falta runtime async. `Watcher` es `Send + Sync`, así
que cabe en el estado de axum.

- **Una recarga rechazada deja vigente el snapshot anterior**: el archivo dice una cosa y el
  servicio hace otra. Regístrala con `Watcher::on_reject`; si no, solo queda en el log de la
  librería, y un filtro por crate lo descarta (ver [Observabilidad](#observabilidad)).
- **Guárdalo de forma atómica** (temporal + rename, como los editores y los `ConfigMap`): una
  escritura en el sitio se puede leer a medias. En caliente eso es un rechazo que llega a
  `on_reject` y se corrige solo al terminar la escritura; al arrancar, un `watch_file` que falla.
- **El archivo, solo en su directorio**: cualquier cambio a su lado lo relee, y `poll_file`
  hashea todo lo que hay en él en cada vuelta.
- **Docker: monta el directorio, no el archivo.** Con un bind mount de un solo archivo, un
  guardado atómico en un host Linux crea un inodo nuevo y el contenedor se queda con el viejo
  para siempre, aunque sondee. En Docker Desktop (Mac, Windows) los eventos no cruzan el montaje:
  ahí, `Flags::poll_file`.
- **No reemplaces el directorio.** Si un despliegue lo borra y lo crea de nuevo (`rm -rf` y
  copiar, un `rsync --delete` del padre), `watch_file` sigue vigilando el que ya no existe y deja
  de ver cambios. Con la recreación inmediata de un despliegue ni siquiera hay rechazo: se aplica
  el archivo nuevo y lo que se pierde es la edición siguiente. Por eso, en cuanto pasa, llega a
  `on_reject` un `LoadError::Watch`, **salvo en Windows si se renombra** (`mv conf conf.viejo` y
  otro en su lugar): el sistema sigue al renombrado sin decir nada, y no llega ningún aviso. Ahí,
  `poll_file`, que vuelve a encontrarlo. Un `ConfigMap` no tiene el problema: cambia un symlink
  dentro de un directorio que sigue vivo.

Cada réplica recarga por su cuenta y durante la propagación pueden diferir: el listado (`GET`) es
informativo y el `require` de la operación (`POST`), la autoridad. `revision()` es un contador por
proceso: no sirve para comparar réplicas. Para eso, `Snapshot::toml()` devuelve el TOML aplicado
tal cual, y un health check puede usarlo de dos formas:

- **Réplica atrasada**: el archivo en disco distinto de `toml()` es una recarga que no se aplicó.
  Cubre también un evento que nunca llegó (Docker Desktop con `watch_file`, un directorio
  recreado), que no llega a `on_reject` porque no hay nada que rechazar. Tolera unos cientos de
  milisegundos de diferencia: es lo que tarda en recargar. **No cubre el bind mount de un solo
  archivo**: el disco que ve el contenedor es el mismo inodo viejo, así que coincide con `toml()`
  aunque el host ya tenga otro.
- **Réplicas que coinciden**: un hash de `toml()` en el health check, comparado con el del
  archivo desplegado, calculado fuera del contenedor. Es la comprobación que detecta también el
  caso anterior. Con SHA-256 es el mismo valor que `sha256sum flags.toml`; el algoritmo lo pone
  la app, no esta librería.

## Observabilidad

`on_change` recibe un `Diff` por cada recarga aplicada, en orden de revisión: keys añadidas,
quitadas, con otro estado efectivo (`changed`) o con otro motivo (`reason_changed`). Los cambios
de `meta` no entran. Una recarga sin cambios visibles (un comentario) también avisa, con las
listas vacías.

La librería emite estos eventos de `tracing`:

| Evento | Target | Nivel |
|---|---|---|
| Recarga aplicada, con su revisión | `breaker_panel::flags` | `info` |
| Recarga rechazada, con la cadena de causas (línea y columna si el TOML no parsea) | `breaker_panel::watch` | `warn` |
| La vigilancia con eventos se paró: el directorio se borró o se recreó (también llega a `on_reject`) | `breaker_panel::watch` | `warn` |
| `poll_file` con un intervalo de menos de 100 ms, que se sube a 100 | `breaker_panel::watch` | `warn` |
| Un callback de `on_change` u `on_reject` entró en pánico | `breaker_panel::flags` | `error` |
| `require` denegado: **uno por llamada**, así que bajo carga con un switch apagado es una línea por petición | `breaker_panel::snapshot` | `debug` |

Si tu filtro es por crate (`EnvFilter` con `mi_app=info`), añade `breaker_panel=info` o usa
`on_reject`. Para registrar un `LoadError` completo, `{:#}`: su `Display` a secas solo da el
primer nivel.

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

El registro lo arma el linker por binario: es el único estado global del crate, y es de solo
lectura. Consecuencia en tus tests: todo TOML que carguen tiene que traer las keys registradas en
ese binario.

## Cuándo usar esto y cuándo no

- **flagd en modo archivo** (`open-feature-flagd`) evalúa en local, recarga el archivo y trae
  targeting con JSONLogic y rollouts por porcentaje. Si necesitas eso, úsalo.
- **LaunchDarkly, Unleash**: si necesitas el plano de control —UI, auditoría, permisos,
  analítica—.
- **breaker-panel**: si lo que quieres es jerarquía con cascada y `disabled_by`, keys validadas
  al arrancar, una API mínima sin estado que se escriba en runtime y un archivo TOML revisado
  por PR. La auditoría,
  el versionado y el rollback son los de Git; `on_change` avisa de cada cambio aplicado.
