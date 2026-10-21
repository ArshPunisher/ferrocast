#!/usr/bin/env bash
# The token the Control Center (ops.clearcraftlabs.com) reads the relay's admin stats with
# (GET /admin/v1/stats: aggregate counts only), made on the server itself: it never passes
# through a chat, a command line, the Mac or GitHub, and this script never prints it.
# Installed as /opt/ferrocast-relay/control-center-key.sh by deploy/relay-ci-setup.sh. From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh'           set it up (safe to run again)
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh --rotate'  a new token on both sides
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/control-center-key.sh --status'  set, missing or different
#
# The same random token (64 hex characters) goes into the relay's relay.env as RELAY_ADMIN_TOKEN
# and into TransformX's .env.prod as FERROCAST_ADMIN_TOKEN (the Control Center API reads it from
# there through /opt/transformx/docker-compose.prod.yml), both 0600. Running it again changes
# nothing when both already hold the same token; a relay token without its Control Center half
# is copied over, so the relay needn't restart. Each file is backed up first; then the relay and
# the Control Center API are restarted only if their own file changed, and the script checks
# from inside the Control Center container that the relay answers. If the relay doesn't come
# back healthy, both files are put back as they were.
#
# The Control Center reaches the relay at http://ferrocast-relay:8787 on the relay's Docker
# network (ferrocast-relay_ferrocast), which its compose file joins; the stats refuse anything
# that came through a public address.
set -euo pipefail

DIR="${RELAY_DIR:-/opt/ferrocast-relay}"
ENV_FILE="$DIR/relay.env"
STATE_DIR="$DIR/state"
TX_DIR="${TX_DIR:-/opt/transformx}"
TX_ENV="$TX_DIR/.env.prod"
TX_STATE="$TX_DIR/deploy/state"
NETWORK="${RELAY_NETWORK:-ferrocast-relay_ferrocast}"
CC=control-center-api
# Overridable for tests only.
NO_RESTART="${NO_RESTART:-0}"

die() {
  echo "$*" >&2
  exit 1
}

# `sudo -u deploy` keeps the caller's working directory, which deploy may not read.
cd "$DIR" 2>/dev/null || die "No $DIR: install the relay first (deploy/deploy-relay.sh)."
[[ -f "$ENV_FILE" ]] || die "No $ENV_FILE: install the relay first (deploy/deploy-relay.sh)."
[[ -f "$TX_ENV" ]] || die "No $TX_ENV: the Control Center runs from TransformX's stack in $TX_DIR (see its DEPLOY.md)."
umask 077
mkdir -p "$STATE_DIR"

# Read as plain KEY=value lines, never executed.
value() { grep -E "^$1=" "$2" | tail -n 1 | cut -d= -f2- || true; }
# good <token>: what control-center-key.sh makes, or any 32 to 128 letters, digits, - or _.
good() { [[ "$1" =~ ^[A-Za-z0-9_-]{32,128}$ ]]; }
describe() {
  if [[ -z "$1" ]]; then echo "missing"; elif good "$1"; then echo "set"; else echo "unusable (run --rotate)"; fi
}

relay_now="$(value RELAY_ADMIN_TOKEN "$ENV_FILE")"
cc_now="$(value FERROCAST_ADMIN_TOKEN "$TX_ENV")"

if [[ "${1:-}" == --status ]]; then
  printf 'Relay (RELAY_ADMIN_TOKEN):                %s\n' "$(describe "$relay_now")"
  printf 'Control Center (FERROCAST_ADMIN_TOKEN):   %s\n' "$(describe "$cc_now")"
  if [[ -n "$relay_now" && -n "$cc_now" ]]; then
    if [[ "$relay_now" == "$cc_now" ]]; then echo "The two match:                            yes"; else echo "The two match:                            NO (run this script again)"; fi
  fi
  if grep -q FERROCAST_ADMIN_TOKEN "$TX_DIR/docker-compose.prod.yml" 2>/dev/null; then
    echo "Control Center compose file reads it:     yes"
  else
    echo "Control Center compose file reads it:     no (upload transformx-infra's stack files, DEPLOY.md §3)"
  fi
  exit 0
fi
case "${1:-}" in
  "" | --rotate) ;;
  *) die "Unknown option '$1'. Use no option, --rotate or --status." ;;
esac

