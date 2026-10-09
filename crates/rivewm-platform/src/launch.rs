//! Starting and stopping other programs, for the config's `exec`,
//! `exec_once` and `stop_on_exit`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TokenStatistics,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, PROCESS_TERMINATE,
    TerminateProcess,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{HSTRING, w};

use crate::from_wide;

/// Starts `command` as the Run dialog (Win+R) would: `%VARIABLES%` are
/// expanded, and the program can be a full path, something on `PATH`, or
/// a registered app name like `wt`. A program path containing spaces must
/// be quoted. Doesn't wait for the program to finish.
///
/// Must run on a thread that has called [`init_launch_thread`].
pub fn launch(command: &str) -> Result<(), String> {
    // Split first, so a variable whose value has spaces in it (like a
    // user folder) stays part of the program.
    let (program, args) = split_program(command.trim());
    let (program, args) = (expand_vars(program), expand_vars(args));
    if program.is_empty() {
        return Err("empty command".into());
    }
    let dir = std::env::home_dir().map(|d| HSTRING::from(d.as_os_str()));
    let result = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(&program),
            &HSTRING::from(&args),
            dir.as_ref().unwrap_or(&HSTRING::new()),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success; the rest are error codes.
    match result.0 as usize {
        code if code > 32 => Ok(()),
        2 | 3 => Err(format!("`{program}` not found")),
        5 => Err("access denied".into()),
        code => Err(format!("error {code}")),
    }
}

/// Prepares the current thread for [`launch`]: the shell needs COM to
/// start some kinds of program.
pub fn init_launch_thread() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Force-stops every process of ours whose executable is named one of
/// `names` (e.g. `Discord.exe`, any case). Processes we aren't allowed to
/// stop are skipped. Returns how many were stopped.
pub fn stop_processes(names: &[String]) -> usize {
    if names.is_empty() {
        return 0;
    }
    let ours = unsafe { GetCurrentProcessId() };
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return 0;
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut stopped = 0;
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        let exe = from_wide(&entry.szExeFile);
        if entry.th32ProcessID != ours && names.iter().any(|n| n.eq_ignore_ascii_case(&exe)) {
            unsafe {
                if let Ok(process) = OpenProcess(PROCESS_TERMINATE, false, entry.th32ProcessID) {
                    if TerminateProcess(process, 0).is_ok() {
                        stopped += 1;
                    }
                    let _ = CloseHandle(process);
                }
            }
        }
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    unsafe {
        let _ = CloseHandle(snapshot);
    }
    stopped
}

/// Identifies the current login: Windows' id for this logon session, and
/// when the machine booted (seconds since the Unix epoch). Logon ids are
/// only unique until a reboot, hence the boot time. The boot time is
/// worked out from the uptime, so it can differ by a second or so between
/// calls.
pub fn login_session() -> Option<(u64, u64)> {
    let logon = unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).ok()?;
        let mut stats = TOKEN_STATISTICS::default();
        let mut len = 0;
        let queried = GetTokenInformation(
            token,
            TokenStatistics,
            Some(&mut stats as *mut TOKEN_STATISTICS as *mut _),
            size_of::<TOKEN_STATISTICS>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        queried.ok()?;
        let id = stats.AuthenticationId;
        ((id.HighPart as u32 as u64) << 32) | id.LowPart as u64
    };
    let uptime = Duration::from_millis(unsafe { GetTickCount64() });
    let booted = SystemTime::now().checked_sub(uptime)?;
    Some((logon, booted.duration_since(UNIX_EPOCH).ok()?.as_secs()))
}

/// Replaces each `%NAME%` with that environment variable, leaving unknown
/// ones as they are, as Windows does.
fn expand_vars(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('%') {
            Some(end) => match std::env::var(&after[..end]) {
                Ok(value) if end > 0 => {
                    out.push_str(&value);
                    rest = &after[end + 1..];
                }
                _ => {
                    out.push('%');
                    rest = after;
                }
            },
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Splits a command line into the program and the rest (its arguments).
/// The program may be "quoted" to include spaces.
fn split_program(command: &str) -> (&str, &str) {
    if let Some(quoted) = command.strip_prefix('"') {
        return match quoted.find('"') {
            Some(end) => (&quoted[..end], quoted[end + 1..].trim_start()),
            None => (quoted, ""),
        };
    }
    match command.find(char::is_whitespace) {
        Some(end) => (&command[..end], command[end..].trim_start()),
        None => (command, ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_program_from_arguments() {
        assert_eq!(split_program("wt"), ("wt", ""));
        assert_eq!(
            split_program("code  --new-window"),
            ("code", "--new-window")
        );
        assert_eq!(
            split_program(r#""C:\Program Files\App\app.exe" -x "y z""#),
            (r"C:\Program Files\App\app.exe", r#"-x "y z""#)
        );
    }

    #[test]
    fn expands_known_variables_only() {
        // SAFETY: tests in this module don't read this variable concurrently.
        unsafe { std::env::set_var("RIVEWM_TEST_DIR", r"C:\x") };
        assert_eq!(expand_vars(r"%RIVEWM_TEST_DIR%\a.exe"), r"C:\x\a.exe");
        assert_eq!(expand_vars("%RIVEWM_NOPE% 50%"), "%RIVEWM_NOPE% 50%");
        assert_eq!(expand_vars("100%%"), "100%%");
    }
}
