use crate::AtomicWriteMode;
use crate::PrivateStorageError;
use std::ffi::c_void;
use std::fs::File;
use std::fs::OpenOptions;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACCESS_ALLOWED_ACE;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::ACL_SIZE_INFORMATION;
use windows_sys::Win32::Security::AclSizeInformation;
use windows_sys::Win32::Security::Authorization::EXPLICIT_ACCESS_W;
use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::NO_MULTIPLE_TRUSTEE;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SET_ACCESS;
use windows_sys::Win32::Security::Authorization::SetEntriesInAclW;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_SID;
use windows_sys::Win32::Security::Authorization::TRUSTEE_IS_USER;
use windows_sys::Win32::Security::Authorization::TRUSTEE_W;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::EqualSid;
use windows_sys::Win32::Security::GetAce;
use windows_sys::Win32::Security::GetAclInformation;
use windows_sys::Win32::Security::GetSecurityDescriptorControl;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::SE_DACL_PROTECTED;
use windows_sys::Win32::Security::SUB_CONTAINERS_AND_OBJECTS_INHERIT;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Storage::FileSystem::CreateFileW;
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_GENERIC_WRITE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE;
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;
use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
use windows_sys::Win32::Storage::FileSystem::OPEN_EXISTING;
use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::OpenProcessToken;

pub(super) fn protect_directory(path: &Path) -> Result<(), PrivateStorageError> {
    apply_current_user_dacl(path, true)
}

pub(super) fn verify_directory(path: &Path) -> Result<(), PrivateStorageError> {
    verify_current_user_dacl(path)
}

pub(super) fn protect_file(path: &Path) -> Result<(), PrivateStorageError> {
    apply_current_user_dacl(path, false)
}

pub(super) fn verify_file(path: &Path) -> Result<(), PrivateStorageError> {
    verify_current_user_dacl(path)
}

pub(super) fn open_private_read_write(path: &Path) -> Result<File, PrivateStorageError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| PrivateStorageError::io("open private file", source))
}

pub(super) fn install_file(
    source: &Path,
    destination: &Path,
    mode: AtomicWriteMode,
) -> Result<(), PrivateStorageError> {
    let source = wide_path(source);
    let destination = wide_path(destination);
    let flags = MOVEFILE_WRITE_THROUGH
        | if mode == AtomicWriteMode::Replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) };
    if result != 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if mode == AtomicWriteMode::NoClobber && error.kind() == std::io::ErrorKind::AlreadyExists {
        Err(PrivateStorageError::AlreadyExists)
    } else {
        Err(PrivateStorageError::io("install private file", error))
    }
}

pub(super) fn sync_directory(path: &Path) -> std::io::Result<()> {
    let path = wide_path(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_WRITE_THROUGH,
            0,
        )
    };
    if handle == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let result = unsafe { FlushFileBuffers(handle) };
    let error = (result == 0).then(std::io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn apply_current_user_dacl(path: &Path, directory: bool) -> Result<(), PrivateStorageError> {
    let sid = current_user_sid()?;
    let mut acl: *mut ACL = std::ptr::null_mut();
    let entry = EXPLICIT_ACCESS_W {
        grfAccessPermissions: FILE_ALL_ACCESS,
        grfAccessMode: SET_ACCESS,
        grfInheritance: if directory {
            SUB_CONTAINERS_AND_OBJECTS_INHERIT
        } else {
            0
        },
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.as_ptr().cast_mut().cast(),
        },
    };
    let result = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) };
    if result != ERROR_SUCCESS {
        return Err(win32_error("build private access list", result));
    }
    let mut wide = wide_path(path);
    let result = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            acl,
            std::ptr::null_mut(),
        )
    };
    unsafe { LocalFree(acl.cast()) };
    if result != ERROR_SUCCESS {
        return Err(win32_error("apply private access list", result));
    }
    verify_current_user_dacl(path)
}

