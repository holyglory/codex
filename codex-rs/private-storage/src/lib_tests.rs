use super::*;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Barrier;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[test]
fn private_directory_and_file_permissions_are_verified() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("private");
    ensure_private_directory(&directory).expect("private directory");
    let file = directory.join("data");
    open_private_read_write(&file).expect("private file");
    verify_private_directory(&directory).expect("verify directory");
    verify_private_file(&file).expect("verify file");
    #[cfg(unix)]
    {
        assert_eq!(
            directory.metadata().expect("metadata").permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            file.metadata().expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn atomic_no_clobber_and_replace_preserve_private_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("private").join("data");
    write_file_atomically(&path, b"first", AtomicWriteMode::NoClobber).expect("initial install");
    assert!(matches!(
        write_file_atomically(&path, b"second", AtomicWriteMode::NoClobber),
        Err(PrivateStorageError::AlreadyExists)
    ));
    assert_eq!(std::fs::read(&path).expect("read"), b"first");
    write_file_atomically(&path, b"replacement", AtomicWriteMode::Replace).expect("replacement");
    assert_eq!(std::fs::read(&path).expect("read"), b"replacement");
    verify_private_file(&path).expect("private replacement");
}

#[test]
fn stale_crash_temporary_does_not_affect_atomic_replacement() {
    let temp = tempfile::tempdir().expect("tempdir");
    let directory = temp.path().join("private");
    ensure_private_directory(&directory).expect("directory");
    let path = directory.join("data");
    write_file_atomically(&path, b"before", AtomicWriteMode::NoClobber).expect("initial");
    let stale = directory.join(".private-stale.tmp");
    std::fs::write(&stale, b"partial").expect("stale temporary");
    write_file_atomically(&path, b"after", AtomicWriteMode::Replace).expect("replace");
    assert_eq!(std::fs::read(path).expect("read"), b"after");
}

#[test]
fn concurrent_no_clobber_has_one_winner() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = Arc::new(temp.path().join("private").join("data"));
    let barrier = Arc::new(Barrier::new(3));
    let mut threads = Vec::new();
    for contents in [b"one".as_slice(), b"two".as_slice()] {
        let path = Arc::clone(&path);
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            write_file_atomically(&path, contents, AtomicWriteMode::NoClobber)
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().expect("writer"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(PrivateStorageError::AlreadyExists)))
            .count(),
        1
    );
    assert!(matches!(
        std::fs::read(&*path).expect("read").as_slice(),
        b"one" | b"two"
    ));
}

#[test]
fn committed_directory_sync_failure_is_typed_and_redacted() {
    let error = atomic::committed_sync_result(Err(std::io::Error::other("sensitive-path")))
        .expect_err("sync failure");
    assert!(matches!(
        error,
        PrivateStorageError::CommittedDurabilityUncertain { .. }
    ));
    assert!(!format!("{error:?}").contains("sensitive-path"));
    assert!(!error.to_string().contains("sensitive-path"));
}
