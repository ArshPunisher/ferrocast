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
# A made-up Brevo key in the right shape, built in parts (prefix, 64 hex, suffix).
brevo="xkey""sib-$(printf 'a1%.0s' {1..32})-$(printf 'Zq%.0s' {1..8})"
smtp="xsmtp""sib-$(printf 'a1%.0s' {1..32})-$(printf 'Zq%.0s' {1..8})"

echo "pro-keys.sh"
fresh_relay
run "" --status
is "status works with nothing set" "$code" 0
has "status says missing" "$out" "GOOGLE_TTS_KEY: *missing"

run "\n\n\n\n\n\n"
is "Enter everywhere changes nothing" "$code" 0
has "says nothing changed" "$out" "as it was"
is "no backup for nothing" "$(backups)" 0

run "$tts\n$app\n$pub\n$secret\n$bot\n$brevo\n"
is "saves all six" "$code" 0
is "brevo key saved" "$(v BREVO_API_KEY)" "$brevo"
is "tts key saved" "$(v GOOGLE_TTS_KEY)" "$tts"
is "app id saved" "$(v DISCORD_APP_ID)" "$app"
is "public key saved" "$(v DISCORD_PUBLIC_KEY)" "$pub"
is "client secret saved" "$(v DISCORD_CLIENT_SECRET)" "$secret"
is "bot token saved" "$(v DISCORD_BOT_TOKEN)" "$bot"
is "other settings kept" "$(v RELAY_VAULT_KEY)" keep
is "relay.env stays 0600" "$(stat -c %a "$tmp/relay/relay.env" 2>/dev/null || stat -f %Lp "$tmp/relay/relay.env")" 600
is "a backup was taken" "$(backups)" 1
for s in "$tts" "$pub" "$secret" "$bot" "$brevo"; do hasnt "never prints a key" "$out" "$s"; done

run "\n\n\n\n\n\n"
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
run "$smtp\nxkeysib-short\nnope\n" --mail
is "the SMTP key or a short one is refused" "$code" 1
has "says what it should look like" "$out" "not the SMTP key"
is "nothing saved for mail" "$(v BREVO_API_KEY)" ""
run "$brevo\n" --mail
is "--mail saves the Brevo key" "$code" 0
is "only the Brevo key is asked" "$(grep -c '^BREVO_API_KEY=' "$tmp/relay/relay.env")" 1
hasnt "the mail key isn't printed" "$out" "$brevo"
run "" --status
has "status lists the mail key" "$out" "BREVO_API_KEY: *set"

# The cloud bot's Twitch app (made-up values in the right shape).
tw_id="$(printf 'a1b2c%.0s' {1..6})"
tw_secret="$(printf 'z9y8x%.0s' {1..6})"
tw_es="$(printf 'e5%.0s' {1..32})"
fresh_relay
run "UPPERCASE_NOT_ALLOWED_xxxxxxxx\nshort\nnope\n" --twitch
is "a bad Twitch client id is refused" "$code" 1
is "nothing saved for Twitch" "$(v TWITCH_CLOUD_CLIENT_ID)" ""
run "$tw_id\n$tw_secret\n$tw_es\n" --twitch
is "--twitch saves all three" "$code" 0
is "client id saved" "$(v TWITCH_CLOUD_CLIENT_ID)" "$tw_id"
is "client secret saved" "$(v TWITCH_CLOUD_CLIENT_SECRET)" "$tw_secret"
is "eventsub secret saved" "$(v TWITCH_CLOUD_EVENTSUB_SECRET)" "$tw_es"
for s in "$tw_secret" "$tw_es"; do hasnt "never prints a Twitch secret" "$out" "$s"; done
run "" --status
has "status lists the Twitch keys" "$out" "TWITCH_CLOUD_EVENTSUB_SECRET: *set"
fresh_relay
run "$tw_id\n\n\n" --twitch
is "part of the Twitch keys works" "$code" 0
has "warns the cloud bot stays off" "$out" "stays off until all three"

# Custom tip addresses: Cloudflare's token and zone (made-up values in the right shape, built
# in pieces so nothing here looks like a real token).
cf_token="$(printf 'Ab3_%.0s' {1..10})"
cf_zone="$(printf '0123456789abcdef%.0s' {1..2})"
fresh_relay
run "short-token\nUPPER0123456789ABCDEF0123456789AB\nnope\n" --domains
is "a short Cloudflare token is refused" "$code" 1
has "says what the token should look like" "$out" "not the Global API Key"
is "nothing saved for domains" "$(v CF_SAAS_TOKEN)" ""
run "$cf_token\nZONE\n0123456789ABCDEF0123456789ABCDEF\nnot-hex-not-hex-not-hex-not-hex!\n" --domains
is "a zone id that isn't 32 lowercase hex is refused" "$code" 1
is "the token isn't saved either (all or nothing)" "$(v CF_SAAS_TOKEN)" ""
run "$cf_token\n$cf_zone\n" --domains
is "--domains saves both" "$code" 0
is "token saved" "$(v CF_SAAS_TOKEN)" "$cf_token"
is "zone id saved" "$(v CF_ZONE_ID)" "$cf_zone"
hasnt "never prints the Cloudflare token" "$out" "$cf_token"
run "" --status
has "status lists the Cloudflare token" "$out" "CF_SAAS_TOKEN: *set"
has "status lists the zone id" "$out" "CF_ZONE_ID: *set"
hasnt "status hides the token" "$out" "$cf_token"
fresh_relay
run "$cf_token\n\n" --domains
is "the token alone works" "$code" 0
has "warns addresses stay off" "$out" "stay off until both are set"

