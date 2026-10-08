//! Default key bindings, until there's a config file.

use rivewm_core::{Axis, Command, Direction};

const RESIZE_STEP: f64 = 0.05;

/// `(hotkey, command)` pairs. Hotkeys are strings so a config file can use
/// the same format.
pub fn defaults() -> Vec<(&'static str, Command)> {
    use Command::*;
    use Direction::*;
    vec![
        ("alt+h", Focus(Left)),
        ("alt+j", Focus(Down)),
        ("alt+k", Focus(Up)),
        ("alt+l", Focus(Right)),
        ("alt+left", Focus(Left)),
        ("alt+down", Focus(Down)),
        ("alt+up", Focus(Up)),
        ("alt+right", Focus(Right)),
        ("alt+v", ToggleSplit),
        ("alt+ctrl+l", resize(Axis::Horizontal, RESIZE_STEP)),
        ("alt+ctrl+h", resize(Axis::Horizontal, -RESIZE_STEP)),
        ("alt+ctrl+j", resize(Axis::Vertical, RESIZE_STEP)),
        ("alt+ctrl+k", resize(Axis::Vertical, -RESIZE_STEP)),
        ("alt+ctrl+right", resize(Axis::Horizontal, RESIZE_STEP)),
        ("alt+ctrl+left", resize(Axis::Horizontal, -RESIZE_STEP)),
        ("alt+ctrl+down", resize(Axis::Vertical, RESIZE_STEP)),
        ("alt+ctrl+up", resize(Axis::Vertical, -RESIZE_STEP)),
        ("alt+shift+r", Retile),
        ("alt+shift+e", Quit),
    ]
}

fn resize(axis: Axis, delta: f64) -> Command {
    Command::Resize { axis, delta }
}
