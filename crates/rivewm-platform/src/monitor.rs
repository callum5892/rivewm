use rivewm_core::{MonitorId, Rect};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;
use windows::core::BOOL;

use crate::from_wide;

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub id: MonitorId,
    /// GDI device name, e.g. `\\.\DISPLAY1`.
    pub device: String,
    /// Full monitor bounds.
    pub bounds: Rect,
    /// Bounds minus the taskbar and other app bars.
    pub work_area: Rect,
    pub primary: bool,
}

/// Lists all attached monitors.
pub fn monitors() -> Vec<MonitorInfo> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&mut handles as *mut _ as isize),
        );
    }
    handles.into_iter().filter_map(monitor_info).collect()
}

unsafe extern "system" fn collect_monitor(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HMONITOR>` passed by `monitors`,
    // which outlives the synchronous enumeration.
    let handles = unsafe { &mut *(lparam.0 as *mut Vec<HMONITOR>) };
    handles.push(hmonitor);
    true.into()
}

pub(crate) fn monitor_info(hmonitor: HMONITOR) -> Option<MonitorInfo> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    let ok = unsafe { GetMonitorInfoW(hmonitor, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    let mi = info.monitorInfo;
    Some(MonitorInfo {
        id: MonitorId(hmonitor.0 as isize),
        device: from_wide(&info.szDevice),
        bounds: rect_from_win32(mi.rcMonitor),
        work_area: rect_from_win32(mi.rcWork),
        primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
    })
}

pub(crate) fn rect_from_win32(r: RECT) -> Rect {
    Rect::from_ltrb(r.left, r.top, r.right, r.bottom)
}
