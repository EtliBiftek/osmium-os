# OsmiumOS: unfinished and not started work

State at commit 9d1bda2 (2026-11-10). CI is green: packages, Hyprland ISO, QEMU install (LUKS and
plain) and boot of the installed system. hyprland-rust gaps are tracked separately in
`docs/hyprland-rust-gaps.md`.

## Not started

1. **Secure Boot + UKI + TPM2 LUKS unlock** (chosen over GRUB snapshot boot). Means replacing GRUB
   with systemd-boot/UKI in `osmium-install`, signing (sbctl), `systemd-cryptenroll --tpm2`.
   grub-btrfs snapshot entries go away, so rollback must move to item 2.
2. **Self-healing boot**: systemd-boot boot counting (`+3` tries); after failed boots, roll back
   to the last good snapper snapshot automatically.
3. **QR code on boot failure**: initramfs/emergency screen shows a QR of the error summary and
   offers "go back to the previous snapshot".
4. **Install next to Windows**: shrink NTFS, dual-boot, then `osmium-migrate`. Installer today
   always wipes the whole disk (backend and QML UI).
5. **Dot preview and mixing**: render a dot in headless hyprland-rust, show a screenshot before
   applying (also in the installer); mix parts of dots (bar from one, colors from another).
6. **Kernel switch to `linux-zen`** (decided, not done). Touches `packages.x86_64`, the
   installer/postinstall preset (`/boot/vmlinuz-linux` is hard-coded), `osmium-hw` headers.
7. **hyprland-rust side of three features** (prompt for the user not written yet): window undo
   (`Super+Z`, event log), projects (workspace + window layout restore), focus lock (dim and
   block chosen apps).

## Half done

- **m3ww socket path** (gaps item 25): `prepare()` rewrites `UNIX-CONNECT:/tmp/hypr/` in the eww
  scripts. Not verified that it covers every script in the gap list (`activewin`, `activews`,
  `workspaces`, `taskbarloop`, `overview-wrapper`, `hyprtest`) or that the bar updates at runtime.
- **osmium-battery**: no restore on SIGTERM (logout/shutdown leaves the screen dimmed and the
  target file); fix with an `ExecStopPost=` restore subcommand.
- **osmium-auto**: the mode to return to lives in memory only (lost on restart); rules are
  identified by line number, so editing the file during a manual override re-applies once.
- **osmium-update prova**: Ctrl-C leaves the container and `/.osmium-prova` until the next run;
  if `pacman -Su` fails after the rehearsed sync databases were copied, no rollback.
- **osmium-guest**: the owner's session is locked only if it has a lock handler (hypridle);
  otherwise Ctrl+Alt+F<n> reaches the unlocked desktop.
- **osmium-try**: NVIDIA proprietary device nodes are not passed into the container.
- **osmium-hw**: NVIDIA open/legacy split by a device-id threshold, not a table.
- **Live initramfs hooks** (`common/airootfs/etc/mkinitcpio.conf.d/archiso.conf`) are a copy of
  releng's line minus `microcode`; must follow releng when archiso changes it.
- **Calamares**: still shipped as the manual-partitioning fallback, no longer tested since the
  KDE/GNOME editions were removed. Keep (and add a CI test) or drop.
- **Installer UI** (`osmium-installer.qml`): never exercised in CI; only the backend is.
- **Reinstall leaves a stale UEFI NVRAM entry** (`Boot000N "OsmiumOS"` pointing to the old ESP);
  the fallback path boots anyway. Remove old entries with `efibootmgr` before `grub-install`.
- **CI**: `actions/download-artifact@v4`/`upload-artifact@v4` run on deprecated Node 20; bump.

## Needs the user (never done by Claude)

- Signing key: secrets `OSMIUM_GPG_PRIVATE_KEY`, `OSMIUM_GPG_KEY_ID` (README "Signing").
  The repo and ISO are unsigned until then.
- `OSMIUM_DISPATCH_TOKEN` in hyprland-rust plus its `notify-osmium` job, for bumps within a
  minute instead of hourly.

## Never verified on real hardware

Everything runs in QEMU/KVM only: GPU drivers (`osmium-hw`), battery (`osmium-battery`),
brightness, multi-monitor rules (`osmium-auto ekran=`), guest session on a real seat, TPM.
The new tools' write paths (`onar`, `undo`, `prova`, `apply`, `guest start`, `try`) have not run
on any system; CI only runs their read-only commands.
