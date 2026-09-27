# Kill switches jerárquicos para Rust — Contexto y requerimientos

> Kill switches jerárquicos embebidos para Rust: archivo local, hot-reload, cascada, sin servicio externo.

Estado: diseño cerrado para el MVP · Revisión: 2026-09-27 · Nombre: pendiente (ver §15)

---

## 1. Contexto

### Problema

Un backend necesita apagar partes de su funcionalidad en caliente (un proveedor de pagos caído, reembolsos pausados) sin redeploy y sin depender de un servicio externo (LaunchDarkly, Unleash, flagd como servidor).

### Caso motivador

- `GET /payment-methods` lista los medios de pago; los apagados no aparecen o aparecen inactivos, con el motivo.
- `POST /payments` y `POST /refunds` ejecutan la operación; si el método o la operación están apagados, devuelven un error controlado con el motivo.
- El mismo switch se consulta desde varios lugares y tiene que dar la misma respuesta.

### Posicionamiento

En la taxonomía de Pete Hodgson ("Feature Toggles", martinfowler.com) esto son **ops toggles / kill switches**. No son release toggles, experimentos ni permisos por usuario.

### Objetivo del proyecto

- Resolver el caso propio.
- Pieza de portfolio.
- Si hay tracción, crate open source de nicho.
- **No** es un producto comercial: el dinero en feature flags está en el plano de control (UI, auditoría, permisos, analítica), que queda fuera de alcance.

### Competencia y diferencial

- Competencia directa: `open-feature-flagd` en modo archivo. Evalúa localmente, recarga el archivo, tiene targeting con JSONLogic y rollouts fraccionales (v0.2.2, julio 2026).
- Lo que no tiene y es el diferencial de este proyecto:
  - Jerarquía con cascada y explicación (`disabled_by`).
  - Keys declaradas en código y validadas al arrancar.
  - API mínima, fácil de testear, sin estado global.
  - Dependencias mínimas y formato TOML.

---

## 2. Alcance

### En alcance (MVP)

- Store en memoria con snapshot inmutable y swap lock-free.
- Carga desde TOML.
- Cascada jerárquica por prefijo de key, resuelta al cargar.
- Hot-reload con file watcher.
- Validación estricta al cargar y al recargar.
- Registro de keys declaradas en código y validación al arrancar.
- Hook de cambios con diff.

### Fuera de alcance (decidido)

| Qué | Por qué / alternativa |
|---|---|
| Trait `Process` / patrón Composite | Los procesos son datos; el árbol sale de las keys con punto |
| Trait `Source` / backends intercambiables | `replace(snapshot)` alcanza; agregar el trait cuando exista un 2º backend real |
| Backends DB (sqlite, sled, redb) | Sin caso real; sled además está estancado |
| YAML | `serde_yaml` archivado desde 2024; TOML alcanza |
| Proc-macro `#[process("…")]` | No ahorra líneas, oculta control de flujo, exige estado global y no sirve para keys dinámicas. Reevaluar solo tras uso real |
| Auditoría, permisos, versionado y rollback propios | Git (historial, PR review, revert) + hook `on_change` |
| Orquestación de workflows | Otro problema (Temporal, Restate, apalis) |
| Circuit breaker automático | Componer con `failsafe`, `recloser` o tower; nunca escribir su estado en el archivo |
| Sistema de plugins | Rust no tiene ABI estable |
| Targeting (tenant, país, %) | Post-MVP, solo con un caso real (§11) |
| Nodos con varios padres (`also = [...]`) | Post-MVP, si los pares de `require` se repiten mucho |

---

## 3. Modelo de datos

### Formato

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

### Campos de cada entrada

| Campo | Tipo | Obligatorio | Notas |
|---|---|---|---|
| `enabled` | `bool` | sí | Estado propio, sin cascada |
| `reason` | `String` | si `enabled = false` | Se muestra al usuario final |
| `meta` | `M` (genérico) | no | La lib no la interpreta. `M: DeserializeOwned + Default` |

Cualquier otro campo es un error.

### Keys

