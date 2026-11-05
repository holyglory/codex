mod atomic;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::fmt;
use std::fs::File;
use std::io;
use std::path::Path;
use thiserror::Error;

pub use atomic::AtomicWriteMode;
pub use atomic::write_file_atomically;

#[derive(Error)]
pub enum PrivateStorageError {
    #[error("private storage target is invalid")]
    InvalidTarget,
    #[error("private storage target already exists")]
    AlreadyExists,
    #[error("private storage permissions do not match the required private policy")]
    ProtectionMismatch,
    #[error("private storage operation failed")]
    Io {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("private storage update failed and temporary-file cleanup also failed")]
    Cleanup {
        operation_error: io::Error,
        cleanup_error: io::Error,
    },
    #[error("private storage update committed, but temporary-file cleanup failed")]
    CommittedCleanupUncertain {
        #[source]
        source: io::Error,
    },
    #[error("private storage update committed, but parent durability is uncertain")]
    CommittedDurabilityUncertain {
        #[source]
        source: io::Error,
    },
    #[error("private storage update committed, but protection verification failed")]
    CommittedProtectionUncertain,
}

impl fmt::Debug for PrivateStorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidTarget => "PrivateStorageError::InvalidTarget",
            Self::AlreadyExists => "PrivateStorageError::AlreadyExists",
            Self::ProtectionMismatch => "PrivateStorageError::ProtectionMismatch",
            Self::Io { .. } => "PrivateStorageError::Io([redacted])",
            Self::Cleanup { .. } => "PrivateStorageError::Cleanup([redacted])",
            Self::CommittedCleanupUncertain { .. } => {
                "PrivateStorageError::CommittedCleanupUncertain([redacted])"
            }
            Self::CommittedDurabilityUncertain { .. } => {
                "PrivateStorageError::CommittedDurabilityUncertain([redacted])"
            }
            Self::CommittedProtectionUncertain => {
                "PrivateStorageError::CommittedProtectionUncertain"
            }
        })
    }
}

impl PrivateStorageError {
    pub fn io(operation: &'static str, source: io::Error) -> Self {
        Self::Io { operation, source }
    }

    pub fn operation(&self) -> Option<&'static str> {
        match self {
            Self::Io { operation, .. } => Some(operation),
            Self::InvalidTarget
            | Self::AlreadyExists
            | Self::ProtectionMismatch
            | Self::Cleanup { .. }
            | Self::CommittedCleanupUncertain { .. }
            | Self::CommittedDurabilityUncertain { .. }
            | Self::CommittedProtectionUncertain => None,
        }
    }
}

pub fn ensure_private_directory(path: &Path) -> Result<(), PrivateStorageError> {
    std::fs::create_dir_all(path)
        .map_err(|source| PrivateStorageError::io("create private directory", source))?;
    platform::protect_directory(path)?;
    verify_private_directory(path)
}

pub fn verify_private_directory(path: &Path) -> Result<(), PrivateStorageError> {
    let metadata = std::fs::metadata(path)
        .map_err(|source| PrivateStorageError::io("inspect private directory", source))?;
    if !metadata.is_dir() {
        return Err(PrivateStorageError::InvalidTarget);
    }
    platform::verify_directory(path)
}

pub fn ensure_private_file(path: &Path) -> Result<(), PrivateStorageError> {
    let metadata = std::fs::metadata(path)
        .map_err(|source| PrivateStorageError::io("inspect private file", source))?;
    if !metadata.is_file() {
        return Err(PrivateStorageError::InvalidTarget);
    }
    platform::protect_file(path)?;
    verify_private_file(path)
}

pub fn verify_private_file(path: &Path) -> Result<(), PrivateStorageError> {
    let metadata = std::fs::metadata(path)
        .map_err(|source| PrivateStorageError::io("inspect private file", source))?;
    if !metadata.is_file() {
        return Err(PrivateStorageError::InvalidTarget);
    }
    platform::verify_file(path)
}

pub fn open_private_read_write(path: &Path) -> Result<File, PrivateStorageError> {
    let parent = path.parent().ok_or(PrivateStorageError::InvalidTarget)?;
    ensure_private_directory(parent)?;
    let file = platform::open_private_read_write(path)?;
    ensure_private_file(path)?;
    Ok(file)
}

pub fn sync_parent_directory(path: &Path) -> Result<(), PrivateStorageError> {
    let parent = path.parent().ok_or(PrivateStorageError::InvalidTarget)?;
    platform::sync_directory(parent)
        .map_err(|source| PrivateStorageError::CommittedDurabilityUncertain { source })
}

#[cfg(unix)]
mod platform {
    pub(super) use crate::unix::*;
}

#[cfg(windows)]
mod platform {
    pub(super) use crate::windows::*;
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
