#!/usr/bin/env bash
# Puts OBS's engine (libobs, its renderers, the plugins the Studio uses and their libraries)
# inside Ferrocast.app, so the Studio works on a Mac without OBS installed.
#
# Takes them unchanged from OBS's own release (OBS_VERSION below, the libobs our ffi.rs is
# written against), checked by sha256, and makes them universal (Apple + Intel) like the app.
# Leaves out what needs OBS's Qt window or Chromium: Qt, CEF, the UI-only plugins, obs-browser
# (web pages are Ferrocast's own WebKit source on a Mac), obs-vst and mac-virtualcam.
#
#   deploy/obs-runtime-macos.sh [out]   # out: default apps/desktop/obs-runtime
#
# Writes <out>/Contents/{Frameworks,PlugIns,Helpers,Resources/OBS-NOTICE.txt} and
# <out>/tauri.obs.json, which a build passes to `cargo tauri build --config` to copy them in.
# Signs every binary with APPLE_SIGNING_IDENTITY (ad-hoc when unset): lipo breaks OBS's
# signatures, and signing the app checks everything inside it is signed.
set -euo pipefail

OBS_VERSION=32.2.2
SHA_APPLE=920d6f26703d2df6e4085bd3c1cbed30488325084136c7a6e9e37021fbd6aaf7
SHA_INTEL=f8d8afe3dffdc86efa0698c02ff0c997866bac3e6208ddaf56d37108baacf197

FRAMEWORKS=(libobs.framework Syphon.framework libobs-metal.dylib libobs-opengl.dylib
  libavcodec.dylib libavdevice.dylib libavfilter.dylib libavformat.dylib libavutil.dylib
  libswresample.dylib libswscale.dylib libx264.dylib libsrt.dylib librist.dylib
  libmbedcrypto.dylib libmbedtls.dylib libmbedx509.dylib libfreetype.dylib libdatachannel.dylib)
PLUGINS=(coreaudio-encoder decklink image-source mac-avcapture mac-avcapture-legacy mac-capture
  mac-syphon mac-videotoolbox obs-ffmpeg obs-filters obs-outputs obs-transitions obs-webrtc
  obs-x264 rtmp-services text-freetype2 vlc-video)

die() {
  echo "obs-runtime: $*" >&2
  exit 1
}

[[ "$(uname)" == Darwin ]] || die "this is for macOS only"
root=$(cd "$(dirname "$0")/.." && pwd)
out=${1:-$root/apps/desktop/obs-runtime}
mkdir -p "$out"
out=$(cd "$out" && pwd)
cache=${OBS_RUNTIME_CACHE:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/ferrocast-obs-$OBS_VERSION}
identity=${APPLE_SIGNING_IDENTITY:--}
mkdir -p "$cache"
work=$(mktemp -d)
mounts=()
cleanup() {
  for m in "${mounts[@]+"${mounts[@]}"}"; do hdiutil detach "$m" -quiet >/dev/null 2>&1 || true; done
  rm -rf "$work"
}
trap cleanup EXIT

# Each arch's files, as OBS ships them.
for arch in Apple Intel; do
  dmg="$cache/OBS-Studio-$OBS_VERSION-macOS-$arch.dmg"
  want=$SHA_APPLE
  [[ $arch == Intel ]] && want=$SHA_INTEL
  if [[ ! -f "$dmg" ]] || [[ "$(shasum -a 256 "$dmg" | cut -d' ' -f1)" != "$want" ]]; then
    curl -fL --retry 3 -o "$dmg.part" \
      "https://github.com/obsproject/obs-studio/releases/download/$OBS_VERSION/OBS-Studio-$OBS_VERSION-macOS-$arch.dmg"
    mv "$dmg.part" "$dmg"
  fi
  got=$(shasum -a 256 "$dmg" | cut -d' ' -f1)
  [[ "$got" == "$want" ]] || die "OBS $arch DMG checksum mismatch: $got"
  mnt="$work/mnt-$arch"
  mkdir -p "$mnt"
  hdiutil attach "$dmg" -nobrowse -readonly -mountpoint "$mnt" >/dev/null \
    || die "OBS $arch DMG wouldn't open (already open elsewhere?)"
  mounts+=("$mnt")
  c="$mnt/OBS.app/Contents"
  t="$work/$arch"
  mkdir -p "$t/Frameworks" "$t/PlugIns" "$t/Helpers"
  for f in "${FRAMEWORKS[@]}"; do
    [[ -e "$c/Frameworks/$f" ]] || die "OBS $OBS_VERSION ($arch) has no Frameworks/$f"
    ditto "$c/Frameworks/$f" "$t/Frameworks/$f"
  done
  for p in "${PLUGINS[@]}"; do
    [[ -d "$c/PlugIns/$p.plugin" ]] || die "OBS $OBS_VERSION ($arch) has no PlugIns/$p.plugin"
    ditto "$c/PlugIns/$p.plugin" "$t/PlugIns/$p.plugin"
  done
  ditto "$c/MacOS/obs-ffmpeg-mux" "$t/Helpers/obs-ffmpeg-mux"
