mod config;
mod wm;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use config::Config;
use rivewm_core::{Command, WindowEvent, WindowId};
use rivewm_platform::{Event, Hotkey};
use tracing_subscriber::EnvFilter;
use wm::Wm;

const USAGE: &str = "\
rivewm - a tiling window manager for Windows

USAGE:
    rivewm [--config <path>] Run the window manager (Ctrl+C to quit). The
                             config defaults to ~\\.config\\rivewm\\config.toml
                             and is created on first run.
    rivewm --list [--all]    List monitors and the windows rivewm manages
                             (--all also shows skipped windows and why)
    rivewm --events [--all]  Log window events live until Ctrl+C
                             (--all includes unmanaged windows)
";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    rivewm_platform::enable_dpi_awareness()
        .context("failed to enable per-monitor DPI awareness")?;

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let config_path = match args.iter().position(|a| a == "--config") {
        Some(i) => {
            args.remove(i);
            if i >= args.len() {
                bail!("--config needs a path\n\n{USAGE}");
            }
            PathBuf::from(args.remove(i))
        }
        None => config::default_path(),
    };
    match args.first().map(String::as_str) {
        Some("--list") => list(args.iter().any(|a| a == "--all")),
        Some("--events") => events(args.iter().any(|a| a == "--all")),
        Some("--help" | "-h") => {
            print!("{USAGE}");
            Ok(())
        }
        None => run(&config_path),
        Some(other) => bail!("unknown argument `{other}`\n\n{USAGE}"),
    }
}

fn run(config_path: &Path) -> Result<()> {
    let config = config::load(config_path)?;
    tracing::info!(path = %config_path.display(), "loaded config");
    let (hotkeys, mut commands) = split_bindings(&config);

    // Hooks first, so no window that opens during startup is missed.
    let (events, rx) =
        rivewm_platform::EventThread::spawn(hotkeys).context("failed to install hooks")?;
    warn_failed_hotkeys(events.failed_hotkeys());

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        wm::restore_all();
        default_hook(info);
    }));
    ctrlc::set_handler(|| {
        tracing::info!("shutting down");
        wm::restore_all();
        std::process::exit(0);
    })
    .context("failed to install Ctrl+C handler")?;

    wm::recover_cloaked();
    let mut wm = Wm::new(config);
    wm.manage_existing();
    tracing::info!("rivewm running. Alt+Shift+E or Ctrl+C to quit.");

    for event in rx {
        match event {
            Event::Window(event) => wm.handle(event),
            Event::Hotkey(i) => match commands.get(i).cloned() {
                Some(Command::ReloadConfig) => match config::load(config_path) {
                    Ok(config) => {
                        let hotkeys;
                        (hotkeys, commands) = split_bindings(&config);
                        warn_failed_hotkeys(&events.set_hotkeys(hotkeys));
                        wm.set_config(config);
                        tracing::info!("reloaded config");
                    }
                    Err(err) => tracing::error!("{err:#}; keeping the current config"),
                },
                Some(command) => {
                    if wm.execute(command).is_break() {
                        break;
                    }
                }
                None => {}
            },
        }
    }
    tracing::info!("shutting down");
    wm::restore_all();
    Ok(())
}

/// Hotkeys to register, and the commands they trigger at the same indices.
fn split_bindings(config: &Config) -> (Vec<Hotkey>, Vec<Command>) {
    config.bindings.iter().cloned().unzip()
}

fn warn_failed_hotkeys(failed: &[(Hotkey, rivewm_platform::Error)]) {
    for (hotkey, err) in failed {
        tracing::warn!(%hotkey, %err, "couldn't register hotkey; is another app using it?");
    }
}

fn list(all: bool) -> Result<()> {
    let monitors = rivewm_platform::monitors();
    println!("Monitors:");
    for m in &monitors {
        println!(
            "  {:#x} {}{}  bounds {}  work {}",
            m.id.0,
            m.device,
            if m.primary { " (primary)" } else { "" },
            m.bounds,
            m.work_area
        );
    }

    let windows = rivewm_platform::enumerate_windows();
    let shown: Vec<_> = windows
        .iter()
        .filter(|w| all || w.is_manageable())
        .collect();
    println!(
        "\nWindows ({} managed, {} total):",
        windows.iter().filter(|w| w.is_manageable()).count(),
        windows.len()
    );
    for w in shown {
        let monitor = monitors
            .iter()
            .find(|m| m.id == w.monitor)
            .map_or("?", |m| m.device.as_str());
        let status = match w.skip {
            Some(reason) => format!("skip:{reason}"),
            None => {
                let mode = if w.floating { "float" } else { "tile" };
                if w.minimized {
                    format!("{mode} (minimized)")
                } else {
                    mode.into()
                }
            }
        };
        println!(
            "  {:#010x} {:<18} {:<22} {:<28} {:<20} {:<14} {}",
            w.id.0,
            status,
            truncate(w.process.as_deref().unwrap_or("?"), 22),
            truncate(&w.class, 28),
            w.frame.to_string(),
            monitor,
            truncate(&w.title, 60),
        );
    }
    Ok(())
}

fn events(all: bool) -> Result<()> {
    let (_thread, rx) =
        rivewm_platform::EventThread::spawn(Vec::new()).context("failed to install hooks")?;
    let start = std::time::Instant::now();

    // Windows we've seen as manageable. Hidden/destroyed windows can no longer
    // be classified, so this is how we decide whether those events matter.
    let mut known: HashSet<WindowId> = rivewm_platform::enumerate_windows()
        .into_iter()
        .filter(|w| w.is_manageable())
        .map(|w| w.id)
        .collect();

    println!(
        "Listening for window events ({} managed windows). Ctrl+C to stop.",
        known.len()
    );
    for event in rx.into_iter().filter_map(|e| match e {
        Event::Window(event) => Some(event),
        Event::Hotkey(_) => None,
    }) {
        let id = event.window();
        let info = rivewm_platform::query_window(id);
        let manageable = info.as_ref().is_some_and(|w| w.is_manageable());
        if manageable {
            known.insert(id);
        }
        let relevant = manageable || known.contains(&id);
        if matches!(event, WindowEvent::Destroyed(_)) {
            known.remove(&id);
        }
        if !(all || relevant) {
            continue;
        }

        let name = event_name(&event);
        let ms = start.elapsed().as_millis();
        let marker = if relevant { "*" } else { " " };
        match info {
            Some(w) => println!(
                "{ms:>8}ms {marker} {name:<16} {:#010x} {:<22} {:<20} {}",
                id.0,
                truncate(w.process.as_deref().unwrap_or("?"), 22),
                w.frame.to_string(),
                truncate(&w.title, 50),
            ),
            None => println!("{ms:>8}ms {marker} {name:<16} {:#010x} (gone)", id.0),
        }
    }
    Ok(())
}

fn event_name(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::Shown(_) => "shown",
        WindowEvent::Hidden(_) => "hidden",
        WindowEvent::Destroyed(_) => "destroyed",
        WindowEvent::Focused(_) => "focused",
        WindowEvent::Minimized(_) => "minimized",
        WindowEvent::Restored(_) => "restored",
        WindowEvent::MoveSizeStarted(_) => "movesize-start",
        WindowEvent::MoveSizeEnded(_) => "movesize-end",
        WindowEvent::LocationChanged(_) => "location",
        WindowEvent::TitleChanged(_) => "title",
        WindowEvent::Cloaked(_) => "cloaked",
        WindowEvent::Uncloaked(_) => "uncloaked",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
