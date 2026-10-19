#!/usr/bin/env bash
# The owner's test Pro: Pro for one install without Razorpay, to try Pro end to end on your
# own desktop. Installed as /opt/ferrocast-relay/pro-grant.sh by deploy/relay-ci-setup.sh.
# From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh <install-id> <days>'     grant or extend (1-90 days)
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh --revoke <install-id>'   end it now
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/pro-grant.sh --list'                  every grant
#
# Runs `ferrocast-relay pro-grant …` inside the running relay container (same database,
# safe while it serves; crates/relay/src/billing/comp.rs). No HTTP route does this, and
# nothing goes to Razorpay. Prints no secrets: install ids aren't secret (the install's
# secret, after the dot, is never needed or asked for).
set -euo pipefail

CONTAINER="${RELAY_CONTAINER:-ferrocast-relay}"

die() {
  echo "$*" >&2
  exit 1
}

# An install id is the relay's random id: letters, digits, - and _ only.
id_ok() { [[ "$1" =~ ^[A-Za-z0-9_-]{8,64}$ ]]; }

case "${1:-}" in
  --list)
    [[ $# -eq 1 ]] || die "Use: pro-grant.sh --list"
    ;;
  --revoke)
    [[ $# -eq 2 ]] || die "Use: pro-grant.sh --revoke <install-id>"
    id_ok "$2" || die "'$2' doesn't look like an install id (only the part before the dot)."
    ;;
  "" | -*)
    die "Use: pro-grant.sh <install-id> <days> | --revoke <install-id> | --list"
    ;;
  *)
    [[ $# -eq 2 ]] || die "Use: pro-grant.sh <install-id> <days>"
    id_ok "$1" || die "'$1' doesn't look like an install id (only the part before the dot)."
    [[ "$2" =~ ^[0-9]{1,2}$ ]] || die "Days must be a number from 1 to 90."
    ;;
esac

command -v docker >/dev/null || die "No docker here: run this on the relay server."
running="$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)"
[[ "$running" == true ]] || die "The relay container ($CONTAINER) isn't running. Start the relay first."
exec docker exec "$CONTAINER" /usr/local/bin/ferrocast-relay pro-grant "$@"
