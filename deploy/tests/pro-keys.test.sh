#!/usr/bin/env bash
# Tests deploy/pro-keys.sh in a temporary relay folder, without Docker:
#   deploy/tests/pro-keys.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../pro-keys.sh"
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
  printf 'RELAY_PUBLIC_URL=https://ferrocast-relay.clearcraftlabs.com\nRELAY_VAULT_KEY=keep\n' >"$tmp/relay/relay.env"
  chmod 600 "$tmp/relay/relay.env"
}
run() { # <stdin> [args…] → output in $out, status in $code
  local input="$1"
  shift
  set +e
  out="$(printf '%b' "$input" | RELAY_DIR="$tmp/relay" NO_RESTART=1 "$script" "$@" 2>&1)"
  code=$?
  set -e
}
v() { grep -E "^$1=" "$tmp/relay/relay.env" | tail -n 1 | cut -d= -f2-; }
backups() { find "$tmp/relay/state" -name 'env-backup-*' | wc -l | tr -d ' '; }

tts="AIza$(printf 'x%.0s' {1..35})"
app="1561463863197835294"
pub="$(printf 'ab%.0s' {1..32})"
secret="Abc_def-ghijklmnopqrstuvwxyz0123"
# A made-up token in the right shape, built in parts so it never looks like a real one.
bot="$(printf 'f%.0s' {1..24}).$(printf 'g%.0s' {1..6}).$(printf 'h%.0s' {1..38})"

echo "pro-keys.sh"
fresh_relay
run "" --status
is "status works with nothing set" "$code" 0
has "status says missing" "$out" "GOOGLE_TTS_KEY: *missing"

run "\n\n\n\n\n"
is "Enter everywhere changes nothing" "$code" 0
has "says nothing changed" "$out" "as it was"
is "no backup for nothing" "$(backups)" 0

run "$tts\n$app\n$pub\n$secret\n$bot\n"
is "saves all five" "$code" 0
is "tts key saved" "$(v GOOGLE_TTS_KEY)" "$tts"
is "app id saved" "$(v DISCORD_APP_ID)" "$app"
is "public key saved" "$(v DISCORD_PUBLIC_KEY)" "$pub"
is "client secret saved" "$(v DISCORD_CLIENT_SECRET)" "$secret"
is "bot token saved" "$(v DISCORD_BOT_TOKEN)" "$bot"
is "other settings kept" "$(v RELAY_VAULT_KEY)" keep
is "relay.env stays 0600" "$(stat -c %a "$tmp/relay/relay.env" 2>/dev/null || stat -f %Lp "$tmp/relay/relay.env")" 600
is "a backup was taken" "$(backups)" 1
for s in "$tts" "$pub" "$secret" "$bot"; do hasnt "never prints a key" "$out" "$s"; done

run "\n\n\n\n\n"
is "running again with Enter keeps them" "$(v DISCORD_BOT_TOKEN)" "$bot"
has "says they're set" "$out" "set; paste a new one"

run "" --status
has "status says set" "$out" "DISCORD_BOT_TOKEN: *set"
hasnt "status hides keys" "$out" "$bot"

fresh_relay
run "nope\nAIzaTooShort\nstill-wrong\n" --tts
is "three bad tries fail" "$code" 1
has "says nothing changed" "$out" "Nothing was changed"
is "nothing saved" "$(v GOOGLE_TTS_KEY)" ""

run "bad\n$tts\n" --tts
is "a bad try then a good one works" "$code" 0
is "the good one is saved" "$(v GOOGLE_TTS_KEY)" "$tts"
is "one line only" "$(grep -c '^GOOGLE_TTS_KEY=' "$tmp/relay/relay.env")" 1

fresh_relay
run "$app\n\n\n\n" --discord
is "part of the Discord keys works" "$code" 0
has "warns the bot stays off" "$out" "stays off until all four"

fresh_relay
run "x\n" --nope
is "unknown option fails" "$code" 1

if ((fails > 0)); then
  echo "$fails failed."
  exit 1
fi
echo "All pro-keys.sh tests passed."
