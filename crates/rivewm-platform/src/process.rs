use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
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
