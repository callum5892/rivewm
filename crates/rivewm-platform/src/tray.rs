//! The notification-area (tray) icon and its menu. Lives on the event
//! thread's hidden window; menu picks come back as [`Event::Tray`].

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIN_SELECT,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyMenu, HICON, ICONINFO, MF_CHECKED,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, PostMessageW, RegisterWindowMessageW, SW_SHOWNORMAL,
    SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WM_APP,
    WM_CONTEXTMENU, WM_NULL,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::autostart;

/// What the user picked from the tray menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ReloadConfig,
    OpenConfig,
    OpenLog,
    ToggleAutostart,
    Quit,
}

/// Sent to our hidden window when something happens on the icon.
pub(crate) const WM_TRAY: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;

const MENU_RELOAD: usize = 1;
const MENU_OPEN_CONFIG: usize = 2;
const MENU_OPEN_LOG: usize = 3;
const MENU_AUTOSTART: usize = 4;
const MENU_QUIT: usize = 5;

/// Explorer broadcasts this when the taskbar is (re)created, e.g. after
/// explorer.exe restarts; the icon has to be added again then.
pub(crate) fn taskbar_created_message() -> u32 {
    static MESSAGE: OnceLock<u32> = OnceLock::new();
    *MESSAGE.get_or_init(|| unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) })
}

pub(crate) fn add_icon(hwnd: HWND) -> bool {
    let mut data = icon_data(hwnd);
    data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    data.uCallbackMessage = WM_TRAY;
    data.hIcon = icon();
    let tip: Vec<u16> = "rivewm".encode_utf16().collect();
    data.szTip[..tip.len()].copy_from_slice(&tip);
    unsafe {
        if !Shell_NotifyIconW(NIM_ADD, &data).as_bool() {
            return false;
        }
        // Version 4: clean WM_CONTEXTMENU / NIN_SELECT notifications with the
        // click position in wParam.
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
    true
}

pub(crate) fn remove_icon(hwnd: HWND) {
    let data = icon_data(hwnd);
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

/// Handles a [`WM_TRAY`] notification, returning the menu pick if any.
pub(crate) fn on_notify(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> Option<TrayAction> {
    let event = (lparam.0 as u32) & 0xFFFF;
    if event != WM_CONTEXTMENU && event != NIN_SELECT {
        return None;
    }
    // With version 4, wParam holds the click position as two signed words.
    let x = (wparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    show_menu(hwnd, x, y)
}

fn show_menu(hwnd: HWND, x: i32, y: i32) -> Option<TrayAction> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let check = if autostart::is_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        let _ = AppendMenuW(menu, MF_STRING, MENU_RELOAD, w!("Reload config"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN_CONFIG, w!("Open config"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN_LOG, w!("Open log"));
        let _ = AppendMenuW(
            menu,
            MF_STRING | check,
            MENU_AUTOSTART,
            w!("Start with Windows"),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, w!("Quit rivewm"));

        // Without this the menu won't close when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let picked = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            x,
            y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        // Documented workaround so the next menu opens reliably.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        match picked.0 as usize {
            MENU_RELOAD => Some(TrayAction::ReloadConfig),
            MENU_OPEN_CONFIG => Some(TrayAction::OpenConfig),
            MENU_OPEN_LOG => Some(TrayAction::OpenLog),
            MENU_AUTOSTART => Some(TrayAction::ToggleAutostart),
            MENU_QUIT => Some(TrayAction::Quit),
            _ => None,
        }
    }
}

/// Opens a file with its default app, falling back to Notepad for types
/// with no association (like `.toml` on a fresh Windows install).
pub fn open_file(path: &std::path::Path) {
    let file = HSTRING::from(path.as_os_str());
    unsafe {
        let result = ShellExecuteW(None, w!("open"), &file, None, None, SW_SHOWNORMAL);
        // Values above 32 mean success.
        if result.0 as usize <= 32 {
            let _ = ShellExecuteW(
                None,
                w!("open"),
                w!("notepad.exe"),
                &file,
                None,
                SW_SHOWNORMAL,
            );
        }
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// A 32x32 icon of three tiles in a dwindle layout, drawn in code so there's
/// no resource file to ship.
fn icon() -> HICON {
    static ICON: OnceLock<isize> = OnceLock::new();
    let raw = *ICON.get_or_init(|| make_icon().map_or(0, |icon| icon.0 as isize));
    HICON(raw as *mut c_void)
}

fn make_icon() -> Option<HICON> {
    const SIZE: i32 = 32;
    const TILE: u32 = 0xFF4C9AFF; // opaque blue, as ARGB
    // Left column, then the right column split top and bottom.
    let tiles = [(2, 2, 15, 30), (17, 2, 30, 15), (17, 17, 30, 30)];

    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE, // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let inside = tiles
                    .iter()
                    .any(|&(l, t, r, b)| x >= l && x < r && y >= t && y < b);
                pixels[(y * SIZE + x) as usize] = if inside { TILE } else { 0 };
            }
        }
        // With a 32-bit colour bitmap the alpha channel does the masking; the
        // mask just has to exist.
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            hbmMask: mask,
            hbmColor: color,
            ..Default::default()
        });
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.ok()
    }
}
