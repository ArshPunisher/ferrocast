#!/usr/bin/env bash
# Tests which server-only settings deploy/deploy-relay.sh carries over when it rewrites
# relay.env (the grep in its config step), without ssh:
#   deploy/tests/deploy-relay-env.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../deploy-relay.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }

# The pattern inside: grep -E \"^(…)=\" $dir/relay.env
pattern="$(sed -n 's/.*grep -E \\"\(\^([^"]*)=\)\\".*/\1/p' "$script" | head -n 1)"
if [[ -z "$pattern" ]]; then
  bad "found the carry-over pattern in deploy-relay.sh"
  exit 1
fi
ok "found the carry-over pattern"

env_file="$(mktemp)"
trap 'rm -f "$env_file"' EXIT
cat >"$env_file" <<'ENV'
RELAY_PUBLIC_URL=https://old.example
RAZORPAY_KEY_ID=rzp_live_x
PRO_EMAIL_KEY=abc
RELAY_VAULT_KEY=vk
GOOGLE_TTS_KEY=AIza
TTS_GLOBAL_MONTHLY_CHARS=800000
DISCORD_APP_ID=1561463863197835294
DISCORD_BOT_TOKEN=bot
DISCORD_PUBLIC_KEY=pk
DISCORD_CLIENT_SECRET=cs
KICK_CLIENT_ID=old-kick
XDISCORD_APP_ID=no
ENV
kept="$(grep -E "$pattern" "$env_file" || true)"
for k in RAZORPAY_KEY_ID PRO_EMAIL_KEY RELAY_VAULT_KEY GOOGLE_TTS_KEY TTS_GLOBAL_MONTHLY_CHARS \
  DISCORD_APP_ID DISCORD_BOT_TOKEN DISCORD_PUBLIC_KEY DISCORD_CLIENT_SECRET; do
  if grep -q "^$k=" <<<"$kept"; then ok "keeps $k"; else bad "keeps $k"; fi
done
for k in RELAY_PUBLIC_URL KICK_CLIENT_ID XDISCORD_APP_ID; do
  if grep -q "^$k=" <<<"$kept"; then bad "rewrites $k (not carried over)"; else ok "rewrites $k (not carried over)"; fi
done

if ((fails > 0)); then
  echo "$fails deploy-relay.sh carry-over test(s) failed." >&2
  exit 1
fi
echo "All deploy-relay.sh carry-over tests passed."
