use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use rivewm_core::{WindowEvent, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::Input::KeyboardAndMouse::{MOD_NOREPEAT, RegisterHotKey, UnregisterHotKey};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GetMessageW,
    MSG, OBJID_WINDOW, PostThreadMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_HOTKEY, WM_QUIT,
};

use crate::Hotkey;

/// Everything the event thread reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Window(WindowEvent),
    /// A registered hotkey was pressed; the value is its index in the list
    /// most recently passed to [`EventThread::spawn`] or
    /// [`EventThread::set_hotkeys`].
    Hotkey(usize),
}

thread_local! {
    /// Where the hook callback sends events. Out-of-context WinEvent hooks
    /// are delivered on the thread that installed them, so a thread-local is
    /// enough and avoids any global state.
    static SENDER: RefCell<Option<Sender<Event>>> = const { RefCell::new(None) };
}

/// Hook ranges we listen to. Two narrow ranges instead of `EVENT_MIN..MAX`
/// so the OS doesn't marshal every accessibility event to us.
const HOOK_RANGES: [(u32, u32); 2] = [
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND),
    (EVENT_OBJECT_DESTROY, EVENT_OBJECT_UNCLOAKED),
];

/// Hotkeys that couldn't be registered (usually because another app owns
/// them), with the reason.
pub type FailedHotkeys = Vec<(Hotkey, windows::core::Error)>;

/// Startup result sent from the event thread: its id and failed hotkeys.
type Ready = windows::core::Result<(u32, FailedHotkeys)>;

/// A hotkey list handed to the event thread to replace the current one,
/// with a channel for its reply.
type HotkeyRequest = Arc<Mutex<Option<(Vec<Hotkey>, SyncSender<FailedHotkeys>)>>>;

/// Posted to the event thread to make it pick up a [`HotkeyRequest`].
const WM_SET_HOTKEYS: u32 = WM_APP + 1;

/// A background thread that pumps Win32 messages, forwarding window events
/// and global hotkey presses.
pub struct EventThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
    failed_hotkeys: FailedHotkeys,
    hotkey_request: HotkeyRequest,
}

impl EventThread {
    /// Installs the WinEvent hooks and registers `hotkeys` on a new thread,
    /// and returns a receiver for the events they produce.
    pub fn spawn(hotkeys: Vec<Hotkey>) -> windows::core::Result<(Self, Receiver<Event>)> {
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let hotkey_request = HotkeyRequest::default();
        let request = hotkey_request.clone();

        let join = std::thread::Builder::new()
            .name("rivewm-events".into())
            .spawn(move || run(event_tx, ready_tx, hotkeys, request))
            .expect("failed to spawn event thread");

        let (thread_id, failed_hotkeys) = ready_rx
            .recv()
            .expect("event thread exited before reporting")?;
        let thread = Self {
            thread_id,
            join: Some(join),
            failed_hotkeys,
            hotkey_request,
        };
        Ok((thread, event_rx))
    }

    /// Hotkeys from `spawn` that couldn't be registered, with the reason.
    pub fn failed_hotkeys(&self) -> &[(Hotkey, windows::core::Error)] {
        &self.failed_hotkeys
    }

    /// Replaces every registered hotkey with `hotkeys`, without disturbing
    /// the window event hooks. Returns the ones that couldn't be registered.
    pub fn set_hotkeys(&self, hotkeys: Vec<Hotkey>) -> FailedHotkeys {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        *self
            .hotkey_request
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((hotkeys, reply_tx));
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_SET_HOTKEYS, WPARAM(0), LPARAM(0));
        }
        reply_rx.recv().unwrap_or_default()
    }

    /// Stops the message loop, removes the hooks and joins the thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(join) = self.join.take() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
            }
            let _ = join.join();
        }
    }
}

