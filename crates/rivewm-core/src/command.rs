use crate::{Axis, Direction};

/// Something the user asked the WM to do. Hotkeys, and later the CLI and
/// IPC, all produce these.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Focus(Direction),
    /// Move the focused window one step in a direction (swap, enter or
    /// leave splits, or cross monitors).
    Move(Direction),
    /// The next window opens beside the focused one along this axis.
    Split(Axis),
    /// Like `Split`, using the opposite axis of the focused window's container.
    ToggleSplit,
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
