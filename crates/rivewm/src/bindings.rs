//! Default key bindings, until there's a config file.

use rivewm_core::{Axis, Command, Direction};

const RESIZE_STEP: f64 = 0.05;

/// `(hotkey, command)` pairs. Hotkeys are strings so a config file can use
/// the same format.
pub fn defaults() -> Vec<(String, Command)> {
    use Command::*;
    use Direction::*;
    let mut bindings: Vec<(String, Command)> = [
        ("alt+h", Focus(Left)),
        ("alt+j", Focus(Down)),
        ("alt+k", Focus(Up)),
        ("alt+l", Focus(Right)),
        ("alt+left", Focus(Left)),
        ("alt+down", Focus(Down)),
        ("alt+up", Focus(Up)),
        ("alt+right", Focus(Right)),
        ("alt+shift+h", Move(Left)),
        ("alt+shift+j", Move(Down)),
        ("alt+shift+k", Move(Up)),
        ("alt+shift+l", Move(Right)),
        ("alt+shift+left", Move(Left)),
        ("alt+shift+down", Move(Down)),
        ("alt+shift+up", Move(Up)),
        ("alt+shift+right", Move(Right)),
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
    .into_iter()
    .map(|(key, command)| (key.to_owned(), command))
    .collect();

    for n in 1..=9 {
        bindings.push((format!("alt+{n}"), Workspace(n)));
        bindings.push((format!("alt+shift+{n}"), MoveToWorkspace(n)));
    }
    bindings
}

fn resize(axis: Axis, delta: f64) -> Command {
    Command::Resize { axis, delta }
}
