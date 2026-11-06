#!/usr/bin/env bash
# Builds packages/*/PKGBUILD plus pinned AUR packages (packages/aur.txt) into ./repo as the "osmium" pacman repo.
# Run as root on Arch. OSMIUM_REPO: reuse already-published package files instead of rebuilding them.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")" && pwd)"
OUT="$ROOT/repo"
WORK="$ROOT/work/pkg"
rm -rf "$OUT" "$WORK" && mkdir -p "$OUT" "$WORK"
id builder &>/dev/null || useradd -m builder
echo 'builder ALL=(ALL) NOPASSWD: /usr/bin/pacman' > /etc/sudoers.d/builder

# build <dir> <makepkg flags>: skips the build when every output file is already published.
build() {
  local dir=$1 flags=$2 pkg name cached=1
  chown -R builder "$dir"
  for pkg in $(cd "$dir" && sudo -u builder makepkg --packagelist); do
    name=$(basename "$pkg")
    if [[ -n ${OSMIUM_REPO:-} ]] && curl -fsL -o "$OUT/$name" "$OSMIUM_REPO/$name"; then
      echo "cached: $name"
    else
      rm -f "$OUT/$name"; cached=0
    fi
  done
  (( cached )) && return 0
  (cd "$dir" && sudo -u builder makepkg $flags -f --noconfirm)
  mv "$dir"/*.pkg.tar.zst "$OUT/"
}

while read -r name commit; do
  [[ -z $name || $name == \#* ]] && continue
  git clone -q "https://aur.archlinux.org/$name.git" "$WORK/$name"
  git -C "$WORK/$name" -c advice.detachedHead=false checkout -q "$commit"
  build "$WORK/$name" -s
done < "$ROOT/packages/aur.txt"

for dir in "$ROOT"/packages/*/; do
  cp -r "$dir" "$WORK/"
  dir="$WORK/$(basename "$dir")"
  # Every CI run gets a new pkgrel so pacman sees config changes as upgrades.
  sed -i "s/^pkgrel=.*/pkgrel=${GITHUB_RUN_NUMBER:-1}/" "$dir/PKGBUILD"
  build "$dir" -d
done

# Calamares silently drops its Python modules when Python is missing at build time.
bsdtar -tf "$OUT"/calamares-[0-9]*.pkg.tar.zst | grep -q 'modules/bootloader/main.py' \
  || { echo "calamares package has no Python modules" >&2; exit 1; }

repo-add "$OUT/osmium.db.tar.gz" "$OUT"/*.pkg.tar.zst
# GitHub release assets cannot be symlinks.
for f in db files; do rm "$OUT/osmium.$f" && cp "$OUT/osmium.$f.tar.gz" "$OUT/osmium.$f"; done
