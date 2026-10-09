use std::fmt;
use std::str::FromStr;

use crate::{Axis, Direction, Layout, WindowId};

/// Something the user asked the WM to do. Hotkeys, and later the CLI and
/// IPC, all produce these.
///
/// Commands also have a text form, used in the config file:
///
/// | Text                              | Command                       |
/// |-----------------------------------|-------------------------------|
/// | `focus left`                      | `Focus(Left)`                 |
/// | `focus-window 0x1234`             | `FocusWindow` (hex or decimal)|
/// | `move right`                      | `Move(Right)`                 |
/// | `workspace 3`                     | `Workspace("3")`              |
/// | `move-to-workspace web`           | `MoveToWorkspace("web")`      |
/// | `split vertical`                  | `Split(Vertical)`             |
/// | `toggle-split`                    | `ToggleSplit`                 |
/// | `toggle-floating`                 | `ToggleFloating`              |
/// | `toggle-fullscreen`               | `ToggleFullscreen`            |
/// | `layout dwindle`                  | `SetLayout(Dwindle)`          |
/// | `resize width +5`                 | grow width by 5% of container |
/// | `resize height -5`                | shrink height by 5%           |
/// | `resize right 5`                  | move an edge right by 5% of   |
/// |                                   | the monitor                   |
/// | `retile`, `reload-config`, `quit` | as named                      |
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Focus(Direction),
    /// Focus a specific window, switching to its workspace if hidden.
    FocusWindow(WindowId),
    /// Move the focused window one step in a direction (swap, enter or
    /// leave splits, or cross monitors).
    Move(Direction),
    /// Show the named workspace, creating it on the focused monitor if
    /// needed.
    Workspace(String),
    /// Send the focused window to the named workspace.
    MoveToWorkspace(String),
    /// The next window opens beside the focused one along this axis.
    Split(Axis),
    /// Like `Split`, using the opposite axis of the focused window's container.
    ToggleSplit,
    /// Switch the focused window between tiled and floating.
    ToggleFloating,
    /// Make the focused window cover its whole monitor, or return it to
    /// normal.
    ToggleFullscreen,
    /// Change how new windows are placed on the focused workspace.
    SetLayout(Layout),
    /// Grow (positive) or shrink the focused window by a fraction of its
    /// container.
    Resize {
        axis: Axis,
        delta: f64,
    },
    /// Move one of the focused window's edges in a direction by a fraction
    /// of the monitor: the edge on that side if there's a window beyond it
    /// (growing), else the opposite edge (shrinking). The edge goes the way
    /// the arrow points, whichever side of the screen the window is on.
    ResizeToward {
        direction: Direction,
        amount: f64,
    },
    /// Re-apply the layout to every window.
    Retile,
    ReloadConfig,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseCommandError(String);

impl fmt::Display for ParseCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseCommandError {}

impl FromStr for Command {
    type Err = ParseCommandError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = |msg: &str| ParseCommandError(format!("invalid command `{s}`: {msg}"));
        let words: Vec<&str> = s.split_whitespace().collect();
        let args = |n: usize| {
            if words.len() == n + 1 {
                Ok(&words[1..])
            } else {
                Err(err(&format!("expected {n} argument(s)")))
            }
        };
        let direction = |word: &str| match word {
            "left" => Ok(Direction::Left),
            "right" => Ok(Direction::Right),
            "up" => Ok(Direction::Up),
            "down" => Ok(Direction::Down),
            _ => Err(err("expected left, right, up or down")),
        };

