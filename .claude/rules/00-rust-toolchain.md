# Toolchain, lints y dependencias

## Versión de Rust

- **Pin**: `rust-toolchain.toml` fija `channel = "1.99.0"` con `rustfmt` y `clippy`. Es el
  toolchain de quien desarrolla el crate; al consumidor no le llega.
- **MSRV** (`rust-version`): en una librería es una **promesa al consumidor** — cargo se niega a
  compilarla con un toolchain anterior. Hoy es 1.98, por debajo del pin, y la verifica el job
  `msrv` del CI: compila lib, tests y ejemplos con esa versión exacta.
  - **Bajarla** para llegar a más consumidores exige mover ese job a la versión nueva. Declarar
    una MSRV que nada compila es escribir una versión que nadie ha probado.
  - **Subirla** rompe a quien esté por debajo: solo si se usa algo que la exija.
  - **Subir el pin no la mueve.** `clippy::incompatible_msrv` avisa de una API de std más nueva
    que la MSRV, también en los tests: o se usa la alternativa antigua, o se sube la MSRV a
    propósito.
- El pin (patch) se sube tras pasar el gate. El pin nunca queda por debajo de la MSRV.
- Tras subirlo, reiniciar rust-analyzer: su servidor de proc-macros sigue siendo el del
  toolchain anterior y da `mismatched ABI`. Es cosmético; no hay que limpiar `target/`.

## Lints (`[lints]` en `Cargo.toml`)

| Lint | Nivel | Por qué |
|---|---|---|
| `unsafe_code` | `deny` | El crate no usa `unsafe`; blindarlo. |
| `missing_docs` | `warn` (→ error en el gate) | En una librería la doc es parte de la API. |
| `clippy::unwrap_used` / `expect_used` | `deny` | Un panic aquí lo paga el binario de otro. |
| `clippy::missing_errors_doc` | `warn` | Todo `pub fn` con `Result` dice en `# Errors` cuándo falla. |
| `clippy::dbg_macro` / `todo` | `warn` | Restos que no deben llegar a `main`. |
| `clippy::allow_attributes` / `allow_attributes_without_reason` | `warn` | Imponen la sección siguiente. |
| `clippy::too_many_lines` | `warn`, fuera de los tests | El límite de líneas de 01. Va en `src/lib.rs` con `cfg_attr(not(test))`, porque `[lints]` no distingue tests. |

- En una librería **no hay excepción de arranque** para `expect`: no hay `main` donde fallar
  rápido sea lo correcto. Si una función panica por contrato (una precondición del llamador), lo
  dice en `# Panics` — y casi siempre es mejor devolver `Result`.
- `clippy.toml` exime a los tests (`allow-unwrap-in-tests`, `allow-expect-in-tests`).
- No activar `clippy::pedantic` de golpe: lint por lint, como `missing_errors_doc` y
  `too_many_lines`.

### `#[expect]`, no `#[allow]`

Toda exención es `#[expect(lint, reason = "por qué")]`. `#[allow]` calla para siempre y
sobrevive al código que lo justificaba; `#[expect]` avisa (`unfulfilled_lint_expectations`) en
cuanto el lint deja de dispararse, y con el gate a `deny` eso rompe el CI. Las exenciones muertas
se limpian solas en vez de fosilizarse. El porqué va en `reason`, no en un comentario: rustc lo
enseña junto al aviso. Lo comprueba clippy (fila anterior), no hace falta acordarse.

## Warnings

- Local: default de Cargo. No commitear `warnings = "deny"` en `.cargo/config.toml`.
- Gate: `CARGO_BUILD_WARNINGS=deny`, **no** `RUSTFLAGS=-Dwarnings`: no invalida la caché de
  compilación de las dependencias y solo afecta al crate. Cubre también a `rustdoc` (verificado:
  un enlace `[`X`]` roto hace fallar `cargo doc`).
- El gate pasa limpio y se mantiene así: un warning nuevo se corrige antes de merge.

## `Cargo.lock`

Versionado, y el CI usa `--locked` para que falle si no está al día en vez de resolver versiones
nuevas en silencio.

