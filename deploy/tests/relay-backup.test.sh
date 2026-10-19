#!/usr/bin/env bash
# Tests deploy/relay-backup.sh with a stand-in docker (no server, no R2):
#   deploy/tests/relay-backup.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../relay-backup.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -q -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }
hasnt() { if grep -q -- "$3" <<<"$2"; then bad "$1: '$3' in: $2"; else ok "$1"; fi; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/relay/state"
echo 'RELAY_VAULT_KEY=x' >"$tmp/relay/relay.env"
# The stub logs every call; `inspect` names the volume unless GONE=1; `lsf` lists one backup.
# shellcheck disable=SC2016 # the stub's own variables, expanded when it runs
printf '#!/bin/sh\necho "$@" >>"%s/calls"\ncase "$*" in\n  inspect*) [ "${GONE:-0}" = 1 ] || echo relay_data ;;\n  *" lsf "*) echo 2026-10-19/ ;;\nesac\n' "$tmp" >"$tmp/bin/docker"
chmod +x "$tmp/bin/docker"
run() { # [args…] → output in $out, status in $code
  rm -f "$tmp/calls"
  set +e
  out="$(PATH="$tmp/bin:$PATH" RELAY_DIR="$tmp/relay" "$script" "$@" 2>&1 </dev/null)"
  code=$?
  set -e
}
calls() { cat "$tmp/calls" 2>/dev/null || true; }

echo "relay-backup.sh"
run --status
is "status works before setup" "$code" 0
has "status says it isn't set up" "$out" "run --setup"
run
is "a backup before setup fails" "$code" 1
has "and says to set up first" "$out" "--setup first"
run --setup
is "setup needs a terminal" "$code" 1
has "and says to use ssh -t" "$out" "ssh -t"
run --nope
is "an unknown option fails" "$code" 1
has "with the usage" "$out" "Use: relay-backup.sh"

touch "$tmp/relay/state/rclone.conf"
run
is "a backup runs" "$code" 0
c="$(calls)"
has "copies the database with SQLite's backup API" "$c" "python:3.13-alpine python -c"
has "uploads the copy and relay.env under today's date" "$c" "copy /src box:db/$(date -u +%F)"
has "mirrors the blobs" "$c" "sync /data/blobs box:blobs"
has "drops copies older than 30 days" "$c" "delete box:db --min-age 30d"
is "records the last good run" "$(test -s "$tmp/relay/state/backup-last-ok" && echo yes)" yes
hasnt "never sends to R2 unencrypted" "$c" " r2:"
is "leaves no work folder behind" "$(find "$tmp/relay/state" -name 'backup.*' | wc -l | tr -d ' ')" 0

GONE=1 run
is "no relay container: fails" "$code" 1
has "and says why" "$out" "isn't there"

run --status
has "status shows the last run" "$out" "Last good backup:"

if [[ $fails -gt 0 ]]; then
  echo "$fails failed"
  exit 1
fi
echo "all passed"
