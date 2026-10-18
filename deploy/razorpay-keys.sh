#!/usr/bin/env bash
# Ferrocast Pro's Razorpay keys on the relay server, typed on the server only: they never
# pass through a chat, a command line, the Mac or GitHub. Installed as
# /opt/ferrocast-relay/razorpay-keys.sh by deploy/relay-ci-setup.sh. From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh'           keys + plans
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --status'  what's set (no secrets)
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --sales open'    start selling Pro
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/razorpay-keys.sh --sales closed'  stop selling
#
# Keys: asks for the Key ID and the Key Secret (hidden), checks Razorpay accepts them,
# backs up relay.env, saves the keys, makes a new webhook secret, creates the two Pro plans
# (₹199 a month, ₹1,999 a year) once per Razorpay mode, prints what to paste into Razorpay →
# Webhooks, and restarts the relay so it reads them. Same keys again: keeps the plans and
# the webhook secret. Anything failing puts relay.env back as it was.
#
# Use the account's existing key pair (the one TransformX and Hookline use). Never press
# "Regenerate" in Razorpay for this: that would break the other products.
#
# Pro stays off sale (PRO_SALES_OPEN=false) until you run --sales open.
set -euo pipefail

DIR="${RELAY_DIR:-/opt/ferrocast-relay}"
ENV_FILE="$DIR/relay.env"
STATE_DIR="$DIR/state"
# Overridable for tests only.
RZP_API="${RZP_API:-https://api.razorpay.com/v1}"
NO_RESTART="${NO_RESTART:-0}"
WEBHOOK_URL="https://ferrocast-relay.clearcraftlabs.com/billing/razorpay/webhook"
EVENTS="subscription.authenticated, subscription.activated, subscription.charged, subscription.pending,
              subscription.halted, subscription.cancelled, subscription.completed, subscription.paused,
              subscription.resumed, subscription.updated, refund.created, refund.processed, refund.failed"
# Pro's prices in paise; the relay refuses checkout if a plan doesn't match its own
# (crates/relay/src/billing/access.rs).
MONTHLY_PAISE=19900
YEARLY_PAISE=199900

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
# The backup taken before relay.env is changed, whether a write to it is under way, the
# temporary copy being written, and curl's credentials folder.
BACKUP=""
WRITING=0
TMP=""
work=""
# Any exit in the middle of rewriting relay.env (a failed write, a signal) puts the backup
# back, so relay.env is never left cut short. relay.env is rewritten in place, not replaced
# by a rename: its folder belongs to root, so deploy can't make files there.
on_exit() {
  local code=$?
  # The new contents and curl's file hold the keys: never leave them lying around.
  [[ -n "$TMP" ]] && rm -f "$TMP"
  [[ -n "$work" ]] && rm -rf "$work"
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
# backup: copies relay.env into the state folder and sets $BACKUP.
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

status() {
  local id mode=unset
  id="$(value RAZORPAY_KEY_ID)"
  [[ "$id" == rzp_live_* ]] && mode="live"
  [[ "$id" == rzp_test_* ]] && mode="test"
  echo "Razorpay mode:   $mode${id:+ ($id)}"
  echo "Key secret:      $([[ -n "$(value RAZORPAY_KEY_SECRET)" ]] && echo set || echo missing)"
  echo "Webhook secret:  $([[ -n "$(value RAZORPAY_WEBHOOK_SECRET)" ]] && echo set || echo missing)"
  echo "Monthly plan:    $(value RAZORPAY_PLAN_MONTHLY)"
  echo "Yearly plan:     $(value RAZORPAY_PLAN_YEARLY)"
  echo "Email hash key:  $([[ -n "$(value PRO_EMAIL_KEY)" ]] && echo set || echo missing)"
  echo "Pro on sale:     $(value PRO_SALES_OPEN)"
}

case "${1:-}" in
  --status)
    status
    exit 0
    ;;
  --sales)
    case "${2:-}" in
      open) want=true ;;
      closed) want=false ;;
      *) die "Use --sales open or --sales closed." ;;
    esac
    if [[ "$want" == true ]]; then
      for k in RAZORPAY_KEY_ID RAZORPAY_KEY_SECRET RAZORPAY_WEBHOOK_SECRET RAZORPAY_PLAN_MONTHLY RAZORPAY_PLAN_YEARLY PRO_EMAIL_KEY; do
        [[ -n "$(value "$k")" ]] || die "$k isn't set: run this script without arguments first. Nothing was changed."
      done
    fi
    backup
    setv PRO_SALES_OPEN "$want"
    restart || put_back "$BACKUP" "The relay didn't come back healthy."
    echo "✓ PRO_SALES_OPEN=$want, relay restarted."
    exit 0
    ;;
  "") ;;
  *) die "Unknown option '$1'. Use no option, --status, or --sales open|closed." ;;
esac

read -r -p "Razorpay Key ID (rzp_live_… or rzp_test_…): " KEY_ID
KEY_ID="$(printf '%s' "$KEY_ID" | tr -d '[:space:]')"
[[ "$KEY_ID" =~ ^rzp_(live|test)_[A-Za-z0-9]{8,32}$ ]] || die "That doesn't look like a Razorpay Key ID. Nothing was changed."
MODE="${BASH_REMATCH[1]}"
read -r -s -p "Key Secret (hidden): " KEY_SECRET
echo
KEY_SECRET="$(printf '%s' "$KEY_SECRET" | tr -d '[:space:]')"
[[ "$KEY_SECRET" =~ ^[A-Za-z0-9]{16,64}$ ]] || die "That doesn't look like a Key Secret. Nothing was changed."

