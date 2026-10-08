//! Events for IPC subscribers, derived by diffing snapshots of WM state.
//!
//! Rather than every code path announcing what it changed, the main loop
//! takes a cheap [`Snapshot`] after handling each message and [`diff`]s it
//! against the previous one. Nothing can be forgotten, and new features get
//! events for free.

use std::collections::HashMap;

use rivewm_core::{MonitorId, Rect, WindowId};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    pub focused_workspace: Option<String>,
    /// The focused window and its title.
    pub focused_window: Option<(WindowId, String)>,
    pub workspaces: Vec<WorkspaceSnapshot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceSnapshot {
    pub name: String,
    pub monitor: MonitorId,
    pub visible: bool,
    /// Where every window in the workspace goes, tiled and floating. Any
    /// change here is a layout change.
    pub windows: Vec<(WindowId, Rect)>,
}

/// The events that turn `old` into `new`, as JSON objects with an `event`
/// field. Event names:
///
/// - `workspace_created`, `workspace_removed`
/// - `workspace_shown`, `workspace_hidden` (on its monitor)
/// - `workspace_focused`
/// - `window_managed`, `window_unmanaged`, `window_moved` (between workspaces)
/// - `window_focused`, `title_changed` (of the focused window)
/// - `layout_changed` (any window's position in a workspace changed)
pub fn diff(old: &Snapshot, new: &Snapshot) -> Vec<Value> {
    let mut events = Vec::new();
    let old_ws: HashMap<&str, &WorkspaceSnapshot> = old
        .workspaces
        .iter()
        .map(|w| (w.name.as_str(), w))
        .collect();
    let new_ws: HashMap<&str, &WorkspaceSnapshot> = new
        .workspaces
        .iter()
        .map(|w| (w.name.as_str(), w))
        .collect();

    for ws in &new.workspaces {
        if !old_ws.contains_key(ws.name.as_str()) {
            events.push(json!({
                "event": "workspace_created",
                "workspace": ws.name,
                "monitor": ws.monitor.0,
            }));
        }
    }

    let old_home = window_homes(old);
    let new_home = window_homes(new);
    for ws in &new.workspaces {
        for &(id, _) in &ws.windows {
            match old_home.get(&id) {
                None => events.push(json!({
                    "event": "window_managed",
                    "window": id.0,
                    "workspace": ws.name,
                })),
                Some(&from) if from != ws.name => events.push(json!({
                    "event": "window_moved",
                    "window": id.0,
                    "from": from,
                    "to": ws.name,
                })),
                Some(_) => {}
            }
        }
    }
    for ws in &old.workspaces {
        for &(id, _) in &ws.windows {
            if !new_home.contains_key(&id) {
                events.push(json!({ "event": "window_unmanaged", "window": id.0 }));
            }
        }
    }

    for ws in &new.workspaces {
        let before = old_ws.get(ws.name.as_str());
        if before.is_some_and(|b| b.windows != ws.windows) {
            events.push(json!({ "event": "layout_changed", "workspace": ws.name }));
        }
        let was_visible = before.is_some_and(|b| b.visible);
        if ws.visible && !was_visible {
            events.push(json!({
                "event": "workspace_shown",
                "workspace": ws.name,
                "monitor": ws.monitor.0,
            }));
        } else if !ws.visible && was_visible {
            events.push(json!({ "event": "workspace_hidden", "workspace": ws.name }));
        }
    }

    for ws in &old.workspaces {
        if !new_ws.contains_key(ws.name.as_str()) {
            events.push(json!({ "event": "workspace_removed", "workspace": ws.name }));
        }
    }

    if new.focused_workspace != old.focused_workspace {
        events.push(json!({
            "event": "workspace_focused",
            "workspace": new.focused_workspace,
        }));
    }

    match (&old.focused_window, &new.focused_window) {
        (Some((old_id, old_title)), Some((id, title))) if old_id == id => {
            if old_title != title {
                events.push(json!({ "event": "title_changed", "window": id.0, "title": title }));
            }
        }
        (old_focus, new_focus) if old_focus != new_focus => {
            events.push(json!({
                "event": "window_focused",
                "window": new_focus.as_ref().map(|(id, _)| id.0),
                "title": new_focus.as_ref().map(|(_, title)| title),
            }));
        }
        _ => {}
    }

    events
}

