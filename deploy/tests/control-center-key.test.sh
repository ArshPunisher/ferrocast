#!/usr/bin/env bash
# Tests deploy/control-center-key.sh in temporary relay and TransformX folders, without Docker:
#   deploy/tests/control-center-key.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../control-center-key.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -q -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }
hasnt() { if grep -q -- "$3" <<<"$2"; then bad "$1: found '$3'"; else ok "$1"; fi; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fresh() {
  rm -rf "$tmp/relay" "$tmp/tx"
  mkdir -p "$tmp/relay/state" "$tmp/tx/deploy/state"
  printf 'RELAY_PUBLIC_URL=https://ferrocast-relay.clearcraftlabs.com\nRELAY_VAULT_KEY=vk\n' >"$tmp/relay/relay.env"
  printf 'POSTGRES_PASSWORD=pg\nHOOKLINE_INTERNAL_TOKEN=hl\n' >"$tmp/tx/.env.prod"
  chmod 644 "$tmp/relay/relay.env" "$tmp/tx/.env.prod"
}
run() { # [args…] → output in $out, status in $code
  set +e
  out="$(RELAY_DIR="$tmp/relay" TX_DIR="$tmp/tx" NO_RESTART=1 "$script" "$@" 2>&1 </dev/null)"
  code=$?
  set -e
}
rv() { grep -E "^$1=" "$tmp/relay/relay.env" | tail -n 1 | cut -d= -f2-; }
tv() { grep -E "^$1=" "$tmp/tx/.env.prod" | tail -n 1 | cut -d= -f2-; }
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
backups() { find "$tmp/relay/state" "$tmp/tx/deploy/state" -name 'env-backup-*' | wc -l | tr -d ' '; }

echo "control-center-key.sh"
fresh
run --status
is "status works before a token" "$code" 0
has "status says the relay's is missing" "$out" "RELAY_ADMIN_TOKEN): *missing"
has "status says the compose file doesn't read it yet" "$out" "reads it: *no"

run
is "makes a token" "$code" 0
token="$(rv RELAY_ADMIN_TOKEN)"
if [[ "$token" =~ ^[0-9a-f]{64}$ ]]; then ok "the token is 64 hex characters"; else bad "the token is 64 hex characters"; fi
is "the Control Center gets the same token" "$(tv FERROCAST_ADMIN_TOKEN)" "$token"
hasnt "never prints the token" "$out" "$token"
is "relay.env is 0600" "$(mode "$tmp/relay/relay.env")" 600
is ".env.prod is 0600" "$(mode "$tmp/tx/.env.prod")" 600
is "relay settings kept" "$(rv RELAY_VAULT_KEY)" vk
is "TransformX settings kept" "$(tv HOOKLINE_INTERNAL_TOKEN)" hl
is "both were backed up" "$(backups)" 2
is "one line each" "$(grep -c '^RELAY_ADMIN_TOKEN=' "$tmp/relay/relay.env")$(grep -c '^FERROCAST_ADMIN_TOKEN=' "$tmp/tx/.env.prod")" 11
is "no temporary copies (they hold the token) are left" "$(find "$tmp" -name 'env.*' | wc -l | tr -d ' ')" 0

run
is "running again works" "$code" 0
has "running again changes nothing" "$out" "nothing changed"
is "the token stays" "$(rv RELAY_ADMIN_TOKEN)" "$token"
is "no new backups" "$(backups)" 2

run --status
has "status says both are set" "$out" "FERROCAST_ADMIN_TOKEN): *set"
has "status says they match" "$out" "match: *yes"
hasnt "status hides the token" "$out" "$token"
# shellcheck disable=SC2016 # a compose file's own ${…}, written as is
printf 'x\n  FERROCAST_ADMIN_TOKEN: ${FERROCAST_ADMIN_TOKEN:-}\n' >"$tmp/tx/docker-compose.prod.yml"
run --status
has "status sees the compose file reads it" "$out" "reads it: *yes"

# The relay has one, the Control Center lost it (or never had it): copied over, relay untouched.
before="$(cat "$tmp/relay/relay.env")"
grep -v '^FERROCAST_ADMIN_TOKEN=' "$tmp/tx/.env.prod" >"$tmp/tx/x" && cat "$tmp/tx/x" >"$tmp/tx/.env.prod" && rm "$tmp/tx/x"
run
is "copies the relay's token over" "$code" 0
has "says the relay keeps it" "$out" "the relay keeps it"
is "the Control Center has it again" "$(tv FERROCAST_ADMIN_TOKEN)" "$token"
is "relay.env untouched" "$(cat "$tmp/relay/relay.env")" "$before"

# Different values on the two sides: the relay's wins.
printf 'FERROCAST_ADMIN_TOKEN=%s\n' "$(printf 'b%.0s' {1..64})" >>"$tmp/tx/.env.prod"
run --status
has "status spots a mismatch" "$out" "match: *NO"
run
is "fixes a mismatch" "$(tv FERROCAST_ADMIN_TOKEN)" "$token"
is "still one line" "$(grep -c '^FERROCAST_ADMIN_TOKEN=' "$tmp/tx/.env.prod")" 1

run --rotate
is "rotates" "$code" 0
new="$(rv RELAY_ADMIN_TOKEN)"
if [[ "$new" != "$token" && "$new" =~ ^[0-9a-f]{64}$ ]]; then ok "a new token"; else bad "a new token"; fi
is "both sides rotate together" "$(tv FERROCAST_ADMIN_TOKEN)" "$new"
hasnt "rotating doesn't print it" "$out" "$new"

# A short relay token was never usable: replaced, not copied.
fresh
printf 'RELAY_ADMIN_TOKEN=short\n' >>"$tmp/relay/relay.env"
run --status
has "status spots an unusable token" "$out" "unusable"
run
is "replaces an unusable token" "$code" 0
if [[ "$(rv RELAY_ADMIN_TOKEN)" =~ ^[0-9a-f]{64}$ ]]; then ok "the new one is good"; else bad "the new one is good"; fi
is "and shared" "$(tv FERROCAST_ADMIN_TOKEN)" "$(rv RELAY_ADMIN_TOKEN)"

# A write that fails halfway (disk full): both files are put back.
fresh
rb="$(cat "$tmp/relay/relay.env")"
tb="$(cat "$tmp/tx/.env.prod")"
mkdir -p "$tmp/bin"
# shellcheck disable=SC2016 # the stub's own $1 and $@, expanded when it runs
printf '#!/bin/sh\ncase "$1" in */deploy/state/env.*) exit 1 ;; esac\nexec /bin/cat "$@"\n' >"$tmp/bin/cat"
chmod +x "$tmp/bin/cat"
set +e
out="$(PATH="$tmp/bin:$PATH" RELAY_DIR="$tmp/relay" TX_DIR="$tmp/tx" NO_RESTART=1 "$script" 2>&1 </dev/null)"
code=$?
set -e
is "a failed write fails the run" "$code" 1
has "says both were put back" "$out" "put back as they were"
is "relay.env is exactly as before" "$(cat "$tmp/relay/relay.env")" "$rb"
is ".env.prod is exactly as before" "$(cat "$tmp/tx/.env.prod")" "$tb"
is "no temporary copy is left" "$(find "$tmp/relay" "$tmp/tx" -name 'env.*' | wc -l | tr -d ' ')" 0

run --nope
is "refuses an unknown option" "$code" 1
rm -f "$tmp/tx/.env.prod"
run
is "refuses without TransformX's settings" "$code" 1
has "says where the Control Center runs" "$out" "Control Center runs from"
rm -rf "$tmp/relay"
run
is "refuses without a relay folder" "$code" 1
has "says to install the relay first" "$out" "install the relay first"

if ((fails)); then
  echo "$fails failed"
  exit 1
fi
echo "All control-center-key.sh tests passed."
