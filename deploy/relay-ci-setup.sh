#!/usr/bin/env bash
# One-time setup so GitHub Actions can deploy the relay (Actions → "🚀 Deploy relay").
# Run it from the repo on your Mac, after the relay has been deployed by hand once:
#
#   deploy/relay-ci-setup.sh [ssh-host]      (host: argument, else RELAY_HOST in ../secrets/relay.env)
#
# It reuses the server's existing `deploy` user (the one TransformX deploys with: in the
# docker group and logged in to ghcr.io) and adds a SEPARATE key for Ferrocast, locked to
# /opt/ferrocast-relay/deploy.sh, so it can do nothing else. TransformX is not touched.
# Then it saves RELAY_DEPLOY_HOST, _PORT, _SSH_KEY and _KNOWN_HOSTS as secrets of the
# ferrocast repo's `relay` environment, which only main can use. Safe to run again.
# shellcheck disable=SC2029 # values are meant to expand locally, before ssh sends them.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
secrets="$repo/../secrets/relay.env"
value() { [[ -f "$secrets" ]] && sed -n "s/^$1=//p" "$secrets" | tail -n 1; }
host="${1:-$(value RELAY_HOST || true)}"
[[ -n "$host" ]] || {
  echo "No server given: pass the ssh host, or set RELAY_HOST in $secrets." >&2
  exit 1
}
gh_repo="ArshPunisher/ferrocast"
key="$HOME/.ssh/ferrocast-relay-deploy"
dir=/opt/ferrocast-relay

command -v gh >/dev/null || {
  echo "Needs the GitHub CLI (gh), logged in." >&2
  exit 1
}

if [[ ! -f "$key" ]]; then
  echo "Making a deploy-only key at $key..."
  ssh-keygen -q -t ed25519 -N "" -C ferrocast-relay-deploy -f "$key"
fi
pub="$(cat "$key.pub")"

echo "Checking the server..."
ssh "$host" "id deploy >/dev/null 2>&1" || {
  echo "The server has no 'deploy' user (see transformx-infra/DEPLOY.md §2)." >&2
  exit 1
}
ssh "$host" "id -nG deploy | grep -qw docker" || {
  echo "'deploy' is not in the docker group." >&2
  exit 1
}
ssh "$host" "sudo test -f $dir/docker-compose.yml" || {
  echo "$dir/docker-compose.yml is missing: run deploy/deploy-relay.sh once first." >&2
  exit 1
}
ssh "$host" "sudo grep -q ghcr.io ~deploy/.docker/config.json 2>/dev/null" ||
  echo "WARNING: 'deploy' is not logged in to ghcr.io; deploys can't pull images until it is (DEPLOY.md §6)." >&2

echo "Installing the deploy, Razorpay-keys, vault-key, pro-keys, pro-grant, bot-check, relay-backup and control-center-key scripts and the current compose file, and handing the relay files to 'deploy'..."
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/deploy.sh" <"$repo/deploy/relay-server-deploy.sh"
# Pro's Razorpay keys are typed on the server with this (deploy/razorpay-keys.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/razorpay-keys.sh" <"$repo/deploy/razorpay-keys.sh"
# The vault key for Pro's stored secrets is made on the server with this (deploy/vault-key.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/vault-key.sh" <"$repo/deploy/vault-key.sh"
# Pro's service keys (Google voices, the Discord bot) are typed on the server with this (deploy/pro-keys.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/pro-keys.sh" <"$repo/deploy/pro-keys.sh"
# The owner's test Pro for one install, without Razorpay (deploy/pro-grant.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/pro-grant.sh" <"$repo/deploy/pro-grant.sh"
# The owner's read-only look at the Twitch cloud bot (deploy/bot-check.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/bot-check.sh" <"$repo/deploy/bot-check.sh"
# The nightly encrypted backup to Cloudflare R2 and its restore drill (deploy/relay-backup.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/relay-backup.sh" <"$repo/deploy/relay-backup.sh"
# The Control Center's admin-stats token for the relay and TransformX's .env.prod (deploy/control-center-key.sh).
ssh "$host" "sudo install -m 755 -o root -g root /dev/stdin $dir/control-center-key.sh" <"$repo/deploy/control-center-key.sh"
# The compose file must read RELAY_IMAGE/RELAY_TAG; older installs don't. Containers keep
# running: compose only reads it on the next deploy.
ssh "$host" "sudo install -m 644 -o deploy -g deploy /dev/stdin $dir/docker-compose.yml" <"$repo/deploy/docker-compose.yml"
ssh "$host" "sudo install -d -m 750 -o deploy -g deploy $dir/state &&
  for f in relay.env tunnel.env; do if sudo test -f $dir/\$f; then sudo chown deploy:deploy $dir/\$f && sudo chmod 600 $dir/\$f; fi; done"

