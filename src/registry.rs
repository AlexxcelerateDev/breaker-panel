use std::collections::BTreeMap;

use crate::{LoadError, Resolved};

/// La usa [`flag_key!`] para comprobar el formato de la key al compilar.
#[doc(hidden)]
pub use crate::key::is_key;

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
/// El registro es por binario (lo arma el linker): es estado global, aunque de solo lectura.
/// Por eso el mismo texto puede cargar en un binario y fallar en otro, y cada test del
/// consumidor que construya un `Snapshot` tiene que incluir todas las keys registradas en su
/// binario. En los tests de este crate, cada fichero de `tests/` solo ve las que declara él.
///
/// Las keys armadas con `format!` no se registran; si faltan, dan
/// [`FlagError::Unknown`](crate::FlagError::Unknown) en runtime.
///
/// # Examples
///
/// Un doctest que la use va con `standalone_crate`: en edición 2024 los doctests se fusionan en
/// un solo binario, y la key registrada se exigiría en todos.
///
/// ```rust,standalone_crate
/// use std::assert_matches;
/// use breaker_panel::{Flags, LoadError, flag_key};
///
/// flag_key!(CHARGE = "payments.ops.charge");
///
/// let flags: Flags = Flags::from_toml_str("[flags]\n\"payments.ops.charge\" = { enabled = true }")?;
/// flags.require(CHARGE)?;
///
/// assert_matches!(Flags::<()>::from_toml_str("[flags]"), Err(LoadError::MissingKey { .. }));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// Una key que no cumple `^[a-z0-9_]+(\.[a-z0-9_]+)*$` no compila (E0080: la evaluación de
/// una constante falla):
///
/// ```compile_fail,E0080
/// breaker_panel::flag_key!(MAL = "payments.Methods.paypal");
/// ```
#[macro_export]
macro_rules! flag_key {
    ($(#[$attr:meta])* $vis:vis $name:ident = $key:literal) => {
        const _: () = ::core::assert!(
            $crate::is_key($key),
            ::core::concat!("flag_key!: ", $key, " no es una key válida (a-z, 0-9, _, y . entre segmentos)"),
        );
        $(#[$attr])*
        #[$crate::linkme::distributed_slice($crate::KEYS)]
        #[linkme(crate = $crate::linkme)]
        $vis static $name: &str = $key;
    };
}

pub(crate) fn check<M>(flags: &BTreeMap<String, Resolved<M>>) -> Result<(), LoadError> {
    // La menor y no la primera: el orden de `KEYS` lo pone el linker y cambia entre builds.
    match KEYS.iter().filter(|key| !flags.contains_key(**key)).min() {
        Some(key) => Err(LoadError::MissingKey {
            key: (*key).to_owned(),
        }),
        None => Ok(()),
    }
}
