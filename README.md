# OsmiumOS

Arch Linux based distribution. Editions: Hyprland (KDE and GNOME planned).

## Build

GitHub Actions builds the ISO on every push to `main`. Download it from the run's artifacts.

Local build on Arch: `sudo pacman -S archiso && sudo ./build.sh hyprland`

## Layout

- `common/`: files and packages shared by every edition
- `editions/<name>/`: per-edition packages and files
- `build.sh`: layers them on top of the archiso `releng` profile

Live user: `osmium` (passwordless sudo).
