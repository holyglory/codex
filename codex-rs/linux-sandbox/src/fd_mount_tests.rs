use super::verify_fd_mounts;
use pretty_assertions::assert_eq;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::Mutex;
use std::sync::MutexGuard;

const PREFERRED_HIGH_DESCRIPTOR_MINIMUM: libc::c_int = 10_000;
static DESCRIPTOR_CLOSURE_ASSERTION_LOCK: Mutex<()> = Mutex::new(());

// Keep closure checks outside the low descriptor range reused by parallel libtest activity.
fn duplicate_to_high_descriptor(file: &File) -> (MutexGuard<'static, ()>, libc::c_int) {
    // These tests intentionally close their raw descriptor, so keep their high-FD allocations
    // serialized until the post-close assertion has observed EBADF.
    let guard = DESCRIPTOR_CLOSURE_ASSERTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut descriptor_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `descriptor_limit` is a valid writable `rlimit` for this process.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut descriptor_limit) };
    assert_eq!(
        result,
        0,
        "descriptor limit should be available: {}",
        std::io::Error::last_os_error()
    );
    let capped_limit = descriptor_limit
        .rlim_cur
        .min(libc::c_int::MAX as libc::rlim_t);
    assert!(
        capped_limit > (libc::STDERR_FILENO + 1) as libc::rlim_t,
        "descriptor limit should leave a non-standard descriptor available"
    );
    let high_descriptor_minimum = ((capped_limit - 1) / 2)
        .max((libc::STDERR_FILENO + 1) as libc::rlim_t)
        .min(PREFERRED_HIGH_DESCRIPTOR_MINIMUM as libc::rlim_t)
        as libc::c_int;
    // SAFETY: `file` keeps the source descriptor live throughout this call, and fcntl returns a
    // distinct descriptor that the caller transfers to `verify_fd_mounts`.
    let descriptor = unsafe {
        libc::fcntl(
            file.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            high_descriptor_minimum,
        )
    };
    assert!(
        descriptor >= high_descriptor_minimum,
        "high descriptor should duplicate: {}",
        std::io::Error::last_os_error()
    );
    (guard, descriptor)
}

/// A matching mount authenticates its original inode and closes the inherited descriptor.
#[test]
fn matching_mount_closes_inherited_descriptor() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let source = File::open(root.path()).expect("directory descriptor should open");
    let (_guard, descriptor) = duplicate_to_high_descriptor(&source);
    let marker = format!("{descriptor}:{}", root.path().display());

    verify_fd_mounts(&[marker]).expect("matching mount should verify");

    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

/// A swapped destination fails closed without leaking the original descriptor.
#[test]
fn mismatched_mount_closes_inherited_descriptor() {
    let source = tempfile::tempdir().expect("source directory should be created");
    let destination = tempfile::tempdir().expect("destination directory should be created");
    let source_file = File::open(source.path()).expect("source directory descriptor should open");
    let (_guard, descriptor) = duplicate_to_high_descriptor(&source_file);
    let marker = format!("{descriptor}:{}", destination.path().display());

    let error = verify_fd_mounts(&[marker]).expect_err("different inodes must be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

/// A symlink to the original inode is not itself the authenticated mount.
#[test]
fn symlinked_mount_destination_closes_inherited_descriptor() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let source = root.path().join("source");
    let destination = root.path().join("destination");
    std::fs::create_dir(&source).expect("source directory should be created");
    std::os::unix::fs::symlink(&source, &destination)
        .expect("mount destination symlink should be created");
    let source_file = File::open(&source).expect("source directory descriptor should open");
    let (_guard, descriptor) = duplicate_to_high_descriptor(&source_file);
    let marker = format!("{descriptor}:{}", destination.display());

    let error = verify_fd_mounts(&[marker]).expect_err("symlinked mounts must be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF)
    );
}

/// Malformed mount markers cannot claim standard streams or relative destinations.
#[test]
fn malformed_mount_markers_are_rejected() {
    for marker in [
        "missing-separator",
        "invalid:/tmp",
        "0:/tmp",
        "1:/tmp",
        "2:/tmp",
    ] {
        let error = verify_fd_mounts(&[marker.to_string()])
            .expect_err("malformed mount marker must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    let root = tempfile::tempdir().expect("temporary directory should be created");
    let source = File::open(root.path()).expect("directory descriptor should open");
    let (_guard, descriptor) = duplicate_to_high_descriptor(&source);
    let error = verify_fd_mounts(&[format!("{descriptor}:relative")])
        .expect_err("relative mount destinations must be rejected");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
}

/// A transferred descriptor can be consumed only once even if a marker is repeated.
#[test]
fn duplicate_mount_descriptors_are_rejected() {
    let root = tempfile::tempdir().expect("temporary directory should be created");
    let source = File::open(root.path()).expect("directory descriptor should open");
    let (_guard, descriptor) = duplicate_to_high_descriptor(&source);
    let marker = format!("{descriptor}:{}", root.path().display());

    let error = verify_fd_mounts(&[marker.clone(), marker])
        .expect_err("a mount descriptor must not be consumed twice");

    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
}
