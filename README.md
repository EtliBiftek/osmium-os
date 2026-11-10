# OsmiumOS

Arch Linux based distribution with one desktop: Hyprland, running on
[hyprland-rust](https://github.com/EtliBiftek/hyprland-rust). It installs offline in about a
minute, keeps btrfs snapshots of the system and a history of your settings, and ships its own
tools for updates, health checks, modes and an offline assistant. The tools speak Turkish.

## Build

GitHub Actions (`.github/workflows/build.yml`) builds everything on every push to `main`:

1. `packages`: tests the tools, builds `packages/*` and the pinned AUR packages, checks the
   shipped configs with the compositor, and publishes the pacman repo as the GitHub release `repo`.
2. `iso`: builds the live ISO with archiso. Download it from the run's artifacts.
3. `install-test`: installs the ISO in QEMU (once with LUKS, once without) and boots the result.
   On failure the serial logs are in the `install-test-logs` artifact.

Packages whose inputs did not change are reused from the published repo instead of being rebuilt;
compiled ones are rebuilt at least once a week against current Arch libraries.

Local build on Arch (as root):

```
pacman -S archiso rust base-devel
cargo run --release --manifest-path tools/Cargo.toml --bin osmium-build -- packages
cargo run --release --manifest-path tools/Cargo.toml --bin osmium-build -- iso hyprland
```

## Layout

- `common/`, `editions/hyprland/`: archiso overlays (files, package lists, file modes); the
  edition is applied on top of `common/`
- `packages/`: our PKGBUILDs; `packages/aur.txt` lists AUR packages pinned to reviewed commits
- `tools/`: Rust crate, std only. `osmium-build` builds the repo and the ISO, `osmium-install` and
  `osmium-postinstall` install the system, the rest are the tools below
- `settings/`: `osmium-settings`, an iced GUI
- `keys/`: the public signing key, once there is one (see Signing)
- `docs/hyprland-rust-gaps.md`: what the compositor still lacks for the shipped desktops

## Live ISO

Boots straight into the `ii` desktop as the user `osmium` (no password, passwordless sudo) and
opens the installer. If this hyprland-rust cannot load `ii`, the session falls back to
`synthwave`; if the compositor cannot start at all (a VM without 3D), the console says what to
change. UEFI only.

## Installer

`osmium-installer` is a window built from ii's own widgets
(`~/.config/quickshell/ii/osmium-installer.qml`). It asks for a disk (optional LUKS), region,
keyboard, user and desktop (built-in dots or one found on GitHub), then runs
`sudo osmium-install run`, which:

- partitions GPT: 1 GiB ESP and btrfs with `@ @home @cache @log`, zstd:1,
- unpacks the live squashfs with every core (`unsquashfs`): no downloads, no internet needed,
- configures locale, keyboard and the user, and runs `osmium-postinstall` (removes the live
  user and files, locks root, sets up the pacman keyring and the `[osmium]` repo, enables greetd),
- builds a zstd -1 initramfs (no fallback image), installs GRUB with grub-btrfs, applies the
  chosen dots with the chosen keyboard layout, and takes a first snapshot named `Kurulum`,
  in parallel where possible.

With LUKS, GRUB asks for the passphrase before any keymap is loaded and reads the keys as a US
keyboard. For the Turkish, German and French layouts the installer therefore adds a second key
slot holding what the same key presses type on a US layout, so the passphrase opens the disk at
boot exactly as it was typed in the installer. The initramfs carries a key file, so the
passphrase is asked once.

`osmium-install disks` lists disks as JSON; `osmium-install check` validates a config read from
stdin (`key=value` lines). For manual partitioning the ISO also has Calamares (`sudo calamares`).

## The installed system

- Login: greetd with tuigreet, session `osmium-dots session`. Root is locked; admin goes
  through sudo (group `wheel`).
- Snapshots: snapper on `/` (timeline and cleanup timers, snap-pac around every pacman
  transaction), bootable from the GRUB menu through grub-btrfs.
- Swap is compressed RAM (zram, no swap file), with matching sysctls. I/O schedulers are set per
  device type, the journal is capped at 300 MB, and weekly TRIM, systemd-oomd and the reflector
  timer are on. Units that only delay boot (waiting for the network or NTP) are off.
- Packages come from Arch plus the `[osmium]` repo, served from this repository's `repo` release.
- Ollama is installed and enabled for the offline assistant; it idles until a model is pulled.
- Per-user timers snapshot `~/.config` every 10 minutes and check updates for risks once a day.
  Opt out with `systemctl --user disable osmium-history.timer osmium-update.timer`.

## Desktops (dots)

`osmium-dots` applies a ready desktop to your home:

| id | What it is |
|----|------------|
| `ii` (default) | end-4's illogical-impulse on Quickshell: AI sidebar (local Ollama model first, Gemini optional), settings app, overview, Material You colors from the wallpaper |
| `waffle` | ii's Windows 11 style panel family ("Windoes") |
| `m3ww` | end-4's archived Material You EWW style, patched for hyprland-rust |
| `synthwave` | OsmiumOS's own 80s synthwave theme (ironbar, anyrun, alacritty) |

```
osmium-dots list
osmium-dots apply waffle
osmium-dots search catppuccin                         # GitHub repos with topic "hyprland"
osmium-dots apply github:owner/repo                   # copies only its config dirs
osmium-dots apply github:owner/repo --mode script     # shows its install script, runs after `çalıştır`
```

Replaced `~/.config/<dir>` entries go to `~/.local/state/osmium/dots-backup/<time>/`. The login
session applies `ii` on first login if nothing was applied yet. For a GitHub repo, `config` mode
lists the programs its config starts that are not installed and offers their packages.
end-4's dots are pinned in `packages/osmium-dots-ii` and `packages/osmium-dots-m3ww` (GPL-3.0).

## Tools

Everything that changes the system shows what it will do and waits for `evet`.

Desktop:

- `osmium-mode` (`Super+M` in synthwave): switch Normal / Oyun / Çalışma / Sunum. A mode sets
  compositor options, power profile, notifications, idle lock and apps together. Add your own
  as `~/.config/osmium/modes/<id>.mode` (keys: `name`, `power`, `notifications`, `idle`, `hypr`,
  `exec`).
- `osmium-auto`: switches modes from context. Rules in `~/.config/osmium/auto.rules`
  (`osmium-auto init` writes an example), e.g. `oyun: uygulama=steam`, `sunum: ekran=2`,
  `calisma: wifi=Ofis ve saat=09:00-18:00`, also `pil<20`, `priz`, `pilde`. A mode you set by
  hand wins until the matching rules change. `osmium-auto check` shows which rule matches and why.
- `osmium-battery hedef 4s`: battery budget. Steps down power profile, brightness and refresh
  rate until the battery lasts 4 hours, and back up when there is room. `osmium-battery kapat`
  restores everything.
- `osmium-settings` (`Super+I` in synthwave; ii has its own settings app): appearance (gaps,
  rounding, blur, animations, keyboard), modes, config history and update checks. Appearance goes
  to `~/.config/hypr/osmium-settings.lua`.
- `osmium-share`: `export [name]` bundles hypr, ironbar, anyrun, alacritty, swaync and mode
  configs into one text file; `publish <file>` makes it a public gist; `try <link>` shows every
  line that can start a program, applies the theme and rolls back unless you confirm within 30 s;
  `undo` restores what the last `try` replaced.
- `osmium-welcome`: the first-login screen with the shortcuts.

System:

- `osmium-update`: before upgrading, reads Arch News and the bug reports opened in the last
  7 days for the pending packages. A risky update is held as a whole (never a partial upgrade)
  for up to 3 days; `--force` skips the hold. Then it rehearses: it upgrades a btrfs snapshot of
  the running system, boots it in a container and compares failed units with the live system.
  Only a clean rehearsal upgrades the real system, with exactly the rehearsed versions.
  `osmium-update prova` rehearses only, `--provasiz` skips the rehearsal, `check` only reports.
  Drivers and the kernel itself are not exercised in a container.
- `osmium-doctor`: health check (failed units, full disks, btrfs errors, stale pacman lock,
  missing package files, orphans, journal errors, `.pacnew` files, package cache, snapshots,
  memory). `onar` offers each fix after an `evet`; `--ai` lets the offline model explain it.
- `osmium-changes`: what changed since a snapshot (default: since yesterday): packages, kernel,
  `/etc` files, enabled units. `osmium-changes --ai "dünden beri sistem neden yavaş?"` asks the
  offline model; `osmium-changes undo <n>` rolls back with snapper.
- `osmium-history`: every 10 minutes `~/.config` is snapshotted into a private local git
  history. `osmium-history log`, `diff <id>`, `restore <id> [path]`; a restore can be undone.
- `osmium-apply [dosya]`: declarative system file `~/.config/osmium/osmium.toml` (packages,
  dots, mode, services, hostname, timezone). `plan` shows the difference, `export` writes the
  current machine, leaving out what the ISO installed.
- `osmium-hw`: detects the GPU and virtual machine; `osmium-hw kur` installs the missing drivers.
- `osmium-try <paket>... [-- komut]`: installs and runs packages in a throwaway snapshot of the
  root (`systemd-nspawn --ephemeral`); nothing remains after it closes.
- `osmium-guest start`: a guest desktop on another VT with a tmpfs home; everything is deleted
  when it ends. `osmium-guest cleanup` ends it by hand.

Assistant and migration:

- `osmium-ai`: offline assistant on a local Ollama model (`osmium-ai setup`, `ask`, `fix`, or no
  argument for a chat). Only localhost is accepted as the model server. It only proposes
  commands; each one runs only after you type `evet` (dangerous ones: `çalıştır`).
- `osmium-migrate`: reads a Windows disk read-only and copies documents, wallpaper, user fonts
  and Wi-Fi names (passwords cannot be read offline), and maps installed Windows programs to
  Linux packages or alternatives. `osmium-migrate scan`, then `osmium-migrate all <windows-user>`.

## Compositor

OsmiumOS runs hyprland-rust, never upstream Hyprland.
`.github/workflows/hyprland-rust-bump.yml` checks its `main` every hour. When a newer commit's CI
is green, it pins `packages/hyprland-rust/PKGBUILD` to that commit and starts a build. Red or
unfinished commits are skipped. Run it by hand from the Actions tab.

CI loads every shipped dot with `hyprland-rust --check-config` and sends every mode's options
to a headless compositor. Errors in third-party dots are warnings, so they never block the ISO.
What the compositor still lacks is in `docs/hyprland-rust-gaps.md`.

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
General Public License, version 3 or any later version. The Calamares branding files in
`packages/osmium-calamares-config/etc/calamares/branding/osmium/` (images, `show.qml`) come
from Calamares under the same license, as their `.license` files and headers say.
`hyprland-rust` keeps its own license (BSD-3-Clause, Smithay parts MIT); end-4's dots are GPL-3.0.
