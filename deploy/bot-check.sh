#!/usr/bin/env bash
# The owner's look at the Twitch cloud bot from the server. Installed as
# /opt/ferrocast-relay/bot-check.sh by deploy/relay-ci-setup.sh. From your Mac:
#
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/bot-check.sh'                every bound channel
#   ssh -t <server> 'sudo -u deploy /opt/ferrocast-relay/bot-check.sh <install-id>'   one install's
#
# Runs `ferrocast-relay bot-check …` inside the running relay container (same env and
# database; crates/relay/src/bot/check.rs): the conduit (saved vs Twitch), shard 0's status
# and callback, the EventSub subscriptions per channel, and each channel's row. Read only:
# nothing is changed at Twitch or in the database. Prints no tokens or secrets. It's a
# separate process, so who's on duty (the running relay's in-memory lease) isn't shown.
set -euo pipefail

CONTAINER="${RELAY_CONTAINER:-ferrocast-relay}"

die() {
  echo "$*" >&2
  exit 1
}

case "${1:-}" in
  --help | -h)
    die "Use: bot-check.sh [<install-id>]"
    ;;
  "")
    [[ $# -eq 0 ]] || die "Use: bot-check.sh [<install-id>]"
    ;;
  *)
    [[ $# -eq 1 ]] || die "Use: bot-check.sh [<install-id>]"
    [[ "$1" =~ ^[A-Za-z0-9_-]{8,64}$ ]] || die "'$1' doesn't look like an install id (only the part before the dot)."
    ;;
esac

command -v docker >/dev/null || die "No docker here: run this on the relay server."
running="$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)"
[[ "$running" == true ]] || die "The relay container ($CONTAINER) isn't running. Start the relay first."
exec docker exec "$CONTAINER" /usr/local/bin/ferrocast-relay bot-check "$@"
