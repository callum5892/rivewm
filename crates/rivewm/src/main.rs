use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use rivewm_core::{WindowEvent, WindowId};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
rivewm - a tiling window manager for Windows

USAGE:
    rivewm --list [--all]    List monitors and tileable windows
                             (--all also shows skipped windows and why)
    rivewm --events [--all]  Log window events live until Ctrl+C
                             (--all includes untileable windows)
";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    rivewm_platform::enable_dpi_awareness()
        .context("failed to enable per-monitor DPI awareness")?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list") => list(args.iter().any(|a| a == "--all")),
        Some("--events") => events(args.iter().any(|a| a == "--all")),
        Some("--help" | "-h") | None => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown argument `{other}`\n\n{USAGE}"),
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
        "\nWindows ({} tileable, {} total):",
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
            None if w.minimized => "tile (minimized)".into(),
            None => "tile".into(),
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
    let (_thread, rx) = rivewm_platform::EventThread::spawn().context("failed to install hooks")?;
    let start = std::time::Instant::now();

    // Windows we've seen as tileable. Hidden/destroyed windows can no longer
    // be classified, so this is how we decide whether those events matter.
    let mut known: HashSet<WindowId> = rivewm_platform::enumerate_windows()
        .into_iter()
        .filter(|w| w.is_manageable())
        .map(|w| w.id)
        .collect();

    println!(
        "Listening for window events ({} tileable windows). Ctrl+C to stop.",
        known.len()
    );
    for event in rx {
        let id = event.window();
        let info = rivewm_platform::query_window(id);
        let tileable = info.as_ref().is_some_and(|w| w.is_manageable());
        if tileable {
            known.insert(id);
        }
        let relevant = tileable || known.contains(&id);
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
