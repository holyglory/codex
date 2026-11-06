//! Private per-user usage storage, using the repository's shared protection API.
//! Only owned usage directories are protected; CODEX_HOME and generic ancestors
//! retain their existing permissions. Payloads are written only after protection.
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Component;
use std::path::Path;

use codex_private_storage::PrivateStorageError;
use codex_private_storage::ensure_private_directory;
use codex_private_storage::ensure_private_file;

use crate::Result;
use crate::StorageError;
use crate::USAGE_DIRECTORY_NAME;

pub(crate) fn ensure_usage_directory(root: &Path) -> Result<()> {
    ensure_private_directory(root).map_err(private_error)?;
    ensure_private_directory(&root.join("segments")).map_err(private_error)
}

pub(crate) fn open_append(path: &Path) -> Result<File> {
    prepare_parent(path)?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)?;
    ensure_private_file(path).map_err(private_error)?;
    Ok(file)
}

pub(crate) fn open_read_write(path: &Path) -> Result<File> {
    prepare_parent(path)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    ensure_private_file(path).map_err(private_error)?;
    Ok(file)
}

pub(crate) fn create_new(path: &Path) -> Result<File> {
    prepare_parent(path)?;
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    ensure_private_file(path).map_err(private_error)?;
    Ok(file)
}

pub(crate) fn protect_existing(path: &Path) -> Result<()> {
    ensure_private_file(path).map_err(private_error)?;
    protect_owned_parent(path)
}

/// DuckDB creates files inside an already-private directory. Protect and verify
/// its completed output before making any manifest reference visible.
pub(crate) fn finish_generated(path: &Path) -> Result<()> {
    protect_existing(path)?;
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

fn prepare_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    protect_owned_parent(path)
}

fn protect_owned_parent(path: &Path) -> Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(root) = parent.ancestors().find(|path| {
        path.file_name()
            .is_some_and(|name| name == USAGE_DIRECTORY_NAME)
    }) else {
        return Ok(());
    };
    // A generic relative path must not redirect directory protection outside
    // the owned namespace. Its file still receives private permissions.
    let suffix = parent
        .strip_prefix(root)
        .map_err(|_| StorageError::InvalidData("invalid usage parent".into()))?;
    if suffix
        .components()
        .any(|component| component == Component::ParentDir)
    {
        return Ok(());
    }
    ensure_private_directory(root).map_err(private_error)?;
    if parent != root {
        ensure_private_directory(parent).map_err(private_error)?;
    }
    Ok(())
}

fn private_error(error: PrivateStorageError) -> StorageError {
    match error {
        PrivateStorageError::Io { source, .. } => StorageError::Io(source),
        error => StorageError::Io(io::Error::other(error)),
    }
}
