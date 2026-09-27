use crate::FlagError;

/// Valida un segmento que llega de fuera antes de armar una key con él.
///
/// Rechaza el `.` —en una key separa niveles— y todo lo que no cumpla `^[a-z0-9_]+$`. Sin esto,
/// un `"method": "paypal.refund"` en el body de una petición consultaría otra key.
///
/// # Errors
///
/// [`FlagError::InvalidSegment`] si `s` está vacío o tiene algo fuera de `[a-z0-9_]`.
///
/// # Examples
///
/// ```
/// use breaker_panel::{FlagError, segment};
///
/// assert_eq!(segment("paypal"), Ok("paypal"));
/// assert!(matches!(segment("paypal.refund"), Err(FlagError::InvalidSegment { .. })));
/// ```
pub fn segment(s: &str) -> Result<&str, FlagError> {
    if is_segment(s) {
        Ok(s)
    } else {
        Err(FlagError::InvalidSegment {
            segment: s.to_owned(),
        })
    }
}

/// `^[a-z0-9_]+(\.[a-z0-9_]+)*$`, sin arrastrar una dependencia de regex.
pub(crate) fn is_key(key: &str) -> bool {
    key.split('.').all(is_segment)
}

fn is_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_solo_acepta_minusculas_digitos_y_guion_bajo() {
        let casos = [
            ("paypal", true),
            ("stripe_v2", true),
            ("0", true),
            ("", false),
            ("paypal.refund", false),
            ("PayPal", false),
            ("pay-pal", false),
            ("pay pal", false),
            ("paypal\n", false),
            // Bytes, no `char::is_alphanumeric`: una `ñ` es alfanumérica pero no entra.
            ("españa", false),
        ];
        for (s, valido) in casos {
            assert_eq!(segment(s).is_ok(), valido, "segment({s:?})");
        }
    }

    #[test]
    fn is_key_exige_segmentos_no_vacios() {
        let casos = [
            ("payments", true),
            ("payments.methods.paypal.refund", true),
            ("", false),
            (".payments", false),
            ("payments.", false),
            ("payments..paypal", false),
            ("payments.PayPal", false),
        ];
        for (key, valida) in casos {
            assert_eq!(is_key(key), valida, "is_key({key:?})");
        }
    }
}