# Credentials go to curl in a private file (0600, removed on exit), never on a command line.
work="$(mktemp -d "$STATE_DIR/rzp.XXXXXX")"
printf 'user = "%s:%s"\n' "$KEY_ID" "$KEY_SECRET" >"$work/auth"
rzp() { # <method> <path> [json body on stdin] → body in $work/out, prints the HTTP status
  local method=$1 path=$2
  if [[ "$method" == POST ]]; then
    curl -s -o "$work/out" -w '%{http_code}' -m 30 -K "$work/auth" -X POST -H 'Content-Type: application/json' \
      --data-binary @- "$RZP_API$path" || echo 000
  else
    curl -s -o "$work/out" -w '%{http_code}' -m 30 -K "$work/auth" "$RZP_API$path" </dev/null || echo 000
  fi
}

case "$(rzp GET '/plans?count=1')" in
  200) echo "✓ Razorpay accepted the $MODE keys." ;;
  401) die "Razorpay rejected these keys (wrong ID or secret, or the other mode's keys). Nothing was changed." ;;
  *) die "Couldn't reach Razorpay to check the keys. Nothing was changed; try again." ;;
esac

same_keys=0
[[ "$(value RAZORPAY_KEY_ID)" == "$KEY_ID" ]] && same_keys=1
have_plans=0
[[ "$(value RAZORPAY_PLAN_MONTHLY)" == plan_* && "$(value RAZORPAY_PLAN_YEARLY)" == plan_* ]] && have_plans=1

backup
b="$BACKUP"
echo "✓ Backed up relay.env to ${b#"$DIR"/}"
setv RAZORPAY_KEY_ID "$KEY_ID"
setv RAZORPAY_KEY_SECRET "$KEY_SECRET"
[[ -n "$(value PRO_SALES_OPEN)" ]] || setv PRO_SALES_OPEN false
# Keys the stored email hashes. Made once and never changed (changing it would stop
# Restore from matching existing subscriptions), whatever the Razorpay mode.
if [[ -z "$(value PRO_EMAIL_KEY)" ]]; then
  setv PRO_EMAIL_KEY "$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
fi

# create_plan <period> <paise> <label> → the new plan id
create_plan() {
  local body code id
  body="$(printf '{"period":"%s","interval":1,"item":{"name":"%s","amount":%s,"currency":"INR","description":"Ferrocast Pro: the hosted extras that cost us money to run."},"notes":{"product":"ferrocast-pro"}}' "$1" "$3" "$2")"
  code="$(printf '%s' "$body" | rzp POST /plans)"
  [[ "$code" == 200 ]] || return 1
  id="$(python3 -I -c 'import json,sys
d = json.load(open(sys.argv[1]))
ok = d.get("item", {}).get("amount") == int(sys.argv[2]) and d.get("item", {}).get("currency") == "INR" and d.get("period") == sys.argv[3]
print(d["id"] if ok and str(d.get("id", "")).startswith("plan_") else "")' "$work/out" "$2" "$1" 2>/dev/null || true)"
  [[ "$id" =~ ^plan_[A-Za-z0-9]{6,40}$ ]] || return 1
  echo "$id"
}

if [[ "$same_keys" == 1 && "$have_plans" == 1 && -n "$(value RAZORPAY_WEBHOOK_SECRET)" ]]; then
  echo "✓ Same keys as before: kept the plans and the webhook secret."
  WEBHOOK_SECRET="$(value RAZORPAY_WEBHOOK_SECRET)"
else
  # Test and live mode have their own plans and webhooks.
  WEBHOOK_SECRET="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
  [[ "$WEBHOOK_SECRET" =~ ^[0-9a-f]{64}$ ]] || put_back "$b" "Couldn't make a webhook secret."
  setv RAZORPAY_WEBHOOK_SECRET "$WEBHOOK_SECRET"
  echo "Creating the Pro plans in Razorpay ($MODE mode)…"
  monthly="$(create_plan monthly "$MONTHLY_PAISE" "Ferrocast Pro (monthly)")" ||
    put_back "$b" "Razorpay didn't create the monthly plan."
  yearly="$(create_plan yearly "$YEARLY_PAISE" "Ferrocast Pro (yearly)")" ||
    put_back "$b" "Razorpay didn't create the yearly plan (the monthly one, $monthly, is unused; ignore it)."
  setv RAZORPAY_PLAN_MONTHLY "$monthly"
  setv RAZORPAY_PLAN_YEARLY "$yearly"
  echo "✓ Plans created: monthly $monthly (₹199), yearly $yearly (₹1,999)"
fi

echo "Restarting the relay so it reads them…"
restart || put_back "$b" "The relay didn't come back healthy with the new keys."
echo "✓ Relay restarted. Pro on sale: $(value PRO_SALES_OPEN)."

cat <<EOT

Now in Razorpay ($MODE mode) → Account & Settings → Webhooks → + Add New Webhook:
  Webhook URL : $WEBHOOK_URL
  Secret      : $WEBHOOK_SECRET
  Alert email : your billing inbox
  Events      : $EVENTS

This is a separate webhook from TransformX's and Hookline's: leave theirs alone. The relay
ignores events for subscriptions that aren't Ferrocast Pro's.
The secret is also saved in relay.env; it isn't shown again unless you change keys.
EOT
