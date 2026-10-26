#!/usr/bin/env bash
# Tests deploy/macos-signing.sh with a stand-in `security` and `uname` (no real keychain is
# touched, so it runs on Linux too):
#   deploy/tests/macos-signing.test.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../macos-signing.sh"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
is() { if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi; }
has() { if grep -qF -- "$3" <<<"$2"; then ok "$1"; else bad "$1: no '$3' in: $2"; fi; }
hasnt() { if grep -qF -- "$3" <<<"$2"; then bad "$1: '$3' in: $2"; else ok "$1"; fi; }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin"
login="$tmp/login.keychain-db"
kc="$tmp/ferrocast-signing.keychain-db"
printf 'login' >"$tmp/list"
# `security`: logs its arguments, keeps the search list in a file, makes keychain files, and
# finds the identity unless NOID=1.
cat >"$tmp/bin/security" <<EOF
#!/usr/bin/env bash
echo "\$*" >>"$tmp/calls"
case "\$1" in
  list-keychains)
    if [[ "\${4:-}" == -s ]]; then shift 4; printf '%s\n' "\$@" >"$tmp/list"
    else sed -e 's#^login\$#$login#' -e 's/^/    "/' -e 's/\$/"/' "$tmp/list"; fi ;;
  create-keychain) touch "\${@: -1}" ;;
  delete-keychain) rm -f "\$2" ;;
  find-identity) [[ "\${NOID:-0}" == 1 ]] || echo '  1) ABC "Ferrocast (ClearCraft Labs)" (CSSMERR_TP_NOT_TRUSTED)' ;;
esac
EOF
printf '#!/bin/sh\necho Darwin\n' >"$tmp/bin/uname"
chmod +x "$tmp/bin/security" "$tmp/bin/uname"
printf 'not really a certificate' >"$tmp/c.p12"

run() { # [args…] → output in $out, status in $code
  rm -f "$tmp/calls"
  set +e
  out="$(PATH="$tmp/bin:$PATH" MACOS_SIGNING_KEYCHAIN="$kc" "$script" "$@" 2>&1 </dev/null)"
  code=$?
  set -e
}

echo "macos-signing.sh"
run
is "no command: refused" "$code" 1
has "no command: says how" "$out" "usage"

run setup
is "no certificate: refused" "$code" 1
has "no certificate: says which" "$out" "MACOS_SIGNING_P12 isn't set"

export MACOS_SIGNING_P12='***not base64***' MACOS_SIGNING_P12_PASSWORD='s3cret-pw'
run setup
is "bad base64: refused" "$code" 1
has "bad base64: says so" "$out" "neither a file nor base64"

export MACOS_SIGNING_P12="$tmp/c.p12"
run setup
is "setup: works" "$code" 0
is "setup: prints only the identity" "$out" "Ferrocast (ClearCraft Labs)"
hasnt "setup: never prints the password" "$out" "s3cret-pw"
has "setup: imports for codesign" "$(cat "$tmp/calls")" "import $tmp/c.p12 -k $kc -P s3cret-pw -T /usr/bin/codesign"
is "setup: keychain first, login kept" "$(tr '\n' ' ' <"$tmp/list")" "$kc $login "

MACOS_SIGNING_P12="$(base64 <"$tmp/c.p12" | tr -d '\n')" run setup
is "setup from base64: works" "$code" 0
is "setup again: listed once" "$(grep -c ferrocast-signing "$tmp/list")" 1

NOID=1 run setup
is "no identity inside: refused" "$code" 1
has "no identity inside: says so" "$out" "no identity called"

run teardown
is "teardown: works" "$code" 0
is "teardown: only login left" "$(tr '\n' ' ' <"$tmp/list")" "$login "
if [[ -e "$kc" ]]; then bad "teardown: keychain deleted"; else ok "teardown: keychain deleted"; fi

printf '#!/bin/sh\necho Linux\n' >"$tmp/bin/uname"
run setup
is "not a Mac: refused" "$code" 1
has "not a Mac: says so" "$out" "macOS only"

if ((fails)); then echo "$fails failed"; exit 1; fi
echo "all passed"
