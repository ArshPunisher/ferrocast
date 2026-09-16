#!/usr/bin/env bash
# Semantic version checks for release scripts (https://semver.org §11 precedence).
#
#   packaging/semver.sh valid <version>      exit 0 if it's MAJOR.MINOR.PATCH[-PRERELEASE]
#   packaging/semver.sh gt <a> <b>           exit 0 if a is newer than b
#   packaging/semver.sh pre <version>        prints true for a prerelease (0.1.0-beta.1)
#
# Build metadata (+…) isn't used by Ferrocast and is refused.
set -euo pipefail

re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-([0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*))?$'

valid() { [[ "$1" =~ $re ]]; }

# cmp_ids <a> <b>: prints -1, 0 or 1 comparing two dot-separated prerelease strings.
cmp_ids() {
  local IFS=.
  local -a a b
  read -ra a <<<"$1"
  read -ra b <<<"$2"
  local i x y n=${#a[@]}
  ((${#b[@]} > n)) && n=${#b[@]}
  for ((i = 0; i < n; i++)); do
    x="${a[i]-}" y="${b[i]-}"
    [[ -z "$x" ]] && { echo -1; return; } # fewer identifiers sort first
    [[ -z "$y" ]] && { echo 1; return; }
    if [[ "$x" =~ ^[0-9]+$ && "$y" =~ ^[0-9]+$ ]]; then
      ((10#$x < 10#$y)) && { echo -1; return; }
      ((10#$x > 10#$y)) && { echo 1; return; }
    elif [[ "$x" =~ ^[0-9]+$ ]]; then
      echo -1; return # numeric identifiers sort before alphanumeric ones
    elif [[ "$y" =~ ^[0-9]+$ ]]; then
      echo 1; return
    elif [[ "$x" < "$y" ]]; then
      echo -1; return
    elif [[ "$x" > "$y" ]]; then
      echo 1; return
    fi
  done
  echo 0
}

gt() {
  if ! valid "$1" || ! valid "$2"; then
    echo "not a version: '$1' or '$2'" >&2
    exit 2
  fi
  local -a a b
  [[ "$1" =~ $re ]] && a=("${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[5]}")
  [[ "$2" =~ $re ]] && b=("${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}" "${BASH_REMATCH[3]}" "${BASH_REMATCH[5]}")
  local k
  for k in 0 1 2; do
    ((a[k] > b[k])) && return 0
    ((a[k] < b[k])) && return 1
  done
  # Same MAJOR.MINOR.PATCH: a release is newer than any of its prereleases.
  local ap="${a[3]}" bp="${b[3]}"
  [[ -z "$ap" && -n "$bp" ]] && return 0
  [[ -z "$ap" || -z "$bp" ]] && return 1
  [[ "$(cmp_ids "$ap" "$bp")" == 1 ]]
}

case "${1:-}" in
  valid) valid "${2:-}" ;;
  gt) gt "${2:-}" "${3:-}" ;;
  pre)
    valid "${2:-}" || { echo "not a version: '${2:-}'" >&2; exit 2; }
    [[ "$2" == *-* ]] && echo true || echo false
    ;;
  *)
    echo "usage: semver.sh valid <v> | gt <a> <b> | pre <v>" >&2
    exit 2
    ;;
esac
