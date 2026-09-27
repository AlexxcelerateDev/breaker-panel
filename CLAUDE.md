# plantilla-lib — plantilla de librería Rust

Plantilla de la que se parten librerías nuevas. Sale de las prácticas de `proy`, quitando lo que
solo tiene sentido en un binario: Axum, sqlx, Redis, config por entorno, Docker, arranque.

Las convenciones están en `.claude/rules/` (toolchain, diseño de la librería, tests). Este
archivo solo cubre lo que no se deduce leyendo el código.

## Gate completo, lo mismo que corre el CI

```bash
cargo fmt --all --check && CARGO_BUILD_WARNINGS=deny cargo clippy --all-targets --locked && cargo test --locked && CARGO_BUILD_WARNINGS=deny cargo doc --no-deps --locked && cargo deny check
```

`cargo deny` no viene con rustup: `cargo install cargo-deny --locked` una vez por máquina.

## Arrancar una librería desde la plantilla

*(Borrar esta sección en la librería derivada.)*

El repo está marcado como *template* en GitHub: la librería nueva nace con un solo commit y sin
el historial de la plantilla.

```bash
gh repo create <nombre> --private --template AlexxcelerateDev/plantilla-lib --clone && cd <nombre>
```

1. `Cargo.toml` → `name`.
2. `grep -rn plantilla .` y cambiar lo que salga. **Es el paso que se olvida**: los doctests
   importan el crate por su nombre (`plantilla_lib`, con `_`), así que renombrar solo el
   `Cargo.toml` los deja sin compilar — y `cargo test --lib` no lo vería.
3. La primera línea del `//!` de `src/lib.rs`: qué hace el crate.
4. `trimmed` y `Error` son el ejemplo de los patrones: se borran con el primer módulo real
   (el `Error` probablemente se queda, con otras variantes).
5. ¿Se va a publicar en crates.io? Quitar `publish = false`, añadir `description`, `license` y
   `repository`, y esa licencia al `allow` de `deny.toml`.