- Formato: `^[a-z0-9_]+(\.[a-z0-9_]+)*$`.
- El `.` es solo separador de jerarquía.
- Padre de una key = la key sin su último segmento.
- Un segmento intermedio no declarado (p. ej. `payments.methods`) es neutro: no apaga nada y no se puede consultar con `require`.

---

## 4. Semántica de la cascada

- `efectivo(k) = enabled(k) AND enabled(a)` para todo ancestro declarado `a` de `k`.
- Sin override: un hijo nunca queda encendido con un ancestro apagado.
- `disabled_by` es el ancestro apagado más alto (el más cercano a la raíz). Si solo está apagada la propia key, es ella misma.
- El `reason` expuesto es el de `disabled_by`.
- Se resuelve una vez por carga y se aplana a `HashMap<String, Resolved>`. En runtime cada consulta es un lookup, sin recorrer árbol.
- Consultar una key no declarada da error `Unknown`, nunca un `false` silencioso.

---

## 5. Guía de modelado

Formato de key: `<dominio>.<dimensión>.<valor>[.<subswitch>]`

1. **Una raíz por dominio.** Todo lo que deba morir con el dominio va debajo de él.
2. **Cada concepto vive en un solo lugar.** Un proveedor existe solo bajo `payments.methods`. Dos ramas = dos fuentes de verdad.
3. **Hermanos del mismo tipo.** Los hijos de un nodo son todos métodos o todas operaciones, nunca mezclados, para que `children()` tenga sentido.
4. **Prueba del padre.** "Si apago el padre, ¿el hijo puede seguir vivo?" Si la respuesta es "nunca", es hijo. Si tiene dos padres naturales (p. ej. `paypal` y `refund`), es una combinación: se ubica según la regla 5 y el otro padre se chequea con un segundo `require`.
5. **Las combinaciones cuelgan del eje que más cambia.** Los métodos se agregan y se quitan; las operaciones casi no cambian. Un método nuevo es un bloque autocontenido y dar de baja un proveedor es borrar su bloque.
6. **Subswitches solo cuando hacen falta, y en todos los hermanos.** Si existe `.refund` en un método, debe existir en todos. Si falta en uno, la key es desconocida y falla cerrado.
7. **Nombres.** Dimensión en plural (`methods`, `ops`), valores en singular (`paypal`, `refund`), snake_case.
8. **Todo lo apagado lleva `reason`.**

Una operación que depende de dos dimensiones hace un `require` por dimensión:

```rust
flags.require(&format!("payments.methods.{m}.refund"))?; // método + su refund (cascada)
flags.require("payments.ops.refund")?;                   // refunds global
```

La lib hace cumplir las reglas 7 (formato) y 8 (`reason`). Las reglas 1 a 6 son de diseño y van documentadas en el README.

---

## 6. API pública (borrador)

```rust
pub struct Flags<M = ()>;      // ArcSwap<Snapshot<M>> + callbacks
pub struct Snapshot<M = ()>;   // HashMap<String, Resolved<M>> + revisión

pub struct Resolved<M = ()> {
    pub enabled: bool,               // efectivo, con la cascada aplicada
    pub disabled_by: Option<String>,
    pub reason: Option<String>,
    pub meta: M,
}

pub enum FlagError {
    Disabled { key: String, disabled_by: String, reason: String },
    Unknown { key: String },
    InvalidSegment { segment: String },
}

impl<M> Flags<M> {
    pub fn from_toml_str(s: &str) -> Result<Self, LoadError>;
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError>;
    pub fn snapshot(&self) -> Arc<Snapshot<M>>;  // varias consultas consistentes, o a través de .await
    pub fn replace(&self, next: Snapshot<M>) -> Result<(), LoadError>; // fuentes propias (DB, HTTP…)
    pub fn on_change(&self, f: impl Fn(&Diff) + Send + Sync + 'static);
    #[cfg(feature = "watch")]
    pub fn watch_file(path: impl AsRef<Path>) -> Result<(Arc<Self>, Watcher), LoadError>;
}

impl<M> Snapshot<M> {
    pub fn from_toml_str(s: &str) -> Result<Self, LoadError>;
    pub fn require(&self, key: impl AsRef<str>) -> Result<(), FlagError>;
    pub fn get(&self, key: impl AsRef<str>) -> Result<&Resolved<M>, FlagError>;
    pub fn children(&self, prefix: &str) -> impl Iterator<Item = (&str, &Resolved<M>)>; // hijos directos declarados
    pub fn revision(&self) -> u64;
}

impl Watcher {
    pub fn reload(&self) -> Result<(), LoadError>; // para SIGHUP o endpoint admin
}                                                  // drop = deja de vigilar

/// Valida un segmento que viene de input externo antes de armar una key.
/// Rechaza `.` y todo lo que no cumpla `^[a-z0-9_]+$`.
pub fn segment(s: &str) -> Result<&str, FlagError>;
```

