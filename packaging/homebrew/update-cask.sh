#!/usr/bin/env bash
# Points the cask at a release: update-cask.sh <version> <path to the universal .dmg>
set -euo pipefail
version="${1:?usage: update-cask.sh <version> <dmg>}"
dmg="${2:?usage: update-cask.sh <version> <dmg>}"
cask="$(cd "$(dirname "$0")" && pwd)/Casks/ferrocast.rb"
sum="$(shasum -a 256 "$dmg" | cut -d' ' -f1)"
sed -i.bak -E "s/^  version \".*\"/  version \"$version\"/; s/^  sha256 \".*\"/  sha256 \"$sum\"/" "$cask"
rm -f "$cask.bak"
echo "ferrocast $version → sha256 $sum"
