#!/usr/bin/env bash
# Builds the relay image, ships it to the server and (re)starts it behind its own
# Cloudflare Tunnel. Nothing else on the server is touched.
#
#   deploy/deploy-relay.sh [ssh-host]
#
# Secrets come from ../secrets/relay.env next to the repo (never committed):
#   RELAY_HOST=...            ssh host (or ~/.ssh/config alias) of the server; an
#                             argument overrides it
#   TUNNEL_TOKEN=...          from Cloudflare → Networking → Tunnels → ferrocast-relay
#   KICK_CLIENT_ID=...        from kick.com → Settings → Developer (optional until then)
#   KICK_CLIENT_SECRET=...
# Razorpay keys for Ferrocast Pro are NOT here: they're typed on the server with
# razorpay-keys.sh (see deploy/README.md), and this script keeps them as they are.
# shellcheck disable=SC2029 # $dir is meant to expand here, before ssh sends the command.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
secrets="$repo/../secrets/relay.env"
dir=/opt/ferrocast-relay

if [[ ! -f "$secrets" ]]; then
  echo "Missing $secrets (needs TUNNEL_TOKEN, and KICK_CLIENT_ID/KICK_CLIENT_SECRET once the Kick app exists)." >&2
  exit 1
fi
# Read as plain KEY=value lines, never executed (a pasted command must not run).
value() { sed -n "s/^$1=//p" "$secrets" | tail -n 1; }
host="${1:-$(value RELAY_HOST)}"
if [[ -z "$host" ]]; then
  echo "No server given: pass the ssh host as an argument, or set RELAY_HOST in $secrets." >&2
  exit 1
fi
TUNNEL_TOKEN="$(value TUNNEL_TOKEN)"
KICK_CLIENT_ID="$(value KICK_CLIENT_ID)"
KICK_CLIENT_SECRET="$(value KICK_CLIENT_SECRET)"
# The dashboard's copy button gives a whole command ("docker run ... --token eyJ..."); keep
# only the token.
TUNNEL_TOKEN="${TUNNEL_TOKEN##* }"
if [[ -z "${TUNNEL_TOKEN:-}" ]]; then
  echo "TUNNEL_TOKEN is empty in $secrets." >&2
  exit 1
fi

# Once GitHub Actions deploys the relay ("🚀 Deploy relay"), this script only updates the
# config and secrets and keeps the version that is live; it never swaps in a local build.
live="$(ssh "$host" "sudo cat $dir/state/current 2>/dev/null || true")"
if [[ -n "$live" && ! "$live" =~ ^1\.0\.[0-9]{6}-[0-9a-f]{7}$ ]]; then
  echo "Unexpected live version '$live' in $dir/state/current; fix it on the server first." >&2
  exit 1
fi
if [[ -n "$live" ]]; then
  echo "GitHub Actions deploys the relay now (live: $live): updating config and secrets only."
  image_env="RELAY_IMAGE=ghcr.io/arshpunisher/ferrocast-relay RELAY_TAG=$live"
else
  echo "Building the relay image..."
  docker build -q -f "$repo/deploy/relay.Dockerfile" -t ferrocast-relay:latest "$repo" >/dev/null
  echo "Sending it to $host..."
  docker save ferrocast-relay:latest | gzip | ssh "$host" 'sudo docker load' >/dev/null
  image_env="RELAY_IMAGE=ferrocast-relay RELAY_TAG=latest"
fi
# The files belong to the server's `deploy` user once deploys come from GitHub Actions.
owner="$(ssh "$host" "id deploy >/dev/null 2>&1 && test -f $dir/deploy.sh && echo deploy || echo root")"

echo "Writing the config..."
# Secrets go straight to their final, root-owned 0600 files through `sudo install` reading
# stdin: nothing passes through a guessable /tmp path another user could pre-create.
ssh "$host" "sudo mkdir -p $dir && sudo chmod 755 $dir"
ssh "$host" "sudo install -m 644 -o $owner -g $owner /dev/stdin $dir/docker-compose.yml" < "$repo/deploy/docker-compose.yml"
# Pro's Razorpay settings (RAZORPAY_*, PRO_SALES_OPEN, PRO_EMAIL_KEY) live only on the server, written
# there by razorpay-keys.sh: they're carried over from the current relay.env on the server
# itself, so they never travel to this Mac and a config update can't wipe them.
{
  printf 'RELAY_PUBLIC_URL=https://ferrocast-relay.clearcraftlabs.com\n'
  printf 'RELAY_TIP_URL=https://tip.clearcraftlabs.com\n'
  printf 'KICK_CLIENT_ID=%s\n' "${KICK_CLIENT_ID:-}"
  printf 'KICK_CLIENT_SECRET=%s\n' "${KICK_CLIENT_SECRET:-}"
  printf 'RUST_LOG=info\n'
} | ssh "$host" "sudo sh -c 'umask 077 && cat > $dir/relay.env.new &&
    { grep -E \"^(RAZORPAY_[A-Z_]+|PRO_SALES_OPEN|PRO_EMAIL_KEY)=\" $dir/relay.env 2>/dev/null || true; } >> $dir/relay.env.new &&
    chown $owner:$owner $dir/relay.env.new && chmod 600 $dir/relay.env.new && mv $dir/relay.env.new $dir/relay.env'"
printf 'TUNNEL_TOKEN=%s\n' "$TUNNEL_TOKEN" | ssh "$host" "sudo install -m 600 -o $owner -g $owner /dev/stdin $dir/tunnel.env"

ssh "$host" "set -e
  cd $dir && sudo env $image_env docker compose up -d --remove-orphans
  sleep 5
  sudo docker ps --filter name=ferrocast --format '{{.Names}}: {{.Status}}'"

echo "Checking it from the internet..."
for _ in 1 2 3 4 5 6; do
  if curl -fsS https://ferrocast-relay.clearcraftlabs.com/health 2>/dev/null | grep -q ok; then
    echo "The relay answers at https://ferrocast-relay.clearcraftlabs.com"
    exit 0
  fi
  sleep 5
done
echo "Started, but https://ferrocast-relay.clearcraftlabs.com/health doesn't answer yet. Check the tunnel's public hostname in Cloudflare." >&2
exit 1