echo "Locking the key to the deploy script..."
line="command=\"$dir/deploy.sh\",restrict $pub"
ssh "$host" "sudo install -d -m 700 -o deploy -g deploy ~deploy/.ssh &&
  sudo touch ~deploy/.ssh/authorized_keys &&
  sudo chown deploy:deploy ~deploy/.ssh/authorized_keys && sudo chmod 600 ~deploy/.ssh/authorized_keys &&
  (sudo grep -qF '$pub' ~deploy/.ssh/authorized_keys || {
    sudo sed -i -e '\$a\\' ~deploy/.ssh/authorized_keys
    echo '$line' | sudo tee -a ~deploy/.ssh/authorized_keys >/dev/null
  })"

public="$(ssh -G "$host" | awk '/^hostname /{print $2}')"
port="$(ssh -G "$host" | awk '/^port /{print $2}')"
if ssh -G "$host" | grep -qE '^proxyjump [^n]'; then
  echo "$host goes through a jump host; GitHub needs to reach the server directly." >&2
  exit 1
fi
echo "Saving GitHub secrets for $gh_repo (server $public, port $port)..."
# The host key you already trust from your own logins, not whatever answers the network.
lookup="$public"
[[ "$port" != 22 ]] && lookup="[$public]:$port"
known="$(ssh-keygen -F "$lookup" 2>/dev/null | grep -v '^#' | grep ' ssh-ed25519 ' | head -n 1 || true)"
if [[ -z "$known" ]]; then
  echo "No saved host key for $lookup in ~/.ssh/known_hosts. Log in once with: ssh -p $port $public" >&2
  exit 1
fi
# Make sure the environment exists and only main may use it.
gh api -X PUT "repos/$gh_repo/environments/relay" --input - >/dev/null <<'JSON'
{"deployment_branch_policy": {"protected_branches": false, "custom_branch_policies": true}}
JSON
gh api "repos/$gh_repo/environments/relay/deployment-branch-policies" --jq '.branch_policies[].name' 2>/dev/null |
  grep -qx main || gh api -X POST "repos/$gh_repo/environments/relay/deployment-branch-policies" -f name=main -f type=branch >/dev/null
printf '%s' "$public" | gh secret set RELAY_DEPLOY_HOST -R "$gh_repo" --env relay
printf '%s' "$port" | gh secret set RELAY_DEPLOY_PORT -R "$gh_repo" --env relay
gh secret set RELAY_DEPLOY_SSH_KEY -R "$gh_repo" --env relay <"$key"
printf '%s\n' "$known" | gh secret set RELAY_DEPLOY_KNOWN_HOSTS -R "$gh_repo" --env relay

echo "Testing the locked key (it may only run the deploy script)..."
out="$(ssh -i "$key" -p "$port" -o IdentitiesOnly=yes -o BatchMode=yes "deploy@$public" "not-a-version x" 2>&1 || true)"
if grep -q "is not a version" <<<"$out"; then
  echo "Done. Release the relay with: Actions → 🚀 Deploy relay → Run workflow."
else
  echo "The key did not reach the deploy script. Server said:" >&2
  echo "$out" >&2
  exit 1
fi
