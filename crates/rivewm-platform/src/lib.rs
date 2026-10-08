//! Win32 layer for rivewm.
//!
//! Thin, safe wrappers over the `windows` crate. Everything that touches an
//! `HWND` or calls into user32/dwmapi belongs here so the rest of rivewm can
//! stay platform independent.

#![cfg(windows)]

mod cloak;
mod events;
mod hotkey;
mod monitor;
mod process;
mod window;

pub use cloak::set_cloaked;
pub use events::{Event, EventThread};
pub use hotkey::{Hotkey, ParseHotkeyError};
pub use monitor::{MonitorInfo, monitors};
pub use window::{
    Skip, WindowInfo, enumerate_windows, focus_desktop, focus_window, foreground_window,
    is_window_cloaked, query_window, set_frame, show_window,
};

use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
/// Errors from Win32 calls.
pub use windows::core::{Error, Result};

/// Opts the process into per-monitor DPI awareness (v2) so every coordinate
/// we read or write is in physical pixels. Must run before any window or
/// monitor API is used.
pub fn enable_dpi_awareness() -> windows::core::Result<()> {
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
}

/// Converts a nul-terminated (or fully used) UTF-16 buffer to a `String`.
fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}
