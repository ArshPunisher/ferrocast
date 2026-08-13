#!/usr/bin/env bash
# Release (or roll back) the Ferrocast relay on the server. Installed as
# /opt/ferrocast-relay/deploy.sh by deploy/relay-ci-setup.sh.
#
# GitHub Actions ("🚀 Deploy relay") reaches this script over SSH as the `deploy` user with
# a key locked to it in ~deploy/.ssh/authorized_keys:
#   command="/opt/ferrocast-relay/deploy.sh",restrict ssh-ed25519 AAAA... ferrocast-relay-deploy
# so the key can do nothing else. The workflow sends "<version> <actor>", which arrives in
# SSH_ORIGINAL_COMMAND. By hand, as `deploy` on the server:
#   /opt/ferrocast-relay/deploy.sh 1.0.261018-ab12cd3 arsh
#
# Steps: pull the image -> switch the relay container -> wait until healthy. If the new
# version isn't healthy, the previous one is started again and the deploy fails. Nothing
# changes if the pull fails. The tunnel container is never touched.
set -euo pipefail

DIR=/opt/ferrocast-relay
STATE_DIR="$DIR/state"
IMAGE="${RELAY_IMAGE:-ghcr.io/arshpunisher/ferrocast-relay}"
CONTAINER=ferrocast-relay
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-90}" # seconds to wait for the new version
KEEP_OLDER="${KEEP_OLDER:-2}"         # older versions kept for a fast rollback

log() { printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*"; }
die() {
  log "ERROR: $*"
  exit 1
}

# ── Parse and validate the request ─────────────────────────────────────────────────────
read -r version actor extra <<<"${SSH_ORIGINAL_COMMAND:-$*}" || true
[[ "${version:-}" =~ ^1\.0\.[0-9]{6}-[0-9a-f]{7}$ ]] || die "'${version:-}' is not a version (expected 1.0.YYMMDD-abcdef1)"
[[ "${actor:-unknown}" =~ ^[A-Za-z0-9-]{1,39}$ ]] || die "invalid actor name"
[[ -z "${extra:-}" ]] || die "unexpected extra arguments"
[[ -f "$DIR/docker-compose.yml" ]] || die "$DIR/docker-compose.yml is missing: run deploy/deploy-relay.sh once from the repo"

mkdir -p "$STATE_DIR"
exec 9>"$STATE_DIR/deploy.lock"
flock -n 9 || die "another relay deploy is already running; try again when it finishes"

previous="$(cat "$STATE_DIR/current" 2>/dev/null || true)"
log "Deploying relay $version (live now: ${previous:-a hand-built image}), requested by ${actor:-unknown}"

# ── Helpers ────────────────────────────────────────────────────────────────────────────
# switch <image> <tag>: recreate only the relay container on that image.
switch() {
  RELAY_IMAGE="$1" RELAY_TAG="$2" docker compose -f "$DIR/docker-compose.yml" \
    --project-directory "$DIR" up -d --no-deps --force-recreate relay
}
wait_healthy() {
  local want="$1:$2" deadline=$((SECONDS + HEALTH_TIMEOUT)) health image
  while ((SECONDS < deadline)); do
    health="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{end}}' "$CONTAINER" 2>/dev/null || true)"
    image="$(docker inspect -f '{{.Config.Image}}' "$CONTAINER" 2>/dev/null || true)"
    if [[ "$health" == "healthy" && "$image" == "$want" ]]; then return 0; fi
    sleep 3
  done
  return 1
}

# ── Pull, switch, check ────────────────────────────────────────────────────────────────
log "Pulling $IMAGE:$version"
docker pull -q "$IMAGE:$version" >/dev/null || die "could not pull $IMAGE:$version (is the build finished, and is this server logged in to ghcr.io?)"

log "Switching the relay"
if switch "$IMAGE" "$version" && wait_healthy "$IMAGE" "$version"; then
  echo "$version" >"$STATE_DIR/current"
  log "Live: relay $version is healthy"
else
  log "relay $version did not become healthy within ${HEALTH_TIMEOUT}s"
  docker logs --tail 30 "$CONTAINER" 2>&1 | sed 's/^/  relay: /' || true
  if [[ -n "$previous" ]]; then
    if switch "$IMAGE" "$previous" && wait_healthy "$IMAGE" "$previous"; then
      die "Rolled back: $previous is live again"
    fi
    die "Rolled back to $previous, but it is not healthy either: check the server"
  elif docker image inspect ferrocast-relay:latest >/dev/null 2>&1; then
    if switch ferrocast-relay latest && wait_healthy ferrocast-relay latest; then
      die "Rolled back: the hand-built image is live again"
    fi
    die "Rolled back to the hand-built image, but it is not healthy either: check the server"
  fi
  die "no earlier version to roll back to"
fi

# ── Tidy: keep the live version, the previous one and a few older ones ────────────────
keep=$((KEEP_OLDER + 2))
docker image ls "$IMAGE" --format '{{.Tag}}' |
  grep -E '^1\.0\.[0-9]{6}-[0-9a-f]{7}$' | sort -r | tail -n +"$((keep + 1))" |
  while read -r old; do
    [[ "$old" == "$version" || "$old" == "$previous" ]] && continue
    docker image rm "$IMAGE:$old" >/dev/null 2>&1 && log "Removed old image $old" || true
  done
exit 0
