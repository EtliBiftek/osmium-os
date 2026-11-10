# OsmiumOS: unfinished and not-started work

Snapshot of `main` at `9d1bda2` (2026-11-10). Last `Build ISO` run (#48) is green, there are no
open issues, `cargo test` for `tools/` passes (184 tests). Compositor gaps are tracked separately
in `docs/hyprland-rust-gaps.md` and are only summarised here.

## 1. Half done (OsmiumOS side)

### 1.1 Calamares install path lags behind `osmium-install`
`osmium-install` got several fixes that the Calamares path (`sudo calamares`, manual
partitioning) did not:
- **Microcode**: `osmium-install` copies `amd-ucode.img`/`intel-ucode.img` from the ISO boot
  mount (`tools/src/bin/osmium-install.rs:866`); `osmium-postinstall` copies only `vmlinuz`
  (`tools/src/bin/osmium-postinstall.rs:~120`), so Calamares installs have no microcode.
- **First snapshot**: the "Kurulum" snapper snapshot is taken only by `osmium-install`.
- **Keyboard**: Calamares does not rewrite `kb_layout` in the user's Hyprland config (see 1.2).
- **ESP size**: Calamares recommends 512 MiB, `osmium-install` uses 1 GiB.
- **Not tested**: CI `install-test` exercises only `osmium-install`; nothing runs Calamares.
Fix: move the shared steps (microcode, snapshot, keyboard) into `osmium-postinstall`, which
both paths run.

### 1.2 Keyboard layout is lost when the dots are re-applied
The installer rewrites `kb_layout` only in the home it just populated
(`apply_kb_layout`, `osmium-install.rs:1044`). The dots themselves hard-code `tr`
(`packages/osmium-dots-ii/overlay/.config/hypr/custom/general.lua`,
`packages/osmium-hyprland/skel/.config/hypr/hyprland.lua:57`, m3ww PKGBUILD sed). So
`osmium-dots apply <id>` after install, `osmium-apply` with a `dots` key, and the first-login
`osmium-dots session` of a Calamares user all bring back the Turkish layout. Fix: let
`osmium-dots` read the system layout (`/etc/X11/xorg.conf.d/00-keyboard.conf` or
`localectl`) and apply it after every apply (move `set_kb_layout` into `lib.rs`).

### 1.3 OsmiumOS tools are wired only into synthwave, not into the default `ii`
- `osmium-settings` writes `~/.config/hypr/osmium-settings.lua`, which only synthwave's
  `hyprland.lua:116` loads. In ii, waffle and m3ww the app's Appearance tab changes nothing.
- `Super+M` (osmium-mode) and `Super+I` (osmium-settings) exist only in synthwave.
- `osmium-welcome --launch` runs only from synthwave, and its shortcut list
  (`tools/src/bin/osmium-welcome.rs`) describes synthwave's keys. A default (ii) user never sees
  it, and the shown keys would be wrong for ii.
Fix: add an `osmium.lua` to the ii overlay `hypr/custom/` that loads `osmium-settings.lua`, binds
mode switching, and launches a per-desktop welcome (or skip welcome outside synthwave).

### 1.4 Stale comments in the synthwave config
`packages/osmium-hyprland/skel/.config/hypr/hyprland.lua` lines 4-5 and 13 say blur, shadow,
animations, rounding, monitors and gestures are not drawn and that only the first monitor is
driven. `docs/hyprland-rust-gaps.md` marks items 1, 16, 32-35 as done at `f0ce131`. The comments
(and possibly options turned down because of them) should be refreshed.

### 1.5 Repository signing is not set up
`keys/` holds only a README; `OSMIUM_GPG_PRIVATE_KEY`/`OSMIUM_GPG_KEY_ID` are not set, so CI logs
"no signing key: repo stays unsigned" and installed systems trust the repo without signatures.
The steps are in the README ("Signing"); only the user can do them.

### 1.6 Fast hyprland-rust follow (optional)
The `notify-osmium` job and `OSMIUM_DISPATCH_TOKEN` described in the README are a hyprland-rust
side step; until they exist the bump runs hourly.

### 1.7 Thin test coverage
`osmium-welcome` (1 test), `osmium-hw` (2), `osmium-history`/`osmium-postinstall` (4) and
`settings/` (5) have few unit tests; `osmium-hw kur`, `osmium-migrate`, `osmium-guest`,
`osmium-try`, `osmium-battery` and `osmium-update prova` are not run end to end in CI (only the
read-only tools are smoke-run in `install-test`).

### 1.8 m3ww reserved bar space (gap #25/#26, OsmiumOS-side workaround)
The `/tmp/hypr` socket path and `fakefullscreen` are patched in
`packages/osmium-dots-m3ww/PKGBUILD`; `fakefullscreen` became a no-op (`exec, true`) and the bind
`=,248,` is deleted, so those two m3ww shortcuts do nothing until hyprland-rust implements them.

## 2. Not started

- Real-hardware testing: everything is verified only in QEMU (no NVIDIA, laptop battery,
  multi-monitor, Wi-Fi checks). `osmium-hw`, `osmium-battery` and `osmium-auto` (`wifi=`,
  `ekran=`, `pil<`) depend on hardware CI never has.
- Legacy BIOS boot: installer is UEFI only (README).
- Secure Boot: no shim / signed kernel.
- Localisation beyond Turkish: user-facing text is Turkish only.
- Release process: ISOs exist only as Actions artifacts; there is no versioned release, checksum
  or signature for the ISO itself.

## 3. Compositor (hyprland-rust) gaps, for reference

From `docs/hyprland-rust-gaps.md` at `f0ce131`: 47 items, 19 done, 14 partial, 14 missing. Most
visible to users of the default ii desktop:
- Tier A: window argument of dispatchers (#7), `mouse:`/scroll binds (#8), `submap_universal`
  (#9), `xdg_popup.grab` (#14, menus do not close), `hyprctl setcursor` (#2), dmabuf v4 (#3),
  real DPMS (#11), `addreserved` (#15).
- Tier B: `getoption` coverage (#17), `dispatch hl.config` (#18), `status` (#19), session lock
  details (#20), window rule effects (#21), minimize (#22), screencopy (#23), session env vars
  (#24, worked around in synthwave and ii), `hyprctl kill` (#27), layer popup constraint (#29),
  caps lock state (#30), optional protocols (#31), input/misc options (#42, #43).
- Tier C: `dim_special`/`dim_around` (#36), `screen_shader` (#39), cursor zoom (#40), snapping and
  border resize (#41).
Per CLAUDE.md these are implemented in hyprland-rust, not worked around here.
