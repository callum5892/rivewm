//! Cloaking other processes' windows through the shell's undocumented
//! `IApplicationView` interface, as Task View does for virtual desktops.
//!
//! A cloaked window is invisible and can't be clicked, but unlike a hidden
//! one it keeps its taskbar button and Alt+Tab entry. `DWMWA_CLOAK` only
//! works on our own windows, hence this detour through explorer.exe.
//!
//! The interface IDs and vtable layouts below are what Windows 10 and 11
//! ship (the same ones komorebi and GlazeWM use). If a future build changes
//! them, `set_cloaked` returns an error rather than crashing.

#![allow(non_snake_case)]

use std::cell::RefCell;
use std::ffi::c_void;

use rivewm_core::WindowId;
use windows::Win32::Foundation::{E_POINTER, HWND};
use windows::Win32::System::Com::{
    CLSCTX_LOCAL_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, IServiceProvider,
};
use windows::core::{GUID, HRESULT, Interface, Result};
use windows_core::{IUnknown, IUnknown_Vtbl, interface};

const CLSID_IMMERSIVE_SHELL: GUID = GUID::from_u128(0xc2f03a33_21f5_47fa_b4bb_156362a2f239);

/// `APPLICATION_VIEW_CLOAK_TYPE` value for cloaking at the shell's request.
const CLOAK_TYPE_SHELL: u32 = 1;
const CLOAK_FLAGS_CLOAK: i32 = 2;
const CLOAK_FLAGS_UNCLOAK: i32 = 0;

#[interface("1841c6d7-4f9d-42c0-af41-8747538f10e5")]
unsafe trait IApplicationViewCollection: IUnknown {
    unsafe fn GetViews(&self, views: *mut *mut c_void) -> HRESULT;
    unsafe fn GetViewsByZOrder(&self, views: *mut *mut c_void) -> HRESULT;
    unsafe fn GetViewsByAppUserModelId(&self, id: *const u16, views: *mut *mut c_void) -> HRESULT;
    unsafe fn GetViewForHwnd(&self, hwnd: HWND, view: *mut Option<IApplicationView>) -> HRESULT;
}

// Really derives from IInspectable; its three methods are spelled out so we
// only depend on IUnknown.
#[interface("372e1d3b-38d3-42e4-a15b-8ab2b178f513")]
unsafe trait IApplicationView: IUnknown {
    unsafe fn GetIids(&self, count: *mut u32, iids: *mut *mut GUID) -> HRESULT;
    unsafe fn GetRuntimeClassName(&self, name: *mut *mut c_void) -> HRESULT;
    unsafe fn GetTrustLevel(&self, level: *mut i32) -> HRESULT;
    unsafe fn SetFocus(&self) -> HRESULT;
    unsafe fn SwitchTo(&self) -> HRESULT;
    unsafe fn TryInvokeBack(&self, callback: *mut c_void) -> HRESULT;
    unsafe fn GetThumbnailWindow(&self, hwnd: *mut HWND) -> HRESULT;
    unsafe fn GetMonitor(&self, monitor: *mut *mut c_void) -> HRESULT;
    unsafe fn GetVisibility(&self, visibility: *mut i32) -> HRESULT;
    unsafe fn SetCloak(&self, cloak_type: u32, flags: i32) -> HRESULT;
}

thread_local! {
    /// COM objects belong to the thread that created them, so each thread
    /// that cloaks (the WM loop, the Ctrl+C handler) gets its own.
    static VIEWS: RefCell<Option<IApplicationViewCollection>> = const { RefCell::new(None) };
}

/// Cloaks or uncloaks a window belonging to any process.
pub fn set_cloaked(id: WindowId, cloaked: bool) -> Result<()> {
    let hwnd = HWND(id.0 as *mut c_void);
    let flags = if cloaked {
        CLOAK_FLAGS_CLOAK
    } else {
        CLOAK_FLAGS_UNCLOAK
    };
    with_views(|views| unsafe {
        let mut view = None;
        views.GetViewForHwnd(hwnd, &mut view).ok()?;
        let view = view.ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;
        view.SetCloak(CLOAK_TYPE_SHELL, flags).ok()
    })
}

fn with_views<R>(f: impl Fn(&IApplicationViewCollection) -> Result<R>) -> Result<R> {
    VIEWS.with(|cell| {
        if cell.borrow().is_none() {
            *cell.borrow_mut() = Some(connect()?);
        }
        let result = f(cell.borrow().as_ref().expect("just connected"));
        if result.is_ok() {
            return result;
        }
        // Explorer may have restarted, leaving us a dead proxy. Reconnect
        // and retry once.
        let fresh = connect()?;
        let retry = f(&fresh);
        *cell.borrow_mut() = Some(fresh);
        retry
    })
}

fn connect() -> Result<IApplicationViewCollection> {
    unsafe {
        // Fine to call repeatedly; an already-initialized thread just gets
        // S_FALSE (or a mode mismatch we can live with).
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let shell: IServiceProvider =
            CoCreateInstance(&CLSID_IMMERSIVE_SHELL, None, CLSCTX_LOCAL_SERVER)?;
        shell.QueryService(&IApplicationViewCollection::IID)
    }
}
