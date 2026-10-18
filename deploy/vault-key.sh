#!/usr/bin/env bash
# The relay's vault key (RELAY_VAULT_KEY), made on the relay server itself: it never passes
# through a chat, a command line, the Mac or GitHub, and this script never prints it.
# Installed as /opt/ferrocast-relay/vault-key.sh by deploy/relay-ci-setup.sh. From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/vault-key.sh'           make it (once)
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/vault-key.sh --status'  set or missing
#
# The key (32 random bytes, base64) encrypts the few secrets Pro keeps on the relay
# (crates/relay/src/vault.rs). Made once and never changed: a new key can't open what the
# old one sealed. Running it again with a good key already there changes nothing. A key
# that isn't 32 bytes of base64 was never usable (the relay refuses it), so it's replaced.
# Backs up relay.env first and restarts the relay so it reads the key; anything failing
# puts relay.env back as it was.
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
  # Same file (same owner and 0600), new contents.
  WRITING=1
  cat "$TMP" >"$ENV_FILE"
  WRITING=0
  rm -f "$TMP"
  TMP=""
  chmod 600 "$ENV_FILE"
}
backup() {
  BACKUP="$STATE_DIR/env-backup-$(date +%y%m%d%H%M%S)"
  cp "$ENV_FILE" "$BACKUP"
  chmod 600 "$BACKUP"
}
# good_key: reads a key on stdin; true when it's 32 bytes of base64.
good_key() {
  python3 -I -c 'import base64, binascii, sys
try:
    ok = len(base64.b64decode(sys.stdin.read().strip(), validate=True)) == 32
except (binascii.Error, ValueError):
    ok = False
sys.exit(0 if ok else 1)'
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

case "${1:-}" in
  --status)
    current="$(value RELAY_VAULT_KEY)"
    if [[ -z "$current" ]]; then
      echo "Vault key:       missing (run this script without arguments)"
    elif printf '%s' "$current" | good_key; then
      echo "Vault key:       set"
    else
      echo "Vault key:       unusable (run this script without arguments to replace it)"
    fi
    exit 0
    ;;
  "") ;;
  *) die "Unknown option '$1'. Use no option, or --status." ;;
esac

current="$(value RELAY_VAULT_KEY)"
if [[ -n "$current" ]] && printf '%s' "$current" | good_key; then
  echo "✓ The vault key is already set; kept it (it never changes)."
  exit 0
fi

key="$(head -c 32 /dev/urandom | base64 | tr -d '\n')"
printf '%s' "$key" | good_key || die "Couldn't make a vault key. Nothing was changed."
backup
echo "✓ Backed up relay.env to ${BACKUP#"$DIR"/}"
[[ -n "$current" ]] && echo "The old RELAY_VAULT_KEY wasn't 32 bytes of base64 (the relay never used it): replacing it."
setv RELAY_VAULT_KEY "$key"
unset key
echo "✓ Made a vault key and saved it in relay.env (not shown)."
echo "Restarting the relay so it reads it…"
restart || put_back "$BACKUP" "The relay didn't come back healthy with the vault key."
echo "✓ Relay restarted."
