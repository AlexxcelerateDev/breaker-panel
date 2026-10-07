use std::collections::BTreeMap;

use crate::{LoadError, Resolved};

/// Used by [`flag_key!`] to check the key's format at compile time.
#[doc(hidden)]
pub use crate::key::is_key;

/// Used by [`flag_key!`] so the consumer does not have to depend on `linkme`.
#[doc(hidden)]
pub use linkme;

/// The keys declared with [`flag_key!`] across the whole binary. Filled in by the linker: it is
/// not state written at runtime.
#[doc(hidden)]
#[linkme::distributed_slice]
pub static KEYS: [&'static str];

/// Declares a key as a `static` and registers it: every load —the initial one and each
/// reload— fails if the file does not have it.
///
/// The registry is per binary (the linker builds it, from the crates it links: a dependency the
/// code never references registers nothing): it is global state, albeit read-only.
/// That is why the same text can load in one binary and fail in another, and every consumer test
/// that builds a `Snapshot` has to include all the keys registered in its binary. In this
/// crate's tests, each file in `tests/` only sees the ones it declares.
///
/// Keys built with `format!` are not registered; if they are missing, they give
/// [`FlagError::Unknown`](crate::FlagError::Unknown) at runtime.
///
/// # Examples
///
/// A doctest that uses it needs `standalone_crate`: in edition 2024 doctests are merged into a
/// single binary, and the registered key would be required in all of them.
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
///
/// A key that does not match `^[a-z0-9_]+(\.[a-z0-9_]+)*$` does not compile (E0080: evaluating
/// a constant fails):
///
/// ```compile_fail,E0080
/// breaker_panel::flag_key!(BAD = "payments.Methods.paypal");
/// ```
#[macro_export]
macro_rules! flag_key {
    ($(#[$attr:meta])* $vis:vis $name:ident = $key:literal) => {
        const _: () = ::core::assert!(
            $crate::is_key($key),
            ::core::concat!("flag_key!: ", $key, " is not a valid key (a-z, 0-9, _, and . between segments)"),
        );
        $(#[$attr])*
        #[$crate::linkme::distributed_slice($crate::KEYS)]
        #[linkme(crate = $crate::linkme)]
        $vis static $name: &str = $key;
    };
}

pub(crate) fn check<M>(flags: &BTreeMap<String, Resolved<M>>) -> Result<(), LoadError> {
    // The smallest and not the first: the order of `KEYS` is set by the linker and changes
    // between builds.
    match KEYS.iter().filter(|key| !flags.contains_key(**key)).min() {
        Some(key) => Err(LoadError::MissingKey {
            key: (*key).to_owned(),
        }),
        None => Ok(()),
    }
}
