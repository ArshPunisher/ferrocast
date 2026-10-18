#!/usr/bin/env bash
# Puts a new phone dashboard build (Pro "Dashboard anywhere") live on the relay server. Run
# there as root by deploy-relay.sh:
#
#   dashboard-swap.sh <dashboard dir> <release> <owner[:group]>   the build's tar.gz on stdin
#   dashboard-swap.sh --tidy <dashboard dir>              after the relay restarted: drop
#                                                         files of the old layout
#
# <dashboard dir> (/opt/ferrocast-relay/dashboard) is bind-mounted into the relay container
# at /dashboard and is never renamed or replaced: a bind mount follows the folder itself, so
# a renamed folder would leave the container serving the old (or a deleted) copy. Inside it:
#   releases/<release>/  each build, unpacked in full before it's used
#   current              a relative symlink to the live release, swapped with one rename, so
#                        a phone never sees half a build (RELAY_DASHBOARD_DIR=/dashboard/current)
# The live release and the newest other one stay; older ones go. Release names sort by time
# (deploy-relay.sh uses the UTC date, 20261019143000).
set -euo pipefail

if [[ "${1:-}" == "--tidy" ]]; then
  dir="${2:?the dashboard folder}"
  # The old layout (a build straight in the folder): everything but releases/ and current.
  find "$dir" -mindepth 1 -maxdepth 1 ! -name releases ! -name current -exec rm -rf {} +
  exit 0
fi

dir="${1:?the dashboard folder}"
name="${2:?a release name}"
owner="${3:-root}"
[[ "$owner" == *:* ]] || owner="$owner:$owner"
if [[ ! "$name" =~ ^[0-9A-Za-z][0-9A-Za-z._-]*$ ]]; then
  echo "A release name is letters, digits, dots, dashes and underscores: '$name'." >&2
  exit 1
fi

mkdir -p "$dir/releases"
chmod 755 "$dir" "$dir/releases"
part="$dir/releases/.part-$name"
rm -rf "$part"
mkdir -p "$part"
tar -xzf - -C "$part" --no-same-owner
if [[ ! -f "$part/index.html" ]]; then
  rm -rf "$part"
  echo "The build has no index.html; the live dashboard is unchanged." >&2
  exit 1
fi
chown -R "$owner" "$part"
chmod -R a+rX "$part"
rm -rf "${dir:?}/releases/$name"
mv "$part" "$dir/releases/$name"

# One rename swaps the link: a reader sees the old release or the new one, never neither.
ln -sfn "releases/$name" "$dir/current.next"
if ! mv -fT "$dir/current.next" "$dir/current" 2>/dev/null; then
  # No `mv -T` (macOS, in the tests): BSD mv would move the link *into* the old target.
  rm -f "$dir/current"
  mv -f "$dir/current.next" "$dir/current"
fi

# Keep the live release and the newest other one.
live="$(readlink "$dir/current")"
kept_other=""
for r in $(cd "$dir/releases" && printf '%s\n' * | sort -r); do
  [[ -d "$dir/releases/$r" && "$r" != .part-* ]] || continue
  [[ "releases/$r" == "$live" ]] && continue
  if [[ -z "$kept_other" ]]; then
    kept_other="$r"
    continue
  fi
  rm -rf "${dir:?}/releases/$r"
done
echo "Phone dashboard: $live"
