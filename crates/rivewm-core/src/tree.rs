//! The window tree: Root → Monitor → Workspace → Split* → Window.
//!
//! Workspaces and splits are both *containers*: they lay their children out
//! along an [`Axis`], each child taking a share of the space proportional to
//! its `weight`. Weights of siblings always sum to 1.
//!
//! Window insertion is delegated to the workspace's [`Layout`], so dynamic
//! layouts can later shape the tree without commands needing to know.

use std::collections::HashMap;

mod saved;
pub use saved::{SavedFloat, SavedNode, SavedWorkspace};

use crate::layout::Layout;
use crate::{MonitorId, Rect, WindowId};

/// Smallest share of its container a child can be resized down to.
const MIN_WEIGHT: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u32);

/// The direction a container lays out its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    /// Children side by side, left to right.
    Horizontal,
    /// Children stacked, top to bottom.
    Vertical,
}

impl Axis {
    pub fn flip(self) -> Self {
        match self {
            Axis::Horizontal => Axis::Vertical,
            Axis::Vertical => Axis::Horizontal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Horizontal,
            Direction::Up | Direction::Down => Axis::Vertical,
        }
    }
}

/// Split sizes captured by [`Tree::save_weights`].
#[derive(Debug, Clone)]
pub struct SavedWeights(Vec<(NodeId, f64)>);

/// A monitor as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorSpec {
    pub id: MonitorId,
    /// Stable across display changes, unlike `id` (e.g. `\\.\DISPLAY2`).
    pub name: String,
    pub bounds: Rect,
    pub work_area: Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Gaps {
    /// Space between adjacent windows.
    pub inner: i32,
    /// Space between windows and the monitor's work area edge.
    pub outer: i32,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    /// Share of the parent container, in `0.0..=1.0`.
    pub weight: f64,
    pub kind: NodeKind,
}

#[derive(Debug, Clone)]
pub enum NodeKind {
    Root,
    Monitor {
        id: MonitorId,
        /// Stable name used to recognise the monitor across display changes
        /// (e.g. `\\.\DISPLAY2`), since `id` can change.
        name: String,
        /// The whole monitor; fullscreen windows cover this.
        bounds: Rect,
        /// The monitor minus the taskbar; tiles fill this.
        work_area: Rect,
        active_workspace: Option<NodeId>,
    },
    Workspace {
        name: String,
        layout: Layout,
        axis: Axis,
        /// Last focused window in this workspace (tiled or floating).
        focus: Option<NodeId>,
        /// Floating windows, oldest first. They aren't part of the tiling
        /// tree: their parent is this workspace, but they aren't among its
        /// `children`.
        floating: Vec<NodeId>,
        /// A window (tiled or floating) covering the whole monitor.
        fullscreen: Option<NodeId>,
        /// Name of the monitor this workspace was moved off when that
        /// monitor disconnected; it moves back if the monitor returns.
        home: Option<String>,
    },
    Split {
        axis: Axis,
    },
    Window {
        id: WindowId,
        floating: bool,
        /// Where the window sits while floating. Remembered while it's tiled
        /// so toggling back restores it.
        float_rect: Option<Rect>,
        /// Minimized windows keep their place in the tree but take no space,
        /// so they come back exactly where they were.
        minimized: bool,
    },
}

