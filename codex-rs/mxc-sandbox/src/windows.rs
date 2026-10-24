//! Resolve executor-local state and launch the decoded MXC helper request.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use windows_sys::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows_sys::Win32::Storage::FileSystem::GetLogicalDrives;
use windows_sys::Win32::System::Com::COINIT_MULTITHREADED;
use windows_sys::Win32::System::Com::CoInitializeEx;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Com::CoUninitialize;
use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows_sys::Win32::UI::Shell::FOLDERID_ProgramData;
use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFiles;
use windows_sys::Win32::UI::Shell::FOLDERID_ProgramFilesX86;
use windows_sys::Win32::UI::Shell::KF_FLAG_DONT_VERIFY;
use windows_sys::Win32::UI::Shell::SHGetKnownFolderPath;

struct ComUninitializeGuard;

impl Drop for ComUninitializeGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub(super) fn run() -> Result<i32> {
    ensure!(
        crate::is_available(),
        "native MXC is unavailable on this Windows build"
    );
    // Read the already-filtered wrapper environment, rejecting lossy values
    // instead of inheriting a different environment inside the sandbox.
    let mut env = std::env::vars_os()
        .map(|(key, value)| {
            let key = key
                .into_string()
                .map_err(|_| anyhow::anyhow!("MXC requires Unicode environment variable names"))?;
            let value = value
                .into_string()
                .map_err(|_| anyhow::anyhow!("MXC requires Unicode environment variable values"))?;
            Ok((key, value))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    // Serde errors can quote arbitrary payload values; do not print them to stderr.
    let command = crate::transport::decode(&mut env)
        .map_err(|_| anyhow::anyhow!("invalid MXC launcher request"))?;
    // The transport itself proves this is an explicit child environment; it
    // may intentionally contain no variables once launcher state is removed.
    let mask = unsafe { GetLogicalDrives() };
    ensure!(
        mask != 0,
        "cannot enumerate Windows volumes: {}",
        std::io::Error::last_os_error()
    );
    let mut volumes: Vec<PathBuf> = (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| PathBuf::from(format!("{}:\\", (b'A' + index as u8) as char)))
        .collect();
    let command_cwd = std::env::current_dir()?;
    // An explicit UNC cwd has no drive letter and is absent from GetLogicalDrives.
    for cwd in [&command.sandbox_policy_cwd, &command_cwd] {
        if let Some(root) = cwd.ancestors().last() {
            volumes.push(root.to_path_buf());
        }
    }
    let request = crate::policy::build_request(
        &command,
        &command_cwd,
        env.into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect(),
        &volumes,
        &platform_read_roots()?,
    )?;
    crate::native::launch(&request)
}

fn platform_read_roots() -> Result<Vec<PathBuf>> {
    use std::os::windows::ffi::OsStringExt;

    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
    ensure!(
        length > 0 && (length as usize) < buffer.len(),
        "cannot locate the Windows directory"
    );
    let com_status = unsafe { CoInitializeEx(std::ptr::null(), COINIT_MULTITHREADED as u32) };
    ensure!(
        com_status >= 0 || com_status == RPC_E_CHANGED_MODE,
        "cannot initialize COM: HRESULT {com_status:#010x}"
    );
    let _com = (com_status >= 0).then(|| ComUninitializeGuard);
    let windows_directory =
        PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
    let system_drive = windows_directory
        .ancestors()
        .last()
        .context("Windows directory has no volume root")?
        .to_owned();
    let mut roots = vec![windows_directory];
    for folder in [
        FOLDERID_ProgramFiles,
        FOLDERID_ProgramFilesX86,
        FOLDERID_ProgramData,
    ] {
        let mut path = std::ptr::null_mut();
        let status = unsafe {
            // A filtered command environment can omit SystemDrive. The shell
            // then returns the configured %SystemDrive% path but cannot verify
            // it. Resolve that one machine variable below and verify ourselves.
            SHGetKnownFolderPath(
                &folder,
                KF_FLAG_DONT_VERIFY as u32,
                std::ptr::null_mut(),
                &mut path,
            )
        };
        if status >= 0 && !path.is_null() {
            let mut length = 0;
            unsafe {
                while *path.add(length) != 0 {
                    length += 1;
                }
                let configured =
                    std::ffi::OsString::from_wide(std::slice::from_raw_parts(path, length));
                CoTaskMemFree(path.cast());
                let text = configured
                    .to_str()
                    .context("Windows platform directory is not Unicode")?;
                let variable = "%SystemDrive%";
                let resolved = if text
                    .get(..variable.len())
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case(variable))
                {
                    let remainder = &text[variable.len()..];
                    ensure!(
                        remainder.starts_with(['\\', '/']),
                        "invalid system-drive platform directory"
                    );
                    system_drive.join(remainder.trim_start_matches(['\\', '/']))
                } else {
                    PathBuf::from(configured)
                };
                ensure!(
                    resolved.is_absolute() && resolved.is_dir(),
                    "required Windows platform directory is missing or not absolute"
                );
                roots.push(resolved);
            }
        } else {
            if !path.is_null() {
                unsafe { CoTaskMemFree(path.cast()) };
            }
            anyhow::bail!(
                "cannot locate required Windows platform directory: HRESULT {status:#010x}"
            );
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::platform_read_roots;
    use std::path::PathBuf;

    #[test]
    fn platform_roots_resolve_without_inheriting_the_command_environment() {
        const EXPECTED: &str = "CODEX_MXC_EXPECTED_PLATFORM_ROOTS";
        if let Some(expected) = std::env::var_os(EXPECTED) {
            assert!(std::env::var_os("SystemDrive").is_none());
            let expected: Vec<PathBuf> = serde_json::from_str(expected.to_str().unwrap()).unwrap();
            assert_eq!(platform_read_roots().unwrap(), expected);
            // Resolution must not add variables to the eventual workload.
            assert!(std::env::var_os("SystemDrive").is_none());
            return;
        }
        let expected = platform_read_roots().unwrap();
        assert_eq!(expected.len(), 4);
        assert!(
            expected
                .iter()
                .all(|path| path.is_absolute() && path.is_dir())
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "windows::tests::platform_roots_resolve_without_inheriting_the_command_environment",
                "--nocapture",
            ])
            .env_clear()
            .env(EXPECTED, serde_json::to_string(&expected).unwrap())
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "filtered-environment platform resolution failed: {}\n{}\n{}",
            child.status,
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
    }
}
