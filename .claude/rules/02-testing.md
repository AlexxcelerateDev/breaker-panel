# Testing

## Unitarios (el estándar de calidad)

- **Inline** en `src/` con `#[cfg(test)] mod tests`, junto al código que prueban.
- **Puros**, sin red ni disco: `cargo test` entero corre en menos de un segundo.
- Pasan por el mismo gate estricto: `clippy --all-targets` lintea los tests. Solo están exentos
  `unwrap`/`expect` (`clippy.toml`) y el límite de líneas de 01: en un test, lo que ocupa son
  datos, y una tabla de casos (abajo) lo pasa enseguida.

## Diseño para que el código sea testeable

- **El entorno no se puede mockear**: en edición 2024 `std::env::set_var` es `unsafe` y el crate
  declara `unsafe_code = "deny"`. En una librería da igual porque no lee el entorno (ver 01) — y
  esta es una de las razones.
- **Nada de estado global**: los tests corren en hilos del mismo proceso y comparten cualquier
  `OnceLock`; uno que lo inicialice vuelve al resto dependiente del orden.
- **El tiempo y la aleatoriedad se inyectan**, o el test de una expiración es no determinista.
- **Async**: si una función solo hace `await` sobre I/O, no hay decisión que probar — es
  cableado. La señal para extraer la decisión a una `fn` síncrona pura y probar esa.

## Tests tabulares: array + `for`

Para matrices de casos, un array y un `for` hacen lo que daría `rstest` sin añadir nada. Lo no
negociable: **el assert identifica la fila que falla**, o el fallo no dice nada.

```rust
for (key, esperado) in casos {
    assert_eq!(is_key(key), esperado, "is_key({key:?})");
}
```

El `for` para en el primer fallo. Aceptable; si una matriz necesita ver todos, un test por fila.

## Doctests

Son los `# Examples` de la doc y corren con `cargo test` (con `--lib` **no**: por eso el gate
no lo lleva). Enseñan la API como la ve el consumidor —importan el crate por su nombre—, así que
también son la prueba de que lo que dice la doc compila.

## `tests/`: el crate visto desde fuera

Cada fichero de `tests/` es un crate aparte que solo ve la API pública. Sirve para flujos que
cruzan varios módulos; para una función suelta, su doctest ya hace ese papel. Datos de prueba
en `tests/fixtures/`, nunca en un `setup` global.

## Reglas de estilo

- Nombre del test = comportamiento (`segment_rechaza_el_punto`), no `test_1`.
- Un comportamiento por test; arrange/act/assert reconocibles.
- `assert_eq!` sobre `assert!(a == b)`: muestra el valor al fallar. Por lo mismo,
  `assert_matches!` (`use std::assert_matches;`, desde 1.96) sobre `assert!(matches!(..))`: enseña
  el valor y el patrón sin pasarle un `"{x:?}"` a mano.
- Nada de `sleep` ni esperas por reloj: si hace falta, faltaba inyectar el tiempo.

## Qué NO añadir (y cuándo sí)

| Herramienta | Cuándo sí |
|---|---|
| `rstest` | Nunca por parametrizar: eso lo hace un `for`. Solo con fixtures compartidas de verdad. |
| `proptest` | Un invariante que valga la pena fuzzear (parse↔format redondo). No para 6 casos borde. |
| `mockall` | Casi nunca: si hay que mockear, la lógica estaba en la capa equivocada. |
| `insta` | Salidas grandes y estables. Para un `assert_eq!` de tres campos es peor. |
| `cargo-nextest` | Cuando la suite crezca o un test necesite aislamiento por proceso. |
| `cargo-llvm-cov` | Diagnóstico puntual. **Nunca como gate con umbral**: genera tests que no prueban nada. |

## Qué testear al añadir código

- Todo validador o parser → casos borde exhaustivos inline (límite exacto, vacío, unicode).
- Toda función pública → su doctest en `# Examples`.
- Enums con transiciones o permisos → la matriz completa.
- Toda lógica con reloj o aleatoriedad → parámetro inyectado + test de los límites exactos.
