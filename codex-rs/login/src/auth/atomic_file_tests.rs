use std::io;

use pretty_assertions::assert_eq;
use tempfile::tempdir;

use super::*;

#[test]
fn failed_replacement_removes_the_temporary_auth_file() {
    let directory = tempdir().expect("create temporary directory");
    let destination = directory.path().join("auth.json");
    std::fs::create_dir(&destination).expect("create conflicting destination directory");

    replace_atomically(&destination, b"replacement")
        .expect_err("a file should not replace a directory");

    assert_eq!(
        std::fs::read_dir(directory.path())
            .expect("read temporary directory")
            .map(|entry| entry.expect("read directory entry").file_name())
            .collect::<Vec<_>>(),
        vec![std::ffi::OsString::from("auth.json")]
    );
}

#[test]
fn cleanup_failure_is_classified_without_exposing_the_temporary_path() {
    let directory = tempdir().expect("create temporary directory");
    let mut temporary = TemporaryFile::create(directory.path()).expect("create temporary file");
    drop(temporary.file.take());
    std::fs::remove_file(&temporary.path).expect("remove temporary file");
    std::fs::create_dir(&temporary.path).expect("replace temporary file with directory");

    let error = failed_write_error(
        &mut temporary,
        io::Error::other("simulated auth write failure"),
    );
    let classified = AuthFileCleanupError::from_io_error(&error)
        .expect("cleanup failure should retain explicit state");

    assert_eq!(
        classified.operation_error().to_string(),
        "simulated auth write failure"
    );
    assert!(!classified.cleanup_error().to_string().is_empty());
    assert_eq!(
        error.to_string(),
        "auth file update failed and temporary-file cleanup also failed"
    );
    let temporary_path = temporary.path.to_string_lossy();
    assert!(!error.to_string().contains(temporary_path.as_ref()));

    std::fs::remove_dir(&temporary.path).expect("remove simulated cleanup obstacle");
}

#[test]
fn directory_sync_failure_reports_that_replacement_committed() {
    let error = committed_sync_result(Err(io::Error::other("simulated directory sync failure")))
        .expect_err("directory sync failure should be reported");
    let classified = AuthFileDurabilityError::from_io_error(&error)
        .expect("committed write should retain explicit durability state");

    assert_eq!(
        classified.synchronization_error().to_string(),
        "simulated directory sync failure"
    );
    assert_eq!(
        error.to_string(),
        "auth file replacement committed, but directory durability is uncertain"
    );
}

#[cfg(windows)]
#[test]
fn windows_replacement_preserves_a_protected_destination_dacl() {
    use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;

    let directory = tempdir().expect("create temporary directory");
    let destination = directory.path().join("auth.json");
    replace_atomically(&destination, b"original").expect("create original auth file");

    let mut protected = WindowsDacl::read(&destination)
        .expect("read original DACL")
        .expect("original auth file should have a DACL");
    protected.security_information =
        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    protected
        .apply_to(&destination)
        .expect("protect original DACL");
    let expected = WindowsDacl::read(&destination)
        .expect("read protected DACL")
        .expect("protected auth file should have a DACL");

    replace_atomically(&destination, b"replacement").expect("replace auth file");

    let actual = WindowsDacl::read(&destination)
        .expect("read replacement DACL")
        .expect("replacement auth file should have a DACL");
    assert_eq!(actual.security_information, expected.security_information);
    assert_eq!(actual.descriptor, expected.descriptor);
}
