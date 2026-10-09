//! Win32 layer for rivewm.
//!
//! Thin, safe wrappers over the `windows` crate. Everything that touches an
//! `HWND` or calls into user32/dwmapi belongs here so the rest of rivewm can
//! stay platform independent.

#![cfg(windows)]

pub mod autostart;
mod cloak;
mod events;
mod hotkey;
pub mod ipc;
mod monitor;
mod process;
mod tray;
mod window;

pub use cloak::set_cloaked;
pub use events::{Event, EventThread, FailedHotkeys};
pub use hotkey::{Hotkey, ParseHotkeyError};
pub use monitor::{MonitorInfo, monitors};
pub use tray::{TrayAction, open_file};
pub use window::{
    BorderColor, Skip, WindowInfo, cursor_position, enumerate_windows, focus_desktop, focus_window,
    foreground_window, frame, is_topmost, is_window_cloaked, mouse_button_down, query_window,
    raise_window, set_border_color, set_frame, set_frame_redraw, set_topmost, show_window, title,
    window_at,
};

use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
/// Errors from Win32 calls.
pub use windows::core::{Error, Result};

/// Whether a Win32 call failed because Windows denied access, e.g. moving a
/// window that belongs to an elevated process.
pub fn is_access_denied(err: &Error) -> bool {
    err.code() == windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult()
}

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

/// Detaches from the console this process was started with, closing its
/// window if nothing else shares it. Used by the background process, which
/// may be launched from the Run registry key with a console of its own.
pub fn detach_console() {
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }
}
