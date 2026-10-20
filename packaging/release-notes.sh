#!/usr/bin/env bash
# Prints one version's section of CHANGELOG.md: what the app shows under "What's new" and
# the GitHub release's text.
#
#   packaging/release-notes.sh <version> [CHANGELOG.md]
#
# Fails when the changelog has no "## <version>" section, so a release can't ship without
# notes by accident (the Release workflow then falls back to a link and warns).
set -euo pipefail
version=${1:?usage: release-notes.sh <version> [changelog]}
file=${2:-"$(dirname "$0")/../CHANGELOG.md"}
[[ -f "$file" ]] || { echo "No changelog at $file" >&2; exit 1; }
notes="$(awk -v want="## $version" '
  { sub(/\r$/, "") }
  $0 == want { on = 1; next }
  on && /^## / { exit }
  on { print }
' "$file" | sed -e '/./,$!d' | sed -e ':a' -e '/^\n*$/{$d;N;ba' -e '}')"
[[ -n "$notes" ]] || { echo "CHANGELOG.md has no notes for $version (add a '## $version' section)" >&2; exit 1; }
printf '%s\n' "$notes"
