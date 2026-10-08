//! The window tree: Root → Monitor → Workspace → Split* → Window.
//!
//! Workspaces and splits are both *containers*: they lay their children out
//! along an [`Axis`], each child taking a share of the space proportional to
//! its `weight`. Weights of siblings always sum to 1.
//!
//! Window insertion is delegated to the workspace's [`Layout`], so dynamic
//! layouts can later shape the tree without commands needing to know.

use std::collections::HashMap;

use crate::layout::Layout;
use crate::{MonitorId, Rect, WindowId};

/// Smallest share of its container a child can be resized down to.
const MIN_WEIGHT: f64 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId(u32);

/// The direction a container lays out its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        work_area: Rect,
        active_workspace: Option<NodeId>,
    },
    Workspace {
        name: String,
        layout: Layout,
        axis: Axis,
        /// Last focused window in this workspace.
        focus: Option<NodeId>,
    },
    Split {
        axis: Axis,
    },
    Window {
        id: WindowId,
    },
}

#[derive(Debug, Clone)]
pub struct Tree {
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    root: NodeId,
    windows: HashMap<WindowId, NodeId>,
    focused_workspace: Option<NodeId>,
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

    /// All windows in a workspace, in tree order.
    pub fn workspace_windows(&self, ws: NodeId) -> Vec<WindowId> {
        let mut out = Vec::new();
        self.collect_windows(ws, &mut out);
        out
    }

    fn collect_windows(&self, node: NodeId, out: &mut Vec<WindowId>) {
        match self.node(node).kind {
            NodeKind::Window { id } => out.push(id),
            _ => {
                for &child in &self.node(node).children {
                    self.collect_windows(child, out);
                }
            }
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

    fn window_id(&self, node: NodeId) -> WindowId {
        match self.node(node).kind {
            NodeKind::Window { id } => id,
            _ => panic!("{node:?} is not a window"),
        }
    }

    fn monitor_of(&self, ws: NodeId) -> NodeId {
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

    fn first_window(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            match self.node(id).kind {
                NodeKind::Window { .. } => return Some(id),
                _ => id = *self.node(id).children.first()?,
            }
        }
    }

    fn last_window(&self, mut id: NodeId) -> Option<NodeId> {
        loop {
            match self.node(id).kind {
                NodeKind::Window { .. } => return Some(id),
                _ => id = *self.node(id).children.last()?,
            }
        }
    }

    // ---- Monitors & workspaces --------------------------------------------

    pub fn add_monitor(&mut self, id: MonitorId, work_area: Rect) -> NodeId {
        let node = self.alloc(NodeKind::Monitor {
            id,
            work_area,
            active_workspace: None,
        });
        self.insert_child(self.root, self.node(self.root).children.len(), node);
        node
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
        });
        self.node_mut(ws).parent = Some(monitor);
        self.node_mut(monitor).children.push(ws);
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(monitor).kind
        {
            active_workspace.get_or_insert(ws);
        }
        self.focused_workspace.get_or_insert(ws);
        ws
    }

    /// Makes `ws` the active workspace on its monitor and gives it focus.
    pub fn focus_workspace(&mut self, ws: NodeId) {
        let monitor = self.monitor_of(ws);
        if let NodeKind::Monitor {
            active_workspace, ..
        } = &mut self.node_mut(monitor).kind
        {
            *active_workspace = Some(ws);
        }
        self.focused_workspace = Some(ws);
    }

    // ---- Windows -----------------------------------------------------------

    /// Adds a window to a workspace. Where it goes is up to the workspace's
    /// layout. Does not change focus.
    pub fn insert_window(&mut self, ws: NodeId, window: WindowId) -> NodeId {
        assert!(
            !self.windows.contains_key(&window),
            "{window:?} is already in the tree"
        );
        let node = self.alloc(NodeKind::Window { id: window });
        self.windows.insert(window, node);
        self.layout_insert(ws, node);
        node
    }

