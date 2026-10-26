#!/usr/bin/env bash
# Puts Ferrocast's own code-signing identity in a throwaway keychain for one build, and takes
# it away after (R-80).
#
# macOS remembers Camera, Microphone and Screen Recording answers by an app's signature. An
# ad-hoc signature is new with every build, so every update looked like a new app and System
# Settings showed the old one switched on. Signed with one long-lived certificate, every build
# is the same app to macOS, and its answers stay. The certificate is self-made (no paid Apple
# account), so Gatekeeper still asks once on first open, exactly as before.
#
#   deploy/macos-signing.sh setup     # needs MACOS_SIGNING_P12 (base64 or a file path) and
#                                     # MACOS_SIGNING_P12_PASSWORD; prints the identity's name
#   deploy/macos-signing.sh teardown  # removes the keychain again
#
# Never prints the certificate, its key or the password.
set -euo pipefail

KEYCHAIN="${MACOS_SIGNING_KEYCHAIN:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ferrocast-signing.keychain-db}"
IDENTITY="Ferrocast (ClearCraft Labs)"

die() {
  echo "macos-signing: $*" >&2
  exit 1
}

search_list() {
  security list-keychains -d user | sed -e 's/^[[:space:]]*"//' -e 's/"$//'
}

setup() {
  [[ "$(uname)" == Darwin ]] || die "this is for macOS only"
  [[ -n "${MACOS_SIGNING_P12:-}" ]] || die "MACOS_SIGNING_P12 isn't set (base64 of the .p12, or its path)"
  [[ -n "${MACOS_SIGNING_P12_PASSWORD:-}" ]] || die "MACOS_SIGNING_P12_PASSWORD isn't set"
  local p12 kpw
  tmp=$(mktemp -d)
  # Whatever happens, the decoded certificate doesn't stay on disk. (Not a RETURN trap: that
  # would fire when teardown, called below, returns, and delete it before the import.)
  trap 'rm -rf "$tmp"' EXIT
  if [[ -f "$MACOS_SIGNING_P12" ]]; then
    p12="$MACOS_SIGNING_P12"
  else
    p12="$tmp/signing.p12"
    printf '%s' "$MACOS_SIGNING_P12" | base64 --decode > "$p12" 2>/dev/null \
      || die "MACOS_SIGNING_P12 is neither a file nor base64"
  fi
  kpw=$(openssl rand -hex 16)
  # A keychain left from an earlier run goes first.
  teardown >/dev/null 2>&1 || true
  security create-keychain -p "$kpw" "$KEYCHAIN"
  # No auto-lock during a long build.
  security set-keychain-settings "$KEYCHAIN"
  security unlock-keychain -p "$kpw" "$KEYCHAIN"
  security import "$p12" -k "$KEYCHAIN" -P "$MACOS_SIGNING_P12_PASSWORD" -T /usr/bin/codesign >/dev/null \
    || die "the certificate didn't import (wrong password?)"
  # codesign may use the key without a prompt.
  security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$kpw" "$KEYCHAIN" >/dev/null
  # codesign finds identities through the user's keychain search list.
  local list=()
  while IFS= read -r k || [[ -n "$k" ]]; do [[ -n "$k" ]] && list+=("$k"); done < <(search_list)
  # (bash 3.2 on macOS calls an empty "${list[@]}" unbound under set -u.)
  security list-keychains -d user -s "$KEYCHAIN" ${list[@]+"${list[@]}"}
  # Self-made, so not "valid" to the system's trust settings; codesign signs with it anyway.
  security find-identity -p codesigning "$KEYCHAIN" | grep -qF "\"$IDENTITY\"" \
    || die "the keychain has no identity called $IDENTITY"
  echo "$IDENTITY"
}

teardown() {
  local list=()
  while IFS= read -r k || [[ -n "$k" ]]; do [[ -n "$k" && "$k" != "$KEYCHAIN" ]] && list+=("$k"); done < <(search_list)
  if ((${#list[@]})); then security list-keychains -d user -s "${list[@]}"; fi
  if [[ -e "$KEYCHAIN" ]]; then security delete-keychain "$KEYCHAIN"; fi
}

case "${1:-}" in
  setup) setup ;;
  teardown) teardown ;;
  *) die "usage: $0 setup|teardown" ;;
esac
