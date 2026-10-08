//! The window manager loop: keeps the tree in sync with OS events and applies
//! the resulting layout to real windows.

use std::collections::{BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::sync::Mutex;

use rivewm_core::{Command, Gaps, Layout, MonitorId, NodeId, Tree, WindowEvent, WindowId};
use tracing::{debug, info, warn};

/// Every window we currently manage, kept outside `Wm` so the Ctrl+C handler
/// and panic hook can still restore them when `Wm` is unreachable.
static MANAGED: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

pub struct Wm {
    tree: Tree,
    gaps: Gaps,
    workspace_for_monitor: HashMap<MonitorId, NodeId>,
}

impl Wm {
    /// Creates one workspace per attached monitor.
    pub fn new(gaps: Gaps) -> Self {
        let mut tree = Tree::new();
        let mut workspace_for_monitor = HashMap::new();
        for (i, m) in rivewm_platform::monitors().into_iter().enumerate() {
            let monitor = tree.add_monitor(m.id, m.work_area);
            let ws = tree.add_workspace(monitor, (i + 1).to_string(), Layout::Manual);
            workspace_for_monitor.insert(m.id, ws);
            info!(device = %m.device, work_area = %m.work_area, "added monitor");
        }
        Self {
            tree,
            gaps,
            workspace_for_monitor,
        }
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
                .workspace_for_monitor
                .get(&w.monitor)
                .copied()
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
                if let Some(target) = self.tree.focus_in_direction(direction)
                    && !rivewm_platform::focus_window(target)
                {
                    warn!(
                        window = format_args!("{:#x}", target.0),
                        "Windows refused focus change"
                    );
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
            WindowEvent::Hidden(id)
            | WindowEvent::Destroyed(id)
            | WindowEvent::Minimized(id)
            | WindowEvent::Cloaked(id) => self.unmanage(id),
            WindowEvent::Focused(id) => {
                // Focus can arrive before Shown for a brand new window.
                self.try_manage(id);
                self.tree.focus_window(id);
            }
            WindowEvent::MoveSizeEnded(id) => {
                // The user dragged or resized a tiled window: snap it back.
                if let Some(ws) = self.tree.window_node(id).and_then(|n| self.tree.workspace_of(n)) {
                    self.apply(ws);
                }
            }
            WindowEvent::MoveSizeStarted(_) | WindowEvent::LocationChanged(_) => {}
        }
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
        lock_managed().insert(id);
        info!(window = format_args!("{:#x}", id.0), title, "managing");
    }

    fn unmanage(&mut self, id: WindowId) {
        if let Some(ws) = self.tree.remove_window(id) {
            lock_managed().remove(&id);
            info!(window = format_args!("{:#x}", id.0), "unmanaged");
            self.apply(ws);
        }
    }

    fn apply_all(&self) {
        for &ws in self.workspace_for_monitor.values() {
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

/// Makes every managed window visible again. Safe to call from a panic hook
/// or signal handler.
pub fn restore_all() {
    for &id in lock_managed().iter() {
        rivewm_platform::show_window(id);
    }
}

fn lock_managed() -> std::sync::MutexGuard<'static, BTreeSet<WindowId>> {
    // A panic while holding the lock must not stop us restoring windows.
    MANAGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
