#!/usr/bin/env bash
# Builds every packages/*/PKGBUILD into ./repo as the "osmium" pacman repo. Run as root on Arch.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
OUT="$ROOT/repo"
rm -rf "$OUT" && mkdir -p "$OUT"
id builder &>/dev/null || useradd -m builder
chown -R builder "$ROOT/packages"

for dir in "$ROOT"/packages/*/; do
  # shortcut: -d skips dependency install, fine for config-only packages; switch to -s + sudo when a package needs build deps.
  (cd "$dir" && sudo -u builder makepkg -d -f --noconfirm)
  mv "$dir"/*.pkg.tar.zst "$OUT/"
done

repo-add "$OUT/osmium.db.tar.gz" "$OUT"/*.pkg.tar.zst
# GitHub release assets cannot be symlinks.
for f in db files; do rm "$OUT/osmium.$f" && cp "$OUT/osmium.$f.tar.gz" "$OUT/osmium.$f"; done
