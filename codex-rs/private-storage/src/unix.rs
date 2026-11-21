use crate::AtomicWriteMode;
use crate::PrivateStorageError;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub(super) fn protect_directory(path: &Path) -> Result<(), PrivateStorageError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|source| PrivateStorageError::io("protect private directory", source))
}

pub(super) fn verify_directory(path: &Path) -> Result<(), PrivateStorageError> {
    verify_mode(path, /*required*/ 0o700)
}

pub(super) fn protect_file(path: &Path) -> Result<(), PrivateStorageError> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|source| PrivateStorageError::io("protect private file", source))
}

pub(super) fn verify_file(path: &Path) -> Result<(), PrivateStorageError> {
    verify_mode(path, /*required*/ 0o600)
}

pub(super) fn open_private_read_write(path: &Path) -> Result<File, PrivateStorageError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).mode(0o600);
    options
        .open(path)
        .map_err(|source| PrivateStorageError::io("open private file", source))
}

pub(super) fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

pub(super) fn install_file(
    source: &Path,
    destination: &Path,
    mode: AtomicWriteMode,
) -> Result<(), PrivateStorageError> {
    match mode {
        AtomicWriteMode::Replace => std::fs::rename(source, destination)
            .map_err(|source| PrivateStorageError::io("replace private file", source)),
        AtomicWriteMode::NoClobber => match std::fs::hard_link(source, destination) {
            Ok(()) => {
                std::fs::remove_file(source)
                    .map_err(|source| PrivateStorageError::CommittedCleanupUncertain { source })?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                Err(PrivateStorageError::AlreadyExists)
            }
            Err(source) => Err(PrivateStorageError::io("install private file", source)),
        },
    }
}

fn verify_mode(path: &Path, required: u32) -> Result<(), PrivateStorageError> {
    let mode = std::fs::metadata(path)
        .map_err(|source| PrivateStorageError::io("inspect private permissions", source))?
        .mode()
        & 0o777;
    if mode == required {
        Ok(())
    } else {
        Err(PrivateStorageError::ProtectionMismatch)
    }
}
