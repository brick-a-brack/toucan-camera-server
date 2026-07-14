#!/usr/bin/env bash
#
# Stage the Sony Camera Remote SDK (CrSDK) into the vendored layout build.rs
# expects at external/SONY/CrSDK/. Unlike the Nikon runtime (dlopen'd at run
# time, best-effort), Cr_Core is linked at build time, so the backend-sony build
# fails to link without this — staging is mandatory whenever the feature is on.
#
# The hosted archive (SONY_CRSDK_URL) bundles Sony's per-platform SDK packages
# verbatim, one zip each (names as shipped, see external/SONY/):
#   CrSDK_v2.02.00_<date>_Win64.zip
#   CrSDK_v2.02.00_<date>_Mac.zip
#   CrSDK_v2.02.00_<date>_Linux64PC.zip
#
# Each platform zip contains a RemoteCli.zip whose payload is the SDK itself:
#   app/CRSDK/*.h        -> headers          -> external/SONY/CrSDK/include/CRSDK/
#   external/crsdk/*     -> Cr_Core + monitor_protocol libs + CrAdapter/ plugins
#                                            -> external/SONY/CrSDK/<platform-dir>/
# So reaching the libraries means unzipping three layers: outer -> platform ->
# RemoteCli.
#
# Usage: stage-sony-crsdk.sh <url> <platform>
#   <url>      hosted CrSDK archive (SONY_CRSDK_URL); empty is a hard error since
#              Cr_Core is linked at build time (backend-sony can't build without it)
#   <platform> in: win64 | mac | linux64pc
#
# Runs identically on the macOS / Linux / Windows (Git bash) runners, so every job
# stages the SDK with a single uniform call.
set -euo pipefail

URL="$1"
PLATFORM="$2"

if [ -z "$URL" ]; then
  echo "ERROR: SONY_CRSDK_URL not set — required for backend-sony (Cr_Core is linked at build time)."
  exit 1
fi

# Repo root (this script lives in scripts/).
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST_ROOT="$ROOT/external/SONY/CrSDK"

# Map the requested platform to (glob for the inner platform zip, vendored libdir).
case "$PLATFORM" in
  win64)      ZIP_GLOB='*_Win64.zip';      LIBDIR="windows/x64" ;;
  mac)        ZIP_GLOB='*_Mac.zip';        LIBDIR="macos" ;;
  linux64pc)  ZIP_GLOB='*_Linux64PC.zip';  LIBDIR="linux/x64" ;;
  *) echo "ERROR: unknown platform '$PLATFORM' (want win64|mac|linux64pc)"; exit 1 ;;
esac

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 0. Fetch the hosted archive.
OUTER_ZIP="$TMP/sony_crsdk.zip"
curl -fsSL "$URL" -o "$OUTER_ZIP"

# 1. Outer archive -> the per-platform SDK zips.
unzip -q -o "$OUTER_ZIP" -d "$TMP/outer"
PLAT_ZIP="$(find "$TMP/outer" -name "$ZIP_GLOB" | head -1)"
[ -n "$PLAT_ZIP" ] || { echo "ERROR: no platform zip matching $ZIP_GLOB in $OUTER_ZIP"; exit 1; }

# 2. Platform zip -> RemoteCli.zip.
unzip -q -o "$PLAT_ZIP" "RemoteCli.zip" -d "$TMP/plat"
REMOTECLI="$TMP/plat/RemoteCli.zip"
[ -f "$REMOTECLI" ] || { echo "ERROR: RemoteCli.zip missing in $(basename "$PLAT_ZIP")"; exit 1; }

# 3. RemoteCli.zip -> headers (app/CRSDK) + libs (external/crsdk).
unzip -q -o "$REMOTECLI" "app/CRSDK/*" "external/crsdk/*" -d "$TMP/sdk"

HDR_SRC="$TMP/sdk/app/CRSDK"
LIB_SRC="$TMP/sdk/external/crsdk"
[ -d "$HDR_SRC" ] || { echo "ERROR: app/CRSDK headers missing in RemoteCli.zip"; exit 1; }
[ -d "$LIB_SRC" ] || { echo "ERROR: external/crsdk libs missing in RemoteCli.zip"; exit 1; }

# Headers are identical across platforms; stage them once (idempotent).
mkdir -p "$DEST_ROOT/include/CRSDK"
cp -f "$HDR_SRC"/*.h "$DEST_ROOT/include/CRSDK/"

# Libraries + the CrAdapter/ transport plugins for this platform. external/crsdk
# holds the Cr_Core / monitor_protocol libs at the top and CrAdapter/ beneath —
# exactly the shape build.rs links against and copies next to the binary.
mkdir -p "$DEST_ROOT/$LIBDIR"
cp -Rf "$LIB_SRC"/. "$DEST_ROOT/$LIBDIR/"

echo "Sony CrSDK ($PLATFORM) staged into $DEST_ROOT/$LIBDIR (+ include/CRSDK)"
