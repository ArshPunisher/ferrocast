#!/usr/bin/env bash
# Tests for the release scripts (run in CI's static job and by hand):
#   packaging/tests.sh
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
fails=0
ok() { printf '  ok   %s\n' "$1"; }
bad() { printf '  FAIL %s\n' "$1"; fails=$((fails + 1)); }
expect() { # <description> <command...>: passes when the command succeeds
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi
}
is() { # <description> <got> <want>
  if [[ "$2" == "$3" ]]; then ok "$1"; else bad "$1: got '$2'"; fi
}
refuse() { # <description> <command...>: passes when the command fails
  local what=$1; shift
  if "$@" >/dev/null 2>&1; then bad "$what"; else ok "$what"; fi
}
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "semver.sh"
s="$here/semver.sh"
for v in 0.1.0 1.20.300 0.1.0-beta.1 0.1.0-rc.2 1.0.0-alpha.beta-x; do expect "valid $v" "$s" valid "$v"; done
for v in "" 1 1.0 v1.0.0 01.0.0 1.0.0+build 1.0.0- 1.0.0-beta..1 "1.0.0 beta"; do refuse "invalid '$v'" "$s" valid "$v"; done
newer=(
  "1.0.0 0.9.9" "0.2.0-beta.1 0.1.9" "0.1.0 0.1.0-beta.9" "0.1.0-beta.10 0.1.0-beta.9"
  "0.1.0-rc.1 0.1.0-beta.9" "0.1.0-beta.1 0.1.0-beta" "0.1.0-alpha.beta 0.1.0-alpha.1" "0.1.1-beta.1 0.1.0"
)
for pair in "${newer[@]}"; do
  read -r a b <<<"$pair"
  expect "$a > $b" "$s" gt "$a" "$b"
  refuse "$b < $a" "$s" gt "$b" "$a"
done
refuse "0.1.0 = 0.1.0 is not newer" "$s" gt 0.1.0 0.1.0
refuse "0.1.0-beta.1 = 0.1.0-beta.1 is not newer" "$s" gt 0.1.0-beta.1 0.1.0-beta.1
refuse "gt refuses non-versions" "$s" gt latest 0.1.0
is "a beta is a prerelease" "$("$s" pre 0.1.0-beta.1)" true
is "a release isn't" "$("$s" pre 0.1.0)" false

echo "check-version.sh"
c="$here/check-version.sh"
printf '[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.1.0-beta.1"\nedition = "2024"\n\n[package]\nversion = "9.9.9"\n' >"$tmp/Cargo.toml"
out="$("$c" v0.1.0-beta.1 "$tmp/Cargo.toml")"
is "matching beta tag" "$out" $'version=0.1.0-beta.1\nprerelease=true'
refuse "a tag that differs from Cargo.toml" "$c" v0.1.0 "$tmp/Cargo.toml"
refuse "a tag without v" "$c" 0.1.0-beta.1 "$tmp/Cargo.toml"
refuse "a tag that isn't a version" "$c" vnext "$tmp/Cargo.toml"
printf '[workspace.package]\nversion = "0.1.0"\n' >"$tmp/Stable.toml"
is "stable tag" "$("$c" v0.1.0 "$tmp/Stable.toml" | tail -n 1)" prerelease=false
expect "this repo's Cargo.toml matches its own version" "$c" "v$(awk '/^\[workspace.package\]/{s=1;next} /^\[/{s=0} s&&$1=="version"{gsub(/"/,"",$3);print $3;exit}' "$here/../Cargo.toml")"

echo "updater-manifest.sh"
m="$here/updater-manifest.sh"
mkdir -p "$tmp/dist"
for f in Ferrocast.app.tar.gz Ferrocast_0.1.0-beta.1_x64-setup.exe Ferrocast_0.1.0-beta.1_amd64.AppImage \
  Ferrocast_0.1.0-beta.1_amd64.deb Ferrocast-0.1.0-beta.1-1.x86_64.rpm; do
  echo bundle >"$tmp/dist/$f"
  printf 'sig-of-%s\n' "$f" >"$tmp/dist/$f.sig"
done
if command -v jq >/dev/null; then
  json="$("$m" 0.1.0-beta.1 "$tmp/dist")"
  is "version" "$(jq -r .version <<<"$json")" 0.1.0-beta.1
  is "urls point at the tag's files on the mirror" "$(jq -r '.platforms["windows-x86_64"].url' <<<"$json")" \
    https://ferrocast.clearcraftlabs.com/dl/v0.1.0-beta.1/Ferrocast_0.1.0-beta.1_x64-setup.exe
  is "every platform" "$(jq -r '.platforms | keys | join(",")' <<<"$json")" \
    darwin-aarch64,darwin-x86_64,linux-x86_64,linux-x86_64-deb,linux-x86_64-rpm,windows-x86_64
  is "signature" "$(jq -r '.platforms["darwin-aarch64"].signature' <<<"$json")" sig-of-Ferrocast.app.tar.gz
  # Betas ship no .rpm (RPM refuses a '-' in the version): the manifest just leaves it out.
  rm "$tmp/dist/"*.rpm "$tmp/dist/"*.rpm.sig
  is "a beta without an rpm" "$(jq -r '.platforms | keys | join(",")' <<<"$("$m" 0.1.0-beta.1 "$tmp/dist")")" \
    darwin-aarch64,darwin-x86_64,linux-x86_64,linux-x86_64-deb,windows-x86_64
  is "no notes: a link to GitHub" "$(jq -r .notes <<<"$json")" "See the release notes on GitHub."
  printf -- '- Faster alerts\n- "Quoted" words\n' >"$tmp/notes.md"
  is "notes from the changelog" "$(jq -r .notes <<<"$("$m" 0.1.0-beta.1 "$tmp/dist" "$tmp/notes.md")")" \
    "$(printf -- '- Faster alerts\n- "Quoted" words')"
  refuse "refuses a notes file that isn't there" "$m" 0.1.0-beta.1 "$tmp/dist" "$tmp/nope.md"
  refuse "refuses a version that isn't one" "$m" "0.1 beta" "$tmp/dist"
  mkdir -p "$tmp/empty"
  refuse "refuses a folder without signed bundles" "$m" 0.1.0 "$tmp/empty"
else
  echo "  (jq not installed, skipped)"
fi

echo "release-notes.sh"
n="$here/release-notes.sh"
cat >"$tmp/CHANGELOG.md" <<'MD'
# What's new

## 1.2.0

- New thing

- Second thing

## 1.1.0
- Old thing
MD
is "one version's section" "$("$n" 1.2.0 "$tmp/CHANGELOG.md")" "$(printf -- '- New thing\n\n- Second thing')"
is "the last section" "$("$n" 1.1.0 "$tmp/CHANGELOG.md")" "- Old thing"
printf '## 2.0.0\r\n- Windows line ends\r\n' >"$tmp/crlf.md"
is "a changelog with Windows line ends" "$("$n" 2.0.0 "$tmp/crlf.md")" "- Windows line ends"
refuse "refuses a version with no section" "$n" 1.3.0 "$tmp/CHANGELOG.md"
refuse "refuses a heading prefix (1.2 is not 1.2.0)" "$n" 1.2 "$tmp/CHANGELOG.md"
"$n" "$(sed -n 's/^version = "\(.*\)"/\1/p' "$here/../Cargo.toml" | head -1)" >/dev/null ||
  { echo "  FAIL: CHANGELOG.md has no section for the version in Cargo.toml"; fails=$((fails + 1)); }

if ((fails)); then
  echo "$fails failed"
  exit 1
fi
echo "All packaging tests passed."