### Keys registradas (feature `registry`)

```rust
flag_key!(PAYPAL = "payments.methods.paypal"); // declara un static y lo registra (linkme)
flags.require(&PAYPAL)?;
```

- Al arrancar, toda key registrada debe existir en el archivo; si no, falla la carga.
- Un reload que elimina una key registrada se rechaza.
- Las keys dinámicas (`format!`) no se validan al arrancar: si no existen, `Unknown` en runtime (falla cerrado).

---

## 7. Hot-reload

- `notify` + `notify-debouncer-full`.
- Vigilar el **directorio padre** y filtrar por path: los editores guardan con temp + rename y los ConfigMaps de Kubernetes cambian un symlink.
- Fallback configurable a `PollWatcher` (bind mounts de Docker en Mac/Windows no propagan eventos).
- `reload()` manual.
- Flujo: leer → parsear → validar → resolver cascada → validar keys registradas → swap.
- Si cualquier paso falla: se conserva el snapshot anterior, se loguea con `tracing` y no se dispara `on_change`.
- Al arrancar, archivo ausente o inválido = error (fail fast), sin defaults.
- El watcher corre en su propio hilo; no requiere runtime async.

---

## 8. Validación al cargar

Cualquiera de estos casos rechaza el archivo completo:

- TOML inválido o key duplicada (el parser ya lo rechaza).
- Campos desconocidos (`deny_unknown_fields`).
- Key que no cumple el formato.
- `enabled = false` sin `reason`.
- `meta` que no deserializa en `M`.
- Key registrada con `flag_key!` ausente en el archivo.

---

## 9. Observabilidad y auditoría

- `on_change(|diff| …)`: se dispara en cada reload exitoso. `Diff` lista keys agregadas, eliminadas y con cambio de estado efectivo, más la revisión anterior y la nueva.
- Revisión: contador incremental por proceso.
- Eventos `tracing`: reload aplicado, reload rechazado (con el error), `require` denegado (nivel `debug`).
- Auditoría, permisos y rollback reales: Git.

---

## 10. Requerimientos no funcionales

- **Sin runtime:** núcleo sync; funciona con tokio, cualquier otro runtime o ninguno.
- **Rendimiento:** `require` = un load de ArcSwap + un lookup en `HashMap`, sin locks ni allocations dentro de la lib.
- **Sin estado global** en la lib.
- **Testing:** `Flags::from_toml_str` en memoria, sin archivo. Cada test crea su propia instancia, así que corren en paralelo sin pisarse.
- **Dependencias del núcleo:** `arc-swap`, `serde`, `toml`, `tracing`.
- **Features:**
  - `watch` (default): `notify`, `notify-debouncer-full`.
  - `registry` (default): `linkme`.
  - `tower` (post-MVP): Layer HTTP.
- **Multi-instancia:** cada réplica recarga por su cuenta y durante la propagación pueden diferir. Documentar que el GET es informativo y el POST es la autoridad.

---

## 11. Post-MVP

- `tower::Layer` por ruta (`RequireFlag::new("…")`) que responde con el `reason`.
- Provider de OpenFeature, para sumarse a ese ecosistema en lugar de competir.
- Targeting simple (allowlist + porcentaje con hash de algoritmo fijo, nunca `DefaultHasher`), solo con un caso real.
- Nodos con varios padres (`also = [...]`), si hace falta.

