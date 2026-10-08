//! Running rivewm without a terminal: the detached `--daemon` process,
//! `--background` to launch one, its log file, and starting at login.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

use crate::config;

/// Win32 process creation flags: no console at all, and not part of the
/// terminal's process group (so its Ctrl+C doesn't reach the daemon).
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// `%LOCALAPPDATA%\rivewm\rivewm.log`, written by the background process.
pub fn log_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("rivewm").join("rivewm.log")
}

/// Sets up logging for the `--daemon` process: it has no console, so logs
/// go to [`log_path`], replaced on each start.
pub fn init_file_logging() -> Result<()> {
    let path = log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::File::create(&path)
        .with_context(|| format!("couldn't create {}", path.display()))?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(false)
        .with_writer(Mutex::new(file))
        .init();
    Ok(())
}

/// The arguments that start a daemon using `config_path`: `--daemon`, plus
/// `--config` if it isn't the default location.
fn daemon_args(config_path: &Path) -> Vec<String> {
    let mut args = vec!["--daemon".to_owned()];
    if config_path != config::default_path() {
        args.push("--config".to_owned());
        args.push(config_path.display().to_string());
    }
    args
}

/// `rivewm --background`: starts a detached rivewm and waits until it's
/// answering IPC, or reports why it didn't come up.
pub fn start(config_path: &Path) -> Result<()> {
    if rivewm_platform::ipc::request("query state").is_ok() {
        bail!("rivewm is already running");
    }
    let exe = std::env::current_exe().context("couldn't find rivewm.exe")?;
    let mut child = std::process::Command::new(exe)
        .args(daemon_args(config_path))
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("couldn't start rivewm in the background")?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if rivewm_platform::ipc::request("query state").is_ok() {
            println!(
                "rivewm is running in the background (pid {}).\nLog: {}",
                child.id(),
                log_path().display()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            let log = std::fs::read_to_string(log_path()).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(10).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            bail!(
                "rivewm exited during startup ({status}). From {}:\n{}",
                log_path().display(),
                tail.join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "rivewm started but isn't answering yet; see {}",
        log_path().display()
    )
}

/// The command line registered to run at login.
fn autostart_command(config_path: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("couldn't find rivewm.exe")?;
    let mut command = format!("\"{}\"", exe.display());
    for arg in daemon_args(config_path) {
        if arg.contains(' ') {
            command.push_str(&format!(" \"{arg}\""));
        } else {
            command.push(' ');
            command.push_str(&arg);
        }
    }
    Ok(command)
}

/// Turns starting at login on or off for this rivewm.exe and config.
pub fn set_autostart(enabled: bool, config_path: &Path) -> Result<()> {
    let command = enabled
        .then(|| autostart_command(config_path))
        .transpose()?;
    rivewm_platform::autostart::set(command.as_deref())
        .context("couldn't update the startup registry entry")?;
    match command {
        Some(command) => tracing::info!(command, "rivewm will start at login"),
        None => tracing::info!("rivewm won't start at login"),
    }
    Ok(())
}

/// `rivewm --autostart [on|off]`.
pub fn autostart_cli(arg: Option<&str>, config_path: &Path) -> Result<()> {
    match arg {
        Some("on") => {
            set_autostart(true, config_path)?;
            println!(
                "rivewm will start at login: {}",
                autostart_command(config_path)?
            );
        }
        Some("off") => {
            set_autostart(false, config_path)?;
            println!("rivewm won't start at login.");
        }
        None => println!(
            "Start at login is {}.",
            if rivewm_platform::autostart::is_enabled() {
                "on"
            } else {
                "off"
            }
        ),
        Some(other) => bail!("expected `on` or `off`, not `{other}`"),
    }
    Ok(())
}
