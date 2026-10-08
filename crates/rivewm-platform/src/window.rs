use std::ffi::c_void;

use rivewm_core::{MonitorId, Rect, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetCursorPos,
    GetForegroundWindow, GetShellWindow, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IsIconic,
    IsWindow, IsWindowVisible, IsZoomed, SW_RESTORE, SW_SHOWNA, SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowPos,
    ShowWindow, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_THICKFRAME,
};
use windows::core::BOOL;

use crate::from_wide;
use crate::monitor::rect_from_win32;
use crate::process::{is_out_of_reach, process_name};

/// Why a top-level window is not a tiling candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Invisible,
    /// Hidden by DWM (UWP apps that are suspended, other virtual desktops).
    Cloaked,
    Child,
    ToolWindow,
    NoActivate,
    NoTitle,
    /// Neither a caption nor a resizable frame, e.g. splash screens.
    NoFrame,
    ZeroSize,
    /// Runs as administrator while rivewm doesn't, so Windows won't let us
    /// move it (e.g. Task Manager).
    Elevated,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Skip::Invisible => "invisible",
            Skip::Cloaked => "cloaked",
            Skip::Child => "child",
            Skip::ToolWindow => "tool-window",
            Skip::NoActivate => "no-activate",
            Skip::Elevated => "elevated",
            Skip::NoTitle => "no-title",
            Skip::NoFrame => "no-frame",
            Skip::ZeroSize => "zero-size",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: WindowId,
    pub title: String,
    pub class: String,
    pub pid: u32,
    pub process: Option<String>,
    /// Visible bounds, excluding the invisible resize borders.
    pub frame: Rect,
    /// Bounds as `GetWindowRect` reports them, including invisible borders.
    /// The difference to `frame` is what we compensate for when positioning.
    pub window_rect: Rect,
    pub monitor: MonitorId,
    pub minimized: bool,
    /// `None` means rivewm should manage the window.
    pub skip: Option<Skip>,
    /// Whether to float it rather than tile it by default: dialogs and other
    /// owned windows, and windows that can't be resized.
    pub floating: bool,
}

impl WindowInfo {
    pub fn is_manageable(&self) -> bool {
        self.skip.is_none()
    }
}

