#!/usr/bin/env bash
# Tests deploy/vault-key.sh in a temporary relay folder, without Docker:
#   deploy/tests/vault-key.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../vault-key.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -q -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }
hasnt() { if grep -q -- "$3" <<<"$2"; then bad "$1: found '$3'"; else ok "$1"; fi; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fresh_relay() {
  rm -rf "$tmp/relay"
  mkdir -p "$tmp/relay/state"
  printf 'RELAY_PUBLIC_URL=https://ferrocast-relay.clearcraftlabs.com\nRAZORPAY_KEY_ID=rzp_test_x\nPRO_EMAIL_KEY=abc\n' >"$tmp/relay/relay.env"
  chmod 600 "$tmp/relay/relay.env"
}
run() { # [args…] → output in $out, status in $code
  set +e
  out="$(RELAY_DIR="$tmp/relay" NO_RESTART=1 "$script" "$@" 2>&1 </dev/null)"
  code=$?
  set -e
}
v() { grep -E "^$1=" "$tmp/relay/relay.env" | tail -n 1 | cut -d= -f2-; }
bytes() { printf '%s' "$1" | python3 -I -c 'import base64,sys; print(len(base64.b64decode(sys.stdin.read(), validate=True)))'; }
backups() { find "$tmp/relay/state" -name 'env-backup-*' | wc -l | tr -d ' '; }

echo "vault-key.sh"
fresh_relay
run --status
is "status works before a key" "$code" 0
has "status says it's missing" "$out" "missing"

run
is "makes a key" "$code" 0
key="$(v RELAY_VAULT_KEY)"
is "the key is 32 bytes of base64" "$(bytes "$key")" 32
hasnt "never prints the key" "$out" "$key"
has "says it was saved" "$out" "saved it in relay.env"
is "other settings kept" "$(v PRO_EMAIL_KEY)" abc
is "relay.env stays 0600" "$(stat -c %a "$tmp/relay/relay.env" 2>/dev/null || stat -f %Lp "$tmp/relay/relay.env")" 600
is "a backup was taken" "$(backups)" 1
is "one line only" "$(grep -c '^RELAY_VAULT_KEY=' "$tmp/relay/relay.env")" 1

run
is "running again works" "$code" 0
has "running again keeps it" "$out" "already set"
is "the key never changes" "$(v RELAY_VAULT_KEY)" "$key"
is "no second backup" "$(backups)" 1

run --status
has "status says set" "$out" "key: *set"
hasnt "status hides the key" "$out" "$key"

fresh_relay
printf 'RELAY_VAULT_KEY=not-a-real-key\n' >>"$tmp/relay/relay.env"
run --status
has "status spots an unusable key" "$out" "unusable"
run
is "replaces an unusable key" "$code" 0
has "says why it replaced it" "$out" "wasn't 32 bytes"
is "the new key is usable" "$(bytes "$(v RELAY_VAULT_KEY)")" 32
is "still one line" "$(grep -c '^RELAY_VAULT_KEY=' "$tmp/relay/relay.env")" 1

# A write to relay.env that fails halfway (disk full): the shell has already emptied the
# file when `cat` fails, so the script must put the backup back.
fresh_relay
before="$(cat "$tmp/relay/relay.env")"
mkdir -p "$tmp/bin"
# shellcheck disable=SC2016 # the stub's own $1 and $@, expanded when it runs
printf '#!/bin/sh\ncase "$1" in */state/env.*) exit 1 ;; esac\nexec /bin/cat "$@"\n' >"$tmp/bin/cat"
chmod +x "$tmp/bin/cat"
set +e
out="$(PATH="$tmp/bin:$PATH" RELAY_DIR="$tmp/relay" NO_RESTART=1 "$script" 2>&1 </dev/null)"
code=$?
set -e
is "a failed write fails the run" "$code" 1
has "says relay.env was put back" "$out" "put back as it was"
is "relay.env is exactly as before" "$(cat "$tmp/relay/relay.env")" "$before"
is "no temporary copy (it holds the key) is left" "$(find "$tmp/relay/state" -name 'env.*' | wc -l | tr -d ' ')" 0

run --rotate
is "refuses an unknown option" "$code" 1
rm -rf "$tmp/relay"
run
is "refuses without a relay folder" "$code" 1
has "says to install the relay first" "$out" "install the relay first"

if ((fails)); then
  echo "$fails failed"
  exit 1
fi
echo "All vault-key.sh tests passed."
