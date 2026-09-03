use rand::RngCore;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use thiserror::Error;

const TEMP_FILE_ATTEMPTS: usize = 16;

/// Indicates that the new auth file is installed but its directory sync failed.
///
/// Callers must treat the replacement as potentially current and re-read auth state before
/// retrying. The previous file cannot be assumed to remain installed.
#[derive(Debug, Error)]
#[error("auth file replacement committed, but directory durability is uncertain")]
pub struct AuthFileDurabilityError {
    #[source]
    source: io::Error,
}

impl AuthFileDurabilityError {
    pub fn from_io_error(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }

    pub fn synchronization_error(&self) -> &io::Error {
        &self.source
    }
}

/// Indicates that an auth write failed and its temporary file could not be removed.
#[derive(Debug, Error)]
#[error("auth file update failed and temporary-file cleanup also failed")]
pub struct AuthFileCleanupError {
    operation_error: io::Error,
    #[source]
    cleanup_error: io::Error,
}

impl AuthFileCleanupError {
    pub fn from_io_error(error: &io::Error) -> Option<&Self> {
        error.get_ref()?.downcast_ref()
    }

    pub fn operation_error(&self) -> &io::Error {
        &self.operation_error
    }

    pub fn cleanup_error(&self) -> &io::Error {
        &self.cleanup_error
    }
}

pub(super) fn replace_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "atomic replacement path has no parent directory",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    let mut temporary = TemporaryFile::create(parent)?;
    let update_result = (|| -> io::Result<()> {
        let file = temporary
            .file
            .as_mut()
            .ok_or(io::Error::other("temporary auth file is unavailable"))?;
        file.write_all(contents)?;
        #[cfg(unix)]
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.sync_all()?;
        drop(temporary.file.take());

        #[cfg(windows)]
        if let Some(dacl) = WindowsDacl::read(path)? {
            dacl.apply_to(&temporary.path)?;
        }

        replace_file(&temporary.path, path)
    })();
    if let Err(operation_error) = update_result {
        return Err(failed_write_error(&mut temporary, operation_error));
    }

    temporary.installed = true;
    committed_sync_result(sync_parent_directory(parent))
}

pub(super) fn repair_parent_directory_durability(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "private storage path has no parent directory",
        )
    })?;
    committed_sync_result(sync_parent_directory(parent))
}

fn failed_write_error(temporary: &mut TemporaryFile, operation_error: io::Error) -> io::Error {
    match temporary.cleanup() {
        Ok(()) => operation_error,
        Err(cleanup_error) => io::Error::other(AuthFileCleanupError {
            operation_error,
            cleanup_error,
        }),
    }
}

fn committed_sync_result(result: io::Result<()>) -> io::Result<()> {
    result.map_err(|source| io::Error::other(AuthFileDurabilityError { source }))
}

struct TemporaryFile {
    path: PathBuf,
    file: Option<File>,
    installed: bool,
}

impl TemporaryFile {
    fn create(parent: &Path) -> io::Result<Self> {
        let mut random_bytes = [0_u8; 8];
        let mut last_collision = None;
        for _ in 0..TEMP_FILE_ATTEMPTS {
            rand::rng().fill_bytes(&mut random_bytes);
            let nonce = u64::from_ne_bytes(random_bytes);
            let path = parent.join(format!(".auth.json.{nonce:016x}.tmp"));
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            #[cfg(unix)]
            options.mode(0o600);
            match options.open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        installed: false,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        Err(last_collision.unwrap_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "failed to allocate a temporary auth file",
            )
        }))
    }

    fn cleanup(&mut self) -> io::Result<()> {
        drop(self.file.take());
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                self.installed = true;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.installed = true;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.installed {
            let _ = self.cleanup();
        }
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
struct WindowsDacl {
    descriptor: Vec<u32>,
    security_information: u32,
}

#[cfg(windows)]
impl WindowsDacl {
    fn read(path: &Path) -> io::Result<Option<Self>> {
        use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
        use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
        use windows_sys::Win32::Security::GetFileSecurityW;
        use windows_sys::Win32::Security::GetSecurityDescriptorControl;
        use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
        use windows_sys::Win32::Security::SE_DACL_PROTECTED;
        use windows_sys::Win32::Security::UNPROTECTED_DACL_SECURITY_INFORMATION;

        let path = wide_path(path);
        let mut bytes_needed = 0_u32;
        let initial_result = unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                0,
                &mut bytes_needed,
            )
        };
        if initial_result != 0 {
            return Err(io::Error::other(
                "security descriptor query unexpectedly returned no data",
            ));
        }
        let initial_error = io::Error::last_os_error();
        if initial_error.kind() == io::ErrorKind::NotFound {
            return Ok(None);
        }
        if initial_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(initial_error);
        }

        let word_count = bytes_needed.div_ceil(4) as usize;
        let mut descriptor = vec![0_u32; word_count];
        let result = unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                descriptor.as_mut_ptr().cast(),
                bytes_needed,
                &mut bytes_needed,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }

        let mut control = 0_u16;
        let mut revision = 0_u32;
        let result = unsafe {
            GetSecurityDescriptorControl(
                descriptor.as_mut_ptr().cast(),
                &mut control,
                &mut revision,
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        let protection_information = if control & SE_DACL_PROTECTED != 0 {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        Ok(Some(Self {
            descriptor,
            security_information: DACL_SECURITY_INFORMATION | protection_information,
        }))
    }

    fn apply_to(&self, path: &Path) -> io::Result<()> {
        use windows_sys::Win32::Security::SetFileSecurityW;

        let path = wide_path(path);
        let result = unsafe {
            SetFileSecurityW(
                path.as_ptr(),
                self.security_information,
                self.descriptor.as_ptr().cast_mut().cast(),
            )
        };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = wide_path(source);
    let destination = wide_path(destination);
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(parent: &Path) -> io::Result<()> {
    match File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_parent: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[path = "atomic_file_tests.rs"]
mod tests;
