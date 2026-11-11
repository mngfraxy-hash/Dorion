#!/usr/bin/env bash
# Copy locally built shelter plugins into the binary (see src-tauri/src/functionality/customize.rs).
# Usage: scripts/bundle-plugins.sh [path to shelter-plugins checkout, default ../shelter-plugins]
set -euo pipefail

SRC="${1:-../shelter-plugins}"
DEST="src-tauri/injection/bundled"

(cd "$SRC" && pnpm lune ci)

for p in dorion-settings dorion-helpers inline-css; do
  mkdir -p "$DEST/$p"
  cp "$SRC/dist/$p/plugin.js" "$SRC/dist/$p/plugin.json" "$DEST/$p/"
done

echo "Bundled: $(ls "$DEST" | tr '\n' ' ')"
