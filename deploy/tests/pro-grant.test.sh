#!/usr/bin/env bash
# Tests deploy/pro-grant.sh with a stand-in docker (no server, no container):
#   deploy/tests/pro-grant.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../pro-grant.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -q -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
# The stub logs its arguments and says the container runs unless DOWN=1.
# shellcheck disable=SC2016 # the stub's own variables, expanded when it runs
printf '#!/bin/sh\necho "$@" >>"%s/calls"\nif [ "$1" = inspect ]; then [ "${DOWN:-0}" = 1 ] && echo false || echo true; fi\n' "$tmp" >"$tmp/bin/docker"
chmod +x "$tmp/bin/docker"
run() { # [args…] → output in $out, status in $code
  rm -f "$tmp/calls"
  set +e
  out="$(PATH="$tmp/bin:$PATH" "$script" "$@" 2>&1 </dev/null)"
  code=$?
  set -e
}
last() { tail -n 1 "$tmp/calls" 2>/dev/null || true; }

echo "pro-grant.sh"
run abcdEFGH1234_- 7
is "a grant runs" "$code" 0
is "runs the relay's subcommand in the container" "$(last)" "exec ferrocast-relay /usr/local/bin/ferrocast-relay pro-grant abcdEFGH1234_- 7"
run --revoke abcdEFGH1234
is "revoke runs" "$code" 0
is "passes --revoke" "$(last)" "exec ferrocast-relay /usr/local/bin/ferrocast-relay pro-grant --revoke abcdEFGH1234"
run --list
is "list runs" "$code" 0
is "passes --list" "$(last)" "exec ferrocast-relay /usr/local/bin/ferrocast-relay pro-grant --list"

run
is "no arguments is refused" "$code" 1
has "says how to use it" "$out" "Use:"
run 'abcdEFGH1234.secret' 7
is "an id with its secret is refused" "$code" 1
has "says only the part before the dot" "$out" "before the dot"
run 'abc;rm -rf /' 7
is "odd characters are refused" "$code" 1
run abcdEFGH1234 seven
is "days must be a number" "$code" 1
run abcdEFGH1234 7 extra
is "extra arguments are refused" "$code" 1
run --revoke
is "revoke needs an id" "$code" 1
run --nope
is "unknown options are refused" "$code" 1
is "nothing reached docker on a refusal" "$(last)" ""

rm -f "$tmp/calls"
set +e
out="$(DOWN=1 PATH="$tmp/bin:$PATH" "$script" --list 2>&1 </dev/null)"
code=$?
set -e
is "a stopped relay is refused" "$code" 1
has "says to start the relay" "$out" "isn't running"

if ((fails)); then
  echo "$fails failed"
  exit 1
fi
echo "All pro-grant.sh tests passed."
