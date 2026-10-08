mod background;
mod config;
mod subscribe;
mod wm;

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use config::Config;
use rivewm_core::{Command, WindowEvent, WindowId};
use rivewm_platform::{Event, EventThread, Hotkey, TrayAction};
use serde_json::{Value, json};
use subscribe::Snapshot;
use tracing_subscriber::EnvFilter;
use wm::Wm;

const USAGE: &str = "\
rivewm - a tiling window manager for Windows

USAGE:
    rivewm [--config <path>] Run the window manager in this terminal (Ctrl+C
                             to quit). The config defaults to
                             ~\\.config\\rivewm\\config.toml and is created on
                             first run.
    rivewm --background      Run it detached from the terminal, with a tray
                             icon. Logs go to %LOCALAPPDATA%\\rivewm\\rivewm.log
    rivewm --autostart [on|off]
                             Start rivewm in the background at login (or show
                             whether it will)
    rivewm --check-config    Check the config file for mistakes without
                             starting anything
    rivewm msg <request>     Send a request to the running rivewm and print
                             its JSON reply. A request is any config command
                             (e.g. `workspace 3`, `focus-window 0x1a2b`) or
                             `query state` for monitors, workspaces, layout
                             and windows, or `subscribe` to stream events
                             (one JSON line each) until interrupted.
    rivewm --list [--all]    List monitors and the windows rivewm manages
                             (--all also shows skipped windows and why)
    rivewm --events [--all]  Log window events live until Ctrl+C
                             (--all includes unmanaged windows)
";

fn main() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let daemon = args.first().is_some_and(|a| a == "--daemon");
    if daemon {
        // Launched from the Run key we'd still get a console window; drop it.
        rivewm_platform::detach_console();
        background::init_file_logging()?;
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
            .init();
    }

    rivewm_platform::enable_dpi_awareness()
        .context("failed to enable per-monitor DPI awareness")?;

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
        Some("--daemon") => run(&config_path).inspect_err(|err| {
            // No console to print to; make sure it reaches the log.
            tracing::error!("{err:#}");
        }),
        Some("--background") => background::start(&config_path),
        Some("--autostart") => {
            background::autostart_cli(args.get(1).map(String::as_str), &config_path)
        }
        Some("--check-config") => {
            let config = config::load(&config_path)?;
            println!(
                "{} is valid: {} bindings, {} rules.",
                config_path.display(),
                config.bindings.len(),
                config.rules.len()
            );
            Ok(())
        }
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
    /// An IPC client subscribed; send it event lines here.
    Subscribe(SyncSender<String>),
}

/// How long display changes must stop before monitors are re-read.
const DISPLAY_SETTLE_TIME: Duration = Duration::from_millis(500);

/// Events a subscriber may have queued before we drop it as too slow.
const SUBSCRIBER_BACKLOG: usize = 256;

