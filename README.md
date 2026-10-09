# rivewm

A tiling window manager for Windows, written in Rust. It's inspired by
[GlazeWM](https://github.com/glzr-io/glazewm) and i3, with
[Hyprland](https://hyprland.org)-style dwindle tiling by default.

rivewm arranges your windows into non-overlapping tiles, gives you
keyboard-driven focus, movement and workspaces, and stays out of the way of
everything else: it runs alongside the normal Windows shell, taskbar and
Alt+Tab.

## Features

- **Tiling layouts**, chosen per workspace:
  - **dwindle** (default): each new window takes half of the focused one,
    side by side if it's wider than tall and stacked otherwise, spiralling
    inwards like Hyprland.
  - **manual**: i3-style; you pick the split direction for the next window.
- **Focus, move and resize** in any direction, across monitors.
- **Mouse support**: drag a tile onto another to rearrange; drag an edge to
  resize, with neighbours following live.
- **Workspaces** (`Alt + 1..9`), created on demand and removed when empty.
  Hidden workspaces are *cloaked*, so their windows keep their taskbar
  buttons and Alt+Tab entries, and picking one switches to its workspace.
- **Floating windows**: dialogs and fixed-size windows float automatically;
  any window can be toggled. Optionally kept always on top.
- **Fullscreen** toggle that covers the whole monitor.
- **Minimize keeps your layout**: a minimized window's neighbours close up
  over it, and restoring it puts it back exactly where it was.
- **Layout survives restarts**: workspaces, splits, sizes and floating
  positions are saved, and restarting rivewm puts every window back (not
  across reboots).
- **Focus follows mouse** (optional): hovering a window focuses it.
- **Focus border**: the focused window's border is coloured (Hyprland's cyan
  by default), using Windows 11's own window border.
- **Multi-monitor**, including monitors being connected or disconnected:
  workspaces move to the primary monitor and return when it comes back.
- **TOML config** with key bindings, gaps, colours and per-app window rules,
  reloaded live.
- **IPC**: control rivewm and read its state from scripts, status bars or AI
  agents, including a live event stream.
- **Tray icon**, background mode and start-at-login.
- **Safe to quit or crash**: windows rivewm hid are always brought back,
  even if it was killed from Task Manager.

## Requirements

- Windows 11. Windows 10 should work apart from the focus border, but it
  hasn't been tested.
- Rust 1.88 or newer, to build it.

## Install

```
cargo install --git https://github.com/callum5892/rivewm rivewm
```

Or from a clone of this repository:

```
cargo install --path crates/rivewm
```

Either puts `rivewm.exe` in `~\.cargo\bin`, which rustup adds to your
`PATH`.

## Running

| Command | What it does |
|---|---|
| `rivewm` | Runs in the current terminal, logging there. Ctrl+C quits. |
| `rivewm --background` | Runs detached from the terminal, with a tray icon. Logs go to `%LOCALAPPDATA%\rivewm\rivewm.log`. |
| `rivewm --autostart on` | Starts rivewm in the background when you log in (`off` to undo). |
| `rivewm --check-config` | Checks the config for mistakes without starting anything. |

Only one rivewm runs at a time. Quit with `Alt + Shift + E`, the tray icon's
menu, or Ctrl+C; every window rivewm hid is shown again on the way out.

## Default key bindings

| Keys | Action |
|---|---|
| `Alt + H/J/K/L` or `Alt + arrows` | Focus left / down / up / right |
| `Alt + Shift + H/J/K/L` or `Alt + Shift + arrows` | Move the focused window |
| `Alt + Ctrl + L / H` or `Alt + Ctrl + Right / Left` | Grow / shrink width |
| `Alt + Ctrl + J / K` or `Alt + Ctrl + Down / Up` | Grow / shrink height |
| `Alt + 1..9` | Switch to workspace 1-9 |
| `Alt + Shift + 1..9` | Send the focused window to workspace 1-9 |
| `Alt + V` | Toggle split direction |
| `Alt + Shift + Space` | Toggle floating |
| `Alt + F` | Toggle fullscreen |
| `Alt + Shift + R` | Re-tile everything |
| `Alt + Shift + C` | Reload the config |
| `Alt + Shift + E` | Quit |

These are global hotkeys, so while rivewm runs they take priority over the
same keys in other apps (e.g. `Alt + Left` is no longer "Back" in browsers).
Rebind anything in the config.

## Configuration

The config lives at `~\.config\rivewm\config.toml`. It's created with
comments explaining every option the first time rivewm runs, and any section
you leave out uses its default. Reload with `Alt + Shift + C`.

```toml
[gaps]
inner = 8
outer = 8

[layout]
default = "dwindle"          # or "manual"

[border]
enabled = true
focused = "#33ccff"          # "#rrggbb", "default" or "none"
unfocused = "#595959"

[focus]
follows_mouse = false        # focus the window under the mouse

[resize]
live = true                  # neighbours follow a dragged edge

[floating]
on_top = false               # keep floating windows above tiles

[programs]
exec = ["rivewm-bar"]        # started every time rivewm starts
exec_once = ["wt"]           # only the first time after you log in
stop_on_exit = ["rivewm-bar.exe"]  # force-closed when rivewm quits

[keybindings]
"alt+h" = "focus left"
"alt+shift+1" = "move-to-workspace 1"
"alt+ctrl+l" = "resize right 5"
# ...

[[rules]]
process = "Spotify.exe"      # and/or class = "...", title = "regex"
workspace = "9"              # and/or action = "float" | "tile" | "ignore"

[[rules]]
title = "^Picture-in-picture$"
action = "float"
```

A `[keybindings]` section replaces all the default bindings, so include
every binding you want. `rivewm --list --all` shows each window's process
and class, and whether rivewm manages it, which helps when writing rules.

Programs are started as the Run dialog (`Win + R`) would start them, so app
names like `wt`, `%VARIABLES%` and arguments all work. `stop_on_exit` takes
executable names as shown in Task Manager's Details tab.

### Commands

Key bindings, IPC requests and `rivewm msg` all use the same commands:

| Command | |
|---|---|
| `focus <left/right/up/down>` | Focus the nearest window that way |
| `focus-window <id>` | Focus a specific window |
| `move <left/right/up/down>` | Move the focused window |
| `workspace <name>` | Show a workspace (created if needed) |
| `move-to-workspace <name>` | Send the focused window to a workspace |
| `split <horizontal/vertical>`, `toggle-split` | Choose the split direction |
| `toggle-floating`, `toggle-fullscreen` | |
| `resize <left/right/up/down> N` | Move an edge that way by N% of the monitor |
| `resize <width/height> <+N/-N>` | Resize by N% of the containing split |
| `layout <dwindle/manual>` | Change the focused workspace's layout |
| `retile` | Re-apply the layout, re-checking apps' minimum sizes |
| `reload-config`, `quit` | |

## Scripting and IPC

rivewm listens on a named pipe that only your user account can open. The
simplest client is rivewm itself:

```
rivewm msg workspace 3
rivewm msg query state       # JSON: monitors, workspaces, layout, windows
rivewm msg subscribe         # streams one JSON event per line
```

Any command above works over IPC. `query state` and `subscribe` are enough to
build a status bar or let an AI agent arrange your windows. The protocol and
event list are in [docs/IPC.md](docs/IPC.md).

rivewm has no built-in status bar. [rivewm-bar](https://github.com/callum5892/rivewm-bar)
is a simple one built on this IPC: it shows each monitor's workspaces, the
focused window's title and a clock. Any other bar that registers as a
Windows app bar (like [YASB](https://github.com/amnweb/yasb)) also gets its
space respected automatically, and can show rivewm's workspaces through the
IPC.

## How it works

rivewm keeps one tree per workspace (monitor → workspace → splits →
windows); layouts only decide where new windows go in that tree, so every
feature works under every layout. The platform-independent core is unit
tested without Windows. Design notes and the Windows quirks rivewm works
around are in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

| Crate | |
|---|---|
| `rivewm-core` | Window tree, layouts, commands; no Windows code |
| `rivewm-platform` | Win32 wrappers: window events, hotkeys, cloaking, IPC pipe, tray |
| `rivewm` | The binary: event loop, config, CLI |

## Known limitations

- Windows of apps running as administrator (e.g. Task Manager) can't be
  managed unless rivewm runs as administrator too, so they're left alone.
- Hiding workspaces uses an undocumented Windows shell interface. If a
  future Windows update changes it, windows on other workspaces stay visible
  rather than anything breaking.
- The focus border is Windows 11's 1-pixel window border.

## AI use

AI was used in parts of this project, but it has been fully reviewed by a human.

## License

[MIT](LICENSE)
