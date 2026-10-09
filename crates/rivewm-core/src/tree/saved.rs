//! Saving the whole layout and rebuilding it later, so restarting rivewm
//! doesn't reshuffle your windows.

use serde::{Deserialize, Serialize};

use super::{Axis, NodeId, NodeKind, Tree};
use crate::{Layout, Rect, WindowId};

/// One workspace's layout, as saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub name: String,
    /// Name of the monitor it was on (see `MonitorSpec::name`).
    pub monitor: String,
    /// Shown on its monitor.
    pub active: bool,
    /// Had keyboard focus.
    pub focused: bool,
    pub layout: Layout,
    pub axis: Axis,
    pub tiles: Vec<SavedNode>,
    pub floating: Vec<SavedFloat>,
    /// Its remembered focused window.
    pub focus: Option<isize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SavedNode {
    Split {
        axis: Axis,
        weight: f64,
        children: Vec<SavedNode>,
    },
    Window {
        id: isize,
        weight: f64,
        minimized: bool,
        /// Where it floats if toggled to floating.
        float_rect: Option<Rect>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedFloat {
    pub id: isize,
    pub rect: Rect,
    pub minimized: bool,
}

impl Tree {
    /// Captures every workspace's layout.
    pub fn save_layout(&self) -> Vec<SavedWorkspace> {
        self.workspaces()
            .map(|ws| {
                let monitor = self.monitor_of(ws);
                let NodeKind::Workspace { layout, axis, .. } = self.node(ws).kind else {
                    unreachable!("workspaces() yields workspaces");
                };
                SavedWorkspace {
                    name: self.workspace_name(ws).to_owned(),
                    monitor: self.monitor_name(monitor).to_owned(),
                    active: self.is_workspace_active(ws),
                    focused: self.focused_workspace == Some(ws),
                    layout,
                    axis,
                    tiles: self
                        .node(ws)
                        .children
                        .iter()
                        .map(|&c| self.save_node(c))
                        .collect(),
                    floating: self
                        .floating_nodes(ws)
                        .iter()
                        .filter_map(|&n| match self.node(n).kind {
                            NodeKind::Window {
                                id,
                                float_rect: Some(rect),
                                minimized,
                                ..
                            } => Some(SavedFloat {
                                id: id.0,
                                rect,
                                minimized,
                            }),
                            _ => None,
                        })
                        .collect(),
                    focus: self.workspace_focused_window(ws).map(|w| w.0),
                }
            })
            .collect()
    }

    fn save_node(&self, node: NodeId) -> SavedNode {
        let n = self.node(node);
        match n.kind {
            NodeKind::Window {
                id,
                minimized,
                float_rect,
                ..
            } => SavedNode::Window {
                id: id.0,
                weight: n.weight,
                minimized,
                float_rect,
            },
            NodeKind::Split { axis } => SavedNode::Split {
                axis,
                weight: n.weight,
                children: n.children.iter().map(|&c| self.save_node(c)).collect(),
            },
            _ => unreachable!("only splits and windows sit under a workspace"),
        }
    }

    /// Rebuilds workspaces from [`Self::save_layout`] output, on a tree that
    /// has its monitors but no workspaces yet.
    ///
    /// `present` says whether a saved window still exists and is the same
    /// window: `None` drops it, `Some(minimized)` keeps it with its current
    /// minimized state. Splits left empty or with one child are tidied up.
    /// A workspace whose monitor is gone goes to the first monitor,
    /// remembering where it belongs. Monitors left without a workspace get a
    /// fresh one. Returns the windows that were placed.
    pub fn restore_layout(
        &mut self,
        saved: &[SavedWorkspace],
        present: impl Fn(WindowId) -> Option<bool>,
    ) -> Vec<WindowId> {
        let mut placed = Vec::new();
        let Some(first_monitor) = self.monitors().next() else {
            return placed;
        };
        let mut active = Vec::new();
        let mut focused = None;

        for saved_ws in saved {
            if self.workspace_by_name(&saved_ws.name).is_some() {
                continue;
            }
            let home = self.monitor_by_name(&saved_ws.monitor);
            let monitor = home.unwrap_or(first_monitor);
            let ws = self.add_workspace(monitor, saved_ws.name.clone(), saved_ws.layout);
            self.set_container_axis(ws, saved_ws.axis);
            if home.is_none()
                && let NodeKind::Workspace { home, .. } = &mut self.node_mut(ws).kind
            {
                *home = Some(saved_ws.monitor.clone());
            }

            for tile in &saved_ws.tiles {
                if let Some(child) = self.restore_node(tile, &present, &mut placed) {
                    self.node_mut(child).parent = Some(ws);
                    self.node_mut(ws).children.push(child);
                }
            }
            self.tidy(ws);

            for float in &saved_ws.floating {
                let id = WindowId(float.id);
                if self.windows.contains_key(&id) {
                    continue;
                }
                if let Some(minimized) = present(id) {
                    self.insert_floating(ws, id, float.rect);
                    self.set_minimized(id, minimized);
                    placed.push(id);
                }
            }

            if let Some(focus) = saved_ws.focus.map(WindowId)
                && let Some(node) = self.window_node(focus)
                && self.workspace_of(node) == Some(ws)
            {
                self.set_workspace_focus(ws, Some(node));
            }
            if saved_ws.active && home.is_some() {
                active.push(ws);
            }
            if saved_ws.focused {
                focused = Some(ws);
            }
        }

        // `add_workspace` made the first workspace on each monitor active;
        // put back the ones that really were.
        for ws in active {
            let monitor = self.monitor_of(ws);
            if let NodeKind::Monitor {
                active_workspace, ..
            } = &mut self.node_mut(monitor).kind
            {
                *active_workspace = Some(ws);
            }
        }
        for monitor in self.monitors().collect::<Vec<_>>() {
            self.ensure_active_workspace(monitor);
        }
        self.focused_workspace = focused
            .filter(|&ws| self.is_workspace_active(ws))
            .or_else(|| self.active_workspace(first_monitor));
        placed
    }

    /// Builds a saved subtree, detached, keeping only windows that are still
    /// present. Returns `None` if nothing in it survived.
    fn restore_node(
        &mut self,
        saved: &SavedNode,
        present: &impl Fn(WindowId) -> Option<bool>,
        placed: &mut Vec<WindowId>,
    ) -> Option<NodeId> {
        match saved {
            SavedNode::Window {
                id,
                weight,
                minimized: _,
                float_rect,
            } => {
                let id = WindowId(*id);
                if self.windows.contains_key(&id) {
                    return None;
                }
                let minimized = present(id)?;
                let node = self.alloc(NodeKind::Window {
                    id,
                    floating: false,
                    float_rect: *float_rect,
                    minimized,
                });
                self.node_mut(node).weight = *weight;
                self.windows.insert(id, node);
                placed.push(id);
                Some(node)
            }
            SavedNode::Split {
                axis,
                weight,
                children,
            } => {
                let split = self.alloc(NodeKind::Split { axis: *axis });
                self.node_mut(split).weight = *weight;
                for child in children {
                    if let Some(c) = self.restore_node(child, present, placed) {
                        self.node_mut(c).parent = Some(split);
                        self.node_mut(split).children.push(c);
                    }
                }
                if self.node(split).children.is_empty() {
                    self.release(split);
                    return None;
                }
                Some(split)
            }
        }
    }

    /// After a restore: collapses splits with a single child and makes each
    /// container's weights sum to one again (dropped windows leave gaps).
    fn tidy(&mut self, node: NodeId) {
        for child in self.node(node).children.clone() {
            self.tidy(child);
        }
        // A split left with one child is replaced by that child.
        for child in self.node(node).children.clone() {
            if matches!(self.node(child).kind, NodeKind::Split { .. })
                && self.node(child).children.len() == 1
            {
                self.replace_with_children(child);
            }
        }
        // A workspace whose only child is a split adopts its orientation and
        // children, as after a removal.
        if let NodeKind::Workspace { .. } = self.node(node).kind
            && let [only] = self.node(node).children[..]
            && let NodeKind::Split { axis } = self.node(only).kind
        {
            self.set_container_axis(node, axis);
            self.replace_with_children(only);
        }
        let children = self.node(node).children.clone();
        let total: f64 = children.iter().map(|&c| self.node(c).weight).sum();
        if total > 0.0 {
            for c in children {
                self.node_mut(c).weight /= total;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tree::MonitorSpec;
    use crate::{Direction, Gaps, MonitorId};

    use super::*;

    fn w(n: isize) -> WindowId {
        WindowId(n)
    }

    fn monitor(id: isize, name: &str, x: i32) -> MonitorSpec {
        let area = Rect::new(x, 0, 1000, 500);
        MonitorSpec {
            id: MonitorId(id),
            name: name.into(),
            bounds: area,
            work_area: area,
        }
    }

    fn open(tree: &mut Tree, ws: NodeId, ids: &[isize]) {
        for &id in ids {
            tree.insert_window(ws, w(id));
            tree.focus_window(w(id));
        }
    }

    /// Two monitors: "1" on L with H[1 V[2 3]] (resized) and a float, a
    /// hidden "3" on L with window 4, and "2" on R with 5.
    fn sample() -> Tree {
        let mut tree = Tree::new();
        let l = tree.add_monitor(&monitor(1, "L", 0));
        let r = tree.add_monitor(&monitor(2, "R", 1000));
        let ws1 = tree.add_workspace(l, "1", Layout::Manual);
        let ws2 = tree.add_workspace(r, "2", Layout::Dwindle);
        let ws3 = tree.add_workspace(l, "3", Layout::Manual);
        open(&mut tree, ws1, &[1, 2]);
        tree.split(w(2), Axis::Vertical);
        open(&mut tree, ws1, &[3]);
        tree.resize(w(1), Axis::Horizontal, 0.2);
        tree.insert_floating(ws1, w(9), Rect::new(10, 20, 300, 200));
        tree.set_minimized(w(3), true);
        tree.focus_workspace(ws3);
        open(&mut tree, ws3, &[4]);
        tree.focus_workspace(ws2);
        open(&mut tree, ws2, &[5]);
        tree.focus_workspace(ws1);
        tree.focus_window(w(2));
        tree
    }

    fn fresh(names: &[(&str, i32)]) -> Tree {
        let mut tree = Tree::new();
        for (i, (name, x)) in names.iter().enumerate() {
            tree.add_monitor(&monitor(i as isize + 10, name, *x));
        }
        tree
    }

    fn all_present(_: WindowId) -> Option<bool> {
        Some(false)
    }

    #[test]
    fn round_trip_restores_everything() {
        let original = sample();
        let saved = original.save_layout();
        // Saved state survives JSON.
        let json = serde_json::to_string(&saved).unwrap();
        let saved: Vec<SavedWorkspace> = serde_json::from_str(&json).unwrap();

        let mut tree = fresh(&[("L", 0), ("R", 1000)]);
        // Window 3 was minimized and still is.
        let placed = tree.restore_layout(&saved, |id| Some(id == w(3)));
        assert_eq!(placed.len(), 6);

        let ws1 = tree.workspace_by_name("1").unwrap();
        let ws2 = tree.workspace_by_name("2").unwrap();
        let ws3 = tree.workspace_by_name("3").unwrap();
        assert_eq!(tree.debug_layout(ws1), "H[1 V[2 3]] F[9]");
        assert_eq!(
            tree.arrange(ws1, Gaps::default()),
            original.arrange(original.workspace_by_name("1").unwrap(), Gaps::default())
        );
        assert!(tree.is_minimized(w(3)));
        assert_eq!(tree.float_rect(w(9)), Some(Rect::new(10, 20, 300, 200)));
        assert_eq!(tree.workspace_layout(ws2), Layout::Dwindle);
        assert!(tree.is_workspace_active(ws1));
        assert!(tree.is_workspace_active(ws2));
        assert!(!tree.is_workspace_active(ws3));
        assert_eq!(tree.focused_workspace(), Some(ws1));
        assert_eq!(tree.focused_window(), Some(w(2)));
        assert_eq!(tree.workspace_focused_window(ws3), Some(w(4)));
    }

    #[test]
    fn missing_windows_are_dropped_and_tree_tidied() {
        let saved = sample().save_layout();
        let mut tree = fresh(&[("L", 0), ("R", 1000)]);
        // 2 and 9 closed while rivewm was off.
        tree.restore_layout(&saved, |id| (id != w(2) && id != w(9)).then_some(false));
        let ws1 = tree.workspace_by_name("1").unwrap();
        // V[3] collapsed into the workspace; weights renormalized.
        assert_eq!(tree.debug_layout(ws1), "H[1 3]");
        let widths: Vec<i32> = tree
            .arrange(ws1, Gaps::default())
            .iter()
            .map(|(_, r)| r.width)
            .collect();
        assert_eq!(widths.iter().sum::<i32>(), 1000);
        // Focus fell back since 2 is gone.
        assert_ne!(tree.focused_window(), Some(w(2)));
    }

    #[test]
    fn workspace_of_a_missing_monitor_goes_to_the_first_and_returns() {
        let saved = sample().save_layout();
        let mut tree = fresh(&[("L", 0)]);
        tree.restore_layout(&saved, all_present);
        let ws2 = tree.workspace_by_name("2").unwrap();
        assert!(!tree.is_workspace_active(ws2), "arrives hidden");
        assert_eq!(tree.debug_layout(ws2), "H[5]");

        // When R is plugged in, "2" goes home.
        let spec_l = monitor(10, "L", 0);
        let spec_r = monitor(11, "R", 1000);
        tree.sync_monitors(&[spec_l, spec_r]);
        let r = tree.monitor_by_id(MonitorId(11)).unwrap();
        assert_eq!(tree.monitor_of(ws2), r);
        assert!(tree.is_workspace_active(ws2));
    }

    #[test]
    fn monitors_without_saved_workspaces_get_fresh_ones() {
        let saved = sample().save_layout();
        let mut tree = fresh(&[("L", 0), ("R", 1000), ("T", 2000)]);
        tree.restore_layout(&saved, all_present);
        let t = tree.monitor_by_id(MonitorId(12)).unwrap();
        let ws = tree.active_workspace(t).unwrap();
        assert_eq!(tree.workspace_name(ws), "4", "lowest unused number");
    }

    #[test]
    fn restored_tree_behaves_normally() {
        let saved = sample().save_layout();
        let mut tree = fresh(&[("L", 0), ("R", 1000)]);
        tree.restore_layout(&saved, all_present);
        let ws1 = tree.workspace_by_name("1").unwrap();
        // New windows, moves and removal all work on the rebuilt tree.
        open(&mut tree, ws1, &[6]);
        assert!(tree.move_in_direction(w(6), Direction::Left));
        tree.remove_window(w(1));
        assert!(tree.workspace_windows(ws1).contains(&w(6)));
    }
}
