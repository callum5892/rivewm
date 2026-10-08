use std::cell::RefCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use rivewm_core::{WindowEvent, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    CHILDID_SELF, DispatchMessageW, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE,
    EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW,
    EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GetMessageW,
    MSG, OBJID_WINDOW, PostThreadMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

thread_local! {
    /// Where the hook callback sends events. Out-of-context WinEvent hooks
    /// are delivered on the thread that installed them, so a thread-local is
    /// enough and avoids any global state.
    static SENDER: RefCell<Option<Sender<WindowEvent>>> = const { RefCell::new(None) };
}

/// Hook ranges we listen to. Two narrow ranges instead of `EVENT_MIN..MAX`
/// so the OS doesn't marshal every accessibility event to us.
const HOOK_RANGES: [(u32, u32); 2] = [
    (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND),
    (EVENT_OBJECT_DESTROY, EVENT_OBJECT_UNCLOAKED),
];

/// A background thread that pumps Win32 messages and forwards window events.
pub struct EventThread {
    thread_id: u32,
    join: Option<JoinHandle<()>>,
}

impl EventThread {
    /// Installs the WinEvent hooks on a new thread and returns a receiver for
    /// the events they produce.
    pub fn spawn() -> windows::core::Result<(Self, Receiver<WindowEvent>)> {
        let (event_tx, event_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);

        let join = std::thread::Builder::new()
            .name("rivewm-events".into())
            .spawn(move || run(event_tx, ready_tx))
            .expect("failed to spawn event thread");

        let thread_id = ready_rx
            .recv()
            .expect("event thread exited before reporting")?;
        let thread = Self {
            thread_id,
            join: Some(join),
        };
        Ok((thread, event_rx))
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

fn run(event_tx: Sender<WindowEvent>, ready_tx: mpsc::SyncSender<windows::core::Result<u32>>) {
    SENDER.with(|s| *s.borrow_mut() = Some(event_tx));

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

    let _ = ready_tx.send(Ok(unsafe { GetCurrentThreadId() }));

    let mut msg = MSG::default();
    // GetMessageW returns 0 on WM_QUIT and -1 on error; stop on both.
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    unhook_all(&hooks);
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
            let _ = tx.send(event);
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