fn verify_current_user_dacl(path: &Path) -> Result<(), PrivateStorageError> {
    let sid = current_user_sid()?;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: *mut c_void = std::ptr::null_mut();
    let mut path = wide_path(path);
    let result = unsafe {
        GetNamedSecurityInfoW(
            path.as_mut_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if result != ERROR_SUCCESS {
        return Err(win32_error("read private access list", result));
    }
    let verification = verify_descriptor(descriptor, dacl, sid.as_ptr().cast());
    unsafe { LocalFree(descriptor) };
    verification
}

fn verify_descriptor(
    descriptor: *mut c_void,
    dacl: *mut ACL,
    expected_sid: *const c_void,
) -> Result<(), PrivateStorageError> {
    if descriptor.is_null() || dacl.is_null() {
        return Err(PrivateStorageError::ProtectionMismatch);
    }
    let mut control = 0_u16;
    let mut revision = 0_u32;
    let result = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    if result == 0 || control & SE_DACL_PROTECTED == 0 {
        return Err(PrivateStorageError::ProtectionMismatch);
    }
    let mut information = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    let result = unsafe {
        GetAclInformation(
            dacl,
            (&mut information as *mut ACL_SIZE_INFORMATION).cast(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    if result == 0 || information.AceCount != 1 {
        return Err(PrivateStorageError::ProtectionMismatch);
    }
    let mut ace: *mut c_void = std::ptr::null_mut();
    if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
        return Err(PrivateStorageError::ProtectionMismatch);
    }
    let ace = unsafe { &*(ace.cast::<ACCESS_ALLOWED_ACE>()) };
    let sid = (&ace.SidStart as *const u32).cast_mut().cast();
    if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8
        || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
        // EqualSid reads both SIDs; PSID uses a mutable pointer in the Win32 ABI.
        || unsafe { EqualSid(sid, expected_sid.cast_mut()) } == 0
    {
        return Err(PrivateStorageError::ProtectionMismatch);
    }
    Ok(())
}

fn current_user_sid() -> Result<Vec<u32>, PrivateStorageError> {
    let mut token: HANDLE = 0;
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(PrivateStorageError::io(
            "open current process token",
            std::io::Error::last_os_error(),
        ));
    }
    let mut bytes_needed = 0_u32;
    let initial = unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut bytes_needed)
    };
    let initial_error = std::io::Error::last_os_error();
    if initial != 0 || initial_error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
        unsafe { CloseHandle(token) };
        return Err(PrivateStorageError::io(
            "query current user identifier",
            initial_error,
        ));
    }
    let mut token_user = vec![0_u32; bytes_needed.div_ceil(4) as usize];
    let result = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            token_user.as_mut_ptr().cast(),
            bytes_needed,
            &mut bytes_needed,
        )
    };
    unsafe { CloseHandle(token) };
    if result == 0 {
        return Err(PrivateStorageError::io(
            "read current user identifier",
            std::io::Error::last_os_error(),
        ));
    }
    let user = unsafe { &*(token_user.as_ptr().cast::<TOKEN_USER>()) };
    let sid_length = unsafe { windows_sys::Win32::Security::GetLengthSid(user.User.Sid) };
    if sid_length == 0 {
        return Err(PrivateStorageError::io(
            "read current user identifier",
            std::io::Error::last_os_error(),
        ));
    }
    let mut sid = vec![0_u32; sid_length.div_ceil(4) as usize];
    if unsafe {
        windows_sys::Win32::Security::CopySid(sid_length, sid.as_mut_ptr().cast(), user.User.Sid)
    } == 0
    {
        return Err(PrivateStorageError::io(
            "copy current user identifier",
            std::io::Error::last_os_error(),
        ));
    }
    Ok(sid)
}

fn win32_error(operation: &'static str, code: u32) -> PrivateStorageError {
    PrivateStorageError::io(operation, std::io::Error::from_raw_os_error(code as i32))
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
