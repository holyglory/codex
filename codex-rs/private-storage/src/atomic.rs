use crate::PrivateStorageError;
use crate::ensure_private_directory;
use crate::ensure_private_file;
use crate::platform;
use crate::verify_private_file;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use uuid::Uuid;

const TEMP_FILE_ATTEMPTS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AtomicWriteMode {
    NoClobber,
    Replace,
}

pub fn write_file_atomically(
    path: &Path,
    contents: &[u8],
    mode: AtomicWriteMode,
) -> Result<(), PrivateStorageError> {
    let parent = path.parent().ok_or(PrivateStorageError::InvalidTarget)?;
    ensure_private_directory(parent)?;
    if mode == AtomicWriteMode::Replace && path.exists() {
        ensure_private_file(path)?;
    }
    let mut temporary = TemporaryFile::create(parent)?;
    let operation = (|| {
        let file = temporary.file.as_mut().ok_or_else(|| {
            PrivateStorageError::io(
                "access temporary file",
                std::io::Error::other("temporary file is unavailable"),
            )
        })?;
        file.write_all(contents)
            .map_err(|source| PrivateStorageError::io("write temporary file", source))?;
        file.sync_all()
            .map_err(|source| PrivateStorageError::io("synchronize temporary file", source))?;
        drop(temporary.file.take());
        platform::install_file(&temporary.path, path, mode)
    })();
    if let Err(error) = operation {
        return Err(failed_write(&mut temporary, error));
    }
    temporary.installed = true;
    verify_private_file(path).map_err(|_| PrivateStorageError::CommittedProtectionUncertain)?;
    committed_sync_result(platform::sync_directory(parent))
}

pub(crate) fn committed_sync_result(
    result: std::io::Result<()>,
) -> Result<(), PrivateStorageError> {
    result.map_err(|source| PrivateStorageError::CommittedDurabilityUncertain { source })
}

fn failed_write(
    temporary: &mut TemporaryFile,
    operation_error: PrivateStorageError,
) -> PrivateStorageError {
    match temporary.cleanup() {
        Ok(()) => operation_error,
        Err(cleanup_error) => {
            let operation_error = match operation_error {
                PrivateStorageError::Io { source, .. } => source,
                PrivateStorageError::AlreadyExists => {
                    return PrivateStorageError::Cleanup {
                        operation_error: std::io::Error::new(
                            std::io::ErrorKind::AlreadyExists,
                            "private storage target already exists",
                        ),
                        cleanup_error,
                    };
                }
                error => std::io::Error::other(error),
            };
            PrivateStorageError::Cleanup {
                operation_error,
                cleanup_error,
            }
        }
    }
}

struct TemporaryFile {
    path: PathBuf,
    file: Option<File>,
    installed: bool,
}

impl TemporaryFile {
    fn create(parent: &Path) -> Result<Self, PrivateStorageError> {
        let mut last_collision = None;
        for _ in 0..TEMP_FILE_ATTEMPTS {
            let path = parent.join(format!(".private-{}.tmp", Uuid::now_v7().simple()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(file) => {
                    if let Err(error) =
                        platform::protect_file(&path).and_then(|()| platform::verify_file(&path))
                    {
                        drop(file);
                        let cleanup = std::fs::remove_file(&path);
                        return Err(match cleanup {
                            Ok(()) => error,
                            Err(cleanup_error) => PrivateStorageError::Cleanup {
                                operation_error: std::io::Error::other(error),
                                cleanup_error,
                            },
                        });
                    }
                    return Ok(Self {
                        path,
                        file: Some(file),
                        installed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    last_collision = Some(error);
                }
                Err(source) => {
                    return Err(PrivateStorageError::io("create temporary file", source));
                }
            }
        }
        Err(PrivateStorageError::io(
            "create temporary file",
            last_collision.unwrap_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "temporary file allocation exhausted",
                )
            }),
        ))
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        drop(self.file.take());
        match std::fs::remove_file(&self.path) {
            Ok(()) => {
                self.installed = true;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
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