#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    root: NodeId,
    windows: HashMap<WindowId, NodeId>,
    focused_workspace: Option<NodeId>,
    /// Layout for workspaces the tree creates itself (e.g. for a newly
    /// connected monitor).
    default_layout: Layout,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Self {
        let root = Node {
            parent: None,
            children: Vec::new(),
            weight: 1.0,
            kind: NodeKind::Root,
        };
        Self {
            nodes: vec![Some(root)],
            free: Vec::new(),
            root: NodeId(0),
            windows: HashMap::new(),
            focused_workspace: None,
            default_layout: Layout::default(),
        }
    }

    // ---- Queries -----------------------------------------------------------

    pub fn node(&self, id: NodeId) -> &Node {
        self.nodes[id.0 as usize]
            .as_ref()
            .expect("NodeId refers to a freed node")
    }

    pub fn window_node(&self, window: WindowId) -> Option<NodeId> {
        self.windows.get(&window).copied()
    }

    pub fn contains_window(&self, window: WindowId) -> bool {
        self.windows.contains_key(&window)
    }

    pub fn focused_workspace(&self) -> Option<NodeId> {
        self.focused_workspace
    }

    pub fn focused_window(&self) -> Option<WindowId> {
        let ws = self.focused_workspace?;
        self.workspace_focus(ws).map(|n| self.window_id(n))
    }

    /// The workspace containing `node`, or `node` itself if it is one.
    pub fn workspace_of(&self, node: NodeId) -> Option<NodeId> {
        let mut cur = Some(node);
        while let Some(id) = cur {
            if matches!(self.node(id).kind, NodeKind::Workspace { .. }) {
                return Some(id);
            }
            cur = self.node(id).parent;
        }
        None
    }

    /// All windows in a workspace: tiled ones in tree order, then floating.
    pub fn workspace_windows(&self, ws: NodeId) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.collect_windows(ws, &mut out);
        out.extend(self.floating_nodes(ws).iter().map(|&n| self.window_id(n)));
        out
    }

    /// Floating windows in a workspace with their positions, oldest first.
    /// A fullscreen one is given the monitor's bounds.
    pub fn floating_windows(&self, ws: NodeId) -> Vec<(WindowId, Rect)> {
        let fullscreen = self.fullscreen_node(ws);
        self.floating_nodes(ws)
            .iter()
            .filter(|&&n| self.is_shown(n))
            .filter_map(|&n| match self.node(n).kind {
                _ if fullscreen == Some(n) => {
                    Some((self.window_id(n), self.monitor_bounds(self.monitor_of(ws))))
                }
                NodeKind::Window { id, float_rect, .. } => Some((id, float_rect?)),
                _ => None,
            })
            .collect()
    }

    /// The window covering the whole monitor in `ws`, if any.
    pub fn fullscreen_window(&self, ws: NodeId) -> Option<WindowId> {
        self.fullscreen_node(ws).map(|n| self.window_id(n))
    }

    fn fullscreen_node(&self, ws: NodeId) -> Option<NodeId> {
        match self.node(ws).kind {
            NodeKind::Workspace { fullscreen, .. } => fullscreen,
            _ => None,
        }
    }

    fn set_fullscreen_node(&mut self, ws: NodeId, node: Option<NodeId>) {
        if let NodeKind::Workspace { fullscreen, .. } = &mut self.node_mut(ws).kind {
            *fullscreen = node;
        }
    }

    /// Makes `window` cover its whole monitor, or returns it to normal. It
    /// gets focus. Focusing another window in the workspace, or moving,
    /// floating or removing this one, also ends fullscreen.
    pub fn toggle_fullscreen(&mut self, window: WindowId) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        let ws = self.workspace_of(node).expect("window outside a workspace");
        let new = (self.fullscreen_node(ws) != Some(node)).then_some(node);
        self.set_workspace_focus(ws, Some(node));
        self.set_fullscreen_node(ws, new);
        true
    }

    pub fn is_floating(&self, window: WindowId) -> bool {
        self.window_node(window)
            .is_some_and(|n| self.node_is_floating(n))
    }

    /// Where a window sits (or would sit) while floating.
    pub fn float_rect(&self, window: WindowId) -> Option<Rect> {
        match self.node(self.window_node(window)?).kind {
            NodeKind::Window { float_rect, .. } => float_rect,
            _ => None,
        }
    }

    /// Records where a floating window is, e.g. after the user dragged it.
    pub fn set_float_rect(&mut self, window: WindowId, rect: Rect) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        if let NodeKind::Window { float_rect, .. } = &mut self.node_mut(node).kind {
            *float_rect = Some(rect);
        }
        true
    }

    pub(crate) fn node_is_floating(&self, node: NodeId) -> bool {
        matches!(
            self.node(node).kind,
            NodeKind::Window { floating: true, .. }
        )
    }

    pub fn is_minimized(&self, window: WindowId) -> bool {
        self.window_node(window).is_some_and(|n| {
            matches!(
                self.node(n).kind,
                NodeKind::Window {
                    minimized: true,
                    ..
                }
            )
        })
    }

    /// Whether a node takes up space: a window that isn't minimized, or a
    /// container with at least one such window inside.
    pub(crate) fn is_shown(&self, node: NodeId) -> bool {
        match self.node(node).kind {
            NodeKind::Window { minimized, .. } => !minimized,
            _ => self.node(node).children.iter().any(|&c| self.is_shown(c)),
        }
    }

    /// Minimizes or restores a window. A minimized window keeps its place in
    /// the tree (tiled) or its position (floating) but takes no space, so
    /// restoring it puts it back exactly where it was. Minimizing hands
    /// focus to another window in the workspace and ends fullscreen.
    pub fn set_minimized(&mut self, window: WindowId, value: bool) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        let NodeKind::Window { minimized, .. } = &mut self.node_mut(node).kind else {
            return false;
        };
        if *minimized == value {
            return false;
        }
        *minimized = value;
        if value {
            let ws = self.workspace_of(node).expect("window outside a workspace");
            if self.fullscreen_node(ws) == Some(node) {
                self.set_fullscreen_node(ws, None);
            }
            if self.workspace_focus(ws) == Some(node) {
                let next = self.fallback_focus(ws);
                self.set_workspace_focus(ws, next);
            }
        }
        true
    }

    fn floating_nodes(&self, ws: NodeId) -> &[NodeId] {
        match &self.node(ws).kind {
            NodeKind::Workspace { floating, .. } => floating,
            _ => &[],
        }
    }

    fn floating_nodes_mut(&mut self, ws: NodeId) -> &mut Vec<NodeId> {
        match &mut self.node_mut(ws).kind {
            NodeKind::Workspace { floating, .. } => floating,
            _ => panic!("{ws:?} is not a workspace"),
        }
    }

    /// Flips a window node between tiled and floating bookkeeping. Callers
    /// handle tree membership.
    fn set_node_floating(&mut self, node: NodeId, value: bool) {
        if let NodeKind::Window { floating, .. } = &mut self.node_mut(node).kind {
            *floating = value;
        }
    }

    fn collect_windows(&self, node: NodeId, out: &mut Vec<WindowId>) {
        match self.node(node).kind {
            NodeKind::Window { id, .. } => out.push(id),
            _ => {
                for &child in &self.node(node).children {
                    self.collect_windows(child, out);
                }
            }
        }
    }

    /// Sets the layout used for workspaces the tree creates on its own.
    pub fn set_default_layout(&mut self, layout: Layout) {
        self.default_layout = layout;
    }

    /// Changes how new windows are placed in `ws`. Windows already there
    /// stay where they are.
    pub fn set_workspace_layout(&mut self, ws: NodeId, new: Layout) {
        if let NodeKind::Workspace { layout, .. } = &mut self.node_mut(ws).kind {
            *layout = new;
        }
    }

    pub fn workspace_layout(&self, ws: NodeId) -> Layout {
        match self.node(ws).kind {
            NodeKind::Workspace { layout, .. } => layout,
            _ => panic!("{ws:?} is not a workspace"),
        }
    }

    pub(crate) fn workspace_focus(&self, ws: NodeId) -> Option<NodeId> {
        match self.node(ws).kind {
            NodeKind::Workspace { focus, .. } => focus,
            _ => None,
        }
    }

    pub(crate) fn window_id_of(&self, node: NodeId) -> WindowId {
        self.window_id(node)
    }

    fn window_id(&self, node: NodeId) -> WindowId {
        match self.node(node).kind {
            NodeKind::Window { id, .. } => id,
            _ => panic!("{node:?} is not a window"),
        }
    }

    pub fn monitor_of(&self, ws: NodeId) -> NodeId {
        self.node(ws).parent.expect("workspace without monitor")
    }

    /// Axis of a workspace or split; `None` for other node kinds.
    pub fn container_axis(&self, id: NodeId) -> Option<Axis> {
        match self.node(id).kind {
            NodeKind::Workspace { axis, .. } | NodeKind::Split { axis } => Some(axis),
            _ => None,
        }
    }

    fn index_in_parent(&self, id: NodeId) -> (NodeId, usize) {
        let parent = self.node(id).parent.expect("node has no parent");
        let idx = self
            .node(parent)
            .children
            .iter()
            .position(|&c| c == id)
            .expect("node missing from parent's children");
        (parent, idx)
    }

    /// The first window under `id` in tree order that isn't minimized.
    fn first_window(&self, id: NodeId) -> Option<NodeId> {
        match self.node(id).kind {
            NodeKind::Window { minimized, .. } => (!minimized).then_some(id),
            _ => self
                .node(id)
                .children
                .iter()
                .find_map(|&c| self.first_window(c)),
        }
    }

    /// The last tiled, non-minimized window in `ws` in tree order: the
    /// innermost of a dwindle spiral.
    pub(crate) fn last_tiled_window(&self, ws: NodeId) -> Option<NodeId> {
        self.last_window(ws)
    }

    /// The last window under `id` in tree order that isn't minimized.
    fn last_window(&self, id: NodeId) -> Option<NodeId> {
        match self.node(id).kind {
            NodeKind::Window { minimized, .. } => (!minimized).then_some(id),
            _ => self
                .node(id)
                .children
                .iter()
                .rev()
                .find_map(|&c| self.last_window(c)),
        }
    }

    // ---- Monitors & workspaces --------------------------------------------

    pub fn add_monitor(&mut self, spec: &MonitorSpec) -> NodeId {
        let node = self.alloc(NodeKind::Monitor {
            id: spec.id,
            name: spec.name.clone(),
            bounds: spec.bounds,
            work_area: spec.work_area,
            active_workspace: None,
        });
        self.insert_child(self.root, self.node(self.root).children.len(), node);
        node
    }

    /// Brings the monitors in line with what's attached now, after a display
    /// change. Monitors are matched by name.
    ///
    /// - A monitor that's gone hands its workspaces to the first monitor in
    ///   `specs` (put the primary first), hidden, each remembering where it
    ///   came from. Empty ones are dropped.
    /// - A monitor that's back reclaims the workspaces it lost; a new one
    ///   gets a fresh workspace named with the lowest unused number.
    /// - A monitor that moved or resized keeps its workspaces; floating
    ///   windows keep their position relative to it.
    ///
    /// Every monitor ends up showing a workspace, and focus stays on a
    /// visible one. An empty `specs` (seen briefly while displays sleep) is
    /// ignored. Returns whether anything changed.
    pub fn sync_monitors(&mut self, specs: &[MonitorSpec]) -> bool {
        if specs.is_empty() {
            return false;
        }
        let mut changed = false;

        let mut gone = Vec::new();
        for monitor in self.monitors().collect::<Vec<_>>() {
            match specs.iter().find(|s| s.name == self.monitor_name(monitor)) {
                Some(spec) => changed |= self.update_monitor(monitor, spec),
                None => gone.push(monitor),
            }
        }

        let mut added = Vec::new();
        for spec in specs {
            if self.monitor_by_name(&spec.name).is_none() {
                added.push(self.add_monitor(spec));
                changed = true;
            }
        }

        let fallback = self
            .monitor_by_name(&specs[0].name)
            .expect("just ensured every spec has a monitor");
        for monitor in gone {
            let name = self.monitor_name(monitor).to_owned();
            for ws in self.node(monitor).children.clone() {
                if self.workspace_windows(ws).is_empty() {
                    if self.focused_workspace == Some(ws) {
                        self.focused_workspace = None;
                    }
                    self.detach(ws);
                    self.release(ws);
                    continue;
                }
                if let NodeKind::Workspace { home, .. } = &mut self.node_mut(ws).kind {
                    home.get_or_insert_with(|| name.clone());
                }
                self.move_workspace(ws, fallback);
            }
            self.detach(monitor);
            self.release(monitor);
            changed = true;
        }

        for &monitor in &added {
            let name = self.monitor_name(monitor).to_owned();
            let returning: Vec<_> = self
                .workspaces()
                .filter(|&ws| self.workspace_home(ws) == Some(&name))
                .collect();
            for ws in returning {
                if let NodeKind::Workspace { home, .. } = &mut self.node_mut(ws).kind {
                    *home = None;
                }
                self.move_workspace(ws, monitor);
            }
        }

        for monitor in self.monitors().collect::<Vec<_>>() {
            self.ensure_active_workspace(monitor);
        }
        let focus_ok = self
            .focused_workspace
            .is_some_and(|ws| self.is_workspace_active(ws));
        if !focus_ok {
            self.focused_workspace = self.active_workspace(fallback);
        }
        changed
    }

    /// Updates a monitor's id and geometry. Floating windows on it keep
    /// their place relative to the work area. Returns whether it changed.
    fn update_monitor(&mut self, monitor: NodeId, spec: &MonitorSpec) -> bool {
        let old_area = self.monitor_work_area(monitor);
        let NodeKind::Monitor {
            id,
            bounds,
            work_area,
            ..
        } = &mut self.node_mut(monitor).kind
        else {
            unreachable!("not a monitor");
        };
        if (*id, *bounds, *work_area) == (spec.id, spec.bounds, spec.work_area) {
            return false;
        }
        (*id, *bounds, *work_area) = (spec.id, spec.bounds, spec.work_area);
        for ws in self.node(monitor).children.clone() {
            self.translate_floats(ws, old_area, spec.work_area);
        }
        true
    }

    /// Moves a workspace, hidden, to another monitor, keeping its floating
    /// windows' relative positions. The monitor it left may be left without
    /// an active workspace; see [`Self::ensure_active_workspace`].
    fn move_workspace(&mut self, ws: NodeId, to: NodeId) {
        let from = self.monitor_of(ws);
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(from).kind
            && *active_workspace == Some(ws)
        {
            *active_workspace = None;
        }
        let (from_area, to_area) = (self.monitor_work_area(from), self.monitor_work_area(to));
        self.detach(ws);
        self.attach_workspace(to, ws);
        self.translate_floats(ws, from_area, to_area);
    }

    /// Gives a monitor an active workspace if it has none: its first one, or
    /// a new one named with the lowest unused number.
    fn ensure_active_workspace(&mut self, monitor: NodeId) {
        if self.active_workspace(monitor).is_some() {
            return;
        }
        let ws = match self.node(monitor).children.first() {
            Some(&ws) => ws,
            None => {
                let name = (1..)
                    .map(|n: u32| n.to_string())
                    .find(|n| self.workspace_by_name(n).is_none())
                    .expect("some number is free");
                self.add_workspace(monitor, name, self.default_layout)
            }
        };
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(monitor).kind
        {
            *active_workspace = Some(ws);
        }
    }

    fn translate_floats(&mut self, ws: NodeId, from: Rect, to: Rect) {
        if from == to {
            return;
        }
        for node in self.floating_nodes(ws).to_vec() {
            if let NodeKind::Window {
                float_rect: Some(r),
                ..
            } = &mut self.node_mut(node).kind
            {
                *r = translate_into(*r, from, to);
            }
        }
    }

    pub fn monitor_name(&self, monitor: NodeId) -> &str {
        match &self.node(monitor).kind {
            NodeKind::Monitor { name, .. } => name,
            _ => panic!("{monitor:?} is not a monitor"),
        }
    }

    fn monitor_by_name(&self, name: &str) -> Option<NodeId> {
        self.monitors().find(|&m| self.monitor_name(m) == name)
    }

    fn workspace_home(&self, ws: NodeId) -> Option<&String> {
        match &self.node(ws).kind {
            NodeKind::Workspace { home, .. } => home.as_ref(),
            _ => None,
        }
    }

    /// Adds a workspace to a monitor. The first workspace on a monitor becomes
    /// its active one, and the first workspace overall gets focus.
    pub fn add_workspace(
        &mut self,
        monitor: NodeId,
        name: impl Into<String>,
        layout: Layout,
    ) -> NodeId {
        let ws = self.alloc(NodeKind::Workspace {
            name: name.into(),
            layout,
            axis: Axis::Horizontal,
            focus: None,
            floating: Vec::new(),
            fullscreen: None,
            home: None,
        });
        self.attach_workspace(monitor, ws);
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(monitor).kind
        {
            active_workspace.get_or_insert(ws);
        }
        self.focused_workspace.get_or_insert(ws);
        ws
    }

    /// Puts a detached workspace on a monitor, keeping workspaces ordered by
    /// name (numerically where possible) so a status bar can list them as-is.
    fn attach_workspace(&mut self, monitor: NodeId, ws: NodeId) {
        let key = workspace_sort_key(self.workspace_name(ws));
        let at = self
            .node(monitor)
            .children
            .iter()
            .position(|&w| workspace_sort_key(self.workspace_name(w)) > key)
            .unwrap_or(self.node(monitor).children.len());
        self.node_mut(ws).parent = Some(monitor);
        self.node_mut(monitor).children.insert(at, ws);
    }

    /// Makes `ws` the active workspace on its monitor and gives it focus.
    /// Returns the workspace it replaced on that monitor, if different.
    pub fn focus_workspace(&mut self, ws: NodeId) -> Option<NodeId> {
        let monitor = self.monitor_of(ws);
        let mut previous = None;
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(monitor).kind
        {
            previous = active_workspace.replace(ws).filter(|&p| p != ws);
        }
        self.focused_workspace = Some(ws);
        previous
    }

    /// Deletes an empty, inactive workspace.
    pub fn remove_workspace(&mut self, ws: NodeId) {
        assert!(self.workspace_windows(ws).is_empty(), "workspace not empty");
        assert!(!self.is_workspace_active(ws), "workspace is active");
        self.detach(ws);
        self.release(ws);
    }

    pub fn monitors(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.node(self.root).children.iter().copied()
    }

    pub fn monitor_by_id(&self, id: MonitorId) -> Option<NodeId> {
        self.monitors()
            .find(|&m| matches!(self.node(m).kind, NodeKind::Monitor { id: mid, .. } if mid == id))
    }

    /// All workspaces on all monitors.
    pub fn workspaces(&self) -> impl Iterator<Item = NodeId> + '_ {
        self.monitors()
            .flat_map(|m| self.node(m).children.iter().copied())
    }

    pub fn workspace_by_name(&self, name: &str) -> Option<NodeId> {
        self.workspaces()
            .find(|&ws| self.workspace_name(ws) == name)
    }

    pub fn workspace_name(&self, ws: NodeId) -> &str {
        match &self.node(ws).kind {
            NodeKind::Workspace { name, .. } => name,
            _ => panic!("{ws:?} is not a workspace"),
        }
    }

    /// Whether `ws` is the one shown on its monitor.
    pub fn is_workspace_active(&self, ws: NodeId) -> bool {
        self.active_workspace(self.monitor_of(ws)) == Some(ws)
    }

    /// The window in `ws` that should get focus when it's shown.
    pub fn workspace_focused_window(&self, ws: NodeId) -> Option<WindowId> {
        self.workspace_focus(ws).map(|n| self.window_id(n))
    }

    // ---- Windows -----------------------------------------------------------

    /// Adds a window to a workspace. Where it goes is up to the workspace's
    /// layout. Does not change focus.
    pub fn insert_window(&mut self, ws: NodeId, window: WindowId) -> NodeId {
        assert!(
            !self.windows.contains_key(&window),
            "{window:?} is already in the tree"
        );
        let node = self.alloc(NodeKind::Window {
            id: window,
            floating: false,
            float_rect: None,
            minimized: false,
        });
        self.windows.insert(window, node);
        self.layout_insert(ws, node);
        node
    }

    /// Adds a floating window to a workspace at `rect`. Does not change
    /// focus.
    pub fn insert_floating(&mut self, ws: NodeId, window: WindowId, rect: Rect) -> NodeId {
        assert!(
            !self.windows.contains_key(&window),
            "{window:?} is already in the tree"
        );
        let node = self.alloc(NodeKind::Window {
            id: window,
            floating: true,
            float_rect: Some(rect),
            minimized: false,
        });
        self.windows.insert(window, node);
        self.node_mut(node).parent = Some(ws);
        self.floating_nodes_mut(ws).push(node);
        node
    }

    /// Switches a window between tiled and floating, keeping focus on it.
    ///
    /// A window floated for the first time is centred on its monitor at 60%
    /// of the work area; after that it returns to where it last floated. A
    /// window tiled again is placed by the workspace's layout as if new.
    pub fn toggle_floating(&mut self, window: WindowId) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        let ws = self.workspace_of(node).expect("window outside a workspace");
        if self.node_is_floating(node) {
            self.floating_nodes_mut(ws).retain(|&n| n != node);
            self.node_mut(node).parent = None;
            self.set_node_floating(node, false);
            // Manual layout inserts beside the focused window; that's this
            // one, which is no longer in the tree, so append instead.
            self.set_workspace_focus(ws, None);
            self.layout_insert(ws, node);
        } else {
            let area = self.monitor_work_area(self.monitor_of(ws));
            let rect = self.float_rect(window).unwrap_or_else(|| {
                let (w, h) = (area.width * 3 / 5, area.height * 3 / 5);
                Rect::new(
                    area.x + (area.width - w) / 2,
                    area.y + (area.height - h) / 2,
                    w,
                    h,
                )
            });
            self.unlink_window(node);
            self.set_node_floating(node, true);
            self.set_float_rect(window, rect);
            self.node_mut(node).parent = Some(ws);
            self.floating_nodes_mut(ws).push(node);
        }
        self.set_workspace_focus(ws, Some(node));
        true
    }

    /// Removes a window, tidies up the tree around it and, if it was focused,
    /// moves focus to its nearest sibling. Returns its workspace.
    pub fn remove_window(&mut self, window: WindowId) -> Option<NodeId> {
        let node = self.windows.remove(&window)?;
        let ws = self.unlink_window(node);
        self.release(node);
        self.layout_after_remove(ws);
        Some(ws)
    }

    /// Takes a window node out of the tree without freeing it: detaches it,
    /// tidies its old container and, if it was its workspace's focus, hands
    /// focus to its nearest sibling. Returns the workspace it left.
    fn unlink_window(&mut self, node: NodeId) -> NodeId {
        let ws = self.workspace_of(node).expect("window outside a workspace");
        if self.fullscreen_node(ws) == Some(node) {
            self.set_fullscreen_node(ws, None);
        }
        if self.node_is_floating(node) {
            self.floating_nodes_mut(ws).retain(|&n| n != node);
            self.node_mut(node).parent = None;
            if self.workspace_focus(ws) == Some(node) {
                let next = self.fallback_focus(ws);
                self.set_workspace_focus(ws, next);
            }
            return ws;
        }
        let (parent, idx) = self.index_in_parent(node);

        let next_focus = (self.workspace_focus(ws) == Some(node)).then(|| {
            let siblings = &self.node(parent).children;
            if let Some(&next) = siblings.get(idx + 1) {
                self.first_window(next)
            } else if idx > 0 {
                self.last_window(siblings[idx - 1])
            } else {
                None
            }
        });

        self.detach(node);
        self.normalize(parent);

        if let Some(next) = next_focus {
            // A window was the only child of its split: fall back to anything
            // left in the workspace.
            let next = next.or_else(|| self.fallback_focus(ws));
            self.set_workspace_focus(ws, next);
        }
        ws
    }

    /// Something in `ws` to focus when nothing nearer applies: the first
    /// tiled window, else the newest floating one.
    fn fallback_focus(&self, ws: NodeId) -> Option<NodeId> {
        self.first_window(ws).or_else(|| {
            self.floating_nodes(ws)
                .iter()
                .rev()
                .copied()
                .find(|&n| self.is_shown(n))
        })
    }

    /// Sends a window to another workspace, placed by that workspace's
    /// layout. Focus stays where it was: the source workspace falls back to
    /// a neighbouring window, and the target remembers the arrival as its
    /// focus only if it had none. Returns the source workspace.
    pub fn move_window_to_workspace(&mut self, window: WindowId, target: NodeId) -> Option<NodeId> {
        let node = self.window_node(window)?;
        let source = self.workspace_of(node)?;
        if source == target {
            return None;
        }
        let floating = self.node_is_floating(node);
        self.unlink_window(node);
        if floating {
            // Keep its position relative to the monitor, nudged back inside
            // if the target monitor is smaller.
            let from = self.monitor_work_area(self.monitor_of(source));
            let to = self.monitor_work_area(self.monitor_of(target));
            if let Some(r) = self.float_rect(window) {
                self.set_float_rect(window, translate_into(r, from, to));
            }
            self.node_mut(node).parent = Some(target);
            self.floating_nodes_mut(target).push(node);
        } else {
            self.layout_insert(target, node);
        }
        if self.workspace_focus(target).is_none() {
            self.set_workspace_focus(target, Some(node));
        }
        Some(source)
    }

    /// Moves a tiled `window` so it sits on `side` of the tiled `target`,
    /// which may be in another workspace or on another monitor. If target's
    /// container runs the other way, target is wrapped in a new split so the
    /// two can sit side by side. The window keeps focus. Returns `false` if
    /// there's nothing to do.
    pub fn move_beside(&mut self, window: WindowId, target: WindowId, side: Direction) -> bool {
        let (Some(node), Some(target)) = (self.window_node(window), self.window_node(target))
        else {
            return false;
        };
        if node == target || self.node_is_floating(node) || self.node_is_floating(target) {
            return false;
        }
        let axis = side.axis();
        let after = matches!(side, Direction::Right | Direction::Down);

        // Unlink first: it may restructure the tree around the old spot,
        // which can include target's container.
        self.unlink_window(node);
        let (parent, idx) = self.index_in_parent(target);
        if self.container_axis(parent) == Some(axis) {
            self.insert_child(parent, if after { idx + 1 } else { idx }, node);
        } else {
            // `split` re-orients a lone child's container instead of
            // wrapping it, which works just as well here.
            self.split(self.window_id(target), axis);
            let (parent, idx) = self.index_in_parent(target);
            self.insert_child(parent, if after { idx + 1 } else { idx }, node);
        }
        self.focus_window(window);
        true
    }

    /// Moves `window` one step in `direction`, i3 style:
    ///
    /// 1. next to a window in its container along that axis: swap with it;
    /// 2. next to a split: move into it, at the edge facing the window;
    /// 3. otherwise: leave the container and land beside the nearest ancestor
    ///    laid out along that axis;
    /// 4. at the workspace edge: cross to the next monitor that way, or if
    ///    there is none, re-orient the workspace so the window can sit
    ///    beside everything else.
    ///
    /// The window keeps focus. Returns `false` if nothing moved, which is
    /// always the case for floating windows.
    pub fn move_in_direction(&mut self, window: WindowId, direction: Direction) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        if self.node_is_floating(node) {
            return false;
        }
        let axis = direction.axis();
        let forward = matches!(direction, Direction::Right | Direction::Down);

        let (parent, idx) = self.index_in_parent(node);
        if self.container_axis(parent) == Some(axis)
            && let Some((t, sibling)) = self.shown_neighbour(parent, idx, forward)
        {
            if matches!(self.node(sibling).kind, NodeKind::Window { .. }) {
                self.swap_siblings(parent, idx, t);
            } else {
                let at = self.facing_edge(sibling, axis, forward);
                self.relocate(node, sibling, at);
            }
            self.focus_window(window);
            return true;
        }

        let mut child = parent;
        while !matches!(self.node(child).kind, NodeKind::Workspace { .. }) {
            let (ancestor, idx) = self.index_in_parent(child);
            if self.container_axis(ancestor) == Some(axis) {
                self.relocate(node, ancestor, if forward { idx + 1 } else { idx });
                self.focus_window(window);
                return true;
            }
            child = ancestor;
        }

        let ws = child;
        if self.move_to_adjacent_monitor(node, direction) {
            self.focus_window(window);
            return true;
        }
        if self.container_axis(ws) != Some(axis) && self.node(ws).children.len() > 1 {
            self.reorient_workspace(ws, axis);
            let at = if forward {
                self.node(ws).children.len()
            } else {
                0
            };
            self.relocate(node, ws, at);
            self.focus_window(window);
            return true;
        }
        false
    }

    /// Moves a window node into the active workspace of the next monitor in
    /// `direction`, at the edge facing where it came from.
    fn move_to_adjacent_monitor(&mut self, node: NodeId, direction: Direction) -> bool {
        let ws = self.workspace_of(node).expect("window outside a workspace");
        let monitor = self.monitor_of(ws);
        let Some(target) = self
            .adjacent_monitor(monitor, direction)
            .and_then(|m| self.active_workspace(m))
        else {
            return false;
        };
        let forward = matches!(direction, Direction::Right | Direction::Down);
        self.unlink_window(node);
        let at = self.facing_edge(target, direction.axis(), forward);
        self.insert_child(target, at, node);
        true
    }

    /// Where to insert into `container` so a window arriving along `axis`
    /// lands on the side it came from. Containers on the other axis get it
    /// appended.
    fn facing_edge(&self, container: NodeId, axis: Axis, forward: bool) -> usize {
        let len = self.node(container).children.len();
        if self.container_axis(container) == Some(axis) && forward {
            0
        } else {
            len
        }
    }

    /// Moves a window node to `index` in `new_parent`, then tidies the
    /// container it left. `new_parent` must not be its current parent.
    fn relocate(&mut self, node: NodeId, new_parent: NodeId, index: usize) {
        let (old_parent, _) = self.index_in_parent(node);
        debug_assert_ne!(old_parent, new_parent);
        self.detach(node);
        self.insert_child(new_parent, index, node);
        self.normalize(old_parent);
    }

    /// Swaps two children's positions; each slot keeps its size.
    fn swap_siblings(&mut self, parent: NodeId, a: usize, b: usize) {
        let children = &mut self.node_mut(parent).children;
        children.swap(a, b);
        let (na, nb) = (children[a], children[b]);
        let wa = self.node(na).weight;
        self.node_mut(na).weight = self.node(nb).weight;
        self.node_mut(nb).weight = wa;
    }

    /// Wraps everything in a workspace into one split keeping the current
    /// orientation, and lays the workspace out along `axis` instead.
    fn reorient_workspace(&mut self, ws: NodeId, axis: Axis) {
        let old_axis = self.container_axis(ws).expect("workspace has an axis");
        let children = std::mem::take(&mut self.node_mut(ws).children);
        let split = self.alloc(NodeKind::Split { axis: old_axis });
        for &c in &children {
            self.node_mut(c).parent = Some(split);
        }
        let s = self.node_mut(split);
        s.parent = Some(ws);
        s.children = children;
        self.node_mut(ws).children = vec![split];
        self.set_container_axis(ws, axis);
    }

    /// Records that `window` has focus, e.g. because the OS reported it.
    pub fn focus_window(&mut self, window: WindowId) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        let ws = self.workspace_of(node).expect("window outside a workspace");
        self.set_workspace_focus(ws, Some(node));
        self.focus_workspace(ws);
        true
    }

    /// Makes `window` its workspace's remembered focus without showing or
    /// focusing that workspace.
    pub fn remember_focus(&mut self, window: WindowId) -> bool {
        let Some(node) = self.window_node(window) else {
            return false;
        };
        let ws = self.workspace_of(node).expect("window outside a workspace");
        self.set_workspace_focus(ws, Some(node));
        true
    }

    /// Also ends fullscreen if focus moves to a different window.
    fn set_workspace_focus(&mut self, ws: NodeId, node: Option<NodeId>) {
        if let NodeKind::Workspace {
            focus, fullscreen, ..
        } = &mut self.node_mut(ws).kind
        {
            *focus = node;
            if fullscreen.is_some() && *fullscreen != node {
                *fullscreen = None;
            }
        }
    }

    /// i3-style split: the next window inserted next to `window` will be
    /// placed along `axis`. If `window` is its container's only child, the
    /// container is simply re-oriented; otherwise it's wrapped in a new split.
    pub fn split(&mut self, window: WindowId, axis: Axis) -> bool {
        let Some(node) = self
            .window_node(window)
            .filter(|&n| !self.node_is_floating(n))
        else {
            return false;
        };
        let (parent, idx) = self.index_in_parent(node);
        if self.node(parent).children.len() == 1 {
            self.set_container_axis(parent, axis);
            return true;
        }
        let weight = self.node(node).weight;
        let split = self.alloc(NodeKind::Split { axis });
        {
            let s = self.node_mut(split);
            s.parent = Some(parent);
            s.weight = weight;
            s.children = vec![node];
        }
        self.node_mut(parent).children[idx] = split;
        let n = self.node_mut(node);
        n.parent = Some(split);
        n.weight = 1.0;
        true
    }

    /// Splits `window` along the opposite axis to its current container, so
    /// one key alternates between side-by-side and stacked.
    /// Hyprland-style `togglesplit`: flips the container holding `window`
    /// between side-by-side and stacked, right away.
    pub fn flip_split(&mut self, window: WindowId) -> bool {
        let Some(node) = self
            .window_node(window)
            .filter(|&n| !self.node_is_floating(n))
        else {
            return false;
        };
        let parent = self.node(node).parent.expect("window has no parent");
        let axis = self.container_axis(parent).expect("parent is a container");
        self.set_container_axis(parent, axis.flip());
        true
    }

    pub fn toggle_split(&mut self, window: WindowId) -> bool {
        let Some(node) = self
            .window_node(window)
            .filter(|&n| !self.node_is_floating(n))
        else {
            return false;
        };
        let parent = self.node(node).parent.expect("window has no parent");
        let axis = self
            .container_axis(parent)
            .expect("window parent is not a container");
        self.split(window, axis.flip())
    }

    /// Grows (positive `delta`) or shrinks the window along `axis`, by taking
    /// share from its siblings in the nearest container laid out on that axis.
    /// `delta` is a fraction of that container, e.g. `0.05` for 5%.
    pub fn resize(&mut self, window: WindowId, axis: Axis, delta: f64) -> bool {
        let Some(mut child) = self
            .window_node(window)
            .filter(|&n| !self.node_is_floating(n))
        else {
            return false;
        };
        loop {
            let Some(parent) = self.node(child).parent else {
                return false;
            };
            let siblings = self.node(parent).children.len();
            match self.container_axis(parent) {
                Some(a) if a == axis && siblings > 1 => {
                    self.adjust_weight(child, delta);
                    return true;
                }
                Some(_) if !matches!(self.node(parent).kind, NodeKind::Workspace { .. }) => {
                    child = parent;
                }
                _ => return false,
            }
        }
    }

    /// The nearest sibling of the child at `idx` in `parent`, after it
    /// (`forward`) or before it, that isn't hidden by being minimized.
    fn shown_neighbour(
        &self,
        parent: NodeId,
        idx: usize,
        forward: bool,
    ) -> Option<(usize, NodeId)> {
        let children = &self.node(parent).children;
        let mut candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(idx + 1..children.len())
        } else {
            Box::new((0..idx).rev())
        };
        candidates.find_map(|i| self.is_shown(children[i]).then_some((i, children[i])))
    }

    /// Records the size of every split and window in a workspace, so a
    /// series of resizes can each start from the same place (see
    /// [`Self::restore_weights`]).
    pub fn save_weights(&self, ws: NodeId) -> SavedWeights {
        let mut saved = Vec::new();
        let mut stack = vec![ws];
        while let Some(node) = stack.pop() {
            for &child in &self.node(node).children {
                saved.push((child, self.node(child).weight));
                stack.push(child);
            }
        }
        SavedWeights(saved)
    }

    /// Puts sizes back as [`Self::save_weights`] found them. Nodes that have
    /// since gone are skipped; arrange copes with weights that no longer
    /// sum to one.
    pub fn restore_weights(&mut self, saved: &SavedWeights) {
        for &(node, weight) in &saved.0 {
            if let Some(Some(n)) = self.nodes.get_mut(node.0 as usize) {
                n.weight = weight;
            }
        }
    }

    /// Moves one edge of a tiled window by `grow` pixels, like dragging a
    /// normal window's edge: positive grows the window outwards on `side`,
    /// negative shrinks it. Only the boundary at that edge moves, so only
    /// the neighbour across it gives up or gains space. If the window shares
    /// that edge with its whole split (e.g. the right edge of a stacked
    /// column), the split's boundary moves instead.
    ///
    /// Returns `false` if nothing lies across that edge (it's the monitor's).
    pub fn resize_edge(
        &mut self,
        window: WindowId,
        side: Direction,
        grow: i32,
        gaps: Gaps,
    ) -> bool {
        let Some(node) = self
            .window_node(window)
            .filter(|&n| !self.node_is_floating(n))
        else {
            return false;
        };
        if grow == 0 {
            return false;
        }
        let ws = self.workspace_of(node).expect("window outside a workspace");
        let rects = self.node_rects(ws, gaps);
        let axis = side.axis();
        let forward = matches!(side, Direction::Right | Direction::Down);

        let mut child = node;
        while child != ws {
            let (parent, idx) = self.index_in_parent(child);
            if self.container_axis(parent) == Some(axis)
                && let Some((_, neighbour)) = self.shown_neighbour(parent, idx, forward)
            {
                // Minimized siblings take no space, so leave them out.
                let children: Vec<NodeId> = self
                    .node(parent)
                    .children
                    .iter()
                    .copied()
                    .filter(|&c| self.is_shown(c))
                    .collect();
                let rect = rects[&parent];
                let along = match axis {
                    Axis::Horizontal => rect.width,
                    Axis::Vertical => rect.height,
                };
                let space = along - gaps.inner * (children.len() as i32 - 1);
                if space <= 0 {
                    return false;
                }
                let total: f64 = children.iter().map(|&c| self.node(c).weight).sum();
                let (mine, theirs) = (self.node(child).weight, self.node(neighbour).weight);
                // Neither side may shrink below the minimum. The bounds
                // always straddle zero, which `clamp` requires.
                let lowest = (MIN_WEIGHT * total - mine).min(0.0);
                let highest = (theirs - MIN_WEIGHT * total).max(0.0);
                let delta = (grow as f64 / space as f64 * total).clamp(lowest, highest);
                self.node_mut(child).weight += delta;
                self.node_mut(neighbour).weight -= delta;
                return true;
            }
            child = parent;
        }
        false
    }

    fn adjust_weight(&mut self, child: NodeId, delta: f64) {
        let parent = self.node(child).parent.expect("child without parent");
        let n = self.node(parent).children.len() as f64;
        let old = self.node(child).weight;
        let new = (old + delta).clamp(MIN_WEIGHT, 1.0 - MIN_WEIGHT * (n - 1.0));
        let scale = (1.0 - new) / (1.0 - old);
        for sibling in self.node(parent).children.clone() {
            let w = &mut self.node_mut(sibling).weight;
            *w = if sibling == child { new } else { *w * scale };
        }
    }

    // ---- Geometry ----------------------------------------------------------

    /// Computes where every tiled window in a workspace should go. A
    /// fullscreen one gets the monitor's bounds; the rest keep their tiles.
    pub fn arrange(&self, ws: NodeId, gaps: Gaps) -> Vec<(WindowId, Rect)> {
        let monitor = self.monitor_of(ws);
        let mut out = Vec::new();
        let area = self.monitor_work_area(monitor).inset(gaps.outer);
        self.arrange_node(ws, area, gaps.inner, &mut out);
        if let Some(fullscreen) = self.fullscreen_window(ws) {
            for (id, rect) in &mut out {
                if *id == fullscreen {
                    *rect = self.monitor_bounds(monitor);
                }
            }
        }
        out
    }

    fn monitor_bounds(&self, monitor: NodeId) -> Rect {
        match self.node(monitor).kind {
            NodeKind::Monitor { bounds, .. } => bounds,
            _ => panic!("{monitor:?} is not a monitor"),
        }
    }

    fn arrange_node(&self, node: NodeId, rect: Rect, inner: i32, out: &mut Vec<(WindowId, Rect)>) {
        self.layout_node(node, rect, inner, &mut |n, r| {
            if let NodeKind::Window { id, .. } = self.node(n).kind {
                out.push((id, r));
            }
        });
    }

    /// Rects of every node in a workspace's tiling tree, containers
    /// included (no fullscreen override).
    fn node_rects(&self, ws: NodeId, gaps: Gaps) -> HashMap<NodeId, Rect> {
        let area = self
            .monitor_work_area(self.monitor_of(ws))
            .inset(gaps.outer);
        let mut rects = HashMap::new();
        self.layout_node(ws, area, gaps.inner, &mut |n, r| {
            rects.insert(n, r);
        });
        rects
    }

    /// Lays out `node` in `rect`, calling `visit` for it and everything
    /// below it.
    fn layout_node(
        &self,
        node: NodeId,
        rect: Rect,
        inner: i32,
        visit: &mut impl FnMut(NodeId, Rect),
    ) {
        visit(node, rect);
        let Some(axis) = self.container_axis(node) else {
            return;
        };
        // Minimized windows (and splits holding only minimized windows)
        // take no space; the rest share it.
        let children: Vec<NodeId> = self
            .node(node)
            .children
            .iter()
            .copied()
            .filter(|&c| self.is_shown(c))
            .collect();
        let count = children.len() as i32;
        if count == 0 {
            return;
        }
        let along = match axis {
            Axis::Horizontal => rect.width,
            Axis::Vertical => rect.height,
        };
        let space = (along - inner * (count - 1)).max(0);
        let total_weight: f64 = children.iter().map(|&c| self.node(c).weight).sum();

        // Place edges at rounded cumulative positions so rounding never
        // leaves stray pixels and the last child ends exactly at the edge.
        let mut cumulative = 0.0;
        let mut start = 0;
        for (i, &child) in children.iter().enumerate() {
            cumulative += self.node(child).weight;
            let end = if i as i32 == count - 1 {
                space
            } else {
                (cumulative / total_weight * space as f64).round() as i32
            };
            let offset = start + inner * i as i32;
            let child_rect = match axis {
                Axis::Horizontal => Rect::new(rect.x + offset, rect.y, end - start, rect.height),
                Axis::Vertical => Rect::new(rect.x, rect.y + offset, rect.width, end - start),
            };
            self.layout_node(child, child_rect, inner, visit);
            start = end;
        }
    }

    /// Moves focus to the nearest window in `direction`, judged by on-screen
    /// position rather than tree structure so it behaves the same under every
    /// layout.
    ///
    /// If nothing lies that way in the focused workspace, focus crosses to
    /// the active workspace of the next monitor in that direction, landing on
    /// its window nearest the one we came from. Crossing to a monitor with no
    /// windows still focuses its workspace (so new windows open there) but
    /// returns `None`.
    pub fn focus_in_direction(&mut self, direction: Direction) -> Option<WindowId> {
        let ws = self.focused_workspace?;
        let monitor = self.monitor_of(ws);
        let rects = self.arrange(ws, Gaps::default());
        let current = self.workspace_focus(ws).map(|n| self.window_id(n));

        // Start from the focused window (tiled or floating), or the whole
        // monitor if nothing is focused. Only tiled windows are targets.
        let from = current
            .and_then(|c| {
                rects
                    .iter()
                    .find(|(id, _)| *id == c)
                    .map(|&(_, r)| r)
                    // A floating window usually overlaps tiles on every
                    // side, so start from its centre point instead.
                    .or_else(|| {
                        let (x, y) = self.float_rect(c)?.center();
                        Some(Rect::new(x, y, 0, 0))
                    })
            })
            .unwrap_or_else(|| self.monitor_work_area(monitor));

        if let Some(current) = current {
            let others = rects.iter().copied().filter(|&(id, _)| id != current);
            if let Some(target) = neighbour(from, others, direction) {
                self.focus_window(target);
                return Some(target);
            }
        }

        let next_ws = self
            .adjacent_monitor(monitor, direction)
            .and_then(|m| self.active_workspace(m))?;
        self.focus_workspace(next_ws);

        // Odd monitor offsets can leave no window strictly in `direction`;
        // fall back to the workspace's last focus, then its first window.
        let target = neighbour(from, self.arrange(next_ws, Gaps::default()), direction)
            .or_else(|| self.workspace_focus(next_ws).map(|n| self.window_id(n)))
            .or_else(|| self.workspace_windows(next_ws).first().copied())?;
        self.focus_window(target);
        Some(target)
    }

    /// The nearest other monitor in `direction`.
    fn adjacent_monitor(&self, monitor: NodeId, direction: Direction) -> Option<NodeId> {
        let others = self.node(self.root).children.iter().copied();
        let others = others
            .filter(|&m| m != monitor)
            .map(|m| (m, self.monitor_work_area(m)));
        neighbour(self.monitor_work_area(monitor), others, direction)
    }

    pub fn active_workspace(&self, monitor: NodeId) -> Option<NodeId> {
        match self.node(monitor).kind {
            NodeKind::Monitor {
                active_workspace, ..
            } => active_workspace,
            _ => panic!("{monitor:?} is not a monitor"),
        }
    }

    pub fn monitor_work_area(&self, monitor: NodeId) -> Rect {
        match self.node(monitor).kind {
            NodeKind::Monitor { work_area, .. } => work_area,
            _ => panic!("{monitor:?} is not a monitor"),
        }
    }

    // ---- Structural helpers ------------------------------------------------

    fn alloc(&mut self, kind: NodeKind) -> NodeId {
        let node = Node {
            parent: None,
            children: Vec::new(),
            weight: 1.0,
            kind,
        };
        match self.free.pop() {
            Some(i) => {
                self.nodes[i as usize] = Some(node);
                NodeId(i)
            }
            None => {
                self.nodes.push(Some(node));
                NodeId(self.nodes.len() as u32 - 1)
            }
        }
    }

    fn release(&mut self, id: NodeId) {
        self.nodes[id.0 as usize] = None;
        self.free.push(id.0);
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes[id.0 as usize]
            .as_mut()
            .expect("NodeId refers to a freed node")
    }

    fn set_container_axis(&mut self, id: NodeId, new_axis: Axis) {
        match &mut self.node_mut(id).kind {
            NodeKind::Workspace { axis, .. } | NodeKind::Split { axis } => *axis = new_axis,
            _ => panic!("{id:?} is not a container"),
        }
    }

    /// Inserts `child` into `parent` at `index`, giving it an equal share and
    /// shrinking existing siblings proportionally.
    pub(crate) fn insert_child(&mut self, parent: NodeId, index: usize, child: NodeId) {
        let n = self.node(parent).children.len() as f64;
        let share = 1.0 / (n + 1.0);
        for sibling in self.node(parent).children.clone() {
            self.node_mut(sibling).weight *= 1.0 - share;
        }
        let c = self.node_mut(child);
        c.parent = Some(parent);
        c.weight = if n == 0.0 { 1.0 } else { share };
        self.node_mut(parent).children.insert(index, child);
    }

    /// Unlinks `child` from its parent and re-normalizes sibling weights.
    fn detach(&mut self, child: NodeId) {
        let (parent, idx) = self.index_in_parent(child);
        self.node_mut(parent).children.remove(idx);
        self.node_mut(child).parent = None;
        let siblings = self.node(parent).children.clone();
        let total: f64 = siblings.iter().map(|&s| self.node(s).weight).sum();
        if total > 0.0 {
            for s in siblings {
                self.node_mut(s).weight /= total;
            }
        }
    }

    /// Removes pointless containers after a removal:
    /// - empty splits are deleted,
    /// - a split with one child is replaced by that child,
    /// - a split laid out on the same axis as its parent is flattened into it,
    /// - a workspace whose only child is a split adopts that split's children.
    fn normalize(&mut self, id: NodeId) {
        match self.node(id).kind {
            NodeKind::Split { axis } => {
                let children = self.node(id).children.clone();
                let parent = self.node(id).parent.expect("split without parent");
                match children.len() {
                    0 => {
                        self.detach(id);
                        self.release(id);
                        self.normalize(parent);
                    }
                    1 => {
                        let child = children[0];
                        self.replace_with_children(id);
                        if matches!(self.node(child).kind, NodeKind::Split { .. }) {
                            self.normalize(child);
                        } else {
                            self.normalize(parent);
                        }
                    }
                    _ if self.container_axis(parent) == Some(axis) => {
                        self.replace_with_children(id);
                        self.normalize(parent);
                    }
                    _ => {}
                }
            }
            NodeKind::Workspace { .. } => {
                let children = &self.node(id).children;
                if let [only] = children[..]
                    && let NodeKind::Split { axis } = self.node(only).kind
                {
                    self.set_container_axis(id, axis);
                    self.replace_with_children(only);
                }
            }
            _ => {}
        }
    }

    /// Splices a split's children into its parent at the split's position,
    /// scaling their weights by the split's own weight, then frees the split.
    fn replace_with_children(&mut self, split: NodeId) {
        let (parent, idx) = self.index_in_parent(split);
        let weight = self.node(split).weight;
        let children = std::mem::take(&mut self.node_mut(split).children);
        for &c in &children {
            let node = self.node_mut(c);
            node.parent = Some(parent);
            node.weight *= weight;
        }
        self.node_mut(parent).children.splice(idx..=idx, children);
        self.release(split);
    }

    /// Renders a workspace's shape compactly for tests and debugging, e.g.
    /// `H[1 V[2 3]]` where numbers are window ids.
    pub fn debug_layout(&self, node: NodeId) -> String {
        let n = self.node(node);
        match n.kind {
            NodeKind::Window { id, .. } => id.0.to_string(),
            _ => {
                let axis = match self.container_axis(node) {
                    Some(Axis::Horizontal) => "H",
                    Some(Axis::Vertical) => "V",
                    None => "?",
                };
                let inner: Vec<_> = n.children.iter().map(|&c| self.debug_layout(c)).collect();
                let mut out = format!("{axis}[{}]", inner.join(" "));
                let floating = self.floating_nodes(node);
                if !floating.is_empty() {
                    let ids: Vec<_> = floating.iter().map(|&f| self.debug_layout(f)).collect();
                    out += &format!(" F[{}]", ids.join(" "));
                }
                out
            }
        }
    }
}

