use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::PWSTR;

use crate::from_wide;

/// Returns the executable file name (e.g. `firefox.exe`) for a process id.
///
/// Returns `None` if the process can't be opened, which happens for some
/// protected and elevated processes when rivewm isn't elevated.
pub(crate) fn process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        let path = from_wide(&buf[..len as usize]);
        Some(path.rsplit('\\').next().unwrap_or(&path).to_owned())
    }
}

/// Whether Windows will stop us moving this process's windows: it runs
/// elevated (as administrator) and we don't. UIPI blocks lower-privilege
/// processes from repositioning higher-privilege windows, so tiling them
/// would just leave a gap where they should be.
pub(crate) fn is_out_of_reach(pid: u32) -> bool {
    static SELF_ELEVATED: OnceLock<bool> = OnceLock::new();
    let self_elevated =
        *SELF_ELEVATED.get_or_init(|| is_elevated(unsafe { GetCurrentProcessId() }));
    !self_elevated && is_elevated(pid)
}

/// Whether a process runs elevated. A process we aren't even allowed to
/// inspect counts as elevated: it's beyond our reach either way.
fn is_elevated(pid: u32) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return true;
        };
        let mut token = HANDLE::default();
        let opened = OpenProcessToken(process, TOKEN_QUERY, &mut token);
        let _ = CloseHandle(process);
        if opened.is_err() {
            return true;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0;
        let queried = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut TOKEN_ELEVATION as *mut c_void),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        queried.is_err() || elevation.TokenIsElevated != 0
    }
}
