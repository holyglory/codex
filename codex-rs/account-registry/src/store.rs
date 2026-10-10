use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use codex_private_storage::AtomicWriteMode;
use codex_private_storage::PrivateStorageError;
use codex_private_storage::ensure_private_directory;
use codex_private_storage::ensure_private_file;
use codex_private_storage::open_private_read_write;
use codex_private_storage::sync_parent_directory;
use codex_private_storage::write_file_atomically;
use thiserror::Error;

use crate::AccountRegistry;
use crate::RegistryValidationError;

const ACCOUNTS_DIR: &str = "accounts";
const REGISTRY_FILE: &str = "index.json";
const REGISTRY_LOCK_FILE: &str = ".registry.lock";

#[derive(Debug, Error)]
pub enum RegistryStoreError {
    #[error("account registry does not exist")]
    NotFound,
    #[error("account registry already exists")]
    AlreadyExists,
    #[error("account registry lock is held by another process")]
    LockBusy,
    #[error("account registry lock guard belongs to a different store")]
    GuardMismatch,
    #[error("account registry generation conflict: expected {expected}, found {actual}")]
    GenerationConflict { expected: u64, actual: u64 },
    #[error("account registry generation overflow")]
    GenerationOverflow,
    #[error("invalid account registry: {0}")]
    Validation(#[from] RegistryValidationError),
    #[error("failed to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse account registry: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "account registry update committed, but durability is uncertain while attempting to {operation}: {source}"
    )]
    CommittedDurabilityUncertain {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("private durable account registry storage is unsupported on this platform")]
    UnsupportedSecurityPlatform,
}

#[derive(Clone)]
pub struct RegistryStore {
    identity: Arc<StoreIdentity>,
    accounts_dir: PathBuf,
    index_path: PathBuf,
    lock_path: PathBuf,
}

struct StoreIdentity;

impl fmt::Debug for RegistryStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryStore")
            .finish_non_exhaustive()
    }
}

/// An advisory cross-process registry lock.
///
/// Dropping the guard explicitly unlocks the file. The kernel also recovers an abandoned lock
/// when its process exits.
pub struct RegistryLockGuard {
    owner: Arc<StoreIdentity>,
    file: File,
}

impl fmt::Debug for RegistryLockGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryLockGuard")
            .finish_non_exhaustive()
    }
}

impl Drop for RegistryLockGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl RegistryStore {
    pub fn new(codex_home: &Path) -> Self {
        let accounts_dir = codex_home.join(ACCOUNTS_DIR);
        Self {
            identity: Arc::new(StoreIdentity),
            index_path: accounts_dir.join(REGISTRY_FILE),
            lock_path: accounts_dir.join(REGISTRY_LOCK_FILE),
            accounts_dir,
        }
    }

    pub fn registry_path(&self) -> &Path {
        &self.index_path
    }

