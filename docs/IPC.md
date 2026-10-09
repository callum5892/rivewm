# rivewm IPC

rivewm listens on the named pipe `\\.\pipe\rivewm-<USERNAME>`. Only the user
running rivewm can open it, and remote clients are rejected.

The easiest client is the binary itself:

```
rivewm msg query state
rivewm msg workspace 3
rivewm msg focus-window 6699
```

`rivewm msg` prints the JSON reply and exits non-zero when `ok` is false.

## Protocol

Open the pipe, write one request line (UTF-8, ending in `\n`), read one JSON
reply line. One request per connection.

Replies are always an object with `ok`:

```json
{"ok": true}
{"ok": false, "error": "invalid command `focus sideways`: expected left, right, up or down"}
{"ok": true, "state": { ... }}
```

## Requests

Any command from the config file's `[keybindings]` section:

| Request                       | Effect                                                      |
|-------------------------------|-------------------------------------------------------------|
| `focus <left/right/up/down>`  | Focus the nearest window that way, across monitors          |
| `focus-window <id>`           | Focus a window by id (hex `0x..` or decimal), switching to its workspace if hidden |
| `move <direction>`            | Move the focused window (swap, enter/leave splits, cross monitors) |
| `workspace <name>`            | Show a workspace, creating it on the focused monitor        |
| `move-to-workspace <name>`    | Send the focused window to a workspace                      |
| `split <horizontal/vertical>` | Next window opens beside the focused one along that axis    |
| `toggle-split`                | Same, alternating axis                                      |
| `toggle-floating`             | Float or tile the focused window                            |
| `toggle-fullscreen`           | Make the focused window cover its monitor, or undo it       |
| `layout <dwindle/manual>`     | Change how new windows are placed on the focused workspace  |
| `resize <width/height> <±N>`  | Grow/shrink the focused window by N% of its container       |
| `retile`, `reload-config`, `quit` | As named                                                |

Commands act on the **focused** window, so to act on a specific window send
`focus-window <id>` first.

And one query:

| Request       | Reply                                                |
|---------------|------------------------------------------------------|
| `query state` | `state`: monitors → workspaces → layout and windows  |

## Subscribing to events

Send `subscribe` and keep the connection open. rivewm replies `{"ok": true}`,
then writes one JSON line per event until you disconnect:

```
rivewm msg subscribe
{"ok":true}
{"event":"window_managed","window":3216412,"workspace":"1"}
{"event":"layout_changed","workspace":"1"}
{"event":"window_focused","window":3216412,"title":"Untitled - Notepad"}
```

| Event               | Fields                                   | When                                         |
|---------------------|------------------------------------------|----------------------------------------------|
| `workspace_created` | `workspace`, `monitor`                   | A workspace comes into existence             |
| `workspace_removed` | `workspace`                              | An empty workspace is deleted (replaces `workspace_hidden` when it goes as it's hidden) |
| `workspace_shown`   | `workspace`, `monitor`                   | It becomes the one shown on its monitor      |
| `workspace_hidden`  | `workspace`                              | Another workspace replaced it on its monitor |
| `workspace_focused` | `workspace` (or null)                    | Keyboard focus moved to another workspace    |
| `window_managed`    | `window`, `workspace`                    | rivewm started managing a window             |
| `window_unmanaged`  | `window`                                 | A window closed, minimized or was released   |
| `window_moved`      | `window`, `from`, `to`                   | A window changed workspace                   |
| `window_focused`    | `window` (or null), `title`              | Focus moved to another window                |
| `title_changed`     | `window`, `title`                        | The *focused* window's title changed         |
| `layout_changed`    | `workspace`                              | Any window's position in it changed          |
| `config_reloaded`   |                                          | The config was reloaded                      |

Events describe what changed, not the full picture: send `query state` (on
a separate connection) when you need details. Several events can arrive for
one action, e.g. switching workspace gives `workspace_hidden`,
`workspace_shown`, `workspace_focused` and `window_focused`.

A subscriber that stops reading and falls 256 events behind is disconnected,
so rivewm never waits on a slow client.

## State shape

```json
{
  "focused_workspace": "1",
  "focused_window": 461068,
  "monitors": [{
    "id": 65695,
    "work_area": {"x": 0, "y": 0, "width": 3440, "height": 1380},
    "active_workspace": "1",
    "workspaces": [{
      "name": "1",
      "visible": true,
      "layout": "manual",
      "focused_window": 461068,
      "tiling": {
        "type": "split", "axis": "horizontal", "weight": 1.0,
        "children": [
          {"type": "window", "id": 461068, "title": "...", "process": "WindowsTerminal.exe",
           "class": "CASCADIA_HOSTING_WINDOW_CLASS",
           "rect": {"x": 8, "y": 8, "width": 1708, "height": 1364}, "weight": 0.5},
          {"type": "split", "axis": "vertical", "weight": 0.5, "children": [ ... ]}
        ]
      },
      "floating": [
        {"type": "window", "id": 1234, "title": "...", "rect": {...}, "weight": null, ...}
      ]
    }]
  }]
}
```

`rect` is where rivewm places the window (for hidden workspaces, where it
would go). `weight` is a node's share of its parent split. Minimized windows
have `"minimized": true` and no `rect`.

## Driving rivewm from scripts or AI agents

A typical loop: `query state`, decide on an arrangement, then issue commands,
e.g. to put a window on workspace 3 and stack it under another:

```
rivewm msg focus-window 461068
rivewm msg move-to-workspace 3
```

Each command is applied immediately, exactly as if its hotkey were pressed.
