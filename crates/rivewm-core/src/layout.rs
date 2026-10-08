//! Per-workspace layout policies.
//!
//! A layout decides *where in the tree* a new window goes. It never owns a
//! separate representation: every layout produces an ordinary split tree, so
//! focus, move, resize and arrange work identically regardless of which
//! layout built it.

use std::fmt;
use std::str::FromStr;

use crate::Gaps;
use crate::tree::{Axis, NodeId, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// i3 / GlazeWM style: new windows go next to the focused one, in the
    /// direction the user last chose with `split`.
    #[default]
    Manual,
    /// Hyprland style: each new window takes half of the focused window,
    /// side by side if that window is wider than it is tall, stacked
    /// otherwise, which spirals inwards as windows open.
    Dwindle,
}

impl FromStr for Layout {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(Layout::Manual),
            "dwindle" => Ok(Layout::Dwindle),
            other => Err(format!(
                "unknown layout `{other}` (expected manual or dwindle)"
            )),
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Layout::Manual => "manual",
            Layout::Dwindle => "dwindle",
        })
    }
}

impl Tree {
    pub(crate) fn layout_insert(&mut self, ws: NodeId, window: NodeId) {
        match self.workspace_layout(ws) {
            Layout::Manual => self.manual_insert(ws, window),
            Layout::Dwindle => self.dwindle_insert(ws, window),
        }
    }

    pub(crate) fn layout_after_remove(&mut self, ws: NodeId) {
        match self.workspace_layout(ws) {
            // Removal already collapses splits that lost a child, which
            // keeps every remaining window's geometry, so neither layout
            // needs anything more.
            Layout::Manual | Layout::Dwindle => {}
        }
    }

    fn manual_insert(&mut self, ws: NodeId, window: NodeId) {
        match self.workspace_focus(ws) {
            // A floating window has no place in the tree to insert beside.
            Some(focused) if !self.node_is_floating(focused) => {
                let parent = self
                    .node(focused)
                    .parent
                    .expect("focused window has no parent");
                let idx = self
                    .node(parent)
                    .children
                    .iter()
                    .position(|&c| c == focused);
                let idx = idx.expect("focused window missing from parent");
                self.insert_child(parent, idx + 1, window);
            }
            _ => {
                let end = self.node(ws).children.len();
                self.insert_child(ws, end, window);
            }
        }
    }

    /// Splits the focused tiled window (or, failing that, the most recently
    /// split-off one) in two along its longer side, putting the new window
    /// in the right or bottom half.
    fn dwindle_insert(&mut self, ws: NodeId, window: NodeId) {
        let target = self
            .workspace_focus(ws)
            .filter(|&f| !self.node_is_floating(f))
            .or_else(|| self.last_tiled_window(ws));
        let Some(target) = target else {
            let end = self.node(ws).children.len();
            self.insert_child(ws, end, window);
            return;
        };
        let target_id = self.window_id_of(target);
        let rect = self
            .arrange(ws, Gaps::default())
            .into_iter()
            .find(|&(id, _)| id == target_id)
            .map(|(_, r)| r)
            .expect("tiled window has a rect");
        let axis = if rect.width >= rect.height {
            Axis::Horizontal
        } else {
            Axis::Vertical
        };
        // Re-orients a lone window's container, or wraps the target in a new
        // two-way split.
        self.split(target_id, axis);
        let parent = self.node(target).parent.expect("target has a parent");
        let idx = self.node(parent).children.iter().position(|&c| c == target);
        self.insert_child(parent, idx.expect("target in its parent") + 1, window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Direction, MonitorSpec};
    use crate::{MonitorId, Rect, WindowId};

    fn w(n: isize) -> WindowId {
        WindowId(n)
    }

    /// One 1600x900 monitor with a dwindle workspace.
    fn dwindle() -> (Tree, NodeId) {
        let mut tree = Tree::new();
        let area = Rect::new(0, 0, 1600, 900);
        let monitor = tree.add_monitor(&MonitorSpec {
            id: MonitorId(1),
            name: "M".into(),
            bounds: area,
            work_area: area,
        });
        let ws = tree.add_workspace(monitor, "1", Layout::Dwindle);
        (tree, ws)
    }

    fn open(tree: &mut Tree, ws: NodeId, ids: &[isize]) {
        for &id in ids {
            tree.insert_window(ws, w(id));
            tree.focus_window(w(id));
        }
    }

    #[test]
    fn spirals_inward() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2, 3, 4, 5]);
        // Wide → side by side; the 800x900 right half is tall → stacked;
        // the 800x450 quarter is wide → side by side; and so on.
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 H[3 V[4 5]]]]");
        let rects = tree.arrange(ws, Gaps::default());
        assert_eq!(rects[0].1, Rect::new(0, 0, 800, 900));
        assert_eq!(rects[1].1, Rect::new(800, 0, 800, 450));
        assert_eq!(rects[2].1, Rect::new(800, 450, 400, 450));
        assert_eq!(rects[3].1, Rect::new(1200, 450, 400, 225));
        assert_eq!(rects[4].1, Rect::new(1200, 675, 400, 225));
    }

    #[test]
    fn splits_whichever_window_has_focus() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2]);
        tree.focus_window(w(1));
        open(&mut tree, ws, &[3]);
        // 1 was 800x900, so it's split top/bottom.
        assert_eq!(tree.debug_layout(ws), "H[V[1 3] 2]");
    }

    #[test]
    fn closing_keeps_the_other_windows_in_place() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2, 3, 4]);
        let before = tree.arrange(ws, Gaps::default());
        tree.remove_window(w(3));
        let after = tree.arrange(ws, Gaps::default());
        // 1 and 2 don't move; 4 takes over 3's half of their split.
        assert_eq!(after[0], before[0]);
        assert_eq!(after[1], before[1]);
        assert_eq!(after[2], (w(4), Rect::new(800, 450, 800, 450)));
    }

    #[test]
    fn falls_back_to_last_window_when_a_float_has_focus() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2]);
        tree.insert_floating(ws, w(9), Rect::new(0, 0, 10, 10));
        tree.focus_window(w(9));
        open(&mut tree, ws, &[3]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 3]] F[9]");
    }

    #[test]
    fn flip_split_swaps_orientation_in_place() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2, 3]);
        assert_eq!(tree.debug_layout(ws), "H[1 V[2 3]]");
        assert!(tree.flip_split(w(3)));
        assert_eq!(tree.debug_layout(ws), "H[1 H[2 3]]");
        assert!(tree.flip_split(w(1)));
        assert_eq!(tree.debug_layout(ws), "V[1 H[2 3]]");
    }

    #[test]
    fn other_operations_work_unchanged() {
        let (mut tree, ws) = dwindle();
        open(&mut tree, ws, &[1, 2, 3]);
        assert_eq!(tree.focus_in_direction(Direction::Left), Some(w(1)));
        assert!(tree.resize(w(1), crate::Axis::Horizontal, 0.1));
        assert!(tree.move_in_direction(w(1), Direction::Right));
    }

    #[test]
    fn layout_names_round_trip() {
        for layout in [Layout::Manual, Layout::Dwindle] {
            assert_eq!(layout.to_string().parse::<Layout>(), Ok(layout));
        }
        assert!("spiral".parse::<Layout>().is_err());
    }
}