/// Picks the candidate whose rect is the best neighbour of `from` in
/// `direction`: it must lie entirely on that side; candidates that overlap
/// `from` on the perpendicular axis win, then the nearest, then the one best
/// aligned with `from`'s centre. Ties go to the earliest candidate.
fn neighbour<T: Copy>(
    from: Rect,
    candidates: impl IntoIterator<Item = (T, Rect)>,
    direction: Direction,
) -> Option<T> {
    let (cx, cy) = from.center();
    candidates
        .into_iter()
        .filter_map(|(id, r)| {
            let (distance, overlaps, (rx, ry)) = match direction {
                Direction::Left => (
                    from.x - r.right(),
                    overlaps(from.y, from.bottom(), r.y, r.bottom()),
                    r.center(),
                ),
                Direction::Right => (
                    r.x - from.right(),
                    overlaps(from.y, from.bottom(), r.y, r.bottom()),
                    r.center(),
                ),
                Direction::Up => (
                    from.y - r.bottom(),
                    overlaps(from.x, from.right(), r.x, r.right()),
                    r.center(),
                ),
                Direction::Down => (
                    r.y - from.bottom(),
                    overlaps(from.x, from.right(), r.x, r.right()),
                    r.center(),
                ),
            };
            let misalignment = match direction.axis() {
                Axis::Horizontal => (ry - cy).abs(),
                Axis::Vertical => (rx - cx).abs(),
            };
            (distance >= 0).then_some(((!overlaps, distance, misalignment), id))
        })
        .min_by_key(|&(score, _)| score)
        .map(|(_, id)| id)
}

