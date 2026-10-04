# breaker-panel — kill switches jerárquicos para Rust

Archivo TOML local, hot-reload y cascada por prefijo de key, sin servicio externo. Los
requerimientos, lo que queda fuera de alcance y por qué, y el roadmap están en
`docs/REQUIREMENTS.md`; el README tiene la guía de modelado de keys.

Las convenciones están en `.claude/rules/` (toolchain, diseño de la librería, tests). Este
archivo solo cubre lo que no se deduce leyendo el código.

## Gate completo, lo mismo que corre el CI

```bash
cargo fmt --all --check && CARGO_BUILD_WARNINGS=deny cargo clippy --all-targets --locked && CARGO_BUILD_WARNINGS=deny cargo clippy --all-targets --locked --no-default-features && cargo test --locked && CARGO_BUILD_WARNINGS=deny cargo doc --no-deps --locked && CARGO_BUILD_WARNINGS=deny cargo doc --no-deps --locked --no-default-features && cargo deny check && cargo +1.98.0 check --all-targets --locked
```

`cargo deny` no viene con rustup: `cargo install cargo-deny --locked` una vez por máquina. El
último paso es la MSRV (`rust-version`, por debajo del pin):
`rustup toolchain install 1.98.0 --profile minimal`, también una vez.

- Es la única copia junto a `.github/workflows/ci.yml`: si cambia, cambian las dos en el mismo
  commit. El agente `gate` lo lee de aquí.
- `cargo fmt --all` corre solo tras cada `Edit`/`Write` (hook en `.claude/settings.json`).
- Commits: `tipo: resumen` en español (`feat`, `fix`, `docs`, `refactor`, `chore`), cuerpo en
  viñetas con el porqué. Un cambio de convención actualiza su regla en el mismo commit.

## Dónde se aparta del borrador de `docs/REQUIREMENTS.md` §6

- **`Snapshot` guarda un `BTreeMap`**, no un `HashMap`: con `HashMap`, `children()` saldría en
  otro orden en cada recarga y en cada réplica (un `GET /payment-methods` que baraja).
- **`replace` no devuelve `Result`**: `Snapshot::from_toml_str` es el único constructor y ya
  valida todo (keys registradas incluidas), así que no queda nada que pueda fallar.
- **`poll_file(path, interval)`** es el fallback a `PollWatcher`; `watch_file` no cambia de firma.
- **El watcher compara contenido, no filtra por path**: con un `ConfigMap` el evento es sobre
  `..data`, no sobre el archivo. Un contenido ya visto (válido o no) no se reaplica.
- **`TomlError`** envuelve el error de `toml` para no meterlo en el contrato.
- **`Watcher::on_reject`**: los requisitos solo pedían `tracing` para un rechazo, y en relay eso
  produjo un fallo mudo: una errata en otra línea dejó el servicio con el estado viejo, y el
  filtro de logs por crate se comió el aviso.
- **`on_change` avisa en orden de revisión**: `replace` se serializa entero, avisos incluidos. El
  precio es que un callback no puede llamar a `replace` ni a `Watcher::reload` sobre los mismos
  flags, porque se esperaría a sí mismo. `forbid_reentry` lo convierte en un pánico con el
  motivo, que `call_all` captura: sin él, en el hilo del watcher era un bloqueo mudo y la
  recarga moría.
- **Un mismo rechazo se avisa una vez** (`Seen` en `watch.rs`), también el de lectura y
  también con `reload()` forzado, que devuelve el error pero no vuelve a avisar. Sin esto, un
  `on_reject` que recargase desbordaba la pila.
- **La relectura del arranque hace fallar `watch_file`** si el archivo ya no es válido: aún no
  hay `on_reject` registrado, y aceptarlo dejaba la réplica atrasada sin aviso.

## Decidido no hacer (revisión de la fase 0, 2026-09-28)

- **No saltarse las recargas sin efecto**: distinguirlas exige comparar `meta`, y eso es un
  bound `M: PartialEq` nuevo. Se avisa con el `Diff` vacío.
- **No soltar `seen` antes de `replace`**: dos recargas podrían aplicarse al revés. El
  comentario en `watch.rs::apply` lo explica.
- **Ni campo `version` ni hash propio para comparar réplicas** (pregunta abierta de §15):
  una `version` la sube a mano quien edita, y si se olvida da por buena una réplica atrasada; un
  hash obliga a elegir algoritmo por el consumidor (`DefaultHasher` cambia entre versiones de
  Rust, SHA-256 añade `sha2` a todos). `Snapshot::toml()` expone el texto aplicado y la app lo
  compara o lo hashea.
- **No bajar la MSRV a 1.97**: exige un job del CI con esa versión (`.claude/rules/00`), y el
  único consumidor en 1.97 es la copia suelta de relay, sin remoto.

## Trampas

- **`flag_key!` registra por binario** (linkme). Un test o doctest que la use contamina a todos
  los del mismo binario: los tests van en su propio fichero de `tests/`, y los doctests con
  `standalone_crate`, porque la edición 2024 fusiona los doctests en un binario.
- **En Linux, leer el archivo genera un evento de acceso**: si el watcher no ignorase los
  accesos, cada recarga dispararía otra.
- **`PollWatcher` compara mtime con resolución de segundos**: por eso `poll_file` activa
  `compare_contents`.
- **FSEvents (macOS) vigila la ruta, no el inodo**: recrear o renombrar el directorio no corta la
  recarga, y `check_dir` no corre con ese backend (`follows_identity`, por `WatcherKind` y no por
  `target_os`: la feature `macos_kqueue` de `notify` lo cambia por kqueue). Además entrega
  eventos de justo antes de `watch()`, también el borrado del propio directorio, y borrar el
  padre no genera ninguno hasta que se recrea.
- `publish = false` hasta la fase 1 del roadmap. Para publicar: quitarlo, añadir `description`,
  `license` y `repository`, y esa licencia al `allow` de `deny.toml`.
