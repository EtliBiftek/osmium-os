# OsmiumOS: notes for Claude

Arch-based distro with one desktop, Hyprland (never KDE or GNOME), built only in GitHub Actions.
User-facing text (tools, installer, messages) is Turkish; code, comments, commits are English.
README.md describes every tool and the installed system: keep it in step with behaviour changes.

## CI (`.github/workflows/build.yml`)

- `packages`: `cargo test` for `tools/`, a live smoke run of `osmium-update check`, then
  `osmium-build packages` builds `packages/*` + `packages/aur.txt` into `./repo`. The shipped
  configs are checked with the compositor just built (`--check-config`; every mode's `hypr` line
  against a headless instance). The repo is published as the GitHub release `repo`: only the
  files in `work/fresh.txt`, packages before the database.
- `iso`: `osmium-build iso hyprland` runs archiso.
- `install-test`: installs the ISO in QEMU (LUKS, then plain over it), boots the result and runs
  the read-only tools there. It greps `OSMIUM-*` markers from the serial log; on failure the logs
  are in the `install-test-logs` artifact. New installer behaviour gets a marker here.
- Pushing to `main` starts a build and cancels the running one (`concurrency: build`). Other
  branches build nothing: `workflow_dispatch` on a branch would publish that branch's packages to
  the live repo, so do not trigger it without asking.

## Layout

- `common/`, `editions/hyprland/`: archiso overlays (`airootfs/`, `packages.x86_64`,
  `profiledef.patch.sh` for file modes). The edition overlay is applied after `common/`.
  Live-only files must also be listed in `LIVE_FILES`/`LIVE_DIRS` of `osmium-postinstall`,
  which removes them from the installed system.
- `packages/<name>/PKGBUILD`: our packages, built with `makepkg -d` (CI installs build deps in
  the "Install tools" step: add new makedepends there). A package is rebuilt (with the run
  number as `pkgrel`) only when its inputs change: its directory plus the top-level repo paths its
  PKGBUILD names through `$OSMIUM_SRC`; otherwise CI reuses the published files
  (`<name>.<hash>.built` in the release lists them). Compiled packages (`arch` not `any`) are
  also rebuilt weekly. Files next to a PKGBUILD are read via `$startdir`; repo files via
  `$OSMIUM_SRC/<path>` (always with a path, so the inputs can be tracked; a test enforces it).
  No `-debug` packages are built. New binaries and data files of `tools/` must be added to
  `packages/osmium-tools/PKGBUILD` by hand.
- `packages/aur.txt`: `name commit` of pinned AUR packages (name = pkgbase). Their
  `validpgpkeys` are fetched automatically. No epoch packages: GitHub assets cannot hold `:`.
- `tools/`: std-only Rust crate, no dependencies (keep it so). One binary per tool in
  `src/bin/`, shared helpers in `src/lib.rs` (`run`/`output`/`succeeds`, XDG dirs, `json_str`,
  `json_objects`, `hyprctl()`), unit tests next to the code. `tools/data/`: modes, systemd
  units, polkit rule.
- `settings/`: separate iced crate (`osmium-settings`); its tests run in the PKGBUILD's `check()`.
- Dots: `/usr/share/osmium/dots/<id>/{info,home/,post-apply}`; `info` may say `inherits=<id>`.
  ii/waffle come from `packages/osmium-dots-ii`, m3ww from `packages/osmium-dots-m3ww`,
  synthwave from `packages/osmium-hyprland/skel`. The dots hard-code `kb_layout = "tr"`; the
  installer rewrites it to the chosen layout.
- Installer: UI `packages/osmium-dots-ii/overlay/.config/quickshell/ii/osmium-installer.qml`
  (uses ii's widgets, so it must live inside the ii config), backend `tools/src/bin/osmium-install.rs`.
  Protocol: `disks` -> JSON; `check`/`run` read `key=value` lines on stdin; `run` prints JSON lines
  `{"step","pct","msg"}`, then `{"done":true,"seconds"}` or `{"error"}`. A test checks that every
  key the QML sends is one `parse_config` reads. Calamares (`osmium-calamares-config`) stays on
  the ISO for manual partitioning and runs the same `osmium-postinstall`.

## Writing the tools

- Split each tool into pure functions (parsing, decisions, argv building) with unit tests and a
  thin layer that touches the system. Anything that reaches a root command or a shell is
  validated first; secrets go through stdin, never argv.
- A step that changes the system prints what it will do and waits for `evet` (dangerous
  commands in `osmium-ai`: `çalıştır`).
- The crate must also compile and pass its tests on Windows: Unix-only calls sit behind
  `#[cfg(unix)]` with a fallback.
- `cargo clippy --all-targets` is clean: keep it so. The code is not rustfmt-formatted
  (lines up to ~130 columns): never run `cargo fmt`, and fix indentation by hand after
  `clippy --fix`.
- hyprctl exits 0 even when the compositor refuses: compare its reply with `ok`.

## Compositor

hyprland-rust (github.com/EtliBiftek/hyprland-rust, the user's own project), never upstream
`hyprland`. Pinned in `packages/hyprland-rust/PKGBUILD`, bumped hourly by
`.github/workflows/hyprland-rust-bump.yml` when its CI is green. Its gaps for our desktops are in
`docs/hyprland-rust-gaps.md`: the user implements those in hyprland-rust; do not work around them
in OsmiumOS beyond keeping the ISO usable.

## Working on this repo from Windows

- Nothing Linux-only runs locally: verify with `cargo test` and CI. Use a short target dir:
  `CARGO_TARGET_DIR=C:/Users/mhmtk/osw/target-tools cargo test --manifest-path tools/Cargo.toml`.
- Write files with the Edit/Write tools. Python via a bash heredoc halves backslashes
  (`\1` becomes byte 0x01); after any scripted edit run `grep -caP '[\x01-\x08]' <file>`.
- `.gitattributes` forces LF. Symlinks exist only in the git index: never `git add -A`, add paths.
- Never set GitHub secrets or signing keys yourself; the README documents the commands.

## License

GPL-3.0-or-later. hyprland-rust keeps BSD-3-Clause/MIT; end-4's dots are GPL-3.0.