/// Which workspace each window is in.
fn window_homes(snapshot: &Snapshot) -> HashMap<WindowId, &str> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|ws| ws.windows.iter().map(|&(id, _)| (id, ws.name.as_str())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MON: MonitorId = MonitorId(1);

    fn ws(name: &str, visible: bool, windows: &[isize]) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            name: name.into(),
            monitor: MON,
            visible,
            windows: windows
                .iter()
                .map(|&w| (WindowId(w), Rect::new(0, 0, 100, 100)))
                .collect(),
        }
    }

    fn names(events: &[Value]) -> Vec<&str> {
        events
            .iter()
            .map(|e| e["event"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn no_change_no_events() {
        let s = Snapshot {
            focused_workspace: Some("1".into()),
            focused_window: Some((WindowId(1), "a".into())),
            workspaces: vec![ws("1", true, &[1])],
        };
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn switching_workspace() {
        let old = Snapshot {
            focused_workspace: Some("1".into()),
            focused_window: Some((WindowId(1), "a".into())),
            workspaces: vec![ws("1", true, &[1])],
        };
        let new = Snapshot {
            focused_workspace: Some("2".into()),
            focused_window: None,
            workspaces: vec![ws("1", false, &[1]), ws("2", true, &[])],
        };
        let events = diff(&old, &new);
        assert_eq!(
            names(&events),
            [
                "workspace_created",
                "workspace_hidden",
                "workspace_shown",
                "workspace_focused",
                "window_focused"
            ]
        );
        assert_eq!(events[4]["window"], Value::Null);
    }

    #[test]
    fn windows_come_go_and_move() {
        let old = Snapshot {
            workspaces: vec![ws("1", true, &[1, 2]), ws("2", true, &[])],
            ..Default::default()
        };
        let new = Snapshot {
            workspaces: vec![ws("1", true, &[3]), ws("2", true, &[2])],
            ..Default::default()
        };
        let events = diff(&old, &new);
        assert_eq!(
            names(&events),
            [
                "window_managed",
                "window_moved",
                "window_unmanaged",
                "layout_changed",
                "layout_changed"
            ]
        );
        assert_eq!(events[0]["window"], 3);
        assert_eq!(events[1]["from"], "1");
        assert_eq!(events[1]["to"], "2");
        assert_eq!(events[2]["window"], 1);
    }

    #[test]
    fn resize_is_a_layout_change() {
        let old = Snapshot {
            workspaces: vec![ws("1", true, &[1])],
            ..Default::default()
        };
        let mut new = old.clone();
        new.workspaces[0].windows[0].1.width = 50;
        assert_eq!(names(&diff(&old, &new)), ["layout_changed"]);
    }

    #[test]
    fn empty_workspace_removed() {
        let old = Snapshot {
            workspaces: vec![ws("1", true, &[]), ws("3", false, &[])],
            ..Default::default()
        };
        let new = Snapshot {
            workspaces: vec![ws("1", true, &[])],
            ..Default::default()
        };
        let events = diff(&old, &new);
        assert_eq!(names(&events), ["workspace_removed"]);
        assert_eq!(events[0]["workspace"], "3");
    }

    #[test]
    fn title_of_focused_window() {
        let old = Snapshot {
            focused_window: Some((WindowId(1), "a".into())),
            ..Default::default()
        };
        let mut new = old.clone();
        new.focused_window = Some((WindowId(1), "b".into()));
        let events = diff(&old, &new);
        assert_eq!(names(&events), ["title_changed"]);
        assert_eq!(events[0]["title"], "b");

        new.focused_window = Some((WindowId(2), "c".into()));
        assert_eq!(names(&diff(&old, &new)), ["window_focused"]);
    }
}
