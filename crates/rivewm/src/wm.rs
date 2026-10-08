//! The window manager loop: keeps the tree in sync with OS events and applies
//! the resulting layout to real windows.
//!
//! Each monitor shows one workspace; windows on the others are cloaked (see
//! `rivewm_platform::set_cloaked`), which keeps them in the taskbar.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rivewm_core::tree::NodeKind;
use rivewm_core::{
    Axis, Command, Direction, Layout, MonitorSpec, NodeId, Rect, Tree, WindowEvent, WindowId,
};
use rivewm_platform::{BorderColor, WindowInfo};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::config::{Config, RuleAction};
use crate::subscribe::{Snapshot, WorkspaceSnapshot};

/// Every window we currently manage, kept outside `Wm` so the Ctrl+C handler
/// and panic hook can still restore them when `Wm` is unreachable.
static MANAGED: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

/// Windows we have cloaked, mirrored to disk for crash recovery.
static CLOAKED: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

/// How long after a focus change to paint borders again (see `sync_border`).
const BORDER_REASSERT_DELAYS: [u64; 3] = [80, 250, 800];

/// Floating windows we made always-on-top, so we can undo exactly those.
static MADE_TOPMOST: Mutex<BTreeSet<WindowId>> = Mutex::new(BTreeSet::new());

pub struct Wm {
    tree: Tree,
    config: Config,
    /// Fullscreen windows as of the last time we positioned them.
    fullscreen: BTreeSet<WindowId>,
    /// Windows Windows refused to let us move during the current operation;
    /// released once it finishes. (`apply` only borrows `self`.)
    refused: RefCell<Vec<WindowId>>,
    /// Windows released for that reason, so they aren't managed again.
    unmovable: HashSet<WindowId>,
    /// The window being dragged and its frame when the drag began, to tell
    /// a move from a resize when it ends.
    drag_start: Option<(WindowId, Rect)>,
    /// The window currently wearing the focused border colour.
    bordered: Option<WindowId>,
    /// The window that wore it before, which should now be unfocused-coloured.
    unbordered: Option<WindowId>,
    /// Re-paints still to come after the last focus change, soonest last.
    border_reasserts: Vec<Instant>,
}

/// Where a dragged tiled window was dropped.
enum Drop {
    /// Beside this tile, on this side.
    Beside(WindowId, Direction),
    /// Onto a monitor showing a workspace with no tiles.
    EmptyWorkspace(NodeId),
}

impl Wm {
    /// Creates one workspace per attached monitor, named "1", "2", ...
    /// (the primary monitor gets "1").
    pub fn new(config: Config) -> Self {
        let mut tree = Tree::new();
        tree.set_default_layout(config.default_layout);
        for (i, spec) in monitor_specs().iter().enumerate() {
            let monitor = tree.add_monitor(spec);
            tree.add_workspace(monitor, (i + 1).to_string(), config.default_layout);
            info!(device = spec.name, work_area = %spec.work_area, "added monitor");
        }
        Self {
            tree,
            config,
            fullscreen: BTreeSet::new(),
            refused: RefCell::new(Vec::new()),
            unmovable: HashSet::new(),
            drag_start: None,
            bordered: None,
            unbordered: None,
            border_reasserts: Vec::new(),
        }
    }

    /// Re-reads the attached monitors after a display change and moves
    /// workspaces and windows to match (see `Tree::sync_monitors`).
    pub fn sync_monitors(&mut self) {
        let specs = monitor_specs();
        let before = self.visible_windows();
        if !self.tree.sync_monitors(&specs) {
            return;
        }
        info!(
            monitors = ?specs.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            "monitors changed"
        );
        let after = self.visible_windows();
        // Uncloak first, so nothing flashes off and back on.
        for &id in after.difference(&before) {
            cloak(id, false);
        }
        for &id in before.difference(&after) {
            cloak(id, true);
        }
        self.apply_all();
        for id in after {
            if self.tree.is_floating(id) {
                self.place_floating(id);
            }
        }
        if let Some(ws) = self.tree.focused_workspace() {
            self.focus_os(ws);
        }
        self.sync_border();
    }

