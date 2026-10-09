//! The config's `[programs]`: starting programs alongside rivewm and
//! stopping them when it quits.
//!
//! `exec_once` should run once per login, not on every restart of rivewm,
//! so the login it last ran for is recorded in
//! `%LOCALAPPDATA%\rivewm\exec_once.json`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::Programs;

/// How far apart two readings of the boot time may be and still mean the
/// same boot (it's worked out from the uptime, so it wobbles slightly).
const BOOT_TIME_SLACK: u64 = 60;

/// The login `exec_once` last ran for (see
/// [`rivewm_platform::launch::login_session`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Login {
    logon: u64,
    booted: u64,
}

impl Login {
    fn current() -> Option<Self> {
        let (logon, booted) = rivewm_platform::launch::login_session()?;
        Some(Self { logon, booted })
    }

    fn same_as(self, other: Self) -> bool {
        self.logon == other.logon && self.booted.abs_diff(other.booted) <= BOOT_TIME_SLACK
    }
}

fn record_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("rivewm")
        .join("exec_once.json")
}

/// Whether `exec_once` has already run since this login, recording that it
/// now has if not.
fn exec_once_already_ran() -> bool {
    let Some(now) = Login::current() else {
        // Can't tell; running again is better than never running.
        return false;
    };
    let path = record_path();
    let last: Option<Login> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    if last.is_some_and(|last| last.same_as(now)) {
        return true;
    }
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, serde_json::to_vec(&now).unwrap_or_default()));
    if let Err(err) = written {
        warn!(path = %path.display(), %err, "couldn't record that exec_once ran");
    }
    false
}

/// Starts `exec`, and `exec_once` if it hasn't run since this login. Runs
/// on its own thread so a slow-starting program can't hold up the WM.
pub fn start(programs: &Programs) {
    let mut commands = programs.exec.clone();
    if !programs.exec_once.is_empty() {
        if exec_once_already_ran() {
            info!("exec_once already ran since this login; skipping it");
        } else {
            commands.extend(programs.exec_once.iter().cloned());
        }
    }
    if commands.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        rivewm_platform::launch::init_launch_thread();
        for command in commands {
            match rivewm_platform::launch::launch(&command) {
                Ok(()) => info!(command, "started program"),
                Err(err) => warn!(command, err, "couldn't start program"),
            }
        }
    });
}

/// Closes the programs listed in `stop_on_exit`.
pub fn stop(programs: &Programs) {
    if programs.stop_on_exit.is_empty() {
        return;
    }
    let stopped = rivewm_platform::launch::stop_processes(&programs.stop_on_exit);
    info!(stopped, "stopped programs listed in stop_on_exit");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_login_tolerates_boot_time_wobble() {
        let a = Login {
            logon: 7,
            booted: 1000,
        };
        assert!(a.same_as(Login { booted: 1002, ..a }));
        assert!(!a.same_as(Login { booted: 5000, ..a }));
        assert!(!a.same_as(Login { logon: 8, ..a }));
    }
}
