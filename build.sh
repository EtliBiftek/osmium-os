#!/usr/bin/env bash
# Usage: ./build.sh <edition>  (needs root + archiso, run on Arch)
set -euo pipefail
EDITION="${1:?edition required, e.g. hyprland}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
[[ -d "$ROOT/editions/$EDITION" ]] || { echo "unknown edition: $EDITION" >&2; exit 1; }
PROFILE="$ROOT/work/profile-$EDITION"

rm -rf "$PROFILE"
mkdir -p "$ROOT/work"
cp -r /usr/share/archiso/configs/releng "$PROFILE"
for layer in "$ROOT/common" "$ROOT/editions/$EDITION"; do
  [[ -d "$layer/airootfs" ]] && cp -rT "$layer/airootfs" "$PROFILE/airootfs"
  [[ -f "$layer/packages.x86_64" ]] && cat "$layer/packages.x86_64" >> "$PROFILE/packages.x86_64"
done
{ echo "EDITION=\"$EDITION\""; cat "$ROOT/common/profiledef.patch.sh"; } >> "$PROFILE/profiledef.sh"

mkarchiso -v -w "$ROOT/work/build-$EDITION" -o "$ROOT/out" "$PROFILE"
