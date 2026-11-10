# OsmiumOS: notes for Claude

Arch-based distro with one desktop, Hyprland (never KDE or GNOME), built only in GitHub Actions
(`.github/workflows/build.yml`): job `packages` builds `packages/*` + `packages/aur.txt` into a
pacman repo published as the GitHub release `repo`, then job `iso` runs archiso per edition.
User-facing text (tools, installer, messages) is Turkish; code, comments, commits are English.

## Layout

- `common/`, `editions/<name>/`: archiso overlays (`airootfs/`, `packages.x86_64`,
  `profiledef.patch.sh` for file modes). The edition overlay is applied after `common/`.
- `packages/<name>/PKGBUILD`: our packages, built with `makepkg -d` (CI installs build deps in
  the "Install tools" step: add new makedepends there). A package is rebuilt (with the run
  number as `pkgrel`) only when its inputs change: its directory plus the top-level repo paths its
  PKGBUILD names through `$OSMIUM_SRC`; otherwise CI reuses the published files
  (`<name>.<hash>.built` in the release lists them). Compiled packages are also rebuilt weekly.
  Files next to a PKGBUILD are read via `$startdir`; repo files via `$OSMIUM_SRC/<path>` (always
  with a path, so the inputs can be tracked). No `-debug` packages are built.
- `packages/aur.txt`: `name commit` of pinned AUR packages (name = pkgbase). Their
  `validpgpkeys` are fetched automatically. No epoch packages: GitHub assets cannot hold `:`.
- `tools/`: std-only Rust crate, no dependencies (keep it so). One binary per tool in
  `src/bin/`, shared helpers in `src/lib.rs`, unit tests next to the code.
- `settings/`: separate iced crate (`osmium-settings`).
- Dots: `/usr/share/osmium/dots/<id>/{info,home/,post-apply}`; `info` may say `inherits=<id>`.
  ii/waffle come from `packages/osmium-dots-ii`, m3ww from `packages/osmium-dots-m3ww`,
  synthwave from `packages/osmium-hyprland/skel`.
- Installer: UI `packages/osmium-dots-ii/overlay/.config/quickshell/ii/osmium-installer.qml`
  (uses ii's widgets, so it must live inside the ii config), backend `tools/src/bin/osmium-install.rs`.
  Protocol: `disks` -> JSON; `check`/`run` read `key=value` lines on stdin; `run` prints JSON lines
  `{"step","pct","msg"}`, then `{"done":true,"seconds"}` or `{"error"}`.

  CI job `install-test` runs it from the Hyprland ISO in QEMU (LUKS, then plain) and boots the
  result; on failure the serial logs are in the `install-test-logs` artifact.

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
- Pushing to `main` starts a build and cancels the running one (`concurrency: build`).

## License

GPL-3.0-or-later. hyprland-rust keeps BSD-3-Clause/MIT; end-4's dots are GPL-3.0.
