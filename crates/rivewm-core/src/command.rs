use crate::{Axis, Direction};

/// Something the user asked the WM to do. Hotkeys, and later the CLI and
/// IPC, all produce these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Focus(Direction),
    /// Move the focused window one step in a direction (swap, enter or
    /// leave splits, or cross monitors).
    Move(Direction),
    /// Show workspace N, creating it on the focused monitor if needed.
    Workspace(u32),
    /// Send the focused window to workspace N.
    MoveToWorkspace(u32),
    /// The next window opens beside the focused one along this axis.
    Split(Axis),
    /// Like `Split`, using the opposite axis of the focused window's container.
    ToggleSplit,
    /// Switch the focused window between tiled and floating.
    ToggleFloating,
    /// Grow (positive) or shrink the focused window by a fraction of its
    /// container.
    Resize {
        axis: Axis,
        delta: f64,
    },
    /// Re-apply the layout to every window.
    Retile,
    Quit,
}
