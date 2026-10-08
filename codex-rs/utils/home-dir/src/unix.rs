use std::ffi::CStr;
use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

pub(super) fn root_home_dir() -> io::Result<PathBuf> {
    let mut buffer = vec![0u8; 1024];
    loop {
        let mut passwd = MaybeUninit::<libc::passwd>::uninit();
        let mut result = ptr::null_mut();
        // SAFETY: all output pointers are writable and buffer covers its declared
        // length. getpwuid_r stores strings in our buffer, not libc shared storage.
        let error = unsafe {
            libc::getpwuid_r(
                /*uid*/ 0,
                passwd.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if error == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        if result.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "UID 0 has no passwd entry",
            ));
        }
        // SAFETY: a successful lookup with non-null result initialized passwd.
        let directory = unsafe { passwd.assume_init() }.pw_dir;
        if directory.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "UID 0 has no home directory",
            ));
        }
        // SAFETY: the successful lookup returns a NUL-terminated string in buffer.
        // Copy its bytes while buffer is still alive; Unix paths need not be UTF-8.
        let path = PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(directory) }.to_bytes(),
        ));
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "UID 0 home directory must be absolute",
            ));
        }
        return Ok(path);
    }
}
