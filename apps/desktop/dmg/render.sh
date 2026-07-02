#!/usr/bin/env bash
# Renders background.html to background.tiff (1× and 2× in one file, so Finder stays sharp
# on Retina screens). Needs ferrocast-web next to this repo, installed and built (fonts).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
node "$here/render.mjs" "$tmp"
tiffutil -cathidpicheck "$tmp/bg1.png" "$tmp/bg2.png" -out "$here/background.tiff" >/dev/null

echo "wrote $here/background.tiff"
