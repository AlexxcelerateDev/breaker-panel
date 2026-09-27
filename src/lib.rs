//! Kill switches jerárquicos embebidos: archivo TOML local, hot-reload y cascada, sin servicio externo.
//!
//! Las keys son jerárquicas (`payments.methods.paypal`): apagar un nodo apaga todo lo que cuelga
//! de él, y la consulta dice quién lo apagó y por qué. La cascada se resuelve al cargar; en
//! runtime cada consulta es un load atómico y un lookup, sin locks.
//!
//! ```
//! use breaker_panel::{FlagError, Flags, segment};
//!
//! let flags: Flags = Flags::from_toml_str(r#"
//!     [flags]
//!     "payments"                       = { enabled = true }
//!     "payments.methods.paypal"        = { enabled = false, reason = "PayPal no responde" }
//!     "payments.methods.paypal.refund" = { enabled = true }
//!     "payments.ops.refund"            = { enabled = true }
//! "#)?;
//!
//! // POST /refunds: un `require` por dimensión. El segmento llega del usuario: se valida antes.
//! let m = segment("paypal")?;
//! let Err(FlagError::Disabled { disabled_by, reason, .. }) =
//!     flags.require(format!("payments.methods.{m}.refund"))
//! else {
//!     panic!("el método está apagado, y con él su refund");
//! };
//! assert_eq!(disabled_by, "payments.methods.paypal");
//! assert_eq!(reason, "PayPal no responde");
//! assert_eq!(flags.require("payments.ops.refund"), Ok(()));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Features
//!
//! - `watch` (default): [`Flags::watch_file`] y [`Flags::poll_file`], la recarga en caliente.
//! - `registry` (default): [`flag_key!`], keys declaradas en código y validadas en cada carga.
//!
//! # Varias réplicas
//!
//! Cada proceso recarga por su cuenta, así que mientras el archivo se propaga dos réplicas
//! pueden responder distinto. Un listado (`GET`) es informativo; la autoridad es el `require`
//! de la operación (`POST`).
//!
//! La guía de modelado de keys está en el README.

mod error;
mod flags;
mod key;
#[cfg(feature = "registry")]
mod registry;
mod snapshot;
#[cfg(feature = "watch")]
mod watch;

pub use error::{FlagError, LoadError, TomlError};
pub use flags::{Diff, Flags};
pub use key::segment;
#[cfg(feature = "registry")]
#[doc(hidden)]
pub use registry::{KEYS, linkme};
pub use snapshot::{Resolved, Snapshot};
#[cfg(feature = "watch")]
pub use watch::Watcher;

/// Los bloques de código del README corren como doctests: el ejemplo de la portada no se pudre.
/// Uno usa `flag_key!`, de ahí la feature.
#[cfg(all(doctest, feature = "registry"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