done

# One universal copy: every Mach-O file merged with its Intel twin. OBS's libraries (ffmpeg,
# x264…) already come universal in both DMGs; its own binaries are one arch each.
rm -rf "$out/Contents"
mkdir -p "$out/Contents"
ditto "$work/Apple" "$out/Contents"
while IFS= read -r -d '' f; do
  rel=${f#"$out/Contents/"}
  intel="$work/Intel/$rel"
  archs=$(lipo -archs "$f" 2>/dev/null) || continue
  case " $archs " in
    *" x86_64 "*) ;;
    *)
      [[ -f "$intel" ]] || die "no Intel copy of $rel"
      lipo -create -output "$f.uni" "$f" "$intel"
      mv "$f.uni" "$f"
      ;;
  esac
  archs=$(lipo -archs "$f")
  [[ " $archs " == *" arm64 "* && " $archs " == *" x86_64 "* ]] || die "$rel isn't universal ($archs)"
done < <(find "$out/Contents" -type f -print0)

# What each plugin and helper links by @rpath must be here (no Qt, no CEF slipped in).
missing=0
while IFS= read -r -d '' f; do
  lipo -archs "$f" >/dev/null 2>&1 || continue
  while read -r dep; do
    lib=${dep#@rpath/}
    [[ -e "$out/Contents/Frameworks/$lib" ]] || { echo "obs-runtime: $f needs $dep" >&2; missing=1; }
  done < <(otool -L "$f" | awk 'NR>1 && $1 ~ /^@rpath\// {print $1}')
done < <(find "$out/Contents" -type f -print0)
[[ $missing == 0 ]] || die "some libraries are missing"

# Inside out: loose libraries, then bundles, then the helper.
sign() { codesign --force --timestamp=none --sign "$identity" "$@" >/dev/null; }
for f in "$out"/Contents/Frameworks/*.dylib; do sign "$f"; done
for b in "$out"/Contents/Frameworks/*.framework "$out"/Contents/PlugIns/*.plugin; do sign "$b"; done
# The helper loads ffmpeg from Contents/Frameworks: like the app (R-77), the hardened runtime
# refuses libraries from another Team ID without this entitlement.
cat > "$work/helper.entitlements" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>com.apple.security.cs.disable-library-validation</key><true/>
</dict></plist>
EOF
sign --options runtime --entitlements "$work/helper.entitlements" "$out/Contents/Helpers/obs-ffmpeg-mux"
while IFS= read -r -d '' f; do
  lipo -archs "$f" >/dev/null 2>&1 || continue
  codesign --verify --strict "$f" || die "$f isn't signed"
done < <(find "$out/Contents" -type f -print0)

mkdir -p "$out/Contents/Resources"
cat > "$out/Contents/Resources/OBS-NOTICE.txt" <<EOF
Ferrocast's Studio runs OBS Studio's engine. Contents/Frameworks, Contents/PlugIns and
Contents/Helpers/obs-ffmpeg-mux are taken unchanged (only merged for Apple and Intel Macs)
from OBS Studio $OBS_VERSION for macOS, by the OBS Project, under the GNU General Public
License version 2 or later. They include FFmpeg, x264, mbedTLS, SRT, librist, FreeType,
libdatachannel and Syphon, each under its own licence (see OBS Studio's).

Source: https://github.com/obsproject/obs-studio/tree/$OBS_VERSION
Libraries' sources: https://github.com/obsproject/obs-deps
EOF

# The bundler's copy list: Contents-relative path → this folder.
{
  echo '{"bundle":{"macOS":{"files":{'
  sep=""
  for d in Frameworks PlugIns; do
    for e in "$out/Contents/$d"/*; do
      printf '%s"%s/%s":"%s"' "$sep" "$d" "$(basename "$e")" "$e"
      sep=","
    done
  done
  printf ',"Helpers/obs-ffmpeg-mux":"%s"' "$out/Contents/Helpers/obs-ffmpeg-mux"
  printf ',"Resources/OBS-NOTICE.txt":"%s"' "$out/Contents/Resources/OBS-NOTICE.txt"
  echo '}}}}'
} > "$out/tauri.obs.json"
python3 -m json.tool "$out/tauri.obs.json" >/dev/null || die "tauri.obs.json isn't valid JSON"

echo "obs-runtime: OBS $OBS_VERSION engine in $out ($(du -sh "$out/Contents" | cut -f1))"
