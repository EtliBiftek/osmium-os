# OsmiumOS

Arch Linux based distribution. Editions: Hyprland (KDE and GNOME planned).

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
- `osmium-history`: every 10 minutes `~/.config` is snapshotted locally.
  `osmium-history log`, `osmium-history diff <id>`, `osmium-history restore <id> [path]`.
