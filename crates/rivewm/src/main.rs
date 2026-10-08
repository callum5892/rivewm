mod config;
mod wm;

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use config::Config;
use rivewm_core::{Command, WindowEvent, WindowId};
use rivewm_platform::{Event, EventThread, Hotkey};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use wm::Wm;

const USAGE: &str = "\
rivewm - a tiling window manager for Windows

USAGE:
    rivewm [--config <path>] Run the window manager (Ctrl+C to quit). The
                             config defaults to ~\\.config\\rivewm\\config.toml
                             and is created on first run.
    rivewm msg <request>     Send a request to the running rivewm and print
                             its JSON reply. A request is any config command
                             (e.g. `workspace 3`, `focus-window 0x1a2b`) or
                             `query state` for monitors, workspaces, layout
                             and windows.
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
        Some("msg") => msg(&args[1..]),
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

/// Everything the main loop reacts to.
enum Msg {
    Event(Event),
    /// An IPC request line, and where to send the response line.
    Request(String, SyncSender<String>),
}

fn run(config_path: &Path) -> Result<()> {
    let config = config::load(config_path)?;
    tracing::info!(path = %config_path.display(), "loaded config");
    let (hotkeys, commands) = split_bindings(&config);
    let (tx, rx) = mpsc::channel::<Msg>();

    // The IPC pipe goes first: it doubles as the check that no other rivewm
    // is running, before we touch any windows.
    let ipc_tx = tx.clone();
    rivewm_platform::ipc::serve(move |line| {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        if ipc_tx
            .send(Msg::Request(line.to_owned(), reply_tx))
            .is_err()
        {
            return error_json("rivewm is shutting down");
        }
        reply_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| error_json("rivewm didn't respond in time"))
    })?;
    tracing::info!(
        pipe = rivewm_platform::ipc::pipe_name(),
        "listening for IPC"
    );

    // Hooks next, so no window that opens during startup is missed.
    let events = rivewm_platform::EventThread::spawn(hotkeys, move |event| {
        let _ = tx.send(Msg::Event(event));
    })
    .context("failed to install hooks")?;
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

    let mut app = App {
        wm,
        events,
        commands,
        config_path,
    };
    for msg in rx {
        let flow = match msg {
            Msg::Event(Event::Window(event)) => {
                app.wm.handle(event);
                ControlFlow::Continue(())
            }
            Msg::Event(Event::Hotkey(i)) => match app.commands.get(i).cloned() {
                Some(command) => app.run_command(command).unwrap_or_else(|err| {
                    tracing::error!("{err:#}");
                    ControlFlow::Continue(())
                }),
                None => ControlFlow::Continue(()),
            },
            Msg::Request(line, reply) => {
                let (response, flow) = app.answer(&line);
                let _ = reply.send(response.to_string());
                flow
            }
        };
        if flow.is_break() {
            break;
        }
    }
    tracing::info!("shutting down");
    wm::restore_all();
    Ok(())
}

/// The running WM plus what's needed to reload its config.
struct App<'a> {
    wm: Wm,
    events: EventThread,
    /// Commands for each registered hotkey, by hotkey index.
    commands: Vec<Command>,
    config_path: &'a Path,
}

impl App<'_> {
    fn run_command(&mut self, command: Command) -> Result<ControlFlow<()>> {
        if command == Command::ReloadConfig {
            let config = config::load(self.config_path).context("keeping the current config")?;
            let hotkeys;
            (hotkeys, self.commands) = split_bindings(&config);
            warn_failed_hotkeys(&self.events.set_hotkeys(hotkeys));
            self.wm.set_config(config);
            tracing::info!("reloaded config");
            return Ok(ControlFlow::Continue(()));
        }
        Ok(self.wm.execute(command))
    }

    /// Handles one IPC request: `query state`, or any command in its config
    /// file form.
    fn answer(&mut self, line: &str) -> (Value, ControlFlow<()>) {
        tracing::debug!(request = line, "IPC");
        if line.trim() == "query state" {
            return (
                json!({ "ok": true, "state": self.wm.state() }),
                ControlFlow::Continue(()),
            );
        }
        let result = line
            .parse::<Command>()
            .map_err(anyhow::Error::from)
            .and_then(|command| self.run_command(command));
        match result {
            Ok(flow) => (json!({ "ok": true }), flow),
            Err(err) => (
                json!({ "ok": false, "error": format!("{err:#}") }),
                ControlFlow::Continue(()),
            ),
        }
    }
}

fn error_json(message: &str) -> String {
    json!({ "ok": false, "error": message }).to_string()
}

/// `rivewm msg <request>`: sends a request to the running rivewm and prints
/// the JSON response. Exits non-zero if the request failed.
fn msg(words: &[String]) -> Result<()> {
    if words.is_empty() {
        bail!("usage: rivewm msg <command>   e.g. rivewm msg workspace 3\n\n{USAGE}");
    }
    let response = rivewm_platform::ipc::request(&words.join(" "))?;
    println!("{response}");
    let ok = serde_json::from_str::<Value>(&response)
        .ok()
        .and_then(|v| v["ok"].as_bool())
        .unwrap_or(false);
    if !ok {
        std::process::exit(1);
    }
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
    let (tx, rx) = mpsc::channel();
    let _thread = EventThread::spawn(Vec::new(), move |event| {
        if let Event::Window(event) = event {
            let _ = tx.send(event);
        }
    })
    .context("failed to install hooks")?;
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
    for event in rx {
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