    /// Windows on workspaces currently shown on some monitor.
    fn visible_windows(&self) -> BTreeSet<WindowId> {
        self.tree
            .monitors()
            .filter_map(|m| self.tree.active_workspace(m))
            .flat_map(|ws| self.tree.workspace_windows(ws))
            .collect()
    }

    /// Swaps in a reloaded config. New gaps and the floating on-top setting
    /// apply immediately; rules apply to windows opened from now on.
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
        self.tree.set_default_layout(self.config.default_layout);
        let managed: Vec<_> = lock(&MANAGED).iter().copied().collect();
        for &id in &managed {
            self.sync_topmost(id);
            self.paint_border(id, self.config.border.unfocused);
        }
        self.bordered = None;
        self.apply_all();
        self.sync_border();
    }

    /// Manages every window that's already open, keeping tiled windows on
    /// their monitor in their left-to-right order.
    pub fn manage_existing(&mut self) {
        let mut windows: Vec<_> = rivewm_platform::enumerate_windows()
            .into_iter()
            .filter(|w| w.is_manageable() && !w.minimized)
            .collect();
        windows.sort_by_key(|w| (w.frame.x, w.frame.y));

        for w in windows {
            self.manage(&w, true);
        }
        if let Some(fg) = rivewm_platform::foreground_window() {
            self.tree.focus_window(fg);
        }
        self.apply_all();
        self.sync_border();
    }

    /// Runs a user command. Returns `Break` when the WM should exit.
    pub fn execute(&mut self, command: Command) -> ControlFlow<()> {
        let flow = self.run(command);
        self.sync_fullscreen();
        self.release_refused();
        self.sync_border();
        flow
    }

    fn run(&mut self, command: Command) -> ControlFlow<()> {
        debug!(?command);
        let focused = self.tree.focused_window();
        match command {
            Command::Focus(direction) => {
                if let Some(target) = self.tree.focus_in_direction(direction) {
                    focus_os_window(target);
                }
            }
            Command::FocusWindow(id) => match self.workspace_of(id) {
                Some(ws) if !self.tree.is_workspace_active(ws) => {
                    self.tree.remember_focus(id);
                    self.show_workspace(ws);
                }
                Some(_) => {
                    self.tree.focus_window(id);
                    focus_os_window(id);
                }
                None => warn!(
                    window = format_args!("{:#x}", id.0),
                    "no such managed window"
                ),
            },
            Command::Move(direction) => {
                // May span two monitors, so re-tile everything.
                if let Some(id) = focused
                    && self.tree.move_in_direction(id, direction)
                {
                    self.apply_all();
                }
            }
            Command::Workspace(name) => {
                let ws = self.workspace_named(&name);
                self.show_workspace(ws);
            }
            Command::MoveToWorkspace(name) => {
                if let Some(id) = focused {
                    let ws = self.workspace_named(&name);
                    self.send_to_workspace(id, ws);
                }
            }
            Command::Split(axis) => {
                if let Some(id) = focused {
                    self.tree.split(id, axis);
                }
            }
            Command::ToggleSplit => {
                let ws = focused.and_then(|id| self.workspace_of(id));
                if let (Some(id), Some(ws)) = (focused, ws) {
                    match self.tree.workspace_layout(ws) {
                        // Affects where the next window opens.
                        Layout::Manual => {
                            self.tree.toggle_split(id);
                        }
                        // Hyprland's togglesplit: flips the split right away.
                        Layout::Dwindle => {
                            if self.tree.flip_split(id) {
                                self.apply(ws);
                            }
                        }
                    }
                }
            }
            Command::SetLayout(layout) => {
                if let Some(ws) = self.tree.focused_workspace() {
                    self.tree.set_workspace_layout(ws, layout);
                    info!(workspace = self.tree.workspace_name(ws), %layout, "layout set");
                }
            }
            Command::ToggleFloating => {
                if let Some(id) = focused
                    && self.tree.toggle_floating(id)
                    && let Some(ws) = self.workspace_of(id)
                {
                    if self.tree.is_floating(id) {
                        self.place_floating(id);
                    }
                    self.sync_topmost(id);
                    self.apply(ws);
                }
            }
            Command::ToggleFullscreen => {
                // Positioning happens in `sync_fullscreen`.
                if let Some(id) = focused {
                    self.tree.toggle_fullscreen(id);
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
            // Handled by the main loop, which owns the hotkey registrations.
            Command::ReloadConfig => {}
            Command::Quit => return ControlFlow::Break(()),
        }
        ControlFlow::Continue(())
    }

    pub fn handle(&mut self, event: WindowEvent) {
        self.on_event(event);
        self.sync_fullscreen();
        self.release_refused();
        self.sync_border();
    }

    fn on_event(&mut self, event: WindowEvent) {
        debug!(?event);
        match event {
            WindowEvent::Shown(id)
            | WindowEvent::Uncloaked(id)
            | WindowEvent::Restored(id)
            // Some apps show their window before giving it a title.
            | WindowEvent::TitleChanged(id) => self.try_manage(id),
            WindowEvent::Destroyed(id) => {
                self.unmovable.remove(&id);
                self.unmanage(id);
            }
            WindowEvent::Hidden(id) | WindowEvent::Minimized(id) => self.unmanage(id),
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
            WindowEvent::MoveSizeEnded(id) if self.tree.is_floating(id) => {
                self.floating_moved(id);
            }
            WindowEvent::MoveSizeEnded(id) if self.tree.contains_window(id) => {
                self.tiled_dropped(id);
            }
            WindowEvent::MoveSizeEnded(_) => {}
            WindowEvent::MoveSizeStarted(id) => {
                self.drag_start = rivewm_platform::query_window(id).map(|w| (id, w.frame));
            }
            WindowEvent::LocationChanged(_) => {}
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
        if !self.tree.is_workspace_active(ws) {
            cloak(id, true);
        } else if self.tree.is_floating(id) {
            self.place_floating(id);
        } else {
            self.apply(ws);
        }
        self.apply(source);
        self.focus_os(source);
    }

    /// The user finished dragging or resizing a floating window: remember
    /// where it is, and if it was dropped on another monitor, hand it to the
    /// workspace shown there.
    fn floating_moved(&mut self, id: WindowId) {
        let Some(info) = rivewm_platform::query_window(id) else {
            return;
        };
        let dropped_on = self
            .tree
            .monitor_by_id(info.monitor)
            .and_then(|m| self.tree.active_workspace(m));
        if let Some(target) = dropped_on
            && self.workspace_of(id) != Some(target)
        {
            self.tree.move_window_to_workspace(id, target);
            self.tree.focus_window(id);
            info!(
                window = format_args!("{:#x}", id.0),
                workspace = self.tree.workspace_name(target),
                "floating window moved to workspace"
            );
        }
        // After any move, so the translated position is replaced by the
        // real one.
        self.tree.set_float_rect(id, info.frame);
    }

    /// Puts a floating window where the tree says it floats (the whole
    /// monitor, if it's fullscreen).
    fn place_floating(&self, id: WindowId) {
        let rect = self.workspace_of(id).and_then(|ws| {
            self.tree
                .floating_windows(ws)
                .into_iter()
                .find(|&(w, _)| w == id)
        });
        if let Some((_, rect)) = rect
            && let Err(err) = rivewm_platform::set_frame(id, rect)
        {
            warn!(window = format_args!("{:#x}", id.0), %err, "failed to position window");
        }
    }

    /// The user let go of a tiled window after dragging it.
    ///
    /// - Dragged by an edge or corner: the boundary at each edge that moved
    ///   follows it, so the neighbours across those edges resize to match.
    /// - Moved: it goes beside whichever tile the cursor is over, on the
    ///   side the cursor is nearest (or onto an empty monitor).
    ///
    /// Anything that can't be honoured (dropped back on its own tile, an
    /// edge against the monitor's edge) snaps back into place.
    fn tiled_dropped(&mut self, id: WindowId) {
        let Some(ws) = self.workspace_of(id) else {
            return;
        };
        let tile = self
            .tree
            .arrange(ws, self.config.gaps)
            .into_iter()
            .find(|&(w, _)| w == id)
            .map(|(_, r)| r);
        let frame = rivewm_platform::query_window(id).map(|w| w.frame);
        // Compare with the frame when the drag began rather than the tile:
        // apps with a minimum size bigger than their tile never match it.
        let start = match self.drag_start.take() {
            Some((w, start)) if w == id => Some(start),
            _ => tile,
        };
        let (Some(start), Some(frame)) = (start, frame) else {
            self.apply_all();
            return;
        };

        // A few pixels of slack absorb rounding in the frame measurements.
        const SLACK: i32 = 4;
        let resized = (frame.width - start.width).abs() > SLACK
            || (frame.height - start.height).abs() > SLACK;
        if resized {
            if self.tree.fullscreen_window(ws) != Some(id) {
                // How far each edge moved outwards.
                let edges = [
                    (Direction::Left, start.x - frame.x),
                    (Direction::Right, frame.right() - start.right()),
                    (Direction::Up, start.y - frame.y),
                    (Direction::Down, frame.bottom() - start.bottom()),
                ];
                for (side, grow) in edges {
                    if grow.abs() > SLACK {
                        self.tree.resize_edge(id, side, grow, self.config.gaps);
                    }
                }
            }
        } else if let Some((x, y)) = rivewm_platform::cursor_position() {
            match self.drop_target(x, y) {
                Some(Drop::Beside(target, side)) if self.tree.move_beside(id, target, side) => {
                    info!(
                        window = format_args!("{:#x}", id.0),
                        ?side,
                        "dropped beside window"
                    );
                }
                Some(Drop::EmptyWorkspace(ws))
                    if self.tree.move_window_to_workspace(id, ws).is_some() =>
                {
                    self.tree.focus_window(id);
                    info!(
                        window = format_args!("{:#x}", id.0),
                        "dropped on empty monitor"
                    );
                }
                _ => {}
            }
        }
        // Covers both monitors if it crossed between them.
        self.apply_all();
    }

    /// What's under the cursor at `(x, y)` for a drop.
    fn drop_target(&self, x: i32, y: i32) -> Option<Drop> {
        let ws = self.tree.monitors().find_map(|m| {
            self.tree
                .monitor_work_area(m)
                .contains_point(x, y)
                .then(|| self.tree.active_workspace(m))
                .flatten()
        })?;
        let tiles = self.tree.arrange(ws, self.config.gaps);
        if tiles.is_empty() {
            return Some(Drop::EmptyWorkspace(ws));
        }
        // Over a gap between tiles: no target.
        let &(target, r) = tiles.iter().find(|(_, r)| r.contains_point(x, y))?;
        let (cx, cy) = r.center();
        let dx = (x - cx) as f64 / r.width.max(1) as f64;
        let dy = (y - cy) as f64 / r.height.max(1) as f64;
        let side = if dx.abs() > dy.abs() {
            if dx < 0.0 {
                Direction::Left
            } else {
                Direction::Right
            }
        } else if dy < 0.0 {
            Direction::Up
        } else {
            Direction::Down
        };
        Some(Drop::Beside(target, side))
    }

    /// Lets go of windows Windows wouldn't let us move, so the rest of the
    /// layout closes up instead of leaving a gap. Classification already
    /// skips windows of elevated processes; this catches anything else that
    /// turns out to be out of reach, and remembers it until it closes.
    fn release_refused(&mut self) {
        let refused = std::mem::take(&mut *self.refused.borrow_mut());
        for id in refused {
            if self.unmovable.insert(id) {
                warn!(
                    window = format_args!("{:#x}", id.0),
                    "Windows won't let rivewm move this window (is it running as \
                     administrator?); leaving it alone"
                );
                self.unmanage(id);
            }
        }
    }

    /// Positions windows after fullscreen started or ended, however that
    /// happened: the command, or implicitly when focus moved on or the
    /// window closed.
    fn sync_fullscreen(&mut self) {
        let now: BTreeSet<WindowId> = self
            .tree
            .workspaces()
            .filter_map(|ws| self.tree.fullscreen_window(ws))
            .collect();
        if now == self.fullscreen {
            return;
        }
        let changed: Vec<_> = now
            .symmetric_difference(&self.fullscreen)
            .copied()
            .collect();
        self.fullscreen = now;
        self.apply_all();
        for id in changed {
            if self.tree.is_floating(id) {
                self.place_floating(id);
            }
        }
    }

    /// Moves the focused border colour to whichever managed window has focus
    /// now, repainting only the two windows involved. Nothing wears it while
    /// focus is on a window rivewm doesn't manage (e.g. Task Manager or the
    /// desktop).
    fn sync_border(&mut self) {
        let focused = self
            .tree
            .focused_window()
            .filter(|_| self.config.border.enabled)
            .filter(|&id| rivewm_platform::foreground_window() == Some(id));
        if focused == self.bordered {
            return;
        }
        if let Some(old) = self.bordered {
            self.paint_border(old, self.config.border.unfocused);
        }
        if let Some(new) = focused {
            self.paint_border(new, self.config.border.focused);
        }
        self.unbordered = self.bordered.filter(|&old| self.tree.contains_window(old));
        self.bordered = focused;

        // Some apps (Windows Terminal, Discord) reset their own border colour
        // when they gain or lose focus, racing the paint above. Paint again
        // a few times over the next second so ours lands last.
        let now = Instant::now();
        self.border_reasserts = BORDER_REASSERT_DELAYS
            .iter()
            .rev()
            .map(|&ms| now + Duration::from_millis(ms))
            .collect();
    }

    /// When [`Self::on_timer`] next needs to run, if at all.
    pub fn next_timer(&self) -> Option<Instant> {
        self.border_reasserts.last().copied()
    }

    /// Runs whatever timed work is due: currently re-painting borders.
    pub fn on_timer(&mut self) {
        let now = Instant::now();
        let mut due = false;
        while self.border_reasserts.last().is_some_and(|&at| at <= now) {
            self.border_reasserts.pop();
            due = true;
        }
        if !due {
            return;
        }
        if let Some(id) = self.bordered {
            self.paint_border(id, self.config.border.focused);
        }
        if let Some(id) = self.unbordered.filter(|&id| self.tree.contains_window(id)) {
            self.paint_border(id, self.config.border.unfocused);
        }
    }

    /// Colours a managed window's border, or leaves Windows' own if borders
    /// are turned off.
    fn paint_border(&self, id: WindowId, color: BorderColor) {
        let color = if self.config.border.enabled {
            color
        } else {
            BorderColor::Default
        };
        // Fails on Windows 10 and for windows that just closed; neither
        // matters.
        let _ = rivewm_platform::set_border_color(id, color);
    }

    /// Makes a window always-on-top if it's floating and the config asks for
    /// that, and undoes it otherwise. Windows that were already on top by
    /// their own choice (e.g. picture-in-picture) are left alone.
    fn sync_topmost(&self, id: WindowId) {
        let want = self.config.floating_on_top && self.tree.is_floating(id);
        let mut ours = lock(&MADE_TOPMOST);
        if want && !ours.contains(&id) && !rivewm_platform::is_topmost(id) {
            if rivewm_platform::set_topmost(id, true).is_ok() {
                ours.insert(id);
            }
        } else if !want && ours.remove(&id) {
            let _ = rivewm_platform::set_topmost(id, false);
        }
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
        self.tree
            .add_workspace(monitor, name, self.config.default_layout)
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

    /// A JSON snapshot of everything rivewm manages, for IPC clients:
    /// monitors, their workspaces, each workspace's tiling tree and floating
    /// windows, and what has focus. Window `id`s are what `focus-window`
    /// takes.
    pub fn state(&self) -> Value {
        let tree = &self.tree;
        let name = |ws: Option<NodeId>| ws.map(|ws| tree.workspace_name(ws));
        let monitors: Vec<Value> = tree
            .monitors()
            .map(|m| {
                let NodeKind::Monitor { id, work_area, .. } = tree.node(m).kind else {
                    unreachable!("monitors() yields monitors");
                };
                let workspaces: Vec<Value> = tree
                    .node(m)
                    .children
                    .iter()
                    .map(|&ws| self.workspace_state(ws))
                    .collect();
                json!({
                    "id": id.0,
                    "work_area": rect_json(work_area),
                    "active_workspace": name(tree.active_workspace(m)),
                    "workspaces": workspaces,
                })
            })
            .collect();
        json!({
            "focused_workspace": name(tree.focused_workspace()),
            "focused_window": tree.focused_window().map(|w| w.0),
            "monitors": monitors,
        })
    }

    /// A cheap summary of state for deriving subscriber events. Only the
    /// focused window's title needs an OS call.
    pub fn snapshot(&self) -> Snapshot {
        let tree = &self.tree;
        let workspaces = tree
            .monitors()
            .flat_map(|m| {
                let NodeKind::Monitor { id, .. } = tree.node(m).kind else {
                    unreachable!("monitors() yields monitors");
                };
                tree.node(m).children.iter().map(move |&ws| {
                    let mut windows = tree.arrange(ws, self.config.gaps);
                    windows.extend(tree.floating_windows(ws));
                    WorkspaceSnapshot {
                        name: tree.workspace_name(ws).to_owned(),
                        monitor: id,
                        visible: tree.is_workspace_active(ws),
                        windows,
                    }
                })
            })
            .collect();
        Snapshot {
            focused_workspace: tree
                .focused_workspace()
                .map(|ws| tree.workspace_name(ws).to_owned()),
            focused_window: tree
                .focused_window()
                .map(|id| (id, rivewm_platform::title(id))),
            workspaces,
        }
    }

    fn workspace_state(&self, ws: NodeId) -> Value {
        let rects: HashMap<WindowId, Rect> = self
            .tree
            .arrange(ws, self.config.gaps)
            .into_iter()
            .collect();
        let floating: Vec<Value> = self
            .tree
            .floating_windows(ws)
            .into_iter()
            .map(|(id, rect)| window_json(id, Some(rect), None))
            .collect();
        json!({
            "name": self.tree.workspace_name(ws),
            "visible": self.tree.is_workspace_active(ws),
            "layout": self.tree.workspace_layout(ws).to_string(),
            "focused_window": self.tree.workspace_focused_window(ws).map(|w| w.0),
            "tiling": self.tiling_json(ws, &rects),
            "floating": floating,
        })
    }

    fn tiling_json(&self, node: NodeId, rects: &HashMap<WindowId, Rect>) -> Value {
        let n = self.tree.node(node);
        match n.kind {
            NodeKind::Window { id, .. } => window_json(id, rects.get(&id).copied(), Some(n.weight)),
            _ => {
                let axis = match self.tree.container_axis(node) {
                    Some(Axis::Horizontal) => "horizontal",
                    _ => "vertical",
                };
                let children: Vec<Value> = n
                    .children
                    .iter()
                    .map(|&c| self.tiling_json(c, rects))
                    .collect();
                json!({ "type": "split", "axis": axis, "weight": n.weight, "children": children })
            }
        }
    }

    fn workspace_of(&self, id: WindowId) -> Option<NodeId> {
        self.tree
            .window_node(id)
            .and_then(|n| self.tree.workspace_of(n))
    }

    fn try_manage(&mut self, id: WindowId) {
        if self.tree.contains_window(id) || self.unmovable.contains(&id) {
            return;
        }
        let Some(info) = rivewm_platform::query_window(id) else {
            return;
        };
        if !info.is_manageable() || info.minimized {
            return;
        }
        if let Some(ws) = self.manage(&info, false)
            && self.tree.is_workspace_active(ws)
            && !self.tree.is_floating(id)
        {
            self.apply(ws);
        }
    }

    /// Starts managing a window, following the first matching window rule.
    ///
    /// Without a rule saying otherwise, a new tiled window opens on the
    /// focused workspace, while at startup it stays on the monitor it's
    /// already on. Floating windows always stay where the app put them, on
    /// whichever workspace that monitor shows. Returns the workspace, or
    /// `None` if a rule says to ignore the window.
    fn manage(&mut self, info: &WindowInfo, at_startup: bool) -> Option<NodeId> {
        let rule = self.config.rule_for(info);
        let action = rule.and_then(|r| r.action);
        let target = rule.and_then(|r| r.workspace.clone());
        if action == Some(RuleAction::Ignore) {
            debug!(window = format_args!("{:#x}", info.id.0), "ignored by rule");
            return None;
        }
        let floating = match action {
            Some(RuleAction::Float) => true,
            Some(RuleAction::Tile) => false,
            _ => info.floating,
        };

        let on_its_monitor = self
            .tree
            .monitor_by_id(info.monitor)
            .and_then(|m| self.tree.active_workspace(m));
        let ws = match target {
            Some(name) => self.workspace_named(&name),
            None if floating || at_startup => on_its_monitor.or(self.tree.focused_workspace())?,
            None => self.tree.focused_workspace()?,
        };

        if floating {
            self.tree.insert_floating(ws, info.id, info.frame);
        } else {
            self.tree.insert_window(ws, info.id);
        }
        lock(&MANAGED).insert(info.id);
        info!(
            window = format_args!("{:#x}", info.id.0),
            title = info.title,
            floating,
            workspace = self.tree.workspace_name(ws),
            "managing"
        );
        if !self.tree.is_workspace_active(ws) {
            // A rule sent it to a workspace that isn't shown.
            cloak(info.id, true);
        }
        self.sync_topmost(info.id);
        self.paint_border(info.id, self.config.border.unfocused);
        Some(ws)
    }

    fn unmanage(&mut self, id: WindowId) {
        let Some(ws) = self.tree.remove_window(id) else {
            return;
        };
        lock(&MANAGED).remove(&id);
        // No longer in the tree, so this undoes any always-on-top of ours.
        self.sync_topmost(id);
        // Give it back its normal border. Fails harmlessly if it's gone.
        let _ = rivewm_platform::set_border_color(id, BorderColor::Default);
        if self.bordered == Some(id) {
            self.bordered = None;
        }
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
        for (id, rect) in self.tree.arrange(ws, self.config.gaps) {
            match rivewm_platform::set_frame(id, rect) {
                Ok(()) => {}
                Err(err) if rivewm_platform::is_access_denied(&err) => {
                    self.refused.borrow_mut().push(id);
                }
                Err(err) => {
                    warn!(window = format_args!("{:#x}", id.0), %err, "failed to position window");
                }
            }
        }
        if let Some(id) = self.tree.fullscreen_window(ws) {
            let _ = rivewm_platform::raise_window(id);
        }
    }
}

fn window_json(id: WindowId, rect: Option<Rect>, weight: Option<f64>) -> Value {
    let info = rivewm_platform::query_window(id);
    json!({
        "type": "window",
        "id": id.0,
        "title": info.as_ref().map(|w| w.title.as_str()),
        "process": info.as_ref().and_then(|w| w.process.as_deref()),
        "class": info.as_ref().map(|w| w.class.as_str()),
        "rect": rect.map(rect_json),
        "weight": weight,
    })
}

fn rect_json(r: Rect) -> Value {
    json!({ "x": r.x, "y": r.y, "width": r.width, "height": r.height })
}

/// Attached monitors, primary first: it's the one that inherits workspaces
/// from monitors that disconnect.
fn monitor_specs() -> Vec<MonitorSpec> {
    let mut monitors = rivewm_platform::monitors();
    monitors.sort_by_key(|m| !m.primary);
    monitors
        .into_iter()
        .map(|m| MonitorSpec {
            id: m.id,
            name: m.device,
            bounds: m.bounds,
            work_area: m.work_area,
        })
        .collect()
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

/// Makes every managed window visible again (uncloaked and shown), drops
/// any always-on-top we added and restores normal borders. Safe to call from a panic hook or signal
/// handler.
pub fn restore_all() {
    for &id in lock(&MANAGED).iter() {
        let _ = rivewm_platform::set_cloaked(id, false);
        rivewm_platform::show_window(id);
        let _ = rivewm_platform::set_border_color(id, BorderColor::Default);
    }
    for &id in lock(&MADE_TOPMOST).iter() {
        let _ = rivewm_platform::set_topmost(id, false);
    }
    lock(&MADE_TOPMOST).clear();
    lock(&CLOAKED).clear();
    let _ = std::fs::remove_file(cloaked_record());
}

fn lock<T>(mutex: &'static Mutex<T>) -> MutexGuard<'static, T> {
    // A panic while holding the lock must not stop us restoring windows.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
