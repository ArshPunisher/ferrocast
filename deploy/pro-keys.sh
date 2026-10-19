#!/usr/bin/env bash
# Pro's service keys, typed on the relay server itself: they never pass through a chat, a
# command line, the Mac or GitHub, and this script never prints them.
# Installed as /opt/ferrocast-relay/pro-keys.sh by deploy/relay-ci-setup.sh. From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh'            all of them
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --tts'      Ferrocast voices only
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --discord'  the Discord bot only
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --mail'     the weekly email only
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --twitch'   the cloud bot's Twitch app only
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-keys.sh --status'   set or missing
#
# Each key is asked for with typing hidden (paste it, then Enter); Enter alone keeps what's
# there. A key that doesn't look right is asked for again, three tries, and nothing is
# changed until every answer is good. Backs up relay.env first and restarts the relay so it
# reads them; anything failing puts relay.env back as it was. deploy-relay.sh keeps them.
#
#   GOOGLE_TTS_KEY         Google Cloud → APIs & Services → Credentials, restricted to the
#                          Cloud Text-to-Speech API (Ferrocast voices)
#   DISCORD_APP_ID         discord.com/developers → Ferrocast → General Information
#   DISCORD_PUBLIC_KEY     same page, Public Key (64 hex characters)
#   DISCORD_CLIENT_SECRET  OAuth2 → Client Secret (Reset Secret)
#   DISCORD_BOT_TOKEN      Bot → Reset Token
#   BREVO_API_KEY          app.brevo.com → SMTP & API → API keys → Generate (starts xkeysib-;
#                          not the SMTP key, which starts xsmtpsib-) (the weekly email)
#   TWITCH_CLOUD_CLIENT_ID      dev.twitch.tv/console → Applications → Ferrocast Cloud → Client ID
#   TWITCH_CLOUD_CLIENT_SECRET  same page → New Secret (a Confidential client)
#   TWITCH_CLOUD_EVENTSUB_SECRET  made by you: `openssl rand -hex 32` (10 to 100 characters;
#                          signs EventSub's webhooks to the relay, never sent anywhere else)
set -euo pipefail

DIR="${RELAY_DIR:-/opt/ferrocast-relay}"
ENV_FILE="$DIR/relay.env"
STATE_DIR="$DIR/state"
# Overridable for tests only.
NO_RESTART="${NO_RESTART:-0}"

die() {
  echo "$*" >&2
  exit 1
}

# `sudo -u deploy` keeps the caller's working directory, which deploy may not read.
cd "$DIR" 2>/dev/null || die "No $DIR: install the relay first (deploy/deploy-relay.sh)."
[[ -f "$ENV_FILE" ]] || die "No $ENV_FILE: install the relay first (deploy/deploy-relay.sh)."
[[ -w "$ENV_FILE" ]] || die "Can't write $ENV_FILE: run this as deploy (sudo -u deploy …)."
umask 077
mkdir -p "$STATE_DIR"
# One change at a time, and never during a deploy (both restart the relay).
if command -v flock >/dev/null; then
  exec 9>"$STATE_DIR/deploy.lock"
  flock -n 9 || die "A relay deploy is running; try again when it finishes."
fi

