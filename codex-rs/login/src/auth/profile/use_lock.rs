use std::fmt;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

const PROFILE_USE_LOCK_VERSION: u32 = 1;

pub(super) struct ProfileUseGuard {
    _file: File,
}

impl fmt::Debug for ProfileUseGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileUseGuard")
            .field("version", &PROFILE_USE_LOCK_VERSION)
            .finish_non_exhaustive()
    }
}

pub(super) struct ProfileRemovalGuard {
    _file: File,
}

impl fmt::Debug for ProfileRemovalGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProfileRemovalGuard")
            .field("version", &PROFILE_USE_LOCK_VERSION)
            .finish_non_exhaustive()
    }
}

pub(super) fn acquire_profile_use(profile_home: &Path) -> io::Result<ProfileUseGuard> {
    let file = open_lock(profile_home)?;
    File::lock_shared(&file)?;
    Ok(ProfileUseGuard { _file: file })
}

pub(super) fn try_acquire_profile_removal(
    profile_home: &Path,
) -> io::Result<Option<ProfileRemovalGuard>> {
    let file = open_lock(profile_home)?;
    match File::try_lock(&file) {
        Ok(()) => Ok(Some(ProfileRemovalGuard { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

fn open_lock(profile_home: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(profile_home.join(format!(".profile-use-lock-v{PROFILE_USE_LOCK_VERSION}")))?;
    set_private_file_permissions(&file)?;
    Ok(file)
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