---

## 12. Ejemplo de uso (caso pagos)

```rust
// GET /payment-methods
let snap = flags.snapshot();
let methods: Vec<MethodDto> = snap
    .children("payments.methods")
    .map(|(key, r)| MethodDto::new(key, r.enabled, r.reason.as_deref(), &r.meta))
    .collect();

// POST /payments
let m = flags::segment(&req.method)?;
flags.require(&format!("payments.methods.{m}"))?;
flags.require("payments.ops.charge")?;

// POST /refunds
let m = flags::segment(&req.method)?;
flags.require(&format!("payments.methods.{m}.refund"))?;
flags.require("payments.ops.refund")?;
```

Mapeo a HTTP (lo decide la app):

| Error | Respuesta sugerida |
|---|---|
| `InvalidSegment` | 400 |
| `Unknown` en una key armada con input del usuario | 400 (método inexistente) |
| `Unknown` en cualquier otra key | 500 (bug de configuración) |
| `Disabled` | 503 (o 409/422) con `reason` |

---

## 13. Criterios de aceptación (tests mínimos)

- Padre apagado → hijo efectivo `false`, `disabled_by` = padre, `reason` del padre.
- Dos ancestros apagados → `disabled_by` = el más alto.
- Un segmento intermedio no declarado no afecta la cascada.
- `require` de una key no declarada → `Unknown`.
- `enabled = false` sin `reason` → la carga falla.
- Campo desconocido o key con formato inválido → la carga falla.
- Key registrada ausente → falla al arrancar; un reload que la elimina se rechaza y el snapshot anterior queda intacto.
- Reload con archivo inválido → snapshot anterior intacto, sin `on_change`.
- Reload válido → `on_change` recibe un diff solo con las keys que cambiaron.
- Guardado atómico (escribir temp + rename) dispara el reload.
- `segment("paypal.refund")` → `InvalidSegment`.
- `children("payments.methods")` → solo hijos directos declarados.

---

## 14. Roadmap

| Fase | Contenido | Criterio para avanzar |
|---|---|---|
| 0 | Módulo interno dentro del backend propio; usarlo semanas en staging/producción | Si nunca se usan la cascada ni `disabled_by`, quedarse con el módulo o migrar a flagd |
| 1 | Extraer crate 0.1: núcleo + `watch` + `registry`, README con guía de modelado, "cuándo usar esto vs flagd/Unleash" y ejemplo con axum | La API sobrevivió la fase 0 sin cambios grandes |
| 2 | `tower::Layer` y provider de OpenFeature | Hay usuarios externos pidiéndolo |

Señales para seguir invirtiendo: issues o PRs de terceros, crates dependientes en crates.io. Sin señales en unos 6 meses → mantenimiento mínimo.

---

## 15. Preguntas abiertas

- Nombre: `fusebox`, `breaker-panel`, `switchboard`… (metáfora del tablero eléctrico). Verificar disponibilidad en crates.io.
- ¿`registry` como feature default o opt-in?
- ¿Campo `version` opcional en el archivo para correlacionar con commits de Git?
- Código HTTP por defecto de `Disabled` en el Layer (503 vs 409/422).
- MSRV.

---

## 16. Referencias

- `open-feature-flagd`: https://lib.rs/crates/open-feature-flagd · https://flagd.dev/providers/rust/
- OpenFeature (spec y SDK Rust): https://openfeature.dev/docs/tutorials/getting-started/rust/
- Pete Hodgson, "Feature Toggles": https://martinfowler.com/articles/feature-toggles.html
- Unleash Yggdrasil (motor de evaluación en Rust): referencia para rollouts por porcentaje.
- `fail` (tikv/fail-rs): puntos nombrados inyectados por macro; prior art del proc-macro descartado.
- `tracing::instrument`: referencia si alguna vez se escribe un atributo que envuelva funciones async.
- `arc-swap` (`load` vs `load_full`), `notify` + `notify-debouncer-full`, `linkme` / `inventory`, `figment` / `config`.