[[ -w "$ENV_FILE" ]] || die "Can't write $ENV_FILE: run this as deploy (sudo -u deploy …)."
[[ -w "$TX_ENV" ]] || die "Can't write $TX_ENV: run this as deploy (sudo -u deploy …)."
# One change at a time, and never during a relay or Control Center deploy (they restart them too).
if command -v flock >/dev/null; then
  exec 9>"$STATE_DIR/deploy.lock"
  flock -n 9 || die "A relay deploy is running; try again when it finishes."
  if [[ -d "$TX_STATE" ]]; then
    exec 8>"$TX_STATE/deploy.lock"
    flock -n 8 || die "A TransformX or Control Center deploy is running; try again when it finishes."
  fi
fi

# What to write: keep a good relay token (no relay restart), else make one.
token=""
if [[ "${1:-}" != --rotate ]] && good "$relay_now"; then
  if [[ "$cc_now" == "$relay_now" ]]; then
    echo "✓ The relay and the Control Center already share an admin token; nothing changed."
    echo "  (A new one: --rotate. Is it working? Open ops.clearcraftlabs.com → Ferrocast.)"
    exit 0
  fi
  token="$relay_now"
else
  token="$(openssl rand -hex 32)"
fi
good "$token" || die "Couldn't make a token. Nothing was changed."
relay_changes=0
if [[ "$token" != "$relay_now" ]]; then relay_changes=1; fi
unset relay_now cc_now

# The backups taken before each file changes, and whether a write is under way.
RELAY_BACKUP=""
TX_BACKUP=""
WRITING=0
TMP=""
put_both_back() {
  local ok=1
  if [[ -n "$RELAY_BACKUP" ]]; then cp "$RELAY_BACKUP" "$ENV_FILE" 2>/dev/null || ok=0; fi
  if [[ -n "$TX_BACKUP" ]]; then cp "$TX_BACKUP" "$TX_ENV" 2>/dev/null || ok=0; fi
  return $((1 - ok))
}
# Any exit in the middle of a write (a failed write, a signal) puts both files back.
on_exit() {
  local code=$?
  # The new contents hold the token: never leave them lying around.
  [[ -n "$TMP" ]] && rm -f "$TMP"
  if [[ "$WRITING" == 1 ]]; then
    if put_both_back; then
      echo "Writing the settings failed; both files were put back as they were." >&2
    else
      echo "Writing the settings failed and a file couldn't be put back: copy the newest env-backup-* over it by hand." >&2
    fi
    exit 1
  fi
  exit "$code"
}
trap on_exit EXIT
trap 'exit 1' INT TERM HUP

# setv <file> <tmp dir> <name> <value>: the value reaches awk through its environment, never
# its arguments (which any user on the server can see in `ps`). The file is rewritten in place
# (same owner), then made 0600.
setv() {
  TMP="$(mktemp "$2/env.XXXXXX")"
  K="$3" V="$4" awk 'BEGIN { k = ENVIRON["K"]; v = ENVIRON["V"]; done = 0 }
    index($0, k "=") == 1 { if (!done) print k "=" v; done = 1; next } { print }
    END { if (!done) print k "=" v }' "$1" >"$TMP"
  cat "$TMP" >"$1"
  rm -f "$TMP"
  TMP=""
  chmod 600 "$1"
}

stamp="$(date +%y%m%d%H%M%S)"
mkdir -p "$TX_STATE"
TX_BACKUP="$TX_STATE/env-backup-$stamp"
cp "$TX_ENV" "$TX_BACKUP"
chmod 600 "$TX_BACKUP"
echo "✓ Backed up TransformX's .env.prod to ${TX_BACKUP#"$TX_DIR"/}"
if [[ "$relay_changes" == 1 ]]; then
  RELAY_BACKUP="$STATE_DIR/env-backup-$stamp"
  cp "$ENV_FILE" "$RELAY_BACKUP"
  chmod 600 "$RELAY_BACKUP"
  echo "✓ Backed up relay.env to ${RELAY_BACKUP#"$DIR"/}"
fi

WRITING=1
if [[ "$relay_changes" == 1 ]]; then setv "$ENV_FILE" "$STATE_DIR" RELAY_ADMIN_TOKEN "$token"; fi
setv "$TX_ENV" "$TX_STATE" FERROCAST_ADMIN_TOKEN "$token"
WRITING=0
unset token
if [[ "$relay_changes" == 1 ]]; then
  echo "✓ Saved a new admin token in relay.env and TransformX's .env.prod (not shown)."
