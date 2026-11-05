use std::fmt;

/// Errors returned by the usage journal, manifest, and in-memory projections.
#[derive(Debug)]
pub enum StorageError {
    /// The underlying file operation failed.
    Io(std::io::Error),
    /// A JSONL or manifest payload could not be decoded.
    Json(serde_json::Error),
    /// A decoded payload violated a storage invariant.
    InvalidData(String),
    /// A requested columnar backend is not linked into this build yet.
    UnsupportedFormat(&'static str),
    /// An Arrow or Parquet codec rejected a segment payload.
    Columnar(String),
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "usage storage I/O error: {error}"),
            Self::Json(error) => write!(f, "usage storage JSON error: {error}"),
            Self::InvalidData(message) => write!(f, "invalid usage storage data: {message}"),
            Self::UnsupportedFormat(format) => {
                write!(f, "usage storage format is not available: {format}")
            }
            Self::Columnar(message) => write!(f, "usage columnar storage error: {message}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::InvalidData(_) | Self::UnsupportedFormat(_) | Self::Columnar(_) => None,
        }
    }
}

impl From<std::io::Error> for StorageError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for StorageError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Result alias used by all public storage operations.
pub type Result<T> = std::result::Result<T, StorageError>;