# Read as plain KEY=value lines, never executed.
value() { grep -E "^$1=" "$ENV_FILE" | tail -n 1 | cut -d= -f2- || true; }
# The backup taken before relay.env is changed, and whether a write to it is under way.
BACKUP=""
WRITING=0
TMP=""
# Any exit in the middle of rewriting relay.env (a failed write, a signal) puts the backup
# back, so relay.env is never left cut short. relay.env is rewritten in place, not replaced
# by a rename: its folder belongs to root, so deploy can't make files there.
on_exit() {
  local code=$?
  # The new contents hold the key: never leave them lying around.
  [[ -n "$TMP" ]] && rm -f "$TMP"
  if [[ "$WRITING" == 1 && -n "$BACKUP" ]]; then
    if cp "$BACKUP" "$ENV_FILE" 2>/dev/null; then
      echo "Writing relay.env failed; it was put back as it was." >&2
    else
      echo "Writing relay.env failed and it couldn't be put back: copy ${BACKUP#"$DIR"/} over it by hand." >&2
    fi
    exit 1
  fi
  exit "$code"
}
trap on_exit EXIT
trap 'exit 1' INT TERM HUP
# setv <name> <value>: the value reaches awk through its environment, never its arguments
# (which any user on the server can see in `ps`). Needs a backup first.
setv() {
  [[ -n "$BACKUP" ]] || die "Internal error: no backup before changing relay.env. Nothing was changed."
  # $DIR belongs to root; the state folder is deploy's.
  TMP="$(mktemp "$STATE_DIR/env.XXXXXX")"
  K="$1" V="$2" awk 'BEGIN { k = ENVIRON["K"]; v = ENVIRON["V"]; done = 0 }
    index($0, k "=") == 1 { if (!done) print k "=" v; done = 1; next } { print }
    END { if (!done) print k "=" v }' "$ENV_FILE" >"$TMP"
  # Same file (same owner and 0600), new contents. The caller marks the whole save as a
  # change in progress (WRITING=1), so a failure on any key puts every key back.
  cat "$TMP" >"$ENV_FILE"
  rm -f "$TMP"
  TMP=""
  chmod 600 "$ENV_FILE"
}
backup() {
  BACKUP="$STATE_DIR/env-backup-$(date +%y%m%d%H%M%S)"
  cp "$ENV_FILE" "$BACKUP"
  chmod 600 "$BACKUP"
}
# restart: recreate the relay container with the live version, wait until it's healthy.
restart() {
  [[ "$NO_RESTART" == 1 ]] && { echo "(restart skipped)"; return 0; }
  local image=ferrocast-relay tag=latest live
  live="$(cat "$STATE_DIR/current" 2>/dev/null || true)"
  if [[ -n "$live" ]]; then image=ghcr.io/arshpunisher/ferrocast-relay tag="$live"; fi
  RELAY_IMAGE="$image" RELAY_TAG="$tag" docker compose -f "$DIR/docker-compose.yml" --project-directory "$DIR" \
    up -d --no-deps --force-recreate relay >/dev/null 2>&1 || return 1
  local deadline=$((SECONDS + 90)) health
  while ((SECONDS < deadline)); do
    health="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' ferrocast-relay 2>/dev/null || true)"
    [[ "$health" == healthy ]] && return 0
    sleep 3
  done
  return 1
}
# put_back <backup> <why>: undo, restart on the old settings, and fail.
put_back() {
  WRITING=1
  cp "$1" "$ENV_FILE"
  WRITING=0
  restart || true
  die "$2 relay.env was put back as it was."
}

# ask <name> <pattern> <what>: reads one key with typing hidden; sets ANSWER ("" keeps it).
ANSWER=""
ask() {
  local name="$1" pattern="$2" what="$3" now try got
  now="$(value "$name")"
  for try in 1 2 3; do
    if [[ -n "$now" ]]; then
      printf '%s (%s), set; paste a new one or Enter to keep it: ' "$name" "$what" >&2
    else
      printf '%s (%s), missing; paste it or Enter to skip: ' "$name" "$what" >&2
    fi
    got=""
    IFS= read -rs got || true
    printf '\n' >&2
    got="${got//[[:space:]]/}"
    if [[ -z "$got" ]]; then
      ANSWER=""
      return 0
    fi
    if [[ "$got" =~ $pattern ]]; then
      ANSWER="$got"
      return 0
    fi
    echo "That doesn't look like $name ($what). Try again ($try of 3)." >&2
  done
  die "No good $name after three tries. Nothing was changed."
}

