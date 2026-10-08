use std::ffi::c_void;

use rivewm_core::{MonitorId, Rect, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    WINDOW_EX_STYLE, WINDOW_STYLE, WS_CAPTION, WS_CHILD, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_THICKFRAME,
};
use windows::core::BOOL;

use crate::from_wide;
use crate::monitor::rect_from_win32;
use crate::process::process_name;

/// Why a top-level window is not a tiling candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Invisible,
    /// Hidden by DWM (UWP apps that are suspended, other virtual desktops).
    Cloaked,
    Child,
    ToolWindow,
    NoActivate,
    /// Has an owner, so it's a dialog or popup of another window. These will
    /// float rather than tile once floating exists.
    Owned,
    NoTitle,
    /// Neither a caption nor a resizable frame, e.g. splash screens.
    NoFrame,
    ZeroSize,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Skip::Invisible => "invisible",
            Skip::Cloaked => "cloaked",
            Skip::Child => "child",
            Skip::ToolWindow => "tool-window",
            Skip::NoActivate => "no-activate",
            Skip::Owned => "owned",
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
    /// `None` means the window is a tiling candidate.
    pub skip: Option<Skip>,
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

pub(crate) fn window_info(hwnd: HWND) -> WindowInfo {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let window_rect = window_rect(hwnd).unwrap_or_default();
    let frame = extended_frame_bounds(hwnd).unwrap_or(window_rect);
    let title = window_title(hwnd);
    let skip = classify(hwnd, &title, frame);
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
    }
}

fn classify(hwnd: HWND, title: &str, frame: Rect) -> Option<Skip> {
    let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32);
    let ex_style = WINDOW_EX_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32);

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Some(Skip::Invisible);
    }
    if is_cloaked(hwnd) {
        return Some(Skip::Cloaked);
    }
    if style.contains(WS_CHILD) {
        return Some(Skip::Child);
    }
    if ex_style.contains(WS_EX_TOOLWINDOW) && !ex_style.contains(WS_EX_APPWINDOW) {
        return Some(Skip::ToolWindow);
    }
    if ex_style.contains(WS_EX_NOACTIVATE) {
        return Some(Skip::NoActivate);
    }
    if unsafe { GetWindow(hwnd, GW_OWNER) }.is_ok_and(|owner| !owner.is_invalid()) {
        return Some(Skip::Owned);
    }
    if title.trim().is_empty() {
        return Some(Skip::NoTitle);
    }
    if !style.contains(WS_CAPTION) && !style.contains(WS_THICKFRAME) {
        return Some(Skip::NoFrame);
    }
    if frame.is_empty() {
        return Some(Skip::ZeroSize);
    }
    None
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