    pub fn read(&self) -> Result<AccountRegistry, RegistryStoreError> {
        if !self.index_path.exists() {
            return Err(RegistryStoreError::NotFound);
        }
        ensure_private_file(&self.index_path).map_err(private_storage_error)?;
        let file = match File::open(&self.index_path) {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(RegistryStoreError::NotFound);
            }
            Err(source) => return Err(io_error("open account registry", source)),
        };
        let registry: AccountRegistry = serde_json::from_reader(BufReader::new(file))?;
        registry.validate()?;
        Ok(registry)
    }

    pub fn acquire_lock(&self) -> Result<RegistryLockGuard, RegistryStoreError> {
        let file = self.open_lock_file()?;
        file.lock()
            .map_err(|source| io_error("acquire account registry lock", source))?;
        Ok(RegistryLockGuard {
            owner: Arc::clone(&self.identity),
            file,
        })
    }

    pub fn try_acquire_lock(&self) -> Result<RegistryLockGuard, RegistryStoreError> {
        let file = self.open_lock_file()?;
        match file.try_lock() {
            Ok(()) => Ok(RegistryLockGuard {
                owner: Arc::clone(&self.identity),
                file,
            }),
            Err(std::fs::TryLockError::WouldBlock) => Err(RegistryStoreError::LockBusy),
            Err(std::fs::TryLockError::Error(source)) => {
                Err(io_error("acquire account registry lock", source))
            }
        }
    }

    pub fn create(&self, registry: &AccountRegistry) -> Result<(), RegistryStoreError> {
        let guard = self.acquire_lock()?;
        self.create_with_guard(&guard, registry)
    }

    pub fn create_with_guard(
        &self,
        guard: &RegistryLockGuard,
        registry: &AccountRegistry,
    ) -> Result<(), RegistryStoreError> {
        self.verify_guard(guard)?;
        if self.index_path.exists() {
            return Err(RegistryStoreError::AlreadyExists);
        }
        registry.validate()?;
        self.write_atomically(registry, AtomicWriteMode::NoClobber)
    }

    pub fn compare_and_swap<F>(
        &self,
        expected_generation: u64,
        mutate: F,
    ) -> Result<AccountRegistry, RegistryStoreError>
    where
        F: FnOnce(&mut AccountRegistry),
    {
        let guard = self.acquire_lock()?;
        self.compare_and_swap_with_guard(&guard, expected_generation, mutate)
    }

    pub fn compare_and_swap_with_guard<F>(
        &self,
        guard: &RegistryLockGuard,
        expected_generation: u64,
        mutate: F,
    ) -> Result<AccountRegistry, RegistryStoreError>
    where
        F: FnOnce(&mut AccountRegistry),
    {
        self.verify_guard(guard)?;
        let current = self.read()?;
        if current.generation != expected_generation {
            return Err(RegistryStoreError::GenerationConflict {
                expected: expected_generation,
                actual: current.generation,
            });
        }
        let mut updated = current;
        mutate(&mut updated);
        updated.generation = expected_generation
            .checked_add(1)
            .ok_or(RegistryStoreError::GenerationOverflow)?;
        updated.validate()?;
        self.write_atomically(&updated, AtomicWriteMode::Replace)?;
        Ok(updated)
    }

    /// Retries only the parent-directory synchronization after a mutation returned
    /// [`RegistryStoreError::CommittedDurabilityUncertain`].
    ///
    /// The caller must retain the guard from the original mutation. This method validates the
    /// installed registry, does not rewrite it or increment its generation, and returns the same
    /// explicit committed/uncertain state if synchronization still fails.
    pub fn repair_committed_durability_with_guard(
        &self,
        guard: &RegistryLockGuard,
    ) -> Result<(), RegistryStoreError> {
        self.verify_guard(guard)?;
        self.read()?;
        sync_parent_directory(&self.index_path).map_err(private_storage_error)
    }

    fn verify_guard(&self, guard: &RegistryLockGuard) -> Result<(), RegistryStoreError> {
        if !Arc::ptr_eq(&self.identity, &guard.owner) {
            return Err(RegistryStoreError::GuardMismatch);
        }
        Ok(())
    }

    fn open_lock_file(&self) -> Result<File, RegistryStoreError> {
        self.ensure_private_directory()?;
        open_private_read_write(&self.lock_path).map_err(private_storage_error)
    }

    fn ensure_private_directory(&self) -> Result<(), RegistryStoreError> {
        ensure_private_directory(&self.accounts_dir).map_err(private_storage_error)
    }

    fn write_atomically(
        &self,
        registry: &AccountRegistry,
        mode: AtomicWriteMode,
    ) -> Result<(), RegistryStoreError> {
        let mut contents = serde_json::to_vec_pretty(registry)?;
        contents.push(b'\n');
        write_file_atomically(&self.index_path, &contents, mode).map_err(private_storage_error)
    }
}

fn io_error(operation: &'static str, source: std::io::Error) -> RegistryStoreError {
    RegistryStoreError::Io { operation, source }
}

#[cfg(test)]
pub(crate) fn committed_directory_sync_result(
    result: std::io::Result<()>,
) -> Result<(), RegistryStoreError> {
    result.map_err(|source| RegistryStoreError::CommittedDurabilityUncertain {
        operation: "synchronize accounts directory",
        source,
    })
}

fn private_storage_error(error: PrivateStorageError) -> RegistryStoreError {
    match error {
        PrivateStorageError::AlreadyExists => RegistryStoreError::AlreadyExists,
        PrivateStorageError::CommittedDurabilityUncertain { source } => {
            RegistryStoreError::CommittedDurabilityUncertain {
                operation: "synchronize accounts directory",
                source,
            }
        }
        PrivateStorageError::CommittedProtectionUncertain => {
            RegistryStoreError::CommittedDurabilityUncertain {
                operation: "verify account registry protection",
                source: std::io::Error::other("committed private file protection is uncertain"),
            }
        }
        PrivateStorageError::CommittedCleanupUncertain { source } => {
            RegistryStoreError::CommittedDurabilityUncertain {
                operation: "clean up temporary account registry",
                source,
            }
        }
        error => io_error(
            "access private account registry storage",
            std::io::Error::other(error),
        ),
    }
}
