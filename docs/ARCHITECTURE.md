# rivewm architecture

## Crates

| Crate             | Role                                                                 |
|-------------------|----------------------------------------------------------------------|
| `rivewm-core`     | Window tree, layout maths, commands. No Win32 — unit tested anywhere. |
| `rivewm-platform` | Safe wrappers over the `windows` crate: enumerate, hooks, move, keys. |
| `rivewm`          | Binary. Event loop wiring core ↔ platform, config, CLI.               |

The config (`~\.config\rivewm\config.toml`) is parsed in `rivewm/src/config.rs`.
Its defaults are `rivewm/src/default_config.toml`, compiled in and written out
on first run, so there's exactly one place defaults live. Command strings
(`focus left`, `workspace 3`, ...) parse in core (`Command::from_str`) so a
future CLI/IPC can reuse them.

## Runtime model

- One thread owns all WM state. Win32 hook callbacks and the hotkey hook only
  translate OS events into `Event`s and push them onto a channel.
- Hotkeys, the CLI and (later) IPC all produce the same `Command` values.
- Layout is a pure function: `tree → Vec<(WindowId, Rect)>`. Only the diff is
  applied to real windows, via `DeferWindowPos` batches.
- On exit or panic every managed window is restored (shown, un-cloaked,
  moved back on-screen).

## Tree

```
Root
└── Monitor
    └── Workspace { layout: Layout }
        └── Split { direction: Horizontal | Vertical, children, ratios }
            ├── Window { state: Tiling | Floating | Fullscreen | Minimized }
            └── Split …
```

## Layouts: tree first, dynamic later

There is exactly **one** representation, the split tree. Dynamic layouts are
*policies that shape that tree*, not a separate data model:

- `Layout::Manual` (i3 / GlazeWM): a new window is inserted next to the focused
  one in the direction the user last chose. The user controls the shape.
- `Layout::Dwindle`, `Layout::MasterStack { ratio, master_count }`, … (later):
  on insert/remove the policy rebuilds the workspace's tree into its canonical
  shape.

Each workspace carries its own `Layout`, so switching layouts is per workspace
and switching back to `Manual` keeps whatever tree the dynamic layout produced.

Directional focus is *geometric*: it picks the nearest window on screen in
that direction from the arranged rects, not by walking the tree. That keeps
it correct for any tree shape a layout produces.

Because everything stays a tree, focus-direction, swap, resize and the
tree → rect function work the same in every layout. The constraint that keeps
this possible: **commands must never assume the user built the tree by hand.**
Insertion and removal go through the workspace's layout policy, never through
ad-hoc tree edits in command handlers.

## Windows quirks we handle

- **Invisible borders** — positions use `DWMWA_EXTENDED_FRAME_BOUNDS`; the
  delta to `GetWindowRect` is added back when calling `SetWindowPos`.
- **Cloaked windows** — `DWMWA_CLOAKED` windows are never managed.
- **DPI** — the process is per-monitor-v2 aware; all coordinates are physical.
- **Minimized windows** report `-32000,-32000`; their real size comes from
  `GetWindowPlacement`.
- **Elevated windows** (e.g. Task Manager) can't be moved unless rivewm is
  elevated too (UIPI). They're classified `Skip::Elevated` and left alone;
  as a backstop, any window whose move fails with access denied is released
  and not managed again until it closes.
- **Inactive workspaces are cloaked**, via the shell's undocumented
  `IApplicationView::SetCloak` (`rivewm-platform/src/cloak.rs`), so their
  windows keep taskbar buttons and Alt+Tab entries. Our cloaks echo back as
  `Cloaked` events; a window is only dropped if it's on a *visible*
  workspace and still cloaked when the event is handled. Cloaked windows are
  recorded in `%TEMP%\rivewm-cloaked.txt` so a run that was killed outright
  can be undone on the next start.
- **Hidden fires before Destroyed** when an app closes; both unmanage.
- **Our own moves echo back** as `LocationChanged`. Ignore location events
  between `MoveSizeStarted`/`MoveSizeEnded` pairs, and for windows we just
  positioned.
