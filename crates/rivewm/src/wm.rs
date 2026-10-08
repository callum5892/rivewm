//! The window manager loop: keeps the tree in sync with OS events and applies
//! the resulting layout to real windows.
//!
//! Each monitor shows one workspace; windows on the others are cloaked (see
//! `rivewm_platform::set_cloaked`), which keeps them in the taskbar.

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use rivewm_core::{Command, Gaps, Layout, NodeId, Tree, WindowEvent, WindowId};
use tracing::{debug, info, warn};

/// Every window we currently manage, kept outside `Wm` so the Ctrl+C handler
/// and panic hook can still restore them when `Wm` is unreachable.
static MANAGED: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

/// Windows we have cloaked, mirrored to disk for crash recovery.
static CLOAKED: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

pub struct Wm {
    tree: Tree,
    gaps: Gaps,
}

impl Wm {
    /// Creates one workspace per attached monitor, named "1", "2", ...
    pub fn new(gaps: Gaps) -> Self {
        let mut tree = Tree::new();
        for (i, m) in rivewm_platform::monitors().into_iter().enumerate() {
            let monitor = tree.add_monitor(m.id, m.work_area);
            tree.add_workspace(monitor, (i + 1).to_string(), Layout::Manual);
            info!(device = %m.device, work_area = %m.work_area, "added monitor");
        }
        Self { tree, gaps }
    }

    /// Tiles every window that's already open, keeping their left-to-right
    /// order on each monitor.
    pub fn manage_existing(&mut self) {
        let mut windows: Vec<_> = rivewm_platform::enumerate_windows()
            .into_iter()
            .filter(|w| w.is_manageable() && !w.minimized)
            .collect();
        windows.sort_by_key(|w| (w.frame.x, w.frame.y));

        for w in windows {
            let Some(ws) = self
                .tree
                .monitor_by_id(w.monitor)
                .and_then(|m| self.tree.active_workspace(m))
                .or(self.tree.focused_workspace())
            else {
                continue;
            };
            self.insert(ws, w.id, &w.title);
        }
        if let Some(fg) = rivewm_platform::foreground_window() {
            self.tree.focus_window(fg);
        }
        self.apply_all();
    }

