#!/usr/bin/env bash
# Writes latest.json, the signed update feed the desktop app checks (R-60), from the
# release's updater bundles and their .sig files in <dir>.
#
#   packaging/updater-manifest.sh <version> <dir> > latest.json
#
# Works the same for betas (0.1.0-beta.1): the URLs point at that tag's own release, and
# the Release workflow publishes the beta feed separately (packaging/beta-feed.sh).
set -euo pipefail
version=${1:?usage: updater-manifest.sh <version> <dir>}
dir=${2:?usage: updater-manifest.sh <version> <dir>}
"$(dirname "$0")/semver.sh" valid "$version" || {
  echo "'$version' isn't a version (MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-beta.N)" >&2
  exit 1
}
base="https://github.com/ArshPunisher/ferrocast/releases/download/v${version}"

entry() { # <platform key> <file>
  local file=$2
  [[ -f "$dir/$file" && -f "$dir/$file.sig" ]] || return 0
  printf '"%s":{"signature":%s,"url":"%s/%s"}' "$1" "$(jq -Rs 'rtrimstr("\n")' < "$dir/$file.sig")" "$base" "$file"
}

shopt -s nullglob
mac=( "$dir"/*.app.tar.gz ); win=( "$dir"/*-setup.exe ); lin=( "$dir"/*.AppImage )
deb=( "$dir"/*.deb ); rpm=( "$dir"/*.rpm )
parts=()
if (( ${#mac[@]} )); then
  m=$(basename "${mac[0]}")
  # One universal build serves both Mac chips.
  parts+=("$(entry darwin-aarch64 "$m")" "$(entry darwin-x86_64 "$m")")
fi
(( ${#win[@]} )) && parts+=("$(entry windows-x86_64 "$(basename "${win[0]}")")")
(( ${#lin[@]} )) && parts+=("$(entry linux-x86_64 "$(basename "${lin[0]}")")")
# deb and rpm installs look for their own entry first, so they never get an AppImage.
(( ${#deb[@]} )) && parts+=("$(entry linux-x86_64-deb "$(basename "${deb[0]}")")")
(( ${#rpm[@]} )) && parts+=("$(entry linux-x86_64-rpm "$(basename "${rpm[0]}")")")
platforms=$(IFS=,; echo "${parts[*]:-}" | sed 's/,,*/,/g; s/^,//; s/,$//')
if [[ -z "$platforms" ]]; then
  echo "No signed updater bundles in $dir (is TAURI_SIGNING_PRIVATE_KEY set?)" >&2
  exit 1
fi
jq -n --arg v "$version" --arg d "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson p "{$platforms}" \
  '{version: $v, notes: "See the release notes on GitHub.", pub_date: $d, platforms: $p}'