impl Drop for EventThread {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    event_tx: Sender<Event>,
    ready_tx: SyncSender<Ready>,
    hotkeys: Vec<Hotkey>,
    hotkey_request: HotkeyRequest,
) {
    SENDER.with(|s| *s.borrow_mut() = Some(event_tx.clone()));

    let mut hooks = Vec::new();
    for (min, max) in HOOK_RANGES {
        let hook = unsafe {
            SetWinEventHook(
                min,
                max,
                None,
                Some(win_event_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        if hook.is_invalid() {
            unhook_all(&hooks);
            let _ = ready_tx.send(Err(windows::core::Error::from_thread()));
            return;
        }
        hooks.push(hook);
    }

    let (mut registered, failed) = register_hotkeys(&hotkeys);
    let _ = ready_tx.send(Ok((unsafe { GetCurrentThreadId() }, failed)));

    let mut msg = MSG::default();
    // GetMessageW returns 0 on WM_QUIT and -1 on error; stop on both.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        if msg.hwnd.is_invalid() {
            match msg.message {
                WM_HOTKEY => {
                    let _ = event_tx.send(Event::Hotkey(msg.wParam.0));
                    continue;
                }
                WM_SET_HOTKEYS => {
                    let request = hotkey_request
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some((hotkeys, reply)) = request {
                        unregister_hotkeys(&registered);
                        let failed;
                        (registered, failed) = register_hotkeys(&hotkeys);
                        let _ = reply.send(failed);
                    }
                    continue;
                }
                _ => {}
            }
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unregister_hotkeys(&registered);
    unhook_all(&hooks);
}

/// Registers hotkeys on the calling thread, using each one's index as its
/// id. With no window, `WM_HOTKEY` goes to this thread's message queue.
/// Returns the ids registered and the hotkeys that failed.
fn register_hotkeys(hotkeys: &[Hotkey]) -> (Vec<i32>, FailedHotkeys) {
    let mut registered = Vec::new();
    let mut failed = Vec::new();
    for (i, hotkey) in hotkeys.iter().enumerate() {
        let id = i as i32;
        match unsafe { RegisterHotKey(None, id, hotkey.modifiers | MOD_NOREPEAT, hotkey.vk) } {
            Ok(()) => registered.push(id),
            Err(err) => failed.push((*hotkey, err)),
        }
    }
    (registered, failed)
}

fn unregister_hotkeys(ids: &[i32]) {
    for &id in ids {
        unsafe {
            let _ = UnregisterHotKey(None, id);
        }
    }
}

fn unhook_all(hooks: &[HWINEVENTHOOK]) {
    for &hook in hooks {
        unsafe {
            let _ = UnhookWinEvent(hook);
        }
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
) {
    // Only events about the window itself, not its caret, scrollbars, etc.
    if hwnd.is_invalid() || id_object != OBJID_WINDOW.0 || id_child != CHILDID_SELF as i32 {
        return;
    }
    let Some(event) = translate(event, WindowId(hwnd.0 as isize)) else {
        return;
    };
    SENDER.with(|s| {
        if let Some(tx) = s.borrow().as_ref() {
            let _ = tx.send(Event::Window(event));
        }
    });
}

fn translate(event: u32, id: WindowId) -> Option<WindowEvent> {
    Some(match event {
        EVENT_OBJECT_SHOW => WindowEvent::Shown(id),
        EVENT_OBJECT_HIDE => WindowEvent::Hidden(id),
        EVENT_OBJECT_DESTROY => WindowEvent::Destroyed(id),
        EVENT_SYSTEM_FOREGROUND => WindowEvent::Focused(id),
        EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized(id),
        EVENT_SYSTEM_MINIMIZEEND => WindowEvent::Restored(id),
        EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MoveSizeStarted(id),
        EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MoveSizeEnded(id),
        EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::LocationChanged(id),
        EVENT_OBJECT_NAMECHANGE => WindowEvent::TitleChanged(id),
        EVENT_OBJECT_CLOAKED => WindowEvent::Cloaked(id),
        EVENT_OBJECT_UNCLOAKED => WindowEvent::Uncloaked(id),
        _ => return None,
    })
}
