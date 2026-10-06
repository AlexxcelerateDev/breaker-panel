# Diseño de la librería

## La API pública es el contrato

- Todo lo `pub` alcanzable desde fuera es semver: quitarlo, cambiar su firma o endurecer lo que
  exige es un **major** (en `0.x`, el minor hace de major).
- **Privado por defecto**; `pub(crate)` para compartir entre módulos. Los módulos van privados y
  `lib.rs` hace `pub use` de lo que se expone: la ruta pública no cambia al reorganizar ficheros.
- **`#[non_exhaustive]`** en los enums de error y en los structs públicos que puedan ganar campos
  (un struct así ya no se construye con literal desde fuera: necesita constructor).
- **Una API pública sin consumidores es mantenimiento a cambio de nada.** Algo entra cuando
  alguien lo llama, y se generaliza cuando aparece el **segundo** consumidor — no antes.
- **Lo que pone la librería es el mecanismo; la decisión llega por parámetro.** Umbrales,
  nombres, políticas de reintento: los decide quien llama. El corte no es "genérico vs.
  específico", que siempre se puede discutir, sino "¿aparece aquí el vocabulario del llamador?".

## Estructura

```text
src/
├── lib.rs     # doc del crate (`//!`), `mod` privados y `pub use` de la API
└── <modulo>.rs
tests/         # opcional: el crate visto desde fuera, solo API pública (ver 02)
examples/      # opcional: programas completos que usan el crate; el CI los compila
```

Un fichero hasta que pida dos. `error.rs` aparece cuando `lib.rs` crezca, no antes.

## Sin estado ni entorno

Una librería **no** lee variables de entorno, no tiene singletons (`OnceLock` global con
config), no instala un subscriber de `tracing` y no crea un runtime. Todo eso es del binario: en
una librería le quita al consumidor la forma de configurarla y a los tests la de variarla
(ver 02). Lo que necesite llega por parámetro.

- El tiempo y la aleatoriedad se inyectan: `now` por parámetro, no `Utc::now()` dentro.
- Emitir eventos con `tracing` vale (si entra la dependencia); decidir dónde acaban, no.

## Errores

- Un tipo propio por crate —o por módulo, si su semántica es distinta—, escrito a mano:
  `Display` + `std::error::Error`. No se usa `thiserror`; si el boilerplate se vuelve
  inmanejable, adoptarlo es razonable.
- **Nunca** `String`, `Box<dyn Error>` ni `anyhow` en la API pública: el consumidor no puede
  hacer `match` sobre ellos. `anyhow` es de binarios.
- Si envuelve el error de otra crate, lo expone por `source()` en vez de aplanarlo a texto — y
  exponer el tipo lo mete en el contrato (ver 00).
- `Display` en minúsculas, sin punto final y sin repetir la causa que ya da `source()`
  (convención de std: quien imprime la cadena la junta).
- Nada de panics en la API (ver 00): una entrada inválida es un `Err`, no un `panic!`.

## Documentación

- `///` en todo lo `pub` (lo exige `missing_docs`). Qué hace, `# Errors` (lo exige clippy),
  `# Panics` si aplica, y `# Examples`.
- Los ejemplos son **doctests ejecutables**: `cargo test` los corre y `cargo doc` falla con
  enlaces rotos, los dos en el gate. Un ejemplo que deja de compilar rompe el CI en vez de
  pudrirse en silencio. Si algo se ilustra sin poder ejecutarse, va en un bloque ```` ```text ````
  — no compila, y no miente sobre estar verificado.
- La primera línea del `//!` de `lib.rs` dice qué hace el crate: es el resumen en docs.rs.
- **Idioma**: inglés en todo lo que ve quien usa el crate —`///`, `//!`, README, `examples/`,
  los mensajes (`Display`, `tracing`, pánicos, el error de compilación de `flag_key!`) y los
  comentarios del código de producción, que docs.rs enseña en la vista de fuente—, y también los
  commits y los PRs, que GitHub enseña en la portada del repo. En español, los tests y la
  documentación del repo (`CLAUDE.md`, `.claude/`, `docs/`): la mantiene quien la escribe, y un
  contribuidor con Claude Code la sigue igual. El `Display` de un error es casi contrato: hay
  quien compara su `to_string()`, como el README.

## Reglas de código

- **Máximo 15 líneas por función**, según la cuenta de `clippy::too_many_lines`, que lo
  comprueba en el código de producción. Si se supera, extraer helpers. Una función que se lee
  mejor entera lleva `#[expect(clippy::too_many_lines, reason = "...")]`. Los tests quedan
  fuera: su longitud son datos (ver 02).
- Nombres: `snake_case` (funciones, módulos, ficheros), `PascalCase` (tipos),
  `SCREAMING_SNAKE_CASE` (constantes).
- Preferir funciones puras: son las que se prueban sin montar nada (ver 02).
