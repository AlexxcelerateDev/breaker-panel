use std::{error::Error, fmt, io, iter, path::PathBuf};

/// Why a query does not let the operation through.
///
/// Mapping it to HTTP is up to the app: `InvalidSegment`, and an `Unknown` on a key built from
/// user input, are usually a 400; an `Unknown` on any other key is a configuration bug (500);
/// `Disabled` is a 503 with the `reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FlagError {
    /// The key is disabled, either itself or a declared ancestor.
    Disabled {
        /// The queried key.
        key: String,
        /// The disabled ancestor closest to the root, or the key itself.
        disabled_by: String,
        /// The `reason` of `disabled_by`, written for the end user.
        reason: String,
    },
    /// The key is not declared in the file: it fails closed, never a silent `false`.
    Unknown {
        /// The queried key.
        key: String,
    },
    /// A segment coming from outside does not match `^[a-z0-9_]+$`. Returned by
    /// [`segment`](crate::segment).
    InvalidSegment {
        /// The segment as it arrived.
        segment: String,
    },
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:?}` on whatever may come from outside: it quotes it and escapes newlines.
        match self {
            Self::Disabled {
                key,
                disabled_by,
                reason,
            } if key == disabled_by => write!(f, "{key:?} is disabled: {reason}"),
            Self::Disabled {
                key,
                disabled_by,
                reason,
            } => write!(f, "{key:?} is disabled by {disabled_by:?}: {reason}"),
            Self::Unknown { key } => write!(f, "key {key:?} is not declared"),
            Self::InvalidSegment { segment } => write!(f, "invalid segment: {segment:?}"),
        }
    }
}

impl Error for FlagError {}

/// Why a flags file could not be loaded or reloaded. On a reload, any of these keeps the
/// previous snapshot in effect.
///
/// `Display` follows the std convention: only the top level, with the cause behind `source()`.
/// With `{:#}` it also writes the chain of causes, including the line and column when the TOML
/// does not parse: that is what you want in a log.
///
/// ```
/// use breaker_panel::Snapshot;
///
/// let e = Snapshot::<()>::from_toml_str("[flags]\n\"a\" = {").unwrap_err();
/// assert_eq!(e.to_string(), "invalid flags file");
/// assert!(format!("{e:#}").contains("line 2"));
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be read.
    Io {
        /// The file, as an absolute path (the original one if it could not be resolved).
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// The file's directory could not be watched: at startup, or later (through
    /// `Watcher::on_reject`) if it was deleted or recreated.
    Watch {
        /// The watched directory, as an absolute path.
        path: PathBuf,
        /// The cause.
        source: io::Error,
    },
    /// Invalid TOML, an unknown field, or a `meta` that does not deserialize into `M`.
    Toml(TomlError),
    /// A key does not match `^[a-z0-9_]+(\.[a-z0-9_]+)*$`.
    InvalidKey {
        /// The key as written in the file.
        key: String,
    },
    /// An entry with `enabled = false` has no `reason`, or an empty one.
    MissingReason {
        /// The entry's key.
        key: String,
    },
    /// A key declared with `flag_key!` is not in the file. If several are missing, the first in
    /// alphabetical order: the same one on every build.
    MissingKey {
        /// The registered key.
        key: String,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.headline(f)?;
        if f.alternate() {
            for cause in iter::successors(self.source(), |&cause| cause.source()) {
                write!(f, ": {cause}")?;
            }
        }
        Ok(())
    }
}

impl LoadError {
    fn headline(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => write!(f, "failed to read {}", path.display()),
            Self::Watch { path, .. } => write!(f, "failed to watch {}", path.display()),
            Self::Toml(_) => f.write_str("invalid flags file"),
            Self::InvalidKey { key } => write!(f, "invalid key format: {key:?}"),
            Self::MissingReason { key } => write!(f, "{key:?} is disabled without a `reason`"),
            Self::MissingKey { key } => {
                write!(f, "registered key {key:?} is missing from the file")
            }
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } | Self::Watch { source, .. } => Some(source),
            Self::Toml(e) => Some(e),
            Self::InvalidKey { .. } | Self::MissingReason { .. } | Self::MissingKey { .. } => None,
        }
    }
}

/// The details of a [`LoadError::Toml`]: the parser's message, with line and column.
///
/// Opaque on purpose: exposing the `toml` error would make every `toml` major release a major
/// release of this crate.
#[derive(Debug)]
pub struct TomlError(pub(crate) toml::de::Error);

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for TomlError {}
