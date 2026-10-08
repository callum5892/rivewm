//! Starting rivewm at login, via the per-user `Run` registry key. No admin
//! rights needed, and it shows up under Task Manager's Startup apps.

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteKeyValueW,
    RegGetValueW, RegOpenKeyExW, RegSetValueExW,
};
use windows::core::{HSTRING, PCWSTR, w};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE_NAME: PCWSTR = w!("rivewm");

/// Whether rivewm is set to start at login.
pub fn is_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    }
    .is_ok()
}

/// Starts `command` at login (a full command line, e.g. `"C:\...\rivewm.exe"
/// --daemon`), or with `None` stops starting rivewm at login.
pub fn set(command: Option<&str>) -> windows::core::Result<()> {
    unsafe {
        let Some(command) = command else {
            let result = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME);
            // Already absent counts as success.
            return if result.is_ok() || !is_enabled() {
                Ok(())
            } else {
                result.ok()
            };
        };
        let mut key = HKEY::default();
        RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, None, KEY_SET_VALUE, &mut key).ok()?;
        let wide = HSTRING::from(command);
        // REG_SZ data is the UTF-16 string including its terminating nul.
        let bytes = std::slice::from_raw_parts(
            wide.as_ptr() as *const u8,
            (wide.len() + 1) * size_of::<u16>(),
        );
        let result = RegSetValueExW(key, VALUE_NAME, None, REG_SZ, Some(bytes));
        let _ = RegCloseKey(key);
        result.ok()
    }
}
