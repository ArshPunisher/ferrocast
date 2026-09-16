#!/usr/bin/env bash
# The release tag must name the version in Cargo.toml ([workspace.package] version), or
# the app would report one version and the update feed another.
#
#   packaging/check-version.sh <tag> [Cargo.toml]
#
# Prints `version=<v>` and `prerelease=true|false` (for $GITHUB_OUTPUT); fails with a
# plain message when the tag and Cargo.toml differ or either isn't a version.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tag="${1:?usage: check-version.sh <tag like v0.1.0-beta.1> [Cargo.toml]}"
manifest="${2:-$here/../Cargo.toml}"

[[ "$tag" == v* ]] || { echo "The tag '$tag' must start with v (like v0.1.0-beta.1)." >&2; exit 1; }
want="${tag#v}"
"$here/semver.sh" valid "$want" || {
  echo "The tag '$tag' isn't a version (expected vMAJOR.MINOR.PATCH or vMAJOR.MINOR.PATCH-beta.N)." >&2
  exit 1
}
# The first `version = "…"` inside [workspace.package].
have="$(awk '
  /^\[/ { section = $0; next }
  section == "[workspace.package]" && $1 == "version" { gsub(/"/, "", $3); print $3; exit }
' "$manifest")"
[[ -n "$have" ]] || { echo "No [workspace.package] version in $manifest." >&2; exit 1; }
if [[ "$have" != "$want" ]]; then
  echo "The tag says $want but Cargo.toml says $have. Bump the workspace version (and Cargo.lock) to $want, or tag v$have." >&2
  exit 1
fi
echo "version=$want"
echo "prerelease=$("$here/semver.sh" pre "$want")"
