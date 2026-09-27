use std::collections::BTreeMap;

use crate::{LoadError, Resolved};

/// La usa [`flag_key!`] para que el consumidor no tenga que depender de `linkme`.
#[doc(hidden)]
pub use linkme;

/// Las keys declaradas con [`flag_key!`] en todo el binario. La rellena el linker: no es estado
/// que se escriba en runtime.
#[doc(hidden)]
#[linkme::distributed_slice]
pub static KEYS: [&'static str];

/// Declara una key como `static` y la registra: toda carga —la inicial y cada recarga— falla si
/// el archivo no la tiene.
///
/// El registro es por binario (lo arma el linker): en los tests, cada fichero de `tests/` solo
/// ve las keys que declara él. Las keys armadas con `format!` no se registran; si faltan, dan
/// [`FlagError::Unknown`](crate::FlagError::Unknown) en runtime.
///
/// # Examples
///
/// Un doctest que la use va con `standalone_crate`: en edición 2024 los doctests se fusionan en
/// un solo binario, y la key registrada se exigiría en todos.
///
/// ```rust,standalone_crate
/// use breaker_panel::{Flags, LoadError, flag_key};
///
/// flag_key!(CHARGE = "payments.ops.charge");
///
/// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments.ops.charge\" = { enabled = true }")?;
/// flags.require(CHARGE)?;
///
/// assert!(matches!(Flags::<()>::from_toml_str("[flags]"), Err(LoadError::MissingKey { .. })));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[macro_export]
macro_rules! flag_key {
    ($(#[$attr:meta])* $vis:vis $name:ident = $key:literal) => {
        $(#[$attr])*
        #[$crate::linkme::distributed_slice($crate::KEYS)]
        #[linkme(crate = $crate::linkme)]
        $vis static $name: &str = $key;
    };
}

pub(crate) fn check<M>(flags: &BTreeMap<String, Resolved<M>>) -> Result<(), LoadError> {
    match KEYS.iter().find(|key| !flags.contains_key(**key)) {
        Some(key) => Err(LoadError::MissingKey {
            key: (*key).to_owned(),
        }),
        None => Ok(()),
    }
}
