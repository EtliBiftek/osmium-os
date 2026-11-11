# OsmiumOS

Arch Linux based distribution with a Hyprland desktop (running hyprland-rust).

## Build

GitHub Actions builds the ISO on every push to `main`. Download it from the run's artifacts.
Packages whose inputs did not change are reused from the published repo instead of being rebuilt.

Local build on Arch (as root):

```
pacman -S archiso rust base-devel
cargo run --release --manifest-path tools/Cargo.toml --bin osmium-build -- packages
cargo run --release --manifest-path tools/Cargo.toml --bin osmium-build -- iso hyprland
```

## Layout

- `common/`: files and packages shared by every edition
- `editions/<name>/`: per-edition packages and files
- `packages/`: our PKGBUILDs; `packages/aur.txt` lists pinned AUR packages
- `tools/`: Rust crate (std only). `osmium-build` builds the repo and ISOs, `osmium-install` and
  `osmium-postinstall` install the system, the rest are the desktop tools below
- `settings/`: `osmium-settings`, an iced GUI
- `docs/hyprland-rust-gaps.md`: what the compositor still lacks for the shipped desktops

Live user: `osmium` (no password).

## Desktops (dots)

The Hyprland edition ships several ready desktops. `osmium-dots` applies one to your home:

| id | What it is |
|----|------------|
| `ii` (default) | end-4's illogical-impulse on Quickshell: AI sidebar (Gemini, Ollama), settings app, overview, Material You colors from the wallpaper |
| `waffle` | ii's Windows 11 style panel family ("Windoes") |
| `m3ww` | end-4's archived Material You EWW style, patched for hyprland-rust |
| `synthwave` | OsmiumOS's own 80s synthwave theme (ironbar, anyrun, alacritty) |

```
osmium-dots list
osmium-dots apply waffle
osmium-dots search catppuccin                         # GitHub repos with topic "hyprland"
osmium-dots apply github:owner/repo                   # copies only its config dirs
osmium-dots apply github:owner/repo --mode script     # shows its install script, runs after `çalıştır`
osmium-dots onizle m3ww                               # try it in a window first (nested hyprland-rust)
osmium-dots parcalar synthwave                        # parts it can give or take
osmium-dots karistir neon ii renk=synthwave duvar=synthwave   # ii with synthwave's colors
```

Mixing works with the parts each dot declares in its `info`: `renk` (accent color), `duvar`
(wallpaper), `cubuk` (bar), `bildirim` (notifications), `baslatici` (launcher) and `kilit` (lock
screen). ii rebuilds its whole palette from a color or wallpaper; a bar or notification daemon
replaces the base's own when the base says how to stop it (synthwave does; ii's are part of its
shell); a launcher or lock screen takes over the base's key (`SUPER, L` and so on). The installer
has a preview button for the built-in desktops.

Replaced `~/.config/<dir>` entries go to `~/.local/state/osmium/dots-backup/<time>/`. The login
session (`osmium-dots session`, used by greetd and the live ISO) applies `ii` on first login.
end-4's dots are pinned in `packages/osmium-dots-ii` and `packages/osmium-dots-m3ww` (GPL-3.0).

## Installer

The Hyprland live ISO boots into ii and opens `osmium-installer`, a window built from ii's own
widgets (`~/.config/quickshell/ii/osmium-installer.qml`). It asks for a disk (optional LUKS),
region, user and desktop (built-in dots or one found on GitHub), then runs
`sudo osmium-install run`, which:

- partitions GPT (1 GiB ESP at `/efi` + btrfs with `@ @home @cache @log @snapshots`, zstd:1,
  `@` as the default subvolume), with LUKS2 if asked,
- unpacks the live squashfs with every core (`unsquashfs`), no downloads,
- configures locale, user and keyboard, runs `osmium-postinstall`, builds a zstd -1 unified
  kernel image (UKI) and installs systemd-boot, in parallel where possible.

## Boot

systemd-boot starts a signed UKI (`osmium-boot`, see `tools/src/bin/osmium-boot.rs`):

