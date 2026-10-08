//! Per-workspace layout policies.
//!
//! A layout decides *where in the tree* a new window goes and may reshape the
//! tree after a removal. It never owns a separate representation: every
//! layout produces an ordinary split tree, so focus, resize and arrange work
//! identically regardless of which layout built it.

use crate::tree::{NodeId, Tree};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// i3 / GlazeWM style: new windows go next to the focused one, in the
    /// direction the user last chose with `split`.
    #[default]
    Manual,
    // Planned: `Dwindle`, `MasterStack { ratio, master_count }`. These will
    // rebuild the workspace's tree into their canonical shape on insert and
    // remove.
}

impl Tree {
    pub(crate) fn layout_insert(&mut self, ws: NodeId, window: NodeId) {
        match self.workspace_layout(ws) {
            Layout::Manual => self.manual_insert(ws, window),
        }
    }

    pub(crate) fn layout_after_remove(&mut self, ws: NodeId) {
        match self.workspace_layout(ws) {
            Layout::Manual => {}
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
}
