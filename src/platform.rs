use std::{env, process::Command};

#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;

#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWDEFAULT;

use crate::{commands::scope_arg_value, SearchScope};

#[cfg(target_os = "windows")]
pub(crate) fn is_process_elevated() -> bool {
    unsafe { IsUserAnAdmin() != 0 }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn is_process_elevated() -> bool {
    true
}

#[cfg(target_os = "windows")]
pub(crate) fn request_self_elevation(scope: &SearchScope) -> Result<(), String> {
    let exe_path = env::current_exe().map_err(|e| e.to_string())?;
    let exe = to_wide(exe_path.to_string_lossy().as_ref());
    let verb = to_wide("runas");
    let params = to_wide(&format!("--show --scope={}", scope_arg_value(scope)));

    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWDEFAULT,
        )
    } as isize;

    if result <= 32 {
        Err(format!(
            "UAC elevation failed or cancelled (code {})",
            result
        ))
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn request_self_elevation(_scope: &SearchScope) -> Result<(), String> {
    Err("Elevation is only supported on Windows".to_string())
}

pub(crate) fn open_path(path: &str) -> Result<(), String> {
    Command::new("cmd")
        .args(["/C", "start", "", path])
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn reveal_path(path: &str) -> Result<(), String> {
    Command::new("explorer")
        .arg(format!("/select,{}", path))
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn to_wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}
