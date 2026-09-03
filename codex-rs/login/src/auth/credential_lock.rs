use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

const CREDENTIAL_LOCK_VERSION: u32 = 1;

/// Global filesystem-lock order for account migration and credential mutation:
///
/// 1. account registry lock;
/// 2. target profile-use lock, when removing a profile;
/// 3. legacy credential lock;
/// 4. target profile credential lock.
///
/// Ordinary login, refresh, and logout paths acquire exactly one credential lock. Callers must
/// never acquire a registry lock while holding a credential lock.
#[derive(Clone)]
pub(super) struct CredentialLock {
    identity: Arc<CredentialLockIdentity>,
    storage_home: PathBuf,
}

struct CredentialLockIdentity;

impl fmt::Debug for CredentialLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialLock")
            .field("version", &CREDENTIAL_LOCK_VERSION)
            .finish_non_exhaustive()
    }
}

impl CredentialLock {
    pub(super) fn new(storage_home: PathBuf) -> Self {
        Self {
            identity: Arc::new(CredentialLockIdentity),
            storage_home,
        }
    }

    pub(super) fn acquire(&self) -> io::Result<CredentialLockGuard> {
        std::fs::create_dir_all(&self.storage_home)?;
        set_private_directory_permissions(&self.storage_home)?;
        let canonical_home = self.storage_home.canonicalize()?;
        let lock_path =
            canonical_home.join(format!(".auth-credential-lock-v{CREDENTIAL_LOCK_VERSION}"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        set_private_file_permissions(&file)?;
        file.lock()?;
        Ok(CredentialLockGuard {
            owner: Arc::clone(&self.identity),
            _file: file,
        })
    }

    pub(super) fn verify(&self, guard: &CredentialLockGuard) -> io::Result<()> {
        if !Arc::ptr_eq(&self.identity, &guard.owner) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "credential lock guard belongs to a different storage instance",
            ));
        }
        Ok(())
    }
}

pub(super) struct CredentialLockGuard {
    owner: Arc<CredentialLockIdentity>,
    _file: File,
}

impl fmt::Debug for CredentialLockGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialLockGuard")
            .field("version", &CREDENTIAL_LOCK_VERSION)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_file: &File) -> io::Result<()> {
    Ok(())
}