/// Lists every top-level window, in z-order (topmost first).
pub fn enumerate_windows() -> Vec<WindowInfo> {
    let mut hwnds: Vec<HWND> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect_window), LPARAM(&mut hwnds as *mut _ as isize));
    }
    hwnds.into_iter().map(window_info).collect()
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` passed by `enumerate_windows`,
    // which outlives the synchronous enumeration.
    let hwnds = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    hwnds.push(hwnd);
    true.into()
}

fn hwnd(id: WindowId) -> HWND {
    HWND(id.0 as *mut c_void)
}

/// The window that currently has keyboard focus, if any.
pub fn foreground_window() -> Option<WindowId> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then_some(WindowId(hwnd.0 as isize))
}

/// Gives keyboard focus to the desktop, so keys don't go to a window that
/// was just cloaked.
pub fn focus_desktop() -> bool {
    unsafe {
        let desktop = GetShellWindow();
        !desktop.is_invalid() && SetForegroundWindow(desktop).as_bool()
    }
}

/// A window border colour for [`set_border_color`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderColor {
    /// Whatever Windows would normally draw.
    Default,
    /// No border at all.
    Hidden,
    Rgb(u8, u8, u8),
}

/// Recolours a window's own 1px border (Windows 11 and later; fails
/// harmlessly on Windows 10). Works on other processes' windows.
pub fn set_border_color(id: WindowId, color: BorderColor) -> windows::core::Result<()> {
    // COLORREF is 0x00BBGGRR, with two reserved sentinel values.
    let value: u32 = match color {
        BorderColor::Default => 0xFFFF_FFFF,
        BorderColor::Hidden => 0xFFFF_FFFE,
        BorderColor::Rgb(r, g, b) => (b as u32) << 16 | (g as u32) << 8 | r as u32,
    };
    unsafe {
        DwmSetWindowAttribute(
            hwnd(id),
            DWMWA_BORDER_COLOR,
            &value as *const u32 as *const c_void,
            size_of::<u32>() as u32,
        )
    }
}

/// Whether the window is "always on top", by its own choice or ours.
pub fn is_topmost(id: WindowId) -> bool {
    let ex_style = WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(hwnd(id), GWL_EXSTYLE) } as u32);
    ex_style.contains(WS_EX_TOPMOST)
}

/// Puts a window in (or takes it out of) the always-on-top band, without
/// moving, resizing or activating it.
pub fn set_topmost(id: WindowId, topmost: bool) -> windows::core::Result<()> {
    let after = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    unsafe {
        SetWindowPos(
            hwnd(id),
            Some(after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        )
    }
}

/// Brings a window to the front of its band without activating it.
pub fn raise_window(id: WindowId) -> windows::core::Result<()> {
    unsafe {
        SetWindowPos(
            hwnd(id),
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
        )
    }
}

/// Where the mouse cursor is, in physical screen pixels.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point) }.ok()?;
    Some((point.x, point.y))
}

/// A window's title. Much cheaper than [`query_window`].
pub fn title(id: WindowId) -> String {
    window_title(hwnd(id))
}

/// Whether DWM currently has the window cloaked, by us or anyone else.
pub fn is_window_cloaked(id: WindowId) -> bool {
    is_cloaked(hwnd(id))
}

/// Gives a window keyboard focus. Returns `false` if Windows refused, which
/// its foreground-lock rules allow it to do.
pub fn focus_window(id: WindowId) -> bool {
    unsafe { SetForegroundWindow(hwnd(id)) }.as_bool()
}

/// Positions a window so its *visible* frame lands exactly on `frame`.
///
/// Most windows have invisible resize borders that `SetWindowPos` counts as
/// part of the window; we measure them and add them back. Maximized windows
/// are restored first, since Windows ignores moves while maximized.
///
/// Uses `SWP_ASYNCWINDOWPOS` so a hung application can't block the WM.
pub fn set_frame(id: WindowId, frame: Rect) -> windows::core::Result<()> {
    let hwnd = hwnd(id);
    unsafe {
        if IsZoomed(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
    }
    let outer = window_rect(hwnd).unwrap_or(frame);
    let visible = extended_frame_bounds(hwnd).unwrap_or(outer);
    let (left, top) = (visible.x - outer.x, visible.y - outer.y);
    let (right, bottom) = (
        outer.right() - visible.right(),
        outer.bottom() - visible.bottom(),
    );
    unsafe {
        SetWindowPos(
            hwnd,
            None,
            frame.x - left,
            frame.y - top,
            frame.width + left + right,
            frame.height + top + bottom,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_ASYNCWINDOWPOS,
        )
    }
}

/// Makes a window visible again without activating it. Used to undo any
/// hiding on shutdown.
pub fn show_window(id: WindowId) {
    let hwnd = hwnd(id);
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() && !IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOWNA);
        }
    }
}

/// Looks up a single window. Returns `None` if it no longer exists.
pub fn query_window(id: WindowId) -> Option<WindowInfo> {
    let hwnd = hwnd(id);
    unsafe { IsWindow(Some(hwnd)) }
        .as_bool()
        .then(|| window_info(hwnd))
}

fn window_info(hwnd: HWND) -> WindowInfo {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let window_rect = window_rect(hwnd).unwrap_or_default();
    let frame = extended_frame_bounds(hwnd).unwrap_or(window_rect);
    let title = window_title(hwnd);
    let (skip, floating) = match classify(hwnd, pid, &title, frame) {
        Ok(floating) => (None, floating),
        Err(skip) => (Some(skip), false),
    };
    WindowInfo {
        id: WindowId(hwnd.0 as isize),
        title,
        class: window_class(hwnd),
        pid,
        process: process_name(pid),
        frame,
        window_rect,
        monitor: MonitorId(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }.0 as isize),
        minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        skip,
        floating,
    }
}

/// Decides how to treat a window: `Err` to leave it alone, otherwise
/// whether it should float.
fn classify(hwnd: HWND, pid: u32, title: &str, frame: Rect) -> Result<bool, Skip> {
    let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32);
    let ex_style = WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32);

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(Skip::Invisible);
    }
    if is_cloaked(hwnd) {
        return Err(Skip::Cloaked);
    }
    if style.contains(WS_CHILD) {
        return Err(Skip::Child);
    }
    if ex_style.contains(WS_EX_TOOLWINDOW) && !ex_style.contains(WS_EX_APPWINDOW) {
        return Err(Skip::ToolWindow);
    }
    if ex_style.contains(WS_EX_NOACTIVATE) {
        return Err(Skip::NoActivate);
    }
    if title.trim().is_empty() {
        return Err(Skip::NoTitle);
    }
    if !style.contains(WS_CAPTION) && !style.contains(WS_THICKFRAME) {
        return Err(Skip::NoFrame);
    }
    if frame.is_empty() {
        return Err(Skip::ZeroSize);
    }
    // Last, as it's the most expensive check.
    if is_out_of_reach(pid) {
        return Err(Skip::Elevated);
    }
    let owned = unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid());
    Ok(owned || !style.contains(WS_THICKFRAME))
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        )
    }
    .is_ok()
        && cloaked != 0
}

fn extended_frame_bounds(hwnd: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(rect_from_win32(rect))
}

fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.ok()?;
    Some(rect_from_win32(rect))
}

fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    from_wide(&buf[..len.max(0) as usize])
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    from_wide(&buf[..len.max(0) as usize])
}