    /// Removes a window, tidies up the tree around it and, if it was focused,
    /// moves focus to its nearest sibling. Returns its workspace.
    pub fn remove_window(&mut self, window: WindowId) -> Option<NodeId> {
        let node = self.windows.remove(&window)?;
        let ws = self.workspace_of(node).expect("window outside a workspace");
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
        self.release(node);
        self.normalize(parent);

        if let Some(next) = next_focus {
            // A window was the only child of its split: fall back to anything
            // left in the workspace.
            let next = next.or_else(|| self.first_window(ws));
            self.set_workspace_focus(ws, next);
        }
        self.layout_after_remove(ws);
        Some(ws)
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

    fn set_workspace_focus(&mut self, ws: NodeId, node: Option<NodeId>) {
        if let NodeKind::Workspace { focus, .. } = &mut self.node_mut(ws).kind {
            *focus = node;
        }
    }

    /// i3-style split: the next window inserted next to `window` will be
    /// placed along `axis`. If `window` is its container's only child, the
    /// container is simply re-oriented; otherwise it's wrapped in a new split.
    pub fn split(&mut self, window: WindowId, axis: Axis) -> bool {
        let Some(node) = self.window_node(window) else {
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
    pub fn toggle_split(&mut self, window: WindowId) -> bool {
        let Some(node) = self.window_node(window) else {
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
        let Some(mut child) = self.window_node(window) else {
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

    /// Computes where every window in a workspace should go.
    pub fn arrange(&self, ws: NodeId, gaps: Gaps) -> Vec<(WindowId, Rect)> {
        let NodeKind::Monitor { work_area, .. } = self.node(self.monitor_of(ws)).kind else {
            unreachable!("workspace parent is not a monitor");
        };
        let mut out = Vec::new();
        self.arrange_node(ws, work_area.inset(gaps.outer), gaps.inner, &mut out);
        out
    }

    fn arrange_node(&self, node: NodeId, rect: Rect, inner: i32, out: &mut Vec<(WindowId, Rect)>) {
        let n = self.node(node);
        if let NodeKind::Window { id } = n.kind {
            out.push((id, rect));
            return;
        }
        let Some(axis) = self.container_axis(node) else {
            return;
        };
        let count = n.children.len() as i32;
        if count == 0 {
            return;
        }
        let along = match axis {
            Axis::Horizontal => rect.width,
            Axis::Vertical => rect.height,
        };
        let space = (along - inner * (count - 1)).max(0);
        let total_weight: f64 = n.children.iter().map(|&c| self.node(c).weight).sum();

        // Place edges at rounded cumulative positions so rounding never
        // leaves stray pixels and the last child ends exactly at the edge.
        let mut cumulative = 0.0;
        let mut start = 0;
        for (i, &child) in n.children.iter().enumerate() {
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
            self.arrange_node(child, child_rect, inner, out);
            start = end;
        }
    }

    /// Moves focus to the nearest window in `direction` within the focused
    /// workspace, judged by on-screen position rather than tree structure so
    /// it behaves the same under every layout. Returns the new focus.
    pub fn focus_in_direction(&mut self, direction: Direction) -> Option<WindowId> {
        let ws = self.focused_workspace?;
        let current = self.window_id(self.workspace_focus(ws)?);
        let rects = self.arrange(ws, Gaps::default());
        let target = neighbour(&rects, current, direction)?;
        self.focus_window(target);
        Some(target)
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
            NodeKind::Window { id } => id.0.to_string(),
            _ => {
                let axis = match self.container_axis(node) {
                    Some(Axis::Horizontal) => "H",
                    Some(Axis::Vertical) => "V",
                    None => "?",
                };
                let inner: Vec<_> = n.children.iter().map(|&c| self.debug_layout(c)).collect();
                format!("{axis}[{}]", inner.join(" "))
            }
        }
    }
}

/// Picks the window whose rect is the best neighbour of `current` in
/// `direction`: it must lie entirely on that side; windows that overlap
/// `current` on the perpendicular axis win, then the nearest, then the one
/// best aligned with `current`'s centre.
fn neighbour(
    rects: &[(WindowId, Rect)],
    current: WindowId,
    direction: Direction,
) -> Option<WindowId> {
    let &(_, from) = rects.iter().find(|(id, _)| *id == current)?;
    let (cx, cy) = from.center();
    rects
        .iter()
        .filter(|(id, _)| *id != current)
        .filter_map(|&(id, r)| {
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

fn overlaps(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    a_start < b_end && b_start < a_end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(n: isize) -> WindowId {
        WindowId(n)
    }

    /// A tree with one 1000x500 monitor and one manual workspace.
    fn setup() -> (Tree, NodeId) {
        let mut tree = Tree::new();
        let mon = tree.add_monitor(MonitorId(1), Rect::new(0, 0, 1000, 500));
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
