#!/usr/bin/env bash
# Tests deploy/dashboard-swap.sh on a temporary folder (no ssh, no root):
#   deploy/tests/dashboard-swap.test.sh
# The mounted folder must stay the same folder (a Docker bind mount follows it), `current`
# must always point at a whole build, and old releases and the old layout must go.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../dashboard-swap.sh"
deploy="$here/../deploy-relay.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
expect() { # expect <name> <command...>
  local name="$1"
  shift
  if "$@"; then ok "$name"; else bad "$name"; fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
dash="$work/dashboard"
me="$(id -un):$(id -gn)"
# The old layout: a build unpacked straight into the mounted folder.
mkdir -p "$dash/assets"
echo old >"$dash/index.html"
echo old >"$dash/assets/app-0.js"
# shellcheck disable=SC2012 # one known folder; `stat` flags differ between macOS and Linux
inode() { ls -di "$dash" | awk '{print $1}'; }
before="$(inode)"
same_folder() { [[ "$(inode)" == "$before" ]]; }
live_is() { [[ "$(cat "$dash/current/index.html")" == "$1" && "$(cat "$dash/current/assets/app.js")" == "$1" ]]; }
link_is() { [[ "$(readlink "$dash/current")" == "$1" ]]; }
exists() { [[ -e "$1" ]]; }
missing() { [[ ! -e "$1" ]]; }
nothing_half_made() { [[ -z "$(find "$dash" -name '.part-*' -o -name current.next)" ]]; }

build() { # build <text>: a fake dashboard build as tar.gz on stdout
  rm -rf "$work/src" && mkdir -p "$work/src/assets"
  echo "$1" >"$work/src/index.html"
  echo "$1" >"$work/src/assets/app.js"
  tar -C "$work/src" -czf - .
}

build one | bash "$script" "$dash" 20261019100000 "$me" >/dev/null
expect "the first release is live" live_is one
expect "current is a relative link into releases/" link_is releases/20261019100000
expect "the mounted folder is the same folder" same_folder
expect "the old layout stays until --tidy (the old relay may still serve it)" exists "$dash/index.html"

bash "$script" --tidy "$dash"
expect "--tidy drops the old layout" missing "$dash/index.html"
expect "--tidy drops the old assets" missing "$dash/assets"
expect "--tidy keeps current" live_is one

build two | bash "$script" "$dash" 20261019110000 "$me" >/dev/null
build three | bash "$script" "$dash" 20261019120000 "$me" >/dev/null
expect "the newest release is live" live_is three
expect "the one before stays (roll back)" exists "$dash/releases/20261019110000"
expect "older ones go" missing "$dash/releases/20261019100000"
expect "still the same mounted folder" same_folder
expect "no half-made release or link left" nothing_half_made

# A broken build (no index.html) never goes live.
rm -rf "$work/src" && mkdir -p "$work/src"
echo x >"$work/src/other.txt"
if tar -C "$work/src" -czf - . | bash "$script" "$dash" 20261019130000 "$me" 2>/dev/null; then
  bad "a build without index.html is refused"
else
  ok "a build without index.html is refused"
fi
expect "the live release didn't change" live_is three
expect "the refused build left nothing" nothing_half_made

# Names that could leave the folder are refused.
if build four 2>/dev/null | bash "$script" "$dash" "../x" "$me" 2>/dev/null; then
  bad "a release name with a path is refused"
else
  ok "a release name with a path is refused"
fi

# deploy-relay.sh uses it and points the relay at the link.
expect "deploy-relay.sh runs dashboard-swap.sh" grep -q "dashboard-swap.sh \$dir/dashboard \$release" "$deploy"
expect "deploy-relay.sh tidies after the restart" grep -q "dashboard-swap.sh --tidy \$dir/dashboard" "$deploy"
expect "deploy-relay.sh sets RELAY_DASHBOARD_DIR=/dashboard/current" grep -q 'RELAY_DASHBOARD_DIR=/dashboard/current' "$deploy"
if grep -q "mv \$dir/dashboard" "$deploy"; then
  bad "deploy-relay.sh never renames the mounted folder"
else
  ok "deploy-relay.sh never renames the mounted folder"
fi

if ((fails > 0)); then
  echo "$fails dashboard-swap.sh test(s) failed." >&2
  exit 1
fi
echo "All dashboard-swap.sh tests passed."
