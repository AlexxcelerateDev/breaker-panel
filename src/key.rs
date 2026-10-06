use crate::FlagError;

/// Validates a segment that comes from outside before building a key with it.
///
/// Rejects `.` —in a key it separates levels— and anything that does not match `^[a-z0-9_]+$`.
/// Without this, a `"method": "paypal.refund"` in a request body would query a different key.
///
/// # Errors
///
/// [`FlagError::InvalidSegment`] if `s` is empty or contains anything outside `[a-z0-9_]`.
///
/// # Examples
///
/// ```
/// use std::assert_matches;
/// use breaker_panel::{FlagError, segment};
///
/// assert_eq!(segment("paypal"), Ok("paypal"));
/// assert_matches!(segment("paypal.refund"), Err(FlagError::InvalidSegment { .. }));
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

/// `^[a-z0-9_]+(\.[a-z0-9_]+)*$`, without pulling in a regex dependency.
///
/// `const` so that `flag_key!` rejects a malformed key at compile time: otherwise it would fail
/// at startup saying it is missing from the file, where it could never be.
#[doc(hidden)]
pub const fn is_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    // `true` at the start and after each `.`: a segment has to begin there.
    let mut at_segment_start = true;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'.' if !at_segment_start => at_segment_start = true,
            b'a'..=b'z' | b'0'..=b'9' | b'_' => at_segment_start = false,
            _ => return false,
        }
        i += 1;
    }
    !at_segment_start
}

fn is_segment(s: &str) -> bool {
    !s.contains('.') && is_key(s)
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