else
  echo "✓ Copied the relay's admin token into TransformX's .env.prod (not shown); the relay keeps it."
fi

if [[ "$NO_RESTART" == 1 ]]; then
  echo "(restarts skipped)"
  exit 0
fi

# wait_healthy <container> <seconds>
wait_healthy() {
  local deadline=$((SECONDS + $2)) health
  while ((SECONDS < deadline)); do
    health="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$1" 2>/dev/null || true)"
    [[ "$health" == healthy ]] && return 0
    sleep 3
  done
  return 1
}
# restart_relay: recreate the relay container with the live version, wait until it's healthy.
restart_relay() {
  local image=ferrocast-relay tag=latest live
  live="$(cat "$STATE_DIR/current" 2>/dev/null || true)"
  if [[ -n "$live" ]]; then image=ghcr.io/arshpunisher/ferrocast-relay tag="$live"; fi
  RELAY_IMAGE="$image" RELAY_TAG="$tag" docker compose -f "$DIR/docker-compose.yml" --project-directory "$DIR" \
    up -d --no-deps --force-recreate relay >/dev/null 2>&1 || return 1
  wait_healthy ferrocast-relay 90
}

if [[ "$relay_changes" == 1 ]]; then
  echo "Restarting the relay so it reads the token…"
  if ! restart_relay; then
    put_both_back || true
    restart_relay || true
    die "The relay didn't come back healthy with the token. relay.env and .env.prod were put back as they were."
  fi
  echo "✓ Relay restarted."
fi

# The Control Center API reads the token only through its compose file, on the relay's network.
if ! grep -q FERROCAST_ADMIN_TOKEN "$TX_DIR/docker-compose.prod.yml" 2>/dev/null; then
  echo "! $TX_DIR/docker-compose.prod.yml doesn't pass FERROCAST_ADMIN_TOKEN to the Control Center yet."
  echo "  Upload transformx-infra's stack files (its DEPLOY.md §3), then run this script again."
  exit 0
fi
if ! docker network inspect "$NETWORK" >/dev/null 2>&1; then
  die "The relay's Docker network $NETWORK isn't there, so the Control Center can't join it. Is the relay running (docker ps)?"
fi
if [[ -z "$(docker ps -q -f "name=^$CC\$" 2>/dev/null)" ]]; then
  echo "The Control Center API isn't running here; release it (control-center-api → Actions → 🚀 Deploy to production)"
  echo "and it will read the token."
  exit 0
fi
echo "Restarting the Control Center API so it reads the token…"
if ! "$TX_DIR/deploy/compose.sh" --profile control up -d --no-deps --force-recreate "$CC" >/dev/null 2>&1 ||
  ! wait_healthy "$CC" 120; then
  die "The Control Center API didn't come back healthy. Its settings are saved: release it again (control-center-api → Actions → 🚀 Deploy to production)."
fi
echo "✓ Control Center API restarted."

# From inside the Control Center: the token stays in its environment, never on a command line.
# shellcheck disable=SC2016 # Python, not shell
answer="$(docker exec "$CC" python -c 'import os, urllib.error, urllib.request
url = os.environ.get("FERROCAST_RELAY_URL", "http://ferrocast-relay:8787").rstrip("/") + "/admin/v1/stats"
req = urllib.request.Request(url, headers={"Authorization": "Bearer " + os.environ.get("FERROCAST_ADMIN_TOKEN", "")})
try:
    print(urllib.request.urlopen(req, timeout=10).status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception:
    print("unreachable")' 2>/dev/null || echo "unreachable")"
case "$answer" in
  200) echo "✓ The Control Center reads the relay's stats. Open ops.clearcraftlabs.com → Ferrocast." ;;
  401) die "The relay refused the Control Center's token. Run this script again with --rotate." ;;
  404) die "The relay has no stats yet: release it first (Actions → 🚀 Deploy relay), then open ops.clearcraftlabs.com." ;;
  503) die "The relay has no admin token: run this script again." ;;
  unreachable) die "The Control Center can't reach the relay at http://ferrocast-relay:8787. Check both run (docker ps) and the Control Center is on $NETWORK." ;;
  *) die "The relay answered $answer. Check its logs: sudo docker logs --since 5m ferrocast-relay" ;;
esac
