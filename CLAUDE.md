# breaker-panel — kill switches jerárquicos para Rust

Archivo TOML local, hot-reload y cascada por prefijo de key, sin servicio externo. Los
requerimientos, lo que queda fuera de alcance y por qué, y el roadmap están en
`docs/REQUIREMENTS.md`; el README tiene la guía de modelado de keys.

Las convenciones están en `.claude/rules/` (toolchain, diseño de la librería, tests). Este
archivo solo cubre lo que no se deduce leyendo el código.

## Gate completo, lo mismo que corre el CI

```bash
cargo fmt --all --check && CARGO_BUILD_WARNINGS=deny cargo clippy --all-targets --locked && CARGO_BUILD_WARNINGS=deny cargo clippy --all-targets --locked --no-default-features && cargo test --locked && CARGO_BUILD_WARNINGS=deny cargo doc --no-deps --locked && cargo deny check
```

`cargo deny` no viene con rustup: `cargo install cargo-deny --locked` una vez por máquina.

## Dónde se aparta del borrador de `docs/REQUIREMENTS.md` §6

- **`Snapshot` guarda un `BTreeMap`**, no un `HashMap`: con `HashMap`, `children()` saldría en
  otro orden en cada recarga y en cada réplica (un `GET /payment-methods` que baraja).
- **`replace` no devuelve `Result`**: `Snapshot::from_toml_str` es el único constructor y ya
  valida todo (keys registradas incluidas), así que no queda nada que pueda fallar.
- **`poll_file(path, interval)`** es el fallback a `PollWatcher`; `watch_file` no cambia de firma.
- **El watcher compara contenido, no filtra por path**: con un `ConfigMap` el evento es sobre
  `..data`, no sobre el archivo. Un contenido ya visto (válido o no) no se reaplica.
- **`TomlError`** envuelve el error de `toml` para no meterlo en el contrato.

## Trampas

- **`flag_key!` registra por binario** (linkme). Un test o doctest que la use contamina a todos
  los del mismo binario: los tests van en su propio fichero de `tests/`, y los doctests con
  `standalone_crate`, porque la edición 2024 fusiona los doctests en un binario.
- **En Linux, leer el archivo genera un evento de acceso**: si el watcher no ignorase los
  accesos, cada recarga dispararía otra.
- **`PollWatcher` compara mtime con resolución de segundos**: por eso `poll_file` activa
  `compare_contents`.
- `publish = false` hasta la fase 1 del roadmap. Para publicar: quitarlo, añadir `description`,
  `license` y `repository`, y esa licencia al `allow` de `deny.toml`.
