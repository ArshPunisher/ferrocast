#!/usr/bin/env bash
# Publishes the beta update feed after a release is published (Actions → "Beta feed").
#
#   packaging/beta-feed.sh <tag>        e.g. v0.1.0-beta.2 or v0.1.0
#
# GitHub's releases/latest/download/latest.json never points at a prerelease, so beta
# users read a feed at a fixed address instead:
#   https://github.com/ArshPunisher/ferrocast/releases/download/beta/latest.json
# It is the `latest.json` asset of a release tagged `beta` (a prerelease that is never
# "latest", made here the first time). The newest published release, beta or stable,
# goes into it, so beta users also get stable releases; a release older than what the
# feed has is skipped. Needs the GitHub CLI with GH_TOKEN (contents: write).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tag="${1:?usage: beta-feed.sh <tag>}"
repo="${GITHUB_REPOSITORY:-ArshPunisher/ferrocast}"
feed=beta
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

[[ "$tag" != "$feed" ]] || { echo "The feed release itself isn't a version; nothing to do."; exit 0; }
draft="$(gh release view "$tag" -R "$repo" --json isDraft --jq .isDraft)"
[[ "$draft" == false ]] || { echo "$tag is still a draft: publish it first." >&2; exit 1; }
gh release download "$tag" -R "$repo" -p latest.json -D "$work/new" ||
  { echo "$tag has no latest.json (was it built by the Release workflow?)." >&2; exit 1; }
new="$(jq -r .version "$work/new/latest.json")"
[[ "v$new" == "$tag" ]] || { echo "$tag's latest.json says $new; refusing a mismatched feed." >&2; exit 1; }

if gh release view "$feed" -R "$repo" >/dev/null 2>&1; then
  if gh release download "$feed" -R "$repo" -p latest.json -D "$work/old" 2>/dev/null; then
    old="$(jq -r .version "$work/old/latest.json")"
    if "$here/semver.sh" valid "$old" && ! "$here/semver.sh" gt "$new" "$old"; then
      echo "The beta feed already offers $old (same or newer than $new): left as it is."
      exit 0
    fi
  fi
else
  echo "Making the '$feed' feed release (a prerelease, never 'latest')..."
  gh release create "$feed" -R "$repo" --prerelease --latest=false --target "${GITHUB_SHA:-main}" \
    --title "Beta update feed" \
    --notes "Not a release: this holds latest.json for Ferrocast's beta update channel (Settings › Updates › Beta). Downloads are on each version's own release page."
fi
gh release upload "$feed" "$work/new/latest.json" -R "$repo" --clobber
echo "Beta feed now offers $new."
