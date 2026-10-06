//! Embedded hierarchical kill switches: a local TOML file, hot reload and cascading, no external service.
//!
//! Keys are hierarchical (`payments.methods.paypal`): disabling a node disables everything
//! under it, and the query says who disabled it and why. The cascade is resolved at load time;
//! at runtime each query is an atomic load and a lookup, with no locks.
//!
//! ```
//! use breaker_panel::{FlagError, Flags, segment};
//!
//! let flags: Flags = Flags::from_toml_str(r#"
//!     [flags]
//!     "payments"                       = { enabled = true }
//!     "payments.methods.paypal"        = { enabled = false, reason = "PayPal is not responding" }
//!     "payments.methods.paypal.refund" = { enabled = true }
//!     "payments.ops.refund"            = { enabled = true }
//! "#)?;
//!
//! // POST /refunds: one `require` per dimension. The segment is user input: validate it first.
//! let m = segment("paypal")?;
//! let Err(FlagError::Disabled { disabled_by, reason, .. }) =
//!     flags.require(format!("payments.methods.{m}.refund"))
//! else {
//!     panic!("the method is disabled, and its refund with it");
//! };
//! assert_eq!(disabled_by, "payments.methods.paypal");
//! assert_eq!(reason, "PayPal is not responding");
//! assert_eq!(flags.require("payments.ops.refund"), Ok(()));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! # Features
//!
//! - `watch` (default): [`Flags::watch_file`] and [`Flags::poll_file`], hot reloading.
//! - `registry` (default): [`flag_key!`], keys declared in code and validated on every load.
//!
//! # Observability
//!
//! A rejected reload keeps the previous snapshot in effect. Record it with
//! [`Watcher::on_reject`]: otherwise it only reaches the library's log, which a per-crate filter
//! drops. To log a full [`LoadError`], use `{:#}`. `tracing` events:
//!
//! | Event | Target | Level |
//! |---|---|---|
//! | Reload applied, with its revision | `breaker_panel::flags` | `info` |
//! | Reload rejected, with the chain of causes (line and column if the TOML does not parse) | `breaker_panel::watch` | `warn` |
//! | Event-based watching stopped: the directory was deleted or recreated (also sent to `on_reject`) | `breaker_panel::watch` | `warn` |
//! | `poll_file` with an interval under 100 ms, which is raised to 100 | `breaker_panel::watch` | `warn` |
//! | An `on_change` or `on_reject` callback panicked | `breaker_panel::flags` | `error` |
//! | `require` denied: **one per call**, so under load with a switch off it is one line per request | `breaker_panel::snapshot` | `debug` |
//!
//! # Multiple replicas
//!
//! Each process reloads on its own, so while the file propagates two replicas may answer
//! differently. A listing (`GET`) is informational; the authority is the operation's `require`
//! (`POST`). To check what each one applied, [`Snapshot::toml`].
//!
//! The key modeling guide is in the README.

// Without the feature, what it brings does not exist and its link above would be broken: point
// it at `# Features`.
#![cfg_attr(
    not(feature = "watch"),
    doc = "",
    doc = "[`Flags::watch_file`]: #features",
    doc = "[`Flags::poll_file`]: #features",
    doc = "[`Watcher::on_reject`]: #features"
)]
#![cfg_attr(not(feature = "registry"), doc = "", doc = "[`flag_key!`]: #features")]
// 15-line ceiling per function (threshold in `clippy.toml`), only for production code: in a test
// the length is data, and `[lints]` in `Cargo.toml` cannot tell tests apart. See
// `.claude/rules/01-library.md`.
#![cfg_attr(not(test), warn(clippy::too_many_lines))]

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
pub use registry::{KEYS, is_key, linkme};
pub use snapshot::{Resolved, Snapshot};
#[cfg(feature = "watch")]
pub use watch::Watcher;

/// The README's code blocks run as doctests: the front-page example does not rot. One uses
/// `flag_key!`, hence the feature.
#[cfg(all(doctest, feature = "registry"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
