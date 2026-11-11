# OsmiumOS: unfinished and not started work

State at the `ccr-5167ad5a-uh14lf` branch (2026-11-11), branch build #62 green. Branch builds run the whole pipeline
without publishing (`workflow_dispatch` on the branch). hyprland-rust gaps are tracked separately
in `docs/hyprland-rust-gaps.md`.

## Done on this branch, checked only in CI (QEMU)

- **Boot chain** (`osmium-boot`): systemd-boot + signed UKIs, Secure Boot keys enrolled during the
  install when the firmware is in Setup Mode, TPM2 unlock enrolled on the first boot (PCR 7, only
  with Secure Boot on), boot counting with automatic rollback, rescue menu with a QR code.
  Covered by `install-test`: Secure Boot OVMF + swtpm (passphrase boot, then TPM-only boot), and a
  good update (blessed) followed by a panicking kernel that must be rolled back.
- **Install next to Windows**: NTFS shrink, XBOOTLDR `/boot`, systemd-boot on Windows' ESP.
  Covered with a stand-in Windows disk (not a real Windows install).
- **Dot preview and mixing** (`osmium-dots onizle | parcalar | karistir`), installer preview button.
- **Installer UI**: loaded in the real live desktop in CI (no QML errors, screenshot artifact).
  Only loading is tested; clicking through the pages is not.
- Calamares removed; kernel is `linux-zen`.

## Not started

- **Installer colors**: the installer no longer loads ii's `MaterialThemeLoader` (its global
  shortcut collides with the running shell's), so it shows ii's default palette instead of the
  wallpaper colors. Reading `~/.local/state/quickshell/user/generated/colors.json` directly would
  bring them back.

- **Dot mixing beyond color, wallpaper and bar**: other parts (launcher, notifications, lock
  screen) need each dot to declare how to stop its own; ii's bar is part of its shell and cannot
  be swapped.
- **Rescue in the initramfs**: the QR rescue menu runs in the real root's emergency mode. A failure
  inside the initramfs (disk not found) only gets boot counting (three power cycles, then the
  previous kernel), no menu.

## Needs the user (never done by Claude)

- hyprland-rust: window undo, projects, focus lock. Prompt in `docs/hyprland-rust-features.md`;
  OsmiumOS then adds the binds and Turkish front ends listed at its end.
- Signing key: secrets `OSMIUM_GPG_PRIVATE_KEY`, `OSMIUM_GPG_KEY_ID` (README "Signing").
  The repo and ISO are unsigned until then.
- `OSMIUM_DISPATCH_TOKEN` in hyprland-rust plus its `notify-osmium` job, for bumps within a
  minute instead of hourly.

## Never verified on real hardware

Everything runs in QEMU/KVM only: GPU drivers (`osmium-hw`), battery (`osmium-battery`),
brightness, multi-monitor rules (`osmium-auto ekran=`), guest session on a real seat, a real TPM
and vendor Secure Boot firmware (key enrollment, option ROMs), shrinking a real Windows (NTFS
with real data, Windows booting afterwards and running its chkdsk).
The tools' write paths (`onar`, `undo`, `prova`, `apply`, `guest start`, `try`, `boot guvenli`,
`boot tpm`, `boot kurtar`) have not run on any system; CI runs their read-only commands and the
boot paths listed above.