    /// Runs a user command. Returns `Break` when the WM should exit.
    pub fn execute(&mut self, command: Command) -> ControlFlow<()> {
        debug!(?command);
        let focused = self.tree.focused_window();
        match command {
            Command::Focus(direction) => {
                if let Some(target) = self.tree.focus_in_direction(direction) {
                    focus_os_window(target);
                }
            }
            Command::Move(direction) => {
                // May span two monitors, so re-tile everything.
                if let Some(id) = focused
                    && self.tree.move_in_direction(id, direction)
                {
                    self.apply_all();
                }
            }
            Command::Workspace(n) => {
                let ws = self.workspace_named(&n.to_string());
                self.show_workspace(ws);
            }
            Command::MoveToWorkspace(n) => {
                if let Some(id) = focused {
                    let ws = self.workspace_named(&n.to_string());
                    self.send_to_workspace(id, ws);
                }
            }
            Command::Split(axis) => {
                if let Some(id) = focused {
                    self.tree.split(id, axis);
                }
            }
            Command::ToggleSplit => {
                if let Some(id) = focused {
                    self.tree.toggle_split(id);
                }
            }
            Command::Resize { axis, delta } => {
                if let Some(id) = focused
                    && self.tree.resize(id, axis, delta)
                    && let Some(ws) = self.tree.focused_workspace()
                {
                    self.apply(ws);
                }
            }
            Command::Retile => self.apply_all(),
            Command::Quit => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }

    pub fn handle(&mut self, event: WindowEvent) {
        debug!(?event);
        match event {
            WindowEvent::Shown(id)
            | WindowEvent::Uncloaked(id)
            | WindowEvent::Restored(id)
            // Some apps show their window before giving it a title.
            | WindowEvent::TitleChanged(id) => self.try_manage(id),
            WindowEvent::Hidden(id) | WindowEvent::Destroyed(id) | WindowEvent::Minimized(id) => {
                self.unmanage(id)
            }
            WindowEvent::Cloaked(id) => {
                // Our own cloaking reports back here too. Only a window on a
                // visible workspace that is *still* cloaked now (e.g. moved
                // to another virtual desktop) has really gone; anything else
                // is us, or a stale event from switching back and forth.
                if self.workspace_of(id).is_some_and(|ws| self.tree.is_workspace_active(ws))
                    && rivewm_platform::is_window_cloaked(id)
                {
                    self.unmanage(id);
                }
            }
            WindowEvent::Focused(id) => self.on_focused(id),
            WindowEvent::MoveSizeEnded(id) => {
                // The user dragged or resized a tiled window: snap it back.
                if let Some(ws) = self.workspace_of(id) {
                    self.apply(ws);
                }
            }
            WindowEvent::MoveSizeStarted(_) | WindowEvent::LocationChanged(_) => {}
        }
    }

    fn on_focused(&mut self, id: WindowId) {
        match self.workspace_of(id) {
            // Alt+Tab or a taskbar click picked a window on a hidden
            // workspace: bring its workspace forward. Ignore the event if
            // focus has already moved on, as it may be a stale one from a
            // workspace switch.
            Some(ws) if !self.tree.is_workspace_active(ws) => {
                if rivewm_platform::foreground_window() == Some(id) {
                    // Not `focus_window`: that would activate the workspace
                    // and `show_workspace` would skip the cloak swap.
                    self.tree.remember_focus(id);
                    self.show_workspace(ws);
                }
            }
            Some(_) => {
                self.tree.focus_window(id);
            }
            None => {
                // Focus can arrive before Shown for a brand new window.
                self.try_manage(id);
                self.tree.focus_window(id);
            }
        }
    }

    /// Makes `ws` visible on its monitor and focuses it, cloaking whatever
    /// workspace it replaces there.
    fn show_workspace(&mut self, ws: NodeId) {
        let previous = self.tree.focus_workspace(ws);
        info!(
            workspace = self.tree.workspace_name(ws),
            "switching workspace"
        );
        if let Some(previous) = previous {
            // Show the new windows before hiding the old ones, so the
            // monitor never flashes empty.
            self.apply(ws);
            for id in self.tree.workspace_windows(ws) {
                cloak(id, false);
            }
            for id in self.tree.workspace_windows(previous) {
                cloak(id, true);
            }
            self.remove_if_unused(previous);
        }
        self.focus_os(ws);
    }

    /// Sends a window to `ws`. It stays visible if `ws` is shown on some
    /// monitor and is cloaked otherwise. Focus stays on the current
    /// workspace.
    fn send_to_workspace(&mut self, id: WindowId, ws: NodeId) {
        let Some(source) = self.tree.move_window_to_workspace(id, ws) else {
            return;
        };
        info!(
            window = format_args!("{:#x}", id.0),
            workspace = self.tree.workspace_name(ws),
            "sent to workspace"
        );
        if self.tree.is_workspace_active(ws) {
            self.apply(ws);
        } else {
            cloak(id, true);
        }
        self.apply(source);
        self.focus_os(source);
    }

    /// Finds a workspace by name, creating it on the focused monitor if
    /// needed (without showing it).
    fn workspace_named(&mut self, name: &str) -> NodeId {
        if let Some(ws) = self.tree.workspace_by_name(name) {
            return ws;
        }
        let monitor = match self.tree.focused_workspace() {
            Some(ws) => self.tree.monitor_of(ws),
            None => self.tree.monitors().next().expect("no monitors"),
        };
        self.tree.add_workspace(monitor, name, Layout::Manual)
    }

    /// Deletes a workspace once it's empty and no longer shown.
    fn remove_if_unused(&mut self, ws: NodeId) {
        if !self.tree.is_workspace_active(ws) && self.tree.workspace_windows(ws).is_empty() {
            debug!(
                workspace = self.tree.workspace_name(ws),
                "removing empty workspace"
            );
            self.tree.remove_workspace(ws);
        }
    }

    /// Gives OS focus to the workspace's remembered window, or to the desktop
    /// if it has none, so keystrokes never land in a cloaked window.
    fn focus_os(&self, ws: NodeId) {
        match self.tree.workspace_focused_window(ws) {
            Some(id) => focus_os_window(id),
            None => {
                rivewm_platform::focus_desktop();
            }
        }
    }

    fn workspace_of(&self, id: WindowId) -> Option<NodeId> {
        self.tree
            .window_node(id)
            .and_then(|n| self.tree.workspace_of(n))
    }

    fn try_manage(&mut self, id: WindowId) {
        if self.tree.contains_window(id) {
            return;
        }
        let Some(info) = rivewm_platform::query_window(id) else {
            return;
        };
        if !info.is_manageable() || info.minimized {
            return;
        }
        let Some(ws) = self.tree.focused_workspace() else {
            return;
        };
        self.insert(ws, id, &info.title);
        self.apply(ws);
    }

    fn insert(&mut self, ws: NodeId, id: WindowId, title: &str) {
        self.tree.insert_window(ws, id);
        lock(&MANAGED).insert(id);
        info!(window = format_args!("{:#x}", id.0), title, "managing");
    }

    fn unmanage(&mut self, id: WindowId) {
        let Some(ws) = self.tree.remove_window(id) else {
            return;
        };
        lock(&MANAGED).remove(&id);
        info!(window = format_args!("{:#x}", id.0), "unmanaged");
        if self.tree.is_workspace_active(ws) {
            self.apply(ws);
        } else {
            // We cloaked it; don't leave it invisible if the app shows it
            // again later. Fails harmlessly if it was destroyed.
            let _ = set_cloaked_tracked(id, false);
            self.remove_if_unused(ws);
        }
    }

    /// Re-tiles every visible workspace.
    fn apply_all(&self) {
        let visible: Vec<_> = self
            .tree
            .monitors()
            .filter_map(|m| self.tree.active_workspace(m))
            .collect();
        for ws in visible {
            self.apply(ws);
        }
    }

    fn apply(&self, ws: NodeId) {
        for (id, rect) in self.tree.arrange(ws, self.gaps) {
            if let Err(err) = rivewm_platform::set_frame(id, rect) {
                // Typically an elevated window we aren't allowed to move.
                warn!(window = format_args!("{:#x}", id.0), %err, "failed to position window");
            }
        }
    }
}

fn focus_os_window(id: WindowId) {
    if !rivewm_platform::focus_window(id) {
        warn!(
            window = format_args!("{:#x}", id.0),
            "Windows refused focus change"
        );
    }
}

fn cloak(id: WindowId, cloaked: bool) {
    if let Err(err) = set_cloaked_tracked(id, cloaked) {
        warn!(window = format_args!("{:#x}", id.0), cloaked, %err, "failed to change cloak");
    }
}

/// Cloaks or uncloaks, recording the set of windows we've cloaked on disk.
/// If rivewm is killed without a chance to clean up, the next run uses that
/// record to bring those windows back (see [`recover_cloaked`]).
fn set_cloaked_tracked(id: WindowId, cloaked: bool) -> rivewm_platform::Result<()> {
    rivewm_platform::set_cloaked(id, cloaked)?;
    let mut set = lock(&CLOAKED);
    let changed = if cloaked {
        set.insert(id)
    } else {
        set.remove(&id)
    };
    if changed {
        let text: String = set.iter().map(|id| format!("{:#x}\n", id.0)).collect();
        if let Err(err) = std::fs::write(cloaked_record(), text) {
            warn!(%err, "failed to record cloaked windows");
        }
    }
    Ok(())
}

fn cloaked_record() -> PathBuf {
    std::env::temp_dir().join("rivewm-cloaked.txt")
}

/// Uncloaks windows a previous run cloaked but never restored, e.g. because
/// it was killed from Task Manager. Call before managing existing windows,
/// since cloaked windows are otherwise skipped.
pub fn recover_cloaked() {
    let path = cloaked_record();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let recovered = text
        .lines()
        .filter_map(|line| isize::from_str_radix(line.trim().trim_start_matches("0x"), 16).ok())
        .filter(|&hwnd| rivewm_platform::set_cloaked(WindowId(hwnd), false).is_ok())
        .count();
    if recovered > 0 {
        info!(recovered, "uncloaked windows left hidden by a previous run");
    }
    let _ = std::fs::remove_file(path);
}

/// Makes every managed window visible again: uncloaked and shown. Safe to
/// call from a panic hook or signal handler.
pub fn restore_all() {
    for &id in lock(&MANAGED).iter() {
        let _ = rivewm_platform::set_cloaked(id, false);
        rivewm_platform::show_window(id);
    }
    lock(&CLOAKED).clear();
    let _ = std::fs::remove_file(cloaked_record());
}

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    // A panic while holding the lock must not stop us restoring windows.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