# The cloud bot's Google client (made-up values in the right shape, built in pieces so nothing
# here looks like a real client or secret).
yt_id="4815162342-$(printf 'q7w8e9r0%.0s' {1..4}).apps.$(printf 'google')usercontent.com"
yt_secret="GOC$(printf 'SPX')-$(printf 'Zz9_%.0s' {1..7})"
fresh_relay
run "not-a-google-client\nnope\nstill-no\n" --youtube
is "a bad Google client id is refused" "$code" 1
has "says what the id should look like" "$out" "apps.googleusercontent.com"
is "nothing saved for YouTube" "$(v YOUTUBE_CLOUD_CLIENT_ID)" ""
run "$yt_id\nshort\nAIza-not-it\nGOCSPX-x\n" --youtube
is "a bad Google secret is refused" "$code" 1
is "the id isn't saved either (all or nothing)" "$(v YOUTUBE_CLOUD_CLIENT_ID)" ""
run "$yt_id\n$yt_secret\n" --youtube
is "--youtube saves both" "$code" 0
is "google client id saved" "$(v YOUTUBE_CLOUD_CLIENT_ID)" "$yt_id"
is "google client secret saved" "$(v YOUTUBE_CLOUD_CLIENT_SECRET)" "$yt_secret"
hasnt "never prints the Google secret" "$out" "$yt_secret"
is "only the Google keys are asked" "$(grep -c '^TWITCH_CLOUD_CLIENT_ID=' "$tmp/relay/relay.env")" 0
run "" --status
has "status lists the Google client" "$out" "YOUTUBE_CLOUD_CLIENT_SECRET: *set"
hasnt "status hides the Google secret" "$out" "$yt_secret"
fresh_relay
run "$yt_id\n\n" --youtube
is "the id alone works" "$code" 0
has "warns YouTube stays off" "$out" "cloud bot on YouTube"

# Google sign-in for accounts: the id is shown as typed, the secret hidden.
g_id="27182818-$(printf 'a1b2c3d4%.0s' {1..4}).apps.$(printf 'google')usercontent.com"
g_secret="GOC$(printf 'SPX')-$(printf 'Yy8_%.0s' {1..7})"
fresh_relay
run "$g_id\nGOCSPX-short\nnope\nAIza\n" --google
is "a bad sign-in secret is refused" "$code" 1
is "nothing saved for sign-in" "$(v RELAY_GOOGLE_CLIENT_ID)" ""
run "$g_id\n$g_secret\n" --google
is "--google saves both" "$code" 0
is "sign-in client id saved" "$(v RELAY_GOOGLE_CLIENT_ID)" "$g_id"
is "sign-in secret saved" "$(v RELAY_GOOGLE_CLIENT_SECRET)" "$g_secret"
hasnt "never prints the sign-in secret" "$out" "$g_secret"
is "only the sign-in keys are asked" "$(grep -c '^YOUTUBE_CLOUD_CLIENT_ID=' "$tmp/relay/relay.env")" 0
run "" --status
has "status lists the sign-in client" "$out" "RELAY_GOOGLE_CLIENT_SECRET: *set"
hasnt "status hides the sign-in secret" "$out" "$g_secret"
fresh_relay
run "$g_id\n\n" --google
is "the sign-in id alone works" "$code" 0
has "warns Google sign-in stays off" "$out" "Google sign-in"

# Smart features: Cloudflare's account id (shown) and token, or a Gemini key (made-up values
# in the right shape, built in pieces so nothing here looks like a real key).
cf_acct="$(printf '0a1b2c3d%.0s' {1..4})"
cf_ai="$(printf 'Wq7_%.0s' {1..11})"
gem="AIza$(printf 'G%.0s' {1..35})"
fresh_relay
run "NOT-HEX\nnope\nstill\n" --ai
is "a bad Cloudflare account id is refused" "$code" 1
is "nothing saved for smart features" "$(v CLOUDFLARE_AI_ACCOUNT_ID)" ""
run "$cf_acct\nshort\nGlobalKey\nnope\n" --ai
is "a short Cloudflare token is refused" "$code" 1
is "the account id isn't saved either (all or nothing)" "$(v CLOUDFLARE_AI_ACCOUNT_ID)" ""
run "$cf_acct\n$cf_ai\n\n" --ai
is "--ai saves Cloudflare's two" "$code" 0
is "account id saved" "$(v CLOUDFLARE_AI_ACCOUNT_ID)" "$cf_acct"
is "token saved" "$(v CLOUDFLARE_AI_TOKEN)" "$cf_ai"
is "no Gemini key without one typed" "$(v GEMINI_API_KEY)" ""
hasnt "never prints the Cloudflare AI token" "$out" "$cf_ai"
is "only the smart keys are asked" "$(grep -c '^RELAY_GOOGLE_CLIENT_ID=' "$tmp/relay/relay.env")" 0
run "" --status
has "status lists the smart keys" "$out" "CLOUDFLARE_AI_TOKEN: *set"
has "status lists Gemini as missing" "$out" "GEMINI_API_KEY: *missing"
hasnt "status hides the AI token" "$out" "$cf_ai"
run "\n\n$gem\n" --ai
is "a Gemini key can be added too" "$(v GEMINI_API_KEY)" "$gem"
has "says Cloudflare wins" "$out" "GEMINI_API_KEY is ignored"
hasnt "never prints the Gemini key" "$out" "$gem"
fresh_relay
run "\n\n$gem\n" --ai
is "Gemini alone works" "$code" 0
fresh_relay
run "$cf_acct\n\n\n" --ai
is "the account id alone works" "$code" 0
has "warns Cloudflare needs both" "$out" "need both CLOUDFLARE_AI_ACCOUNT_ID"

fresh_relay
run "x\n" --nope
is "unknown option fails" "$code" 1

if ((fails > 0)); then
  echo "$fails failed."
  exit 1
fi
echo "All pro-keys.sh tests passed."
