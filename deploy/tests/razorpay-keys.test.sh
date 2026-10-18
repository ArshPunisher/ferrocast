#!/usr/bin/env bash
# Tests deploy/razorpay-keys.sh against a fake Razorpay (deploy/tests/fake_razorpay.py),
# in a temporary relay folder, without Docker:
#   deploy/tests/razorpay-keys.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../razorpay-keys.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -q -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }
hasnt() { if grep -q -- "$3" <<<"$2"; then bad "$1: found '$3'"; else ok "$1"; fi; }

tmp="$(mktemp -d)"
pids=()
cleanup() {
  for p in "${pids[@]}"; do
    kill "$p" 2>/dev/null || true
    wait "$p" 2>/dev/null || true
  done
  rm -rf "$tmp"
}
trap cleanup EXIT

start_fake() { # [FAIL_PLANS] → sets $api
  FAIL_PLANS="${1:-0}" python3 -I "$here/fake_razorpay.py" >"$tmp/port" &
  pids+=($!)
  for _ in $(seq 50); do [[ -s "$tmp/port" ]] && break; sleep 0.1; done
  api="http://127.0.0.1:$(cat "$tmp/port")/v1"
  rm -f "$tmp/port"
}
fresh_relay() {
  rm -rf "$tmp/relay"
  mkdir -p "$tmp/relay/state"
  printf 'RELAY_PUBLIC_URL=https://ferrocast-relay.clearcraftlabs.com\nKICK_CLIENT_ID=kid\nKICK_CLIENT_SECRET=ksecret\n' >"$tmp/relay/relay.env"
  chmod 600 "$tmp/relay/relay.env"
}
run() { # <stdin> [args…] → output in $out, status in $code
  local input=$1
  shift
  set +e
  out="$(printf '%b' "$input" | RELAY_DIR="$tmp/relay" RZP_API="$api" NO_RESTART=1 "$script" "$@" 2>&1)"
  code=$?
  set -e
}
v() { grep -E "^$1=" "$tmp/relay/relay.env" | tail -n 1 | cut -d= -f2-; }
KEY_ID=rzp_test_FakeKey000001
SECRET=FakeSecretForTestsOnly00

echo "razorpay-keys.sh"
start_fake 0
fresh_relay
run "not-a-key\n"
is "refuses a malformed Key ID" "$code" 1
has "says nothing changed" "$out" "Nothing was changed"
run "$KEY_ID\nshort\n"
is "refuses a malformed secret" "$code" 1
run "$KEY_ID\nWrongSecretWrongSecret00\n"
is "refuses keys Razorpay rejects" "$code" 1
has "explains the rejection" "$out" "rejected these keys"
is "relay.env untouched after a refusal" "$(v RAZORPAY_KEY_ID)" ""

run "$KEY_ID\n$SECRET\n"
is "saves good keys" "$code" 0
is "key id saved" "$(v RAZORPAY_KEY_ID)" "$KEY_ID"
is "secret saved" "$(v RAZORPAY_KEY_SECRET)" "$SECRET"
hasnt "never prints the key secret" "$out" "$SECRET"
is "monthly plan id saved" "$(v RAZORPAY_PLAN_MONTHLY)" plan_Fake000001
is "yearly plan id saved" "$(v RAZORPAY_PLAN_YEARLY)" plan_Fake000002
is "sales stay closed" "$(v PRO_SALES_OPEN)" false
is "other settings kept" "$(v KICK_CLIENT_SECRET)" ksecret
secret1="$(v RAZORPAY_WEBHOOK_SECRET)"
emailkey="$(v PRO_EMAIL_KEY)"
is "an email hash key (64 hex digits)" "$(grep -cE '^[0-9a-f]{64}$' <<<"$emailkey")" 1
hasnt "never prints the email key" "$out" "$emailkey"
is "a strong webhook secret (64 hex digits)" "$(grep -cE '^[0-9a-f]{64}$' <<<"$secret1")" 1
has "prints the webhook URL" "$out" "https://ferrocast-relay.clearcraftlabs.com/billing/razorpay/webhook"
has "prints the webhook secret to paste" "$out" "$secret1"
has "lists the refund events" "$out" "refund.processed"
is "relay.env stays 0600" "$(stat -c %a "$tmp/relay/relay.env" 2>/dev/null || stat -f %Lp "$tmp/relay/relay.env")" 600
is "a backup was taken" "$(find "$tmp/relay/state" -name 'env-backup-*' | wc -l | tr -d ' ')" 1
is "no duplicate lines" "$(grep -c '^RAZORPAY_KEY_ID=' "$tmp/relay/relay.env")" 1