- **Secure Boot**: the installer creates OsmiumOS keys (sbctl) and signs systemd-boot and every
  UKI. If the firmware is in Setup Mode during the install, the keys are enrolled (with
  Microsoft's, for option ROMs) and Secure Boot is on from the first boot; otherwise
  `sudo osmium-boot guvenli` explains how to get there.
- **TPM2 unlock**: with LUKS and Secure Boot on, the first boot enrolls the TPM (PCR 7), so the
  passphrase is typed once and never again on that machine. The installer says beforehand
  whether this happens on its own; otherwise a notification after the first login lists the
  steps (`sudo osmium-boot guvenli`, then `sudo osmium-boot tpm`). Without Secure Boot the disk keeps
  asking (a TPM bound to a firmware without Secure Boot would open for any boot medium).
  The passphrase prompt uses the system keymap. `sudo osmium-boot tpm` re-enrolls.
- **Self-healing updates**: a new kernel/initramfs is installed as `osmium+3.efi`; systemd-boot
  tries it three times and keeps it only once the login screen came up. Otherwise it boots the
  previous image (`osmium-kurtarma.efi`), which rolls the root back to the snapshot taken before
  the update and reboots into the old, working system. `osmium-boot` shows the state.
- **Rescue screen**: when the system lands in emergency mode, also inside the initramfs (disk not
  found, cannot unlock), a Turkish menu shows the errors and a QR code of them, and offers to go
  back to the previous snapshot or to boot the previous kernel once.

`osmium-install disks` lists disks as JSON; `osmium-install check` validates a config read from
stdin (`key=value` lines). UEFI only.

## Tools

- `osmium-mode` (`Super+M` in synthwave): switch Normal / Oyun / Çalışma / Sunum modes. Add your own as
  `~/.config/osmium/modes/<id>.mode` (keys: `name`, `power`, `notifications`, `idle`, `hypr`, `exec`).
- `osmium-update`: before `pacman -Syu`, reads Arch News and bug reports opened in the last 7 days
  for the pending packages. A risky update is held as a whole (never a partial upgrade) for up to
  3 days; `osmium-update --force` skips the hold. A daily timer sends a notification.
- `osmium-share`: `export [name]` bundles hypr, ironbar, anyrun, alacritty, swaync and mode configs
  into one text file; `publish <file>` makes it a public gist link; `try <link>` shows every line
  that can start a program, applies the theme and rolls back unless you confirm within 30 s;
  `undo` restores what the last `try` replaced.
- `osmium-settings` (`Super+I` in synthwave; ii has its own settings app): GUI for appearance (gaps, rounding, blur, animations, keyboard),
  modes, config history and update checks. Appearance goes to `~/.config/hypr/osmium-settings.lua`.
- `osmium-ai`: offline assistant on a local Ollama model (`osmium-ai setup`, `ask`, `fix`). It only
  proposes commands; each one runs only after you type `evet` (dangerous ones: `çalıştır`).
- `osmium-migrate`: reads a Windows disk read-only and copies documents, wallpaper, user fonts and
  Wi-Fi names (passwords cannot be read offline) and maps installed Windows programs to Linux
  packages or alternatives. `osmium-migrate scan`, then `osmium-migrate all <windows-user>`.
- `osmium-history`: every 10 minutes `~/.config` is snapshotted locally.
  `osmium-history log`, `osmium-history diff <id>`, `osmium-history restore <id> [path]`.
- `osmium-update prova`: rehearses the update. It upgrades a btrfs snapshot of the running system,
  boots it in a container and compares failed units with the live system. Only a clean rehearsal
  upgrades the real system, with exactly the rehearsed versions (`--provasiz` skips it). Drivers
  and the kernel itself are not exercised in a container.
- `osmium-doctor`: health check (failed units, full disks, btrfs errors, stale pacman lock,
  orphans, journal errors, `.pacnew` files, snapshots, memory). `onar` offers each fix after an
  `evet`; `--ai` lets the offline model explain the findings.
- `osmium-changes`: what changed since a snapshot (default: since yesterday): packages, kernel,
  `/etc` files, enabled units. `osmium-changes --ai "dünden beri sistem neden yavaş?"` asks the
  offline model; `osmium-changes undo <n>` rolls back with snapper after an `evet`.
- `osmium-try <paket>... [-- komut]`: installs and runs packages in a throwaway snapshot of the
  root (`systemd-nspawn --ephemeral`); nothing remains after it closes.
- `osmium-guest start`: a guest desktop on another VT with a tmpfs home; everything is deleted
  when it ends. `osmium-guest cleanup` ends it by hand.
- `osmium-auto`: switches modes from context. Rules in `~/.config/osmium/auto.rules`
  (`osmium-auto init` writes an example), e.g. `oyun: uygulama=steam`, `sunum: ekran=2`,
  `calisma: wifi=Ofis ve saat=09:00-18:00`, also `pil<20`, `priz`, `pilde`.
  `osmium-auto check` shows which rule matches and why.
- `osmium-battery hedef 4s`: battery budget. Steps down power profile, brightness and refresh
  rate until the battery lasts 4 hours, and back up when there is room. `osmium-battery kapat`
  restores everything.
- `osmium-apply [dosya]`: declarative system file `~/.config/osmium/osmium.toml` (packages,
  dots, mode, services, hostname, timezone). `plan` shows the difference, `export` writes the
  current machine; share it with `osmium-share` and apply it on another machine.
- `osmium-hw`: detects the GPU and virtual machine; `osmium-hw kur` installs the missing drivers.

## Compositor

The Hyprland edition runs [hyprland-rust](https://github.com/EtliBiftek/hyprland-rust), not
upstream Hyprland. `.github/workflows/hyprland-rust-bump.yml` checks its `main` every hour. When
a newer commit's CI is green, it pins `packages/hyprland-rust/PKGBUILD` to that commit and starts
an ISO build. Red or unfinished commits are skipped. Run it by hand from the Actions tab.

CI loads every shipped dot with `hyprland-rust --check-config`; errors in third-party dots are
warnings, so they never block the ISOs. If `ii` ever fails that check at boot, the live ISO falls
back to `synthwave`. What the compositor still lacks is in `docs/hyprland-rust-gaps.md`.

To update within a minute of a push instead of an hour, add a fine-grained token with
"Contents: read and write" on osmium-os as the secret `OSMIUM_DISPATCH_TOKEN` in hyprland-rust.
Then add this job to hyprland-rust's CI, after the build job:

```yaml
  notify-osmium:
    needs: build
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-latest
    steps:
      - run: gh api repos/EtliBiftek/osmium-os/dispatches -f event_type=hyprland-rust-updated
        env:
          GH_TOKEN: ${{ secrets.OSMIUM_DISPATCH_TOKEN }}
```

## Signing

The repo is unsigned until you set up a key. Once set up, packages and the repo database are
signed in CI and the ISO and installed systems require valid signatures. On a Linux machine:

```
gpg --quick-gen-key "OsmiumOS Repo <you@example.com>" ed25519 sign never   # no passphrase
gpg --armor --export "OsmiumOS Repo" > keys/osmium.asc                     # commit this
gpg --armor --export-secret-keys "OsmiumOS Repo" | gh secret set OSMIUM_GPG_PRIVATE_KEY
gpg --list-keys --with-colons "OsmiumOS Repo" | awk -F: '/^fpr:/ {print $10; exit}' | gh secret set OSMIUM_GPG_KEY_ID
```

Keep the secret key backed up offline; anyone holding it can publish packages your users trust.

## License

Copyright (C) 2026 EtliBiftek

GPL-3.0-or-later, see `LICENSE`: you can redistribute and modify OsmiumOS under the GNU
General Public License, version 3 or any later version.
`hyprland-rust` keeps its own license (BSD-3-Clause, Smithay parts MIT).
