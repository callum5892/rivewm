use anyhow::{Context, Result, bail};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
rivewm - a tiling window manager for Windows

USAGE:
    rivewm --list [--all]    List monitors and tileable windows
                             (--all also shows skipped windows and why)
";

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    rivewm_platform::enable_dpi_awareness().context("failed to enable per-monitor DPI awareness")?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--list") => list(args.iter().any(|a| a == "--all")),
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
    let shown: Vec<_> = windows.iter().filter(|w| all || w.is_manageable()).collect();
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

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}