        let command = match words.first().copied() {
            Some("focus") => Command::Focus(direction(args(1)?[0])?),
            Some("move") => Command::Move(direction(args(1)?[0])?),
            Some("workspace") => Command::Workspace(args(1)?[0].to_owned()),
            Some("move-to-workspace") => Command::MoveToWorkspace(args(1)?[0].to_owned()),
            Some("layout") => Command::SetLayout(args(1)?[0].parse().map_err(|e: String| err(&e))?),
            Some("focus-window") => {
                let id = args(1)?[0];
                let parsed = match id.strip_prefix("0x") {
                    Some(hex) => isize::from_str_radix(hex, 16),
                    None => id.parse(),
                };
                Command::FocusWindow(WindowId(
                    parsed.map_err(|_| err("expected a window id like 0x1a2b or 6699"))?,
                ))
            }
            Some("split") => Command::Split(match args(1)?[0] {
                "horizontal" => Axis::Horizontal,
                "vertical" => Axis::Vertical,
                _ => return Err(err("expected horizontal or vertical")),
            }),
            Some("resize") => {
                let a = args(2)?;
                let percent: f64 = a[1]
                    .parse()
                    .map_err(|_| err("expected a percentage like +5 or -5"))?;
                match a[0] {
                    "width" => Command::Resize {
                        axis: Axis::Horizontal,
                        delta: percent / 100.0,
                    },
                    "height" => Command::Resize {
                        axis: Axis::Vertical,
                        delta: percent / 100.0,
                    },
                    other => Command::ResizeToward {
                        direction: direction(other)
                            .map_err(|_| err("expected width, height, left, right, up or down"))?,
                        amount: percent / 100.0,
                    },
                }
            }
            Some(simple) => {
                args(0)?;
                match simple {
                    "toggle-split" => Command::ToggleSplit,
                    "toggle-floating" => Command::ToggleFloating,
                    "toggle-fullscreen" => Command::ToggleFullscreen,
                    "retile" => Command::Retile,
                    "reload-config" => Command::ReloadConfig,
                    "quit" => Command::Quit,
                    _ => return Err(err("unknown command")),
                }
            }
            None => return Err(err("empty")),
        };
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Command {
        s.parse().unwrap()
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(parse("focus left"), Command::Focus(Direction::Left));
        assert_eq!(parse("move down"), Command::Move(Direction::Down));
        assert_eq!(
            parse("focus-window 0x1a2b"),
            Command::FocusWindow(WindowId(0x1a2b))
        );
        assert_eq!(
            parse("focus-window 6699"),
            Command::FocusWindow(WindowId(6699))
        );
        assert_eq!(parse("workspace 3"), Command::Workspace("3".into()));
        assert_eq!(
            parse("move-to-workspace web"),
            Command::MoveToWorkspace("web".into())
        );
        assert_eq!(parse("split vertical"), Command::Split(Axis::Vertical));
        assert_eq!(parse("toggle-split"), Command::ToggleSplit);
        assert_eq!(parse("toggle-floating"), Command::ToggleFloating);
        assert_eq!(parse("toggle-fullscreen"), Command::ToggleFullscreen);
        assert_eq!(parse("layout dwindle"), Command::SetLayout(Layout::Dwindle));
        assert_eq!(parse("layout manual"), Command::SetLayout(Layout::Manual));
        assert_eq!(
            parse("resize width +5"),
            Command::Resize {
                axis: Axis::Horizontal,
                delta: 0.05
            }
        );
        assert_eq!(
            parse("resize height -10"),
            Command::Resize {
                axis: Axis::Vertical,
                delta: -0.1
            }
        );
        assert_eq!(
            parse("resize left 5"),
            Command::ResizeToward {
                direction: Direction::Left,
                amount: 0.05
            }
        );
        assert_eq!(parse("retile"), Command::Retile);
        assert_eq!(parse("reload-config"), Command::ReloadConfig);
        assert_eq!(parse("  quit  "), Command::Quit);
    }

    #[test]
    fn rejects_bad_commands() {
        for bad in [
            "",
            "focus",
            "focus sideways",
            "focus-window",
            "focus-window zz",
            "focus left now",
            "workspace",
            "split diagonal",
            "layout spiral",
            "resize width",
            "resize depth +5",
            "resize width lots",
            "quit now",
            "explode",
        ] {
            assert!(bad.parse::<Command>().is_err(), "`{bad}` should fail");
        }
    }
}
