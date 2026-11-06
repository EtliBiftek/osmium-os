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

Live user: `osmium` (passwordless sudo).

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
- `osmium-history`: every 10 minutes `~/.config` is snapshotted locally.
  `osmium-history log`, `osmium-history diff <id>`, `osmium-history restore <id> [path]`.

## License

MIT, see `LICENSE`. Exception: the Calamares branding files in
`packages/osmium-calamares-config/etc/calamares/branding/osmium/` (images, `show.qml`) are
GPL-3.0-or-later from Calamares, as their `.license` files and headers say.
