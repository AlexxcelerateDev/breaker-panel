use std::{error::Error, fmt, io, path::PathBuf};

/// Por qué una consulta no deja pasar.
///
/// Cómo se traduce a HTTP lo decide la app: `InvalidSegment` y un `Unknown` sobre una key armada
/// con input del usuario suelen ser un 400; un `Unknown` sobre cualquier otra key, un bug de
/// configuración (500); `Disabled`, un 503 con el `reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FlagError {
    /// La key está apagada, ella o un ancestro declarado.
    Disabled {
        /// La key consultada.
        key: String,
        /// El ancestro apagado más cercano a la raíz, o la propia key.
        disabled_by: String,
        /// El `reason` de `disabled_by`, pensado para el usuario final.
        reason: String,
    },
    /// La key no está declarada en el archivo: falla cerrado, nunca un `false` silencioso.
    Unknown {
        /// La key consultada.
        key: String,
    },
    /// Un segmento llegado de fuera no cumple `^[a-z0-9_]+$`. Lo devuelve [`segment`](crate::segment).
    InvalidSegment {
        /// El segmento tal como llegó.
        segment: String,
    },
}

impl fmt::Display for FlagError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `{:?}` en lo que puede venir de fuera: lo entrecomilla y escapa saltos de línea.
        match self {
            Self::Disabled {
                key,
                disabled_by,
                reason,
            } if key == disabled_by => write!(f, "{key:?} está apagado: {reason}"),
            Self::Disabled {
                key,
                disabled_by,
                reason,
            } => write!(f, "{key:?} está apagado por {disabled_by:?}: {reason}"),
            Self::Unknown { key } => write!(f, "la key {key:?} no está declarada"),
            Self::InvalidSegment { segment } => write!(f, "segmento inválido: {segment:?}"),
        }
    }
}

impl Error for FlagError {}

/// Por qué no se pudo cargar o recargar un archivo de flags. Al recargar, cualquiera de estos
/// deja vigente el snapshot anterior.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// No se pudo leer el archivo.
    Io {
        /// El archivo.
        path: PathBuf,
        /// La causa.
        source: io::Error,
    },
    /// No se pudo vigilar el directorio del archivo.
    Watch {
        /// El directorio vigilado.
        path: PathBuf,
        /// La causa.
        source: io::Error,
    },
    /// TOML inválido, campo desconocido o `meta` que no deserializa en `M`.
    Toml(TomlError),
    /// Una key no cumple `^[a-z0-9_]+(\.[a-z0-9_]+)*$`.
    InvalidKey {
        /// La key tal como está en el archivo.
        key: String,
    },
    /// Una entrada con `enabled = false` no trae `reason`, o lo trae vacío.
    MissingReason {
        /// La key de la entrada.
        key: String,
    },
    /// Una key declarada con [`flag_key!`](crate::flag_key) no está en el archivo.
    MissingKey {
        /// La key registrada.
        key: String,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, .. } => write!(f, "no se pudo leer {}", path.display()),
            Self::Watch { path, .. } => write!(f, "no se pudo vigilar {}", path.display()),
            Self::Toml(_) => f.write_str("el archivo de flags no es válido"),
            Self::InvalidKey { key } => write!(f, "key con formato inválido: {key:?}"),
            Self::MissingReason { key } => write!(f, "{key:?} está apagado sin `reason`"),
            Self::MissingKey { key } => {
                write!(f, "la key registrada {key:?} no está en el archivo")
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

/// El detalle de un [`LoadError::Toml`]: el mensaje del parser, con línea y columna.
///
/// Opaco a propósito: exponer el error de `toml` haría de cada major de `toml` un major de este
/// crate.
#[derive(Debug)]
pub struct TomlError(pub(crate) toml::de::Error);

impl fmt::Display for TomlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for TomlError {}