fn run(config_path: &Path) -> Result<()> {
    let config = config::load(config_path)?;
    tracing::info!(path = %config_path.display(), "loaded config");
    let (hotkeys, commands) = split_bindings(&config);
    let (tx, rx) = mpsc::channel::<Msg>();

    // The IPC pipe goes first: it doubles as the check that no other rivewm
    // is running, before we touch any windows.
    let ipc_tx = tx.clone();
    rivewm_platform::ipc::serve(move |line, conn| {
        if line.trim() == "subscribe" {
            // Runs on this client's own thread until it disconnects.
            let (events_tx, events_rx) = mpsc::sync_channel(SUBSCRIBER_BACKLOG);
            if ipc_tx.send(Msg::Subscribe(events_tx)).is_err()
                || conn.send_line(&json!({ "ok": true }).to_string()).is_err()
            {
                return;
            }
            for event in events_rx {
                if conn.send_line(&event).is_err() {
                    break;
                }
            }
            return;
        }
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let response = if ipc_tx
            .send(Msg::Request(line.to_owned(), reply_tx))
            .is_err()
        {
            error_json("rivewm is shutting down")
        } else {
            reply_rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap_or_else(|_| error_json("rivewm didn't respond in time"))
        };
        let _ = conn.send_line(&response);
    })?;
    tracing::info!(
        pipe = rivewm_platform::ipc::pipe_name(),
        "listening for IPC"
    );

    // Hooks next, so no window that opens during startup is missed.
    let events = rivewm_platform::EventThread::spawn(hotkeys, true, move |event| {
        let _ = tx.send(Msg::Event(event));
    })
    .context("failed to install hooks")?;
    warn_failed_hotkeys(events.failed_hotkeys());

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        wm::restore_all();
        // The background process has no console, so log it as well.
        tracing::error!("{info}");
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
        subscribers: Vec::new(),
        last_snapshot: Snapshot::default(),
    };
    // Display changes arrive in bursts while monitors settle; act once, a
    // little after the last one.
    let mut resync_monitors_at: Option<Instant> = None;
    loop {
        // Sleep until the next message or whichever timer is due first.
        let deadline = [resync_monitors_at, app.wm.next_timer()]
            .into_iter()
            .flatten()
            .min();
        let msg = match deadline {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => {
                    if resync_monitors_at.is_some_and(|at| at <= Instant::now()) {
                        resync_monitors_at = None;
                        app.wm.sync_monitors();
                    }
                    app.wm.on_timer();
                    app.publish_changes();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            },
        };
        let flow = match msg {
            Msg::Event(Event::Window(event)) => {
                app.wm.handle(event);
                ControlFlow::Continue(())
            }
            Msg::Event(Event::DisplayChanged) => {
                resync_monitors_at = Some(Instant::now() + DISPLAY_SETTLE_TIME);
                ControlFlow::Continue(())
            }
            Msg::Event(Event::Tray(action)) => app.tray(action),
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
            Msg::Subscribe(subscriber) => {
                app.subscribe(subscriber);
                ControlFlow::Continue(())
            }
        };
        app.publish_changes();
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
    /// IPC clients receiving events.
    subscribers: Vec<SyncSender<String>>,
    /// State as of the last events sent, for diffing. Only kept current
    /// while there are subscribers.
    last_snapshot: Snapshot,
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
            self.broadcast(&json!({ "event": "config_reloaded" }));
            return Ok(ControlFlow::Continue(()));
        }
        Ok(self.wm.execute(command))
    }

    fn subscribe(&mut self, subscriber: SyncSender<String>) {
        if self.subscribers.is_empty() {
            // Nobody was watching, so the last snapshot is stale.
            self.last_snapshot = self.wm.snapshot();
        }
        self.subscribers.push(subscriber);
        tracing::debug!(count = self.subscribers.len(), "IPC subscriber added");
    }

    /// Sends subscribers an event for everything that changed since the last
    /// call. Free when nobody is subscribed.
    fn publish_changes(&mut self) {
        if self.subscribers.is_empty() {
            return;
        }
        let snapshot = self.wm.snapshot();
        if snapshot == self.last_snapshot {
            return;
        }
        for event in subscribe::diff(&self.last_snapshot, &snapshot) {
            self.broadcast(&event);
        }
        self.last_snapshot = snapshot;
    }

    /// Sends one event to every subscriber, dropping any that have gone away
    /// or fallen too far behind to keep up.
    fn broadcast(&mut self, event: &Value) {
        let line = event.to_string();
        self.subscribers
            .retain(|s| s.try_send(line.clone()).is_ok());
    }

    /// Acts on a pick from the tray icon's menu.
    fn tray(&mut self, action: TrayAction) -> ControlFlow<()> {
        match action {
            TrayAction::ReloadConfig => {
                if let Err(err) = self.run_command(Command::ReloadConfig) {
                    tracing::error!("{err:#}");
                }
            }
            TrayAction::OpenConfig => rivewm_platform::open_file(self.config_path),
            TrayAction::OpenLog => {
                let log = background::log_path();
                if log.exists() {
                    rivewm_platform::open_file(&log);
                } else {
                    tracing::warn!(
                        "no log file: rivewm logs to its terminal unless started with --background"
                    );
                }
            }
            TrayAction::ToggleAutostart => {
                let enable = !rivewm_platform::autostart::is_enabled();
                if let Err(err) = background::set_autostart(enable, self.config_path) {
                    tracing::error!("{err:#}");
                }
            }
            TrayAction::Quit => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
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
    let request = words.join(" ");
    if request == "subscribe" {
        // Print events as they arrive until rivewm goes away.
        for line in rivewm_platform::ipc::stream(&request)? {
            println!("{}", line?);
        }
        return Ok(());
    }
    let response = rivewm_platform::ipc::request(&request)?;
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
    let _thread = EventThread::spawn(Vec::new(), false, move |event| {
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
