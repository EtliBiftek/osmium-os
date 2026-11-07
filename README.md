# OsmiumOS

Arch Linux based distribution. Editions: Hyprland, KDE Plasma, GNOME.

## Build

GitHub Actions builds the ISO on every push to `main`. Download it from the run's artifacts.

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
- `tools/`: Rust crate. `osmium-build` builds the repo and ISOs, `osmium-postinstall` finishes installs

Live user: `osmium`.

## Tools

- `osmium-mode` (`Super+M`): switch Normal / Oyun / Çalışma / Sunum modes. Add your own as
  `~/.config/osmium/modes/<id>.mode` (keys: `name`, `power`, `notifications`, `idle`, `hypr`, `exec`).
- `osmium-update`: before `pacman -Syu`, reads Arch News and bug reports opened in the last 7 days
  for the pending packages. A risky update is held as a whole (never a partial upgrade) for up to
  3 days; `osmium-update --force` skips the hold. A daily timer sends a notification.
- `osmium-share`: `export [name]` bundles hypr, ironbar, anyrun, alacritty, swaync and mode configs
  into one text file; `publish <file>` makes it a public gist link; `try <link>` shows every line
  that can start a program, applies the theme and rolls back unless you confirm within 30 s;
  `undo` restores what the last `try` replaced.
- `osmium-settings` (`Super+I`): GUI for appearance (gaps, rounding, blur, animations, keyboard),
  modes, config history and update checks. Appearance goes to `~/.config/hypr/osmium-settings.lua`.
- `osmium-ai`: offline assistant on a local Ollama model (`osmium-ai setup`, `ask`, `fix`). It only
  proposes commands; each one runs only after you type `evet` (dangerous ones: `çalıştır`).
- `osmium-migrate`: reads a Windows disk read-only and copies documents, wallpaper, user fonts and
  Wi-Fi names (passwords cannot be read offline) and maps installed Windows programs to Linux
  packages or alternatives. `osmium-migrate scan`, then `osmium-migrate all <windows-user>`.
- `osmium-history`: every 10 minutes `~/.config` is snapshotted locally.
  `osmium-history log`, `osmium-history diff <id>`, `osmium-history restore <id> [path]`.

## Compositor

The Hyprland edition runs [hyprland-rust](https://github.com/EtliBiftek/hyprland-rust), not
upstream Hyprland. `.github/workflows/hyprland-rust-bump.yml` checks its `main` every hour. When
a newer commit's CI is green, it pins `packages/hyprland-rust/PKGBUILD` to that commit and starts
an ISO build. Red or unfinished commits are skipped. Run it by hand from the Actions tab.

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

MIT, see `LICENSE`. Exception: the Calamares branding files in
`packages/osmium-calamares-config/etc/calamares/branding/osmium/` (images, `show.qml`) are
GPL-3.0-or-later from Calamares, as their `.license` files and headers say.
