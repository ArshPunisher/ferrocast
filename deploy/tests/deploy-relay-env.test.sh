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
BREVO_API_KEY=bk
MAIL_FROM=Ferrocast <reports@mail.clearcraftlabs.com>
MAIL_TOKEN_KEY=mk
MAIL_DAILY_BUDGET=270
TWITCH_CLOUD_CLIENT_ID=tc
TWITCH_CLOUD_CLIENT_SECRET=ts
TWITCH_CLOUD_EVENTSUB_SECRET=te
CF_SAAS_TOKEN=cft
CF_ZONE_ID=cfz
YOUTUBE_CLOUD_CLIENT_ID=yc
YOUTUBE_CLOUD_CLIENT_SECRET=ys
YOUTUBE_CLOUD_DAILY_UNITS=9000
YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB=1500
YOUTUBE_CLOUD_TIMER_GAP_MIN=20
RELAY_ADMIN_TOKEN=adm
RELAY_GOOGLE_CLIENT_ID=gid
RELAY_GOOGLE_CLIENT_SECRET=gs
RELAY_REMOTE_LOGIN=optional
CLOUDFLARE_AI_ACCOUNT_ID=cfa
CLOUDFLARE_AI_TOKEN=cft2
GEMINI_API_KEY=gem
SMART_DAILY_CALLS=140
XCLOUDFLARE_AI_TOKEN=no
XGEMINI_API_KEY=no
XRELAY_GOOGLE_CLIENT_ID=no
XRELAY_ADMIN_TOKEN=no
XYOUTUBE_CLOUD_CLIENT_ID=no
XCF_ZONE_ID=no
CF_ZONE_ID_OLD=no
XTWITCH_CLOUD_CLIENT_ID=no
KICK_CLIENT_ID=old-kick
XBREVO_API_KEY=no
XDISCORD_APP_ID=no
ENV
kept="$(grep -E "$pattern" "$env_file" || true)"
for k in RAZORPAY_KEY_ID PRO_EMAIL_KEY RELAY_VAULT_KEY GOOGLE_TTS_KEY TTS_GLOBAL_MONTHLY_CHARS \
  DISCORD_APP_ID DISCORD_BOT_TOKEN DISCORD_PUBLIC_KEY DISCORD_CLIENT_SECRET BREVO_API_KEY MAIL_FROM MAIL_TOKEN_KEY MAIL_DAILY_BUDGET \
  TWITCH_CLOUD_CLIENT_ID TWITCH_CLOUD_CLIENT_SECRET TWITCH_CLOUD_EVENTSUB_SECRET CF_SAAS_TOKEN CF_ZONE_ID \
  YOUTUBE_CLOUD_CLIENT_ID YOUTUBE_CLOUD_CLIENT_SECRET YOUTUBE_CLOUD_DAILY_UNITS YOUTUBE_CLOUD_DAILY_UNITS_PER_SUB YOUTUBE_CLOUD_TIMER_GAP_MIN RELAY_ADMIN_TOKEN \
  RELAY_GOOGLE_CLIENT_ID RELAY_GOOGLE_CLIENT_SECRET RELAY_REMOTE_LOGIN \
  CLOUDFLARE_AI_ACCOUNT_ID CLOUDFLARE_AI_TOKEN GEMINI_API_KEY SMART_DAILY_CALLS; do
  if grep -q "^$k=" <<<"$kept"; then ok "keeps $k"; else bad "keeps $k"; fi
done
for k in RELAY_PUBLIC_URL KICK_CLIENT_ID XDISCORD_APP_ID XBREVO_API_KEY XTWITCH_CLOUD_CLIENT_ID XCF_ZONE_ID CF_ZONE_ID_OLD XYOUTUBE_CLOUD_CLIENT_ID XRELAY_ADMIN_TOKEN XRELAY_GOOGLE_CLIENT_ID XCLOUDFLARE_AI_TOKEN XGEMINI_API_KEY; do
  if grep -q "^$k=" <<<"$kept"; then bad "rewrites $k (not carried over)"; else ok "rewrites $k (not carried over)"; fi
done

if ((fails > 0)); then
  echo "$fails deploy-relay.sh carry-over test(s) failed." >&2
  exit 1
fi
echo "All deploy-relay.sh carry-over tests passed."