TTS_KEYS=(GOOGLE_TTS_KEY)
DISCORD_KEYS=(DISCORD_APP_ID DISCORD_PUBLIC_KEY DISCORD_CLIENT_SECRET DISCORD_BOT_TOKEN)
MAIL_KEYS=(BREVO_API_KEY)
TWITCH_KEYS=(TWITCH_CLOUD_CLIENT_ID TWITCH_CLOUD_CLIENT_SECRET TWITCH_CLOUD_EVENTSUB_SECRET)
pattern() {
  case "$1" in
    GOOGLE_TTS_KEY) echo '^AIza[0-9A-Za-z_-]{35}$' ;;
    DISCORD_APP_ID) echo '^[0-9]{17,20}$' ;;
    DISCORD_PUBLIC_KEY) echo '^[0-9a-fA-F]{64}$' ;;
    DISCORD_CLIENT_SECRET) echo '^[A-Za-z0-9_-]{16,64}$' ;;
    DISCORD_BOT_TOKEN) echo '^[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{20,}$' ;;
    # Brevo's v3 keys: xkeysib-, 64 hex, a dash, a short suffix (its length isn't documented,
    # so 8 to 32 letters and digits are taken).
    BREVO_API_KEY) echo '^xkeysib-[0-9a-fA-F]{64}-[A-Za-z0-9]{8,32}$' ;;
    # Twitch's client ids and secrets are 30 lowercase letters and digits.
    TWITCH_CLOUD_CLIENT_ID | TWITCH_CLOUD_CLIENT_SECRET) echo '^[a-z0-9]{30}$' ;;
    TWITCH_CLOUD_EVENTSUB_SECRET) echo '^[A-Za-z0-9_-]{10,100}$' ;;
  esac
}
what() {
  case "$1" in
    GOOGLE_TTS_KEY) echo "starts with AIza, 39 characters" ;;
    DISCORD_APP_ID) echo "the Application ID, digits only" ;;
    DISCORD_PUBLIC_KEY) echo "64 hex characters" ;;
    DISCORD_CLIENT_SECRET) echo "OAuth2 client secret" ;;
    DISCORD_BOT_TOKEN) echo "three parts with dots" ;;
    BREVO_API_KEY) echo "an API key starting xkeysib-, not the SMTP key" ;;
    TWITCH_CLOUD_CLIENT_ID) echo "the Ferrocast Cloud app's Client ID, 30 characters" ;;
    TWITCH_CLOUD_CLIENT_SECRET) echo "the Ferrocast Cloud app's secret, 30 characters" ;;
    TWITCH_CLOUD_EVENTSUB_SECRET) echo "10 to 100 letters, digits, - or _ (openssl rand -hex 32)" ;;
  esac
}
status_of() { if [[ -n "$(value "$1")" ]]; then echo set; else echo missing; fi; }

keys=()
case "${1:-}" in
  --status)
    for k in "${TTS_KEYS[@]}" "${DISCORD_KEYS[@]}" "${MAIL_KEYS[@]}" "${TWITCH_KEYS[@]}"; do printf '%-22s %s\n' "$k:" "$(status_of "$k")"; done
    exit 0
    ;;
  --tts) keys=("${TTS_KEYS[@]}") ;;
  --discord) keys=("${DISCORD_KEYS[@]}") ;;
  --mail) keys=("${MAIL_KEYS[@]}") ;;
  --twitch) keys=("${TWITCH_KEYS[@]}") ;;
  "") keys=("${TTS_KEYS[@]}" "${DISCORD_KEYS[@]}" "${MAIL_KEYS[@]}" "${TWITCH_KEYS[@]}") ;;
  *) die "Unknown option '$1'. Use no option, --tts, --discord, --mail, --twitch or --status." ;;
esac

names=()
answers=()
for k in "${keys[@]}"; do
  ask "$k" "$(pattern "$k")" "$(what "$k")"
  if [[ -n "$ANSWER" ]]; then
    names+=("$k")
    answers+=("$ANSWER")
  fi
done
ANSWER=""
if ((${#names[@]} == 0)); then
  echo "Nothing new typed; relay.env is as it was."
  exit 0
fi

backup
echo "✓ Backed up relay.env to ${BACKUP#"$DIR"/}"
# Until every key is written, any exit (a failed write, a signal) puts the backup back.
WRITING=1
for i in "${!names[@]}"; do
  setv "${names[$i]}" "${answers[$i]}"
done
WRITING=0
for k in "${names[@]}"; do echo "✓ Saved $k (not shown)."; done
unset answers
missing=()
for k in "${DISCORD_KEYS[@]}"; do [[ -n "$(value "$k")" ]] || missing+=("$k"); done
if ((${#missing[@]} > 0 && ${#missing[@]} < ${#DISCORD_KEYS[@]})); then
  echo "Still missing for the Discord bot: ${missing[*]}. It stays off until all four are set."
fi
missing=()
for k in "${TWITCH_KEYS[@]}"; do [[ -n "$(value "$k")" ]] || missing+=("$k"); done
if ((${#missing[@]} > 0 && ${#missing[@]} < ${#TWITCH_KEYS[@]})); then
  echo "Still missing for the cloud bot on Twitch: ${missing[*]}. It stays off until all three are set."
fi
echo "Restarting the relay so it reads them…"
restart || put_back "$BACKUP" "The relay didn't come back healthy with the new keys."
echo "✓ Relay restarted."
