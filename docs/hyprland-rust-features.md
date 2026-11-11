# Prompt: three OsmiumOS features for hyprland-rust

The user implements these in hyprland-rust (OsmiumOS never works around the compositor). Paste
the section below into a Claude Code session on the hyprland-rust repo. It assumes that repo's
`CLAUDE.md` (pure modules, `shortcut:` comments, shell tests in `tests/`, `docs/PARITY.md`).

None of the three exists in upstream Hyprland, so there is no parity target: names below are
proposals. Keep them out of `is_known_unimplemented` and list them in `docs/PARITY.md` under a new
"hyprland-rust extensions" heading.

---

Add three compositor features that OsmiumOS needs: window undo, projects, focus lock. Each gets a
pure module with unit tests, a dispatcher, a hyprctl request (text and `-j`), a socket2 event, a
Lua API under `hl.*`, and a headless shell test. No default binds in the compositor; OsmiumOS binds
them in its dots.

## 1. Window undo (`src/undo.rs`)

Goal: `Super+Z` reverts the last window-management action, `Super+Shift+Z` redoes it.

- Record every reversible action as an entry `{ id, time, kind, window address, before, after }`
  in a bounded log (option `undo:max_entries`, default 50; `undo:enabled`, default true). Kinds:
  move to workspace (also silent), toggle/set floating and tiled, fullscreen, pin, floating move
  and resize (interactive grabs record one entry when the grab ends, not per motion), swapwindow,
  movewindow, togglesplit/splitratio, layoutmsg, group in/out, renameworkspace, movecurrentworkspacetomonitor.
- `before`/`after` hold what is needed to put the state back: workspace id, floating flag and
  geometry, fullscreen mode, the window's place in the dwindle tree (sibling address and split
  side) or master stack index. Undo applies `before` through the same code paths as the
  dispatchers, so events (movewindow, changefloatingmode, ...) are emitted as usual.
- An entry whose window is gone is skipped, not an error. A new action after an undo drops the
  redo tail.
- Closed windows (`killactive`, `closewindow`, client exit): record class, title, workspace,
  floating geometry and the client's command line (`/proc/<pid>/cmdline`, Wayland and XWayland).
  Undo relaunches it with `exec` plus a one-shot rule that puts the next window of that class from
  the new pid where the old one was. `shortcut: relaunch, not the old window's state, upgrade path:
  none (the client is gone)`. `undo:reopen_closed`, default true.
- Dispatchers `undo` and `redo`. `hyprctl undolog [-j]`: entries newest first with an
  English description (`moved kitty to workspace 3`). Event `undo>>ID,DESCRIPTION` and
  `redo>>ID,DESCRIPTION`. Lua: `hl.dsp.undo()`, `hl.dsp.redo()`, `hl.get_undo_log()`.
- Actions done by undo/redo themselves are not recorded as new entries.
- Test (`tests/undo.sh`, headless): open two clients, move one to workspace 2, float it, resize
  it; three `undo`s restore workspace/tiling/geometry (check with `hyprctl -j clients`); `redo`
  re-applies; `undolog -j` shape; close a `weston-terminal`, `undo` brings one back on the same
  workspace.

## 2. Projects (`src/project.rs`)

Goal: save "what I had open, where" and restore it later, also after a reboot.

- `hyprctl project save NAME` writes `$XDG_STATE_HOME/hypr/projects/NAME.json` (fallback
  `~/.local/state`): per workspace (id, name, monitor by description, layout) its windows with
  class, initialClass, title, command line, cwd of the client process, floating flag and
  geometry, fullscreen mode, pinned, group membership, and the dwindle tree / master order with
  split ratios. Names: `[A-Za-z0-9_-]{1,64}`.
- `hyprctl project load NAME`: for every saved window, reuse an existing unclaimed window with
  the same initialClass (then class) on any workspace; otherwise `exec` its command line in its
  cwd. New windows are claimed in launch order by initialClass and pid (first matching saved entry
  wins), moved to their workspace silently, and the saved tree is rebuilt once all claimed windows
  are mapped, or after `project:load_timeout` (default 15 s) with whatever arrived. Windows that
  never appear are listed in the reply.
- `project list [-j]`, `project delete NAME`, `project current` (last loaded or saved). Events
  `projectsaved>>NAME`, `projectloaded>>NAME,MISSING_COUNT`. Lua: `hl.project.save/load/list/delete`.
- The JSON carries `"version": 1`; unknown fields are ignored on load so the format can grow.
- Pure parts to unit-test: serialisation round trip, the claiming algorithm (saved entries vs.
  arriving windows), tree rebuild from saved leaves when some are missing.
- Test (`tests/projects.sh`, headless): three clients on workspaces 1 and 2 with a split ratio,
  `project save t`, kill them, `project load t`, compare `hyprctl -j clients` workspaces and sizes.

## 3. Focus lock (`src/focuslock.rs`)

Goal: "only these apps for the next 50 minutes": everything else is dimmed and cannot take focus.

- `hyprctl focuslock on [DURATION] ALLOW...` where ALLOW are window selectors
  (`class:REGEX`, `title:REGEX`, `address:0x...`, `pid:N`, as in window rules) and DURATION is
  `25m`/`1h` (optional). `focuslock off`, `focuslock [-j]` (state, allow list, seconds left).
  Dispatcher `focuslock` with the same arguments, Lua `hl.focuslock.on{allow={...}, minutes=25}` /
  `off()` / `get()`.
- While on: windows not matching ALLOW are drawn with `focuslock:dim` (default 0.7, same path as
  `dim_inactive`) and are skipped by focus changes from every source: click, focus-follows-mouse,
  movefocus/cyclenext/focuswindow, xdg-activation, foreign-toplevel activate, `urgent`. Keyboard
  and pointer input never reach them; pointer motion over them shows the default cursor.
  New windows that do not match open on `special:focuslock` (not shown) and come back to their
  workspace when the lock ends. Layer surfaces are not affected, except namespaces listed in
  `focuslock:hide_layers` (default `notifications`), which are not drawn while on.
- `focuslock:allow_unlock` (default true): when false, `focuslock off` before the timer ends is
  refused with an error, so a distracted user cannot undo it with one command. Session lock still
  works.
- Events `focuslock>>1,SECONDS_LEFT` and `focuslock>>0`. State is not persisted across compositor
  restarts.
- Test (`tests/focuslock.sh`, headless): two clients, lock to one class, `focuswindow` the other
  is refused (active window unchanged), a third client opens on `special:focuslock`, `off`
  returns it; timer expiry with `on 2s`.

## OsmiumOS side (after the compositor has it)

Not part of the hyprland-rust work; listed so the API fits:
- dots bind `Super+Z` / `Super+Shift+Z` to `undo`/`redo`;
- an `osmium-proje` tool (Turkish UI) wrapping `project save/load/list`, and project entries in
  the ii launcher;
- an ii sidebar toggle and `osmium-odak <süre> <uygulama>...` for focus lock.