/// Moves `r` from area `from` to area `to`, keeping its offset from the
/// top-left corner but nudging it back inside if `to` is smaller.
fn translate_into(r: Rect, from: Rect, to: Rect) -> Rect {
    let x = to.x + (r.x - from.x).clamp(0, (to.width - r.width).max(0));
    let y = to.y + (r.y - from.y).clamp(0, (to.height - r.height).max(0));
    Rect::new(x, y, r.width, r.height)
}

/// Numeric names sort numerically ("2" < "10") and before other names.
fn workspace_sort_key(name: &str) -> (u64, String) {
    (name.parse().unwrap_or(u64::MAX), name.to_owned())
}

fn overlaps(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: isize) -> WindowId {
        WindowId(n)
    }

    /// A monitor with a 40px taskbar along the bottom of `bounds`.
    fn spec(id: isize, name: &str, bounds: Rect) -> MonitorSpec {
        MonitorSpec {
            id: MonitorId(id),
            name: name.into(),
            bounds,
            work_area: Rect::new(bounds.x, bounds.y, bounds.width, bounds.height - 40),
        }
    }

    fn left_spec() -> MonitorSpec {
        spec(1, "L", Rect::new(0, 0, 1000, 540))
    }

    /// Offset upwards, like a real mismatched desk setup.
    fn right_spec() -> MonitorSpec {
        spec(2, "R", Rect::new(1000, -30, 800, 520))
    }

    /// A tree with one 1000x500 monitor and one manual workspace.
    fn setup() -> (Tree, NodeId) {
        let mut tree = Tree::new();
        let mon = tree.add_monitor(&left_spec());
        let ws = tree.add_workspace(mon, "1", Layout::Manual);
        (tree, ws)
    }

    /// Inserts windows one after another, focusing each as the OS would.
    fn open(tree: &mut Tree, ws: NodeId, ids: &[isize]) {
        for &id in ids {
            tree.insert_window(ws, w(id));
            tree.focus_window(w(id));
        }
    }

    fn weights_sum_to_one(tree: &Tree, node: NodeId) {
        let children = &tree.node(node).children;
        if !children.is_empty() {
            let sum: f64 = children.iter().map(|&c| tree.node(c).weight).sum();
            assert!(
                (sum - 1.0).abs() < 1e-9,
                "weights under {node:?} sum to {sum}"
            );
        }
        for &c in children {
            weights_sum_to_one(tree, c);
        }
    }

    #[test]
    fn windows_tile_side_by_side() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert_eq!(tree.debug_layout(ws), "H[1 2]");
        assert_eq!(
            tree.arrange(ws, Gaps::default()),
            vec![
                (w(1), Rect::new(0, 0, 500, 500)),
                (w(2), Rect::new(500, 0, 500, 500))
            ]
        );
    }

    #[test]
    fn new_window_goes_after_focused() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.focus_window(w(1));
        open(&mut tree, ws, &[3]);
        assert_eq!(tree.debug_layout(ws), "H[1 3 2]");
    }

    #[test]
    fn split_wraps_focused_window() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 3]]");
        assert_eq!(
            tree.arrange(ws, Gaps::default()),
            vec![
                (w(1), Rect::new(0, 0, 500, 500)),
                (w(2), Rect::new(500, 0, 500, 250)),
                (w(3), Rect::new(500, 250, 500, 250)),
            ]
        );
    }

    #[test]
    fn toggle_split_alternates_axis() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.toggle_split(w(2));
        open(&mut tree, ws, &[3]);
        tree.toggle_split(w(3));
        open(&mut tree, ws, &[4]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 H[3 4]]]");
    }

    #[test]
    fn split_of_only_child_reorients_container() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1]);
        tree.split(w(1), Axis::Vertical);
        open(&mut tree, ws, &[2]);
        assert_eq!(tree.debug_layout(ws), "V[1 2]");
    }

    #[test]
    fn removing_collapses_single_child_split() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        tree.remove_window(w(3));
        assert_eq!(tree.debug_layout(ws), "H[1 2]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn removing_flattens_same_axis_nesting() {
        // H[1 V[2 H[3 4]]] → remove 2 → V has one child H[3 4], which is
        // hoisted into the outer H and flattened: H[1 3 4].
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        tree.split(w(3), Axis::Horizontal);
        open(&mut tree, ws, &[4]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 H[3 4]]]");
        tree.remove_window(w(2));
        assert_eq!(tree.debug_layout(ws), "H[1 3 4]");
        weights_sum_to_one(&tree, ws);
        // 3 and 4 keep their half of the screen between them.
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[1], (w(3), Rect::new(500, 0, 250, 500)));
    }

    #[test]
    fn workspace_adopts_orientation_of_lone_split() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        tree.remove_window(w(1));
        assert_eq!(tree.debug_layout(ws), "V[2 3]");
    }

    #[test]
    fn focus_moves_to_sibling_on_remove() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        tree.focus_window(w(2));
        tree.remove_window(w(2));
        assert_eq!(tree.focused_window(), Some(w(3)));
        tree.remove_window(w(3));
        assert_eq!(tree.focused_window(), Some(w(1)));
        tree.remove_window(w(1));
        assert_eq!(tree.focused_window(), None);
        assert_eq!(tree.debug_layout(ws), "H[]");
    }

    #[test]
    fn removing_unfocused_window_keeps_focus() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        tree.remove_window(w(1));
        assert_eq!(tree.focused_window(), Some(w(3)));
    }

    #[test]
    fn gaps_are_applied() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        let gaps = Gaps {
            inner: 10,
            outer: 5,
        };
        assert_eq!(
            tree.arrange(ws, gaps),
            vec![
                (w(1), Rect::new(5, 5, 490, 490)),
                (w(2), Rect::new(505, 5, 490, 490))
            ]
        );
    }

    #[test]
    fn arrange_fills_space_exactly_with_uneven_division() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        let rects = tree.arrange(ws, Gaps::default());
        let widths: Vec<i32> = rects.iter().map(|(_, r)| r.width).collect();
        assert_eq!(widths.iter().sum::<i32>(), 1000);
        assert_eq!(rects[2].1.right(), 1000);
    }

    #[test]
    fn directional_focus() {
        // H[1 V[2 3]]
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);

        assert_eq!(tree.focus_in_direction(Direction::Up), Some(w(2)));
        assert_eq!(tree.focus_in_direction(Direction::Left), Some(w(1)));
        assert_eq!(tree.focus_in_direction(Direction::Left), None);
        // From 1, windows 2 and 3 tie on overlap, distance and alignment, so
        // the first in tree order wins.
        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(2)));
        assert_eq!(tree.focus_in_direction(Direction::Down), Some(w(3)));
        assert_eq!(tree.focused_window(), Some(w(3)));
    }

    /// Two monitors side by side.
    fn setup_dual() -> (Tree, NodeId, NodeId) {
        let mut tree = Tree::new();
        let left = tree.add_monitor(&left_spec());
        let right = tree.add_monitor(&right_spec());
        let ws1 = tree.add_workspace(left, "1", Layout::Manual);
        let ws2 = tree.add_workspace(right, "2", Layout::Manual);
        (tree, ws1, ws2)
    }

    fn names(tree: &Tree, monitor: NodeId) -> Vec<&str> {
        let children = &tree.node(monitor).children;
        children.iter().map(|&ws| tree.workspace_name(ws)).collect()
    }

    #[test]
    fn unchanged_monitors_are_a_no_op() {
        let (mut tree, ..) = setup_dual();
        assert!(!tree.sync_monitors(&[left_spec(), right_spec()]));
        assert!(!tree.sync_monitors(&[]));
        assert_eq!(tree.monitors().count(), 2);
    }

    #[test]
    fn unplugged_monitor_hands_workspaces_to_primary_and_reclaims_them() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[2, 3]);
        tree.insert_floating(ws2, w(4), Rect::new(1100, 20, 100, 100));
        tree.add_workspace(tree.monitor_of(ws2), "5", Layout::Manual);

        // Unplug the right monitor while its workspace has focus.
        assert!(tree.sync_monitors(&[left_spec()]));
        let left = tree.monitors().next().unwrap();
        assert_eq!(tree.monitors().count(), 1);
        assert_eq!(names(&tree, left), ["1", "2"], "empty \"5\" is dropped");
        assert!(tree.is_workspace_active(ws1));
        assert!(!tree.is_workspace_active(ws2));
        assert_eq!(tree.focused_workspace(), Some(ws1));
        assert_eq!(tree.debug_layout(ws2), "H[2 3] F[4]");
        // The float keeps its offset within the monitor.
        assert_eq!(tree.float_rect(w(4)), Some(Rect::new(100, 50, 100, 100)));

        // Plug it back in, with a new HMONITOR as Windows may hand out.
        let mut back = right_spec();
        back.id = MonitorId(22);
        assert!(tree.sync_monitors(&[left_spec(), back]));
        let right = tree.monitor_by_id(MonitorId(22)).unwrap();
        assert_eq!(names(&tree, left), ["1"]);
        assert_eq!(names(&tree, right), ["2"]);
        assert!(tree.is_workspace_active(ws2));
        assert_eq!(tree.float_rect(w(4)), Some(Rect::new(1100, 20, 100, 100)));
    }

    #[test]
    fn new_monitor_gets_lowest_free_workspace_number() {
        let (mut tree, ..) = setup_dual();
        let third = spec(3, "T", Rect::new(-800, 0, 800, 540));
        assert!(tree.sync_monitors(&[left_spec(), right_spec(), third]));
        let t = tree.monitor_by_id(MonitorId(3)).unwrap();
        assert_eq!(names(&tree, t), ["3"]);
        let ws = tree.active_workspace(t).unwrap();
        assert_eq!(tree.workspace_name(ws), "3");
    }

    #[test]
    fn unplugging_the_primary_falls_back_to_whatever_remains() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1]);
        assert!(tree.sync_monitors(&[right_spec()]));
        let right = tree.monitors().next().unwrap();
        assert_eq!(names(&tree, right), ["1", "2"]);
        assert!(tree.is_workspace_active(ws2));
        assert_eq!(tree.focused_workspace(), Some(ws2));
    }

    #[test]
    fn resized_monitor_keeps_workspaces_and_moves_floats() {
        let (mut tree, ws1, _) = setup_dual();
        tree.insert_floating(ws1, w(1), Rect::new(900, 0, 100, 100));
        // Narrower now, e.g. a resolution change.
        let smaller = spec(1, "L", Rect::new(0, 0, 800, 540));
        assert!(tree.sync_monitors(&[smaller, right_spec()]));
        let left = tree.monitor_of(ws1);
        assert_eq!(tree.monitor_work_area(left).width, 800);
        assert_eq!(tree.float_rect(w(1)), Some(Rect::new(700, 0, 100, 100)));
        assert!(tree.is_workspace_active(ws1));
    }

    #[test]
    fn hidden_workspace_returns_hidden_when_its_monitor_does() {
        let (mut tree, ws1, ws2) = setup_dual();
        let right = tree.monitor_of(ws2);
        open(&mut tree, ws1, &[1]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[2]);
        let ws3 = tree.add_workspace(right, "3", Layout::Manual);
        tree.focus_workspace(ws3);
        open(&mut tree, ws3, &[3]);

        tree.sync_monitors(&[left_spec()]);
        tree.sync_monitors(&[left_spec(), right_spec()]);
        let right = tree.monitor_by_id(MonitorId(2)).unwrap();
        assert_eq!(names(&tree, right), ["2", "3"]);
        // One of them is shown; the other stays hidden.
        assert_eq!(
            [ws2, ws3]
                .iter()
                .filter(|&&ws| tree.is_workspace_active(ws))
                .count(),
            1
        );
    }

    #[test]
    fn focus_crosses_to_next_monitor() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1, 2]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[3, 4]);
        tree.focus_window(w(2));

        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(3)));
        assert_eq!(tree.focused_workspace(), Some(ws2));
        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(4)));
        assert_eq!(tree.focus_in_direction(Direction::Right), None);
        assert_eq!(tree.focused_window(), Some(w(4)));

        tree.focus_window(w(3));
        assert_eq!(tree.focus_in_direction(Direction::Left), Some(w(2)));
        assert_eq!(tree.focused_workspace(), Some(ws1));
    }

    #[test]
    fn focus_crossing_picks_nearest_window_on_target_monitor() {
        // Right monitor is stacked V[3 4]; from 1 (vertically centred at
        // y=250) the lower window 4 (centre y=330) is better aligned than 3
        // (centre y=90), and both overlap 1 vertically.
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[3]);
        tree.split(w(3), Axis::Vertical);
        open(&mut tree, ws2, &[4]);
        tree.focus_window(w(1));
        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(4)));
    }

    #[test]
    fn focus_can_cross_to_and_from_empty_monitor() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1, 2]);

        // Into the empty monitor: its workspace gets focus, no window does.
        assert_eq!(tree.focus_in_direction(Direction::Right), None);
        assert_eq!(tree.focused_workspace(), Some(ws2));
        assert_eq!(tree.focused_window(), None);

        // And back out, landing on the window nearest the edge.
        assert_eq!(tree.focus_in_direction(Direction::Left), Some(w(2)));
        assert_eq!(tree.focused_workspace(), Some(ws1));
    }

    #[test]
    fn focus_stops_at_outer_edge() {
        let (mut tree, ws1, _) = setup_dual();
        open(&mut tree, ws1, &[1]);
        assert_eq!(tree.focus_in_direction(Direction::Left), None);
        assert_eq!(tree.focused_workspace(), Some(ws1));
        assert_eq!(tree.focused_window(), Some(w(1)));
    }

    #[test]
    fn move_swaps_with_neighbouring_window() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        tree.resize(w(1), Axis::Horizontal, 0.2);
        let first_slot = tree.arrange(ws, Gaps::default())[0].1;

        assert!(tree.move_in_direction(w(1), Direction::Right));
        assert_eq!(tree.debug_layout(ws), "H[2 1 3]");
        // Slots keep their size; the windows trade places.
        assert_eq!(tree.arrange(ws, Gaps::default())[0].1, first_slot);
        assert_eq!(tree.focused_window(), Some(w(1)));
    }

    #[test]
    fn move_enters_neighbouring_split() {
        // H[1 V[2 3]]: 1 moves into the V split, which is then the only
        // thing in the workspace, so the workspace adopts it.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert!(tree.move_in_direction(w(1), Direction::Right));
        assert_eq!(tree.debug_layout(ws), "V[2 3 1]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_swaps_then_leaves_nested_split() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        tree.split(w(3), Axis::Horizontal);
        open(&mut tree, ws, &[4]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 H[3 4]]]");
        // From 4, moving left swaps with 3; moving left again leaves H[..]
        // for the nearest horizontal ancestor (the workspace).
        assert!(tree.move_in_direction(w(4), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 H[4 3]]]");
        assert!(tree.move_in_direction(w(4), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[1 4 V[2 3]]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_leaves_split_beside_ancestor() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert!(tree.move_in_direction(w(3), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[1 3 2]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_at_edge_reorients_workspace() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        assert!(tree.move_in_direction(w(3), Direction::Down));
        assert_eq!(tree.debug_layout(ws), "V[H[1 2] 3]");
        // Moving back up enters the neighbouring split: a clean round trip.
        assert!(tree.move_in_direction(w(3), Direction::Up));
        assert_eq!(tree.debug_layout(ws), "H[1 2 3]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_from_split_at_edge_reorients_workspace() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert!(tree.move_in_direction(w(3), Direction::Down));
        assert_eq!(tree.debug_layout(ws), "V[H[1 2] 3]");
    }

    #[test]
    fn move_at_outer_edge_does_nothing() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(!tree.move_in_direction(w(1), Direction::Left));
        assert!(!tree.move_in_direction(w(2), Direction::Right));
        assert_eq!(tree.debug_layout(ws), "H[1 2]");
        // A lone window has nowhere to go either.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1]);
        assert!(!tree.move_in_direction(w(1), Direction::Down));
        assert_eq!(tree.debug_layout(ws), "H[1]");
    }

    #[test]
    fn move_crosses_to_next_monitor() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1, 2]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[3]);

        assert!(tree.move_in_direction(w(2), Direction::Right));
        assert_eq!(tree.debug_layout(ws1), "H[1]");
        assert_eq!(tree.debug_layout(ws2), "H[2 3]");
        assert_eq!(tree.focused_workspace(), Some(ws2));
        assert_eq!(tree.focused_window(), Some(w(2)));
        // The workspace it left still remembers a focused window.
        tree.focus_in_direction(Direction::Left);
        assert_eq!(tree.focused_window(), Some(w(1)));

        assert!(tree.move_in_direction(w(1), Direction::Right));
        assert_eq!(tree.debug_layout(ws1), "H[]");
        assert_eq!(tree.debug_layout(ws2), "H[1 2 3]");
        weights_sum_to_one(&tree, ws2);
    }

    #[test]
    fn move_into_empty_monitor_and_back() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1]);
        assert!(tree.move_in_direction(w(1), Direction::Right));
        assert_eq!(tree.debug_layout(ws2), "H[1]");
        assert!(tree.move_in_direction(w(1), Direction::Left));
        assert_eq!(tree.debug_layout(ws1), "H[1]");
        assert_eq!(tree.focused_workspace(), Some(ws1));
    }

    #[test]
    fn workspaces_are_sorted_and_found_by_name() {
        let (mut tree, ws1) = setup();
        let mon = tree.monitor_of(ws1);
        tree.add_workspace(mon, "10", Layout::Manual);
        tree.add_workspace(mon, "web", Layout::Manual);
        tree.add_workspace(mon, "2", Layout::Manual);
        let names: Vec<_> = tree
            .workspaces()
            .map(|ws| tree.workspace_name(ws))
            .collect();
        assert_eq!(names, ["1", "2", "10", "web"]);
        assert_eq!(tree.workspace_by_name("1"), Some(ws1));
        assert_eq!(tree.workspace_by_name("3"), None);
    }

    #[test]
    fn focusing_workspace_swaps_active_on_its_monitor() {
        let (mut tree, ws1) = setup();
        let mon = tree.monitor_of(ws1);
        let ws2 = tree.add_workspace(mon, "2", Layout::Manual);
        assert!(tree.is_workspace_active(ws1));
        assert!(!tree.is_workspace_active(ws2));

        assert_eq!(tree.focus_workspace(ws2), Some(ws1));
        assert!(tree.is_workspace_active(ws2));
        assert!(!tree.is_workspace_active(ws1));
        assert_eq!(tree.focused_workspace(), Some(ws2));
        // Re-focusing the active workspace replaces nothing.
        assert_eq!(tree.focus_workspace(ws2), None);
    }

    #[test]
    fn empty_inactive_workspace_can_be_removed() {
        let (mut tree, ws1) = setup();
        let mon = tree.monitor_of(ws1);
        let ws2 = tree.add_workspace(mon, "2", Layout::Manual);
        tree.remove_workspace(ws2);
        assert_eq!(tree.workspace_by_name("2"), None);
        assert_eq!(tree.workspaces().count(), 1);
    }

    #[test]
    fn move_window_to_workspace_keeps_source_focus_sensible() {
        let (mut tree, ws1) = setup();
        let mon = tree.monitor_of(ws1);
        let ws2 = tree.add_workspace(mon, "2", Layout::Manual);
        open(&mut tree, ws1, &[1, 2, 3]);
        tree.focus_window(w(2));

        assert_eq!(tree.move_window_to_workspace(w(2), ws2), Some(ws1));
        assert_eq!(tree.debug_layout(ws1), "H[1 3]");
        assert_eq!(tree.debug_layout(ws2), "H[2]");
        // Source falls back to the neighbour; target remembers the arrival.
        assert_eq!(tree.focused_window(), Some(w(3)));
        assert_eq!(tree.workspace_focused_window(ws2), Some(w(2)));

        // Arrivals go next to the target's focused window.
        tree.move_window_to_workspace(w(1), ws2);
        assert_eq!(tree.debug_layout(ws2), "H[2 1]");
        assert_eq!(tree.workspace_focused_window(ws2), Some(w(2)));

        assert_eq!(tree.move_window_to_workspace(w(1), ws2), None);
    }

    #[test]
    fn monitors_can_be_found_by_os_id() {
        let (tree, ws1, ws2) = setup_dual();
        assert_eq!(tree.monitor_by_id(MonitorId(2)), Some(tree.monitor_of(ws2)));
        assert_eq!(tree.monitor_by_id(MonitorId(1)), Some(tree.monitor_of(ws1)));
        assert_eq!(tree.monitor_by_id(MonitorId(9)), None);
    }

    #[test]
    fn minimized_window_gives_up_space_and_comes_back_in_place() {
        // H[1 V[2 3]] with 1 widened.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        tree.resize(w(1), Axis::Horizontal, 0.1);
        let before = tree.arrange(ws, Gaps::default());

        assert!(tree.set_minimized(w(2), true));
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects.len(), 2, "minimized window isn't placed");
        // 3 takes the whole right column; 1 is unchanged.
        assert_eq!(rects[0], before[0]);
        assert_eq!(rects[1], (w(3), Rect::new(600, 0, 400, 500)));
        assert!(tree.is_minimized(w(2)));
        assert_eq!(tree.workspace_windows(ws).len(), 3, "still managed");

        assert!(tree.set_minimized(w(2), false));
        assert_eq!(tree.arrange(ws, Gaps::default()), before);
        assert!(!tree.set_minimized(w(2), false), "already restored");
    }

    #[test]
    fn minimizing_moves_focus_and_ends_fullscreen() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.toggle_fullscreen(w(2));
        tree.set_minimized(w(2), true);
        assert_eq!(tree.focused_window(), Some(w(1)));
        assert_eq!(tree.fullscreen_window(ws), None);

        tree.set_minimized(w(1), true);
        assert_eq!(tree.focused_window(), None);
        assert!(tree.arrange(ws, Gaps::default()).is_empty());
    }

    #[test]
    fn layout_operations_skip_minimized_windows() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        tree.set_minimized(w(2), true);
        tree.focus_window(w(1));
        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(3)));

        // Moving 3 left swaps with 1, past the hidden 2.
        assert!(tree.move_in_direction(w(3), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[3 2 1]");

        // Dragging 3's right edge resizes 1, not the hidden 2.
        let gaps = Gaps::default();
        assert!(tree.resize_edge(w(3), Direction::Right, 100, gaps));
        let widths: Vec<i32> = tree
            .arrange(ws, gaps)
            .iter()
            .map(|(_, r)| r.width)
            .collect();
        assert_eq!(widths, [600, 400]);
    }

    #[test]
    fn new_windows_avoid_minimized_ones() {
        let (mut tree, ws) = setup();
        tree.set_workspace_layout(ws, Layout::Dwindle);
        open(&mut tree, ws, &[1, 2]);
        tree.set_minimized(w(2), true);
        // Focus fell back to 1, so the new window splits 1 (now the whole
        // screen, so side by side), not the hidden 2.
        open(&mut tree, ws, &[3]);
        assert_eq!(tree.debug_layout(ws), "H[H[1 3] 2]");

        tree.set_minimized(w(3), true);
        tree.set_minimized(w(1), true);
        // Nothing shown to split: it just goes in.
        open(&mut tree, ws, &[4]);
        assert_eq!(tree.arrange(ws, Gaps::default()).len(), 1);
    }

    #[test]
    fn minimized_floats_arent_positioned() {
        let (mut tree, ws) = setup();
        tree.insert_floating(ws, w(1), Rect::new(0, 0, 10, 10));
        tree.set_minimized(w(1), true);
        assert!(tree.floating_windows(ws).is_empty());
        tree.set_minimized(w(1), false);
        assert_eq!(tree.floating_windows(ws).len(), 1);
    }

    #[test]
    fn floating_windows_sit_outside_the_tiling_tree() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1]);
        tree.insert_floating(ws, w(2), Rect::new(10, 10, 100, 100));
        assert_eq!(tree.debug_layout(ws), "H[1] F[2]");
        assert_eq!(tree.arrange(ws, Gaps::default()).len(), 1);
        assert_eq!(tree.workspace_windows(ws), [w(1), w(2)]);
        assert_eq!(
            tree.floating_windows(ws),
            [(w(2), Rect::new(10, 10, 100, 100))]
        );
        assert!(tree.is_floating(w(2)));
        assert!(!tree.is_floating(w(1)));
    }

    #[test]
    fn toggle_floating_round_trip() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        tree.focus_window(w(2));

        assert!(tree.toggle_floating(w(2)));
        assert_eq!(tree.debug_layout(ws), "H[1 3] F[2]");
        assert_eq!(tree.focused_window(), Some(w(2)));
        // First float: centred at 60% of the 1000x500 work area.
        assert_eq!(tree.float_rect(w(2)), Some(Rect::new(200, 100, 600, 300)));
        weights_sum_to_one(&tree, ws);

        tree.set_float_rect(w(2), Rect::new(50, 60, 300, 200));
        assert!(tree.toggle_floating(w(2)));
        assert_eq!(tree.debug_layout(ws), "H[1 3 2]");
        assert_eq!(tree.focused_window(), Some(w(2)));
        weights_sum_to_one(&tree, ws);

        // Floating again returns to where it was last dragged.
        tree.toggle_floating(w(2));
        assert_eq!(tree.float_rect(w(2)), Some(Rect::new(50, 60, 300, 200)));
    }

    #[test]
    fn removing_focused_float_falls_back_to_tiles() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.insert_floating(ws, w(3), Rect::new(0, 0, 10, 10));
        tree.focus_window(w(3));
        tree.remove_window(w(3));
        assert_eq!(tree.focused_window(), Some(w(1)));

        // And a lone float is the fallback when the last tile goes.
        tree.insert_floating(ws, w(4), Rect::new(0, 0, 10, 10));
        tree.remove_window(w(1));
        tree.remove_window(w(2));
        assert_eq!(tree.focused_window(), Some(w(4)));
    }

    #[test]
    fn new_tile_appends_when_a_float_has_focus() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.focus_window(w(1));
        tree.insert_floating(ws, w(3), Rect::new(0, 0, 10, 10));
        tree.focus_window(w(3));
        open(&mut tree, ws, &[4]);
        assert_eq!(tree.debug_layout(ws), "H[1 2 4] F[3]");
    }

    #[test]
    fn tiling_operations_ignore_floats() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1]);
        tree.insert_floating(ws, w(2), Rect::new(0, 0, 10, 10));
        assert!(!tree.move_in_direction(w(2), Direction::Right));
        assert!(!tree.split(w(2), Axis::Vertical));
        assert!(!tree.toggle_split(w(2)));
        assert!(!tree.resize(w(2), Axis::Horizontal, 0.1));
        assert_eq!(tree.debug_layout(ws), "H[1] F[2]");
    }

    #[test]
    fn directional_focus_from_float_uses_its_centre() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.insert_floating(ws, w(3), Rect::new(200, 100, 600, 300));
        tree.focus_window(w(3));
        assert_eq!(tree.focus_in_direction(Direction::Right), Some(w(2)));
        tree.focus_window(w(3));
        assert_eq!(tree.focus_in_direction(Direction::Left), Some(w(1)));
    }

    #[test]
    fn floats_keep_relative_position_across_monitors() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1]);
        tree.insert_floating(ws1, w(2), Rect::new(900, 100, 200, 100));
        assert_eq!(tree.move_window_to_workspace(w(2), ws2), Some(ws1));
        assert_eq!(tree.debug_layout(ws2), "H[] F[2]");
        // x clamped so it fits the narrower monitor; y kept relative.
        assert_eq!(tree.float_rect(w(2)), Some(Rect::new(1600, 70, 200, 100)));
        assert!(tree.is_floating(w(2)));
    }

    #[test]
    fn fullscreen_covers_monitor_and_toggles_off() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(tree.toggle_fullscreen(w(1)));
        assert_eq!(tree.fullscreen_window(ws), Some(w(1)));
        assert_eq!(tree.focused_window(), Some(w(1)));
        let rects = tree.arrange(ws, Gaps { inner: 8, outer: 8 });
        // Whole monitor, taskbar included, no gaps; 2 keeps its tile.
        assert_eq!(rects[0], (w(1), Rect::new(0, 0, 1000, 540)));
        assert_eq!(rects[1].1.width, 488);

        tree.toggle_fullscreen(w(1));
        assert_eq!(tree.fullscreen_window(ws), None);
        assert_eq!(tree.arrange(ws, Gaps::default())[0].1.width, 500);
    }

    #[test]
    fn fullscreen_ends_when_focus_or_window_moves_on() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.toggle_fullscreen(w(1));
        tree.focus_window(w(2));
        assert_eq!(tree.fullscreen_window(ws), None);

        tree.toggle_fullscreen(w(2));
        tree.remove_window(w(2));
        assert_eq!(tree.fullscreen_window(ws), None);

        open(&mut tree, ws, &[3]);
        tree.toggle_fullscreen(w(3));
        tree.toggle_floating(w(3));
        assert_eq!(tree.fullscreen_window(ws), None);
    }

    #[test]
    fn floating_window_can_be_fullscreen() {
        let (mut tree, ws) = setup();
        tree.insert_floating(ws, w(1), Rect::new(10, 10, 100, 100));
        tree.toggle_fullscreen(w(1));
        assert_eq!(
            tree.floating_windows(ws),
            [(w(1), Rect::new(0, 0, 1000, 540))]
        );
        tree.toggle_fullscreen(w(1));
        assert_eq!(
            tree.floating_windows(ws),
            [(w(1), Rect::new(10, 10, 100, 100))]
        );
    }

    #[test]
    fn move_beside_along_container_axis() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        assert!(tree.move_beside(w(1), w(3), Direction::Right));
        assert_eq!(tree.debug_layout(ws), "H[2 3 1]");
        assert!(tree.move_beside(w(1), w(2), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[1 2 3]");
        assert_eq!(tree.focused_window(), Some(w(1)));
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_beside_across_axis_splits_target() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        assert!(tree.move_beside(w(3), w(1), Direction::Down));
        assert_eq!(tree.debug_layout(ws), "H[V[1 3] 2]");
        assert!(tree.move_beside(w(2), w(3), Direction::Up));
        // H[V[1 3]] collapses: the workspace takes the V orientation.
        assert_eq!(tree.debug_layout(ws), "V[1 2 3]");
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn move_beside_lone_window_reorients() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(tree.move_beside(w(2), w(1), Direction::Up));
        assert_eq!(tree.debug_layout(ws), "V[2 1]");
    }

    #[test]
    fn move_beside_across_monitors() {
        let (mut tree, ws1, ws2) = setup_dual();
        open(&mut tree, ws1, &[1, 2]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[3]);
        assert!(tree.move_beside(w(2), w(3), Direction::Left));
        assert_eq!(tree.debug_layout(ws1), "H[1]");
        assert_eq!(tree.debug_layout(ws2), "H[2 3]");
        assert_eq!(tree.focused_workspace(), Some(ws2));
        assert_eq!(tree.workspace_focused_window(ws1), Some(w(1)));
    }

    #[test]
    fn move_beside_refuses_self_and_floats() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.insert_floating(ws, w(3), Rect::new(0, 0, 10, 10));
        assert!(!tree.move_beside(w(1), w(1), Direction::Left));
        assert!(!tree.move_beside(w(3), w(1), Direction::Left));
        assert!(!tree.move_beside(w(1), w(3), Direction::Left));
        assert_eq!(tree.debug_layout(ws), "H[1 2] F[3]");
    }

    fn widths(tree: &Tree, ws: NodeId, gaps: Gaps) -> Vec<i32> {
        tree.arrange(ws, gaps)
            .iter()
            .map(|(_, r)| r.width)
            .collect()
    }

    #[test]
    fn edge_resize_only_moves_the_neighbour_across_that_edge() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        assert_eq!(widths(&tree, ws, Gaps::default()), [333, 334, 333]);

        // Drag 1's right edge 100px right: 2 shrinks, 3 doesn't move.
        assert!(tree.resize_edge(w(1), Direction::Right, 100, Gaps::default()));
        assert_eq!(widths(&tree, ws, Gaps::default()), [433, 234, 333]);

        // Drag 3's left edge 50px left: 2 shrinks again, 1 doesn't move.
        assert!(tree.resize_edge(w(3), Direction::Left, 50, Gaps::default()));
        assert_eq!(widths(&tree, ws, Gaps::default()), [433, 184, 383]);
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn edge_resize_climbs_to_the_split_that_owns_the_edge() {
        // H[1 V[2 3]]: 3's left edge is the boundary between 1 and the V.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert!(tree.resize_edge(w(3), Direction::Left, 100, Gaps::default()));
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[0].1.width, 400);
        assert_eq!(rects[1].1.width, 600);
        assert_eq!(rects[2].1.width, 600);

        // Its top edge is the boundary with 2, inside the V.
        assert!(tree.resize_edge(w(3), Direction::Up, 50, Gaps::default()));
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!((rects[1].1.height, rects[2].1.height), (200, 300));
    }

    #[test]
    fn repeated_resizes_from_saved_weights_dont_accumulate() {
        // What a live drag does: restore, then apply the total delta so far.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2, 3]);
        let saved = tree.save_weights(ws);
        for grow in [40, 120, 80, 100] {
            tree.restore_weights(&saved);
            tree.resize_edge(w(1), Direction::Right, grow, Gaps::default());
        }
        assert_eq!(widths(&tree, ws, Gaps::default()), [433, 234, 333]);

        // Dragging far past the minimum and back ends up exactly undone.
        tree.restore_weights(&saved);
        tree.resize_edge(w(1), Direction::Right, 5000, Gaps::default());
        tree.restore_weights(&saved);
        tree.resize_edge(w(1), Direction::Right, 0, Gaps::default());
        assert_eq!(widths(&tree, ws, Gaps::default()), [333, 334, 333]);
    }

    #[test]
    fn edge_resize_at_monitor_edge_does_nothing() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(!tree.resize_edge(w(1), Direction::Left, 50, Gaps::default()));
        assert!(!tree.resize_edge(w(1), Direction::Up, 50, Gaps::default()));
        assert!(!tree.resize_edge(w(2), Direction::Right, 50, Gaps::default()));
        assert_eq!(widths(&tree, ws, Gaps::default()), [500, 500]);
    }

    #[test]
    fn edge_resize_is_pixel_accurate_with_gaps_and_clamped() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        let gaps = Gaps {
            inner: 10,
            outer: 0,
        };
        assert_eq!(widths(&tree, ws, gaps), [495, 495]);
        assert!(tree.resize_edge(w(1), Direction::Right, 99, gaps));
        assert_eq!(widths(&tree, ws, gaps), [594, 396]);

        // Can't squeeze either side below 5% of the 990px they share.
        tree.resize_edge(w(1), Direction::Right, 5000, gaps);
        assert_eq!(widths(&tree, ws, gaps), [941, 49]);
        tree.resize_edge(w(1), Direction::Right, -5000, gaps);
        assert_eq!(widths(&tree, ws, gaps), [50, 940]);
    }

    #[test]
    fn resize_takes_space_from_siblings() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(tree.resize(w(1), Axis::Horizontal, 0.2));
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[0].1.width, 700);
        assert_eq!(rects[1].1.width, 300);
        weights_sum_to_one(&tree, ws);
    }

    #[test]
    fn resize_walks_up_to_matching_axis() {
        // H[1 V[2 3]]: growing 3 horizontally resizes the V split.
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws, &[3]);
        assert!(tree.resize(w(3), Axis::Horizontal, 0.1));
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[0].1.width, 400);
        assert_eq!(rects[2].1.width, 600);
    }

    #[test]
    fn resize_is_clamped() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        tree.resize(w(1), Axis::Horizontal, 5.0);
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[1].1.width, 50);
    }

    #[test]
    fn resize_without_matching_container_fails() {
        let (mut tree, ws) = setup();
        open(&mut tree, ws, &[1, 2]);
        assert!(!tree.resize(w(1), Axis::Vertical, 0.1));
    }

    #[test]
    fn node_slots_are_reused() {
        let (mut tree, ws) = setup();
        for i in 0..100 {
            open(&mut tree, ws, &[i]);
            tree.remove_window(w(i));
        }
        assert!(tree.nodes.len() < 10);
    }
}