run "$KEY_ID\n$SECRET\n"
is "same keys again works" "$code" 0
has "same keys keep the plans" "$out" "kept the plans"
is "same webhook secret" "$(v RAZORPAY_WEBHOOK_SECRET)" "$secret1"
is "same plan" "$(v RAZORPAY_PLAN_MONTHLY)" plan_Fake000001
is "the email key never changes" "$(v PRO_EMAIL_KEY)" "$emailkey"

run "" --status
is "status works" "$code" 0
has "status shows the mode" "$out" "test"
hasnt "status hides the key secret" "$out" "$SECRET"
hasnt "status hides the webhook secret" "$out" "$secret1"

run "" --sales open
is "sales open" "$(v PRO_SALES_OPEN)" true
run "" --sales closed
is "sales closed again" "$(v PRO_SALES_OPEN)" false
run "" --sales maybe
is "refuses an unknown sales value" "$code" 1
run "" --frobnicate
is "refuses an unknown option" "$code" 1

fresh_relay
run "" --sales open
is "won't open sales without keys and plans" "$code" 1
is "nothing changed" "$(v PRO_SALES_OPEN)" ""

start_fake 1
fresh_relay
before="$(cat "$tmp/relay/relay.env")"
run "$KEY_ID\n$SECRET\n"
is "a failed plan fails the run" "$code" 1
has "says it put relay.env back" "$out" "put back"
is "relay.env is exactly as before" "$(cat "$tmp/relay/relay.env")" "$before"

# A write to relay.env that fails halfway (disk full): the shell has already emptied the
# file when `cat` fails, so the script must put the backup back.
start_fake 0
fresh_relay
before="$(cat "$tmp/relay/relay.env")"
mkdir -p "$tmp/bin"
# shellcheck disable=SC2016 # the stub's own $1 and $@, expanded when it runs
printf '#!/bin/sh\ncase "$1" in */state/env.*) exit 1 ;; esac\nexec /bin/cat "$@"\n' >"$tmp/bin/cat"
chmod +x "$tmp/bin/cat"
set +e
out="$(printf '%b' "$KEY_ID\n$SECRET\n" | PATH="$tmp/bin:$PATH" RELAY_DIR="$tmp/relay" RZP_API="$api" NO_RESTART=1 "$script" 2>&1)"
code=$?
set -e
is "a failed write fails the run" "$code" 1
has "says relay.env was put back" "$out" "put back as it was"
is "relay.env is exactly as before (after a failed write)" "$(cat "$tmp/relay/relay.env")" "$before"
is "no temporary copy (it holds the keys) is left" "$(find "$tmp/relay/state" -name 'env.*' | wc -l | tr -d ' ')" 0
is "no curl credentials are left" "$(find "$tmp/relay/state" -name 'rzp.*' | wc -l | tr -d ' ')" 0
# The same through --sales (a write with no keys involved).
set +e
out="$(PATH="$tmp/bin:$PATH" RELAY_DIR="$tmp/relay" RZP_API="$api" NO_RESTART=1 "$script" --sales closed 2>&1 </dev/null)"
code=$?
set -e
is "a failed --sales write fails" "$code" 1
is "relay.env still as before" "$(cat "$tmp/relay/relay.env")" "$before"
rm -rf "${tmp:?}/bin"

if ((fails)); then
  echo "$fails failed"
  exit 1
fi
echo "All razorpay-keys.sh tests passed."