Pero en una librería **no viaja**: cuando otro crate la usa, cargo ignora este lock y resuelve
con el del binario. Consecuencia: la versión escrita en `Cargo.toml` es el **mínimo que se
promete**. Si el código empieza a usar algo de una versión posterior, el lock local lo tapa y el
consumidor que tenga una más vieja no compila. Al usar algo nuevo de una dependencia, se sube su
versión en `Cargo.toml`, no solo el lock. Comprobación puntual (nightly, fuera del gate):
`cargo +nightly update -Z direct-minimal-versions && cargo test`.

## Dependencias

- **Features mínimas**, y en una librería con más motivo: cargo **unifica** las features de todo
  el grafo, así que una feature que se active aquí se le impone a cada consumidor y no la puede
  quitar. Nada de `tokio = { features = ["full"] }`.
- **Runtime y TLS los elige el binario.** "Un solo stack TLS por binario" no lo puede cumplir una
  librería que trae el suyo: si hace falta, la dependencia va con `default-features = false` y
  la elección se expone como feature propia.
- **Un tipo de una dependencia en la API pública la mete en el contrato**: subirla de major pasa a
  ser un major de esta librería. Se hace a propósito (el `AppError` de `web-kit` lleva un
  `sqlx::Error`), nunca por descuido — si no, se envuelve en un tipo propio.
- **Lo que solo usan los tests va en `[dev-dependencies]`.**
- **Un test que fija un contrato con el exterior no puede calcular lo esperado con el código que
  prueba** (una firma, un formato de fichero): pasaría aunque ese código estuviera roto. La
  dependencia que permite calcularlo a mano se queda en `[dev-dependencies]`.
- Errores a mano, sin `thiserror` (ver 01).
- **`cargo deny check`** (`deny.toml`), en el CI y semanal:

  | Comprobación | Nivel | Por qué |
  |---|---|---|
  | Vulnerabilidades RustSec | `deny` | Se actualiza la dependencia, no se silencia. |
  | Sin mantenimiento | `deny` solo si es **directa** | Sobre una transitiva no hay nada que hacer; denegarla deja el CI rojo meses y el gate se acaba ignorando. |
  | Licencias | `deny` fuera de la allow-list | Una licencia rara se revisa antes de que entre. |
  | Versiones duplicadas | `warn` | Casi siempre son transitivas ajenas. |
  | Fuentes | `deny` salvo crates.io | Una dependencia git no se cuela en un `cargo add`. |

  Ignorar un advisory va en `[advisories] ignore` **con motivo y fecha de revisión**.

## Perfiles

No hay `[profile.*]`: los defaults son correctos, y además cargo **ignora** los perfiles de una
dependencia — solo cuentan los del binario final. Un workaround de máquina (p. ej. `LNK1318` en
Windows) va en `~/.cargo/config.toml` de esa máquina, nunca aquí.

## CI (`.github/workflows/ci.yml`)

Corre el gate de `CLAUDE.md` en dos jobs: `ci` con el pin, y `msrv` solo con el `cargo check`
de la versión de `rust-version`. El gate vive en esos dos sitios y cambia en los dos a la vez:
una tercera copia aquí ya se quedó sin el paso `--no-default-features`.

Un tercer job, `macos`, corre clippy y los tests en macOS: FSEvents vigila por ruta y no por
inodo, y los tests de lo que cambia por eso (`cfg(target_os = "macos")`) no compilan en Linux, ni
para lintearlos. Solo esos dos pasos: en un repo privado, un minuto de macOS cuenta por diez.

No hay `rustfmt.toml`: defaults de `style_edition 2024`. No crear uno para legalizar desviaciones.

Endurecido, no opcional aunque el repo sea privado:

- **`permissions: contents: read`**: sin él, el `GITHUB_TOKEN` hereda los permisos por defecto,
  que suelen ser de escritura.
- **`persist-credentials: false`**: si no, el token queda en `.git/config` al alcance de
  cualquier step posterior, incluido el build de una dependencia.
- **`concurrency`** con `cancel-in-progress` solo en `pull_request`.

Los tags mayores flotantes (`@v1`) se actualizan solos; un salto de major (`checkout@v7` →
`@v8`) hay que hacerlo a mano y se queda atrás en silencio.
