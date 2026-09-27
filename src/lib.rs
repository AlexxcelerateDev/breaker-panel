//! Qué hace este crate, en una línea: es lo primero que se ve en docs.rs y en el IDE.
//!
//! [`trimmed`] y [`Error`] son el ejemplo de los patrones de la plantilla —error propio,
//! `# Errors`, doctest, test tabular— y se borran con el primer módulo de verdad.

use std::fmt;

/// Lo que puede fallar al llamar a este crate.
///
/// `#[non_exhaustive]` porque es API pública: añadir una variante deja de ser un cambio
/// incompatible, ya que quien hace `match` desde fuera está obligado a llevar un brazo `_`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// No queda nada después de recortar los espacios.
    Empty,
    /// Pasa del máximo de caracteres permitido.
    TooLong {
        /// El máximo que se pidió.
        max: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("el texto está vacío"),
            Self::TooLong { max } => write!(f, "el texto pasa de {max} caracteres"),
        }
    }
}

impl std::error::Error for Error {}

/// Recorta los espacios de los extremos y exige que quede algo de a lo sumo `max` caracteres.
///
/// Cuenta caracteres, no bytes: `"ñandú"` son 5 aunque ocupe 7.
///
/// # Errors
///
/// [`Error::Empty`] si solo había espacios, y [`Error::TooLong`] si pasa de `max`.
///
/// # Examples
///
/// ```
/// use plantilla_lib::{Error, trimmed};
///
/// assert_eq!(trimmed("  hola  ", 10), Ok("hola"));
/// assert_eq!(trimmed("   ", 10), Err(Error::Empty));
/// ```
pub fn trimmed(raw: &str, max: usize) -> Result<&str, Error> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(Error::Empty);
    }
    if s.chars().count() > max {
        return Err(Error::TooLong { max });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimmed_recorta_y_acota() {
        let casos = [
            ("  hola  ", 10, Ok("hola")),
            ("", 10, Err(Error::Empty)),
            (" \t\n ", 10, Err(Error::Empty)),
            // Justo en el límite vale; uno más, no.
            ("hola", 4, Ok("hola")),
            ("hola", 3, Err(Error::TooLong { max: 3 })),
            // Caracteres, no bytes: con `len()` serían 7 y se rechazaría un texto válido.
            ("ñandú", 5, Ok("ñandú")),
            // Los espacios recortados no cuentan para el máximo.
            ("  ab  ", 2, Ok("ab")),
        ];
        for (raw, max, esperado) in casos {
            assert_eq!(trimmed(raw, max), esperado, "trimmed({raw:?}, {max})");
        }
    }
}
