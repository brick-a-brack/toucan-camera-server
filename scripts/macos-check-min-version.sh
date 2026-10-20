#!/usr/bin/env bash
#
# Assert the minimum macOS version recorded in every Mach-O we ship.
#
# A build machine's own OS version leaks into the artifact whenever
# MACOSX_DEPLOYMENT_TARGET is unset: the compiler then treats every API as
# unconditionally present, references symbols introduced by the *build* OS
# strongly instead of weak_import, and `@available` runtime guards never get a
# chance to run — dyld aborts at launch instead:
#
#   dyld: Symbol not found: (_AVCaptureDeviceTypeExternal)   # macOS 14+ symbol
#   Expected in: .../AVFoundation.framework/.../AVFoundation # ...on macOS 12.7
#
# The CI runners track the newest macOS, so that failure is invisible there and
# only shows up on a user's older machine. This check makes it a build failure.
#
# Usage: macos-check-min-version.sh <dir> <floor> [gphoto2-manifest]
#   <floor>  the macOS version the release claims to support, e.g. 11.0
#            (must match MACOSX_DEPLOYMENT_TARGET used for the build)
#
# Binaries we build ourselves must not require more than <floor>; bundled
# third-party runtimes (Canon, Sony, Nikon) are only reported, since their
# minimum is whatever the vendor shipped and we cannot lower it. Those NOTE lines
# are worth reading: a vendor library that is linked at build time (Sony's
# Cr_Core) is loaded at launch, so its minimum is the release's real floor.

set -eu

DIR="${1%/}"
FLOOR="$2"
MANIFEST="${3:-}"
BINARY="toucan-camera-server"

command -v otool >/dev/null 2>&1 || { echo "ERROR: otool not found (needs Xcode CLT)"; exit 1; }

# Every minimum-version value in the file, one per arch slice. Both slices are
# asked explicitly because the shipped files are universal; a thin file simply
# answers nothing for the arch it does not contain. Handles the modern
# LC_BUILD_VERSION ("minos") and the legacy LC_VERSION_MIN_MACOSX ("version").
minos_values() {
  { otool -arch arm64 -l "$1" 2>/dev/null || true
    otool -arch x86_64 -l "$1" 2>/dev/null || true
  } | awk '
    /cmd LC_BUILD_VERSION/      { bv = 1; vm = 0; next }
    /cmd LC_VERSION_MIN_MACOSX/ { vm = 1; bv = 0; next }
    bv && $1 == "minos"   { print $2 }
    vm && $1 == "version" { print $2 }
  ' | sort -u
}

# True when $1 is a higher version than $2.
higher_than() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$1" ]
}

# Files we build: the server itself, the libgphoto2 dylibs we compile from source
# (the manifest names them) and their plugins. Everything else in the bundle is a
# vendor runtime.
#
# Matched by PATH, never by base name alone: a vendor runtime directory can ship a
# library whose name collides with one of ours. Sony's CrAdapter/ carries its own
# libusb-1.0.0.dylib — classifying that as ours reported a vendor minimum (11.1)
# as our own build leaking its deployment target.
is_ours() {
  case "$1" in
    "$DIR"/camlibs/*|"$DIR"/iolibs/*) return 0 ;;
  esac
  # Only flat files at the bundle root can be ours; anything deeper that is not a
  # plugin dir belongs to a vendor.
  [ "$(dirname "$1")" = "$DIR" ] || return 1
  base="$(basename "$1")"
  [ "$base" = "$BINARY" ] && return 0
  [ -n "$MANIFEST" ] && [ -f "$MANIFEST" ] && grep -qx "$base" "$MANIFEST"
}

# A minimum higher than the floor is the leak we are looking for: the build
# machine's OS ending up in the artifact. A lower one is harmless — it runs
# anywhere the floor runs. The binary itself is held to the floor exactly, being
# the one file whose minimum the release advertises.
too_high() {
  file="$1"
  version="$2"
  higher_than "$version" "$FLOOR" && return 0
  [ "$(basename "$file")" = "$BINARY" ] && [ "$version" != "$FLOOR" ]
}

fail=0
while IFS= read -r f; do
  # Skip anything that is not a Mach-O (plists, configs, text).
  file -b "$f" | grep -q "Mach-O" || continue
  versions="$(minos_values "$f")"
  if [ -z "$versions" ]; then
    if is_ours "$f"; then
      echo "MIN VERSION FAIL: $(basename "$f") records no minimum macOS version"
      fail=1
    fi
    continue
  fi
  for v in $versions; do
    if is_ours "$f"; then
      if too_high "$f" "$v"; then
        echo "MIN VERSION FAIL: $(basename "$f") targets macOS $v, expected $FLOOR — set MACOSX_DEPLOYMENT_TARGET for this build"
        fail=1
      fi
    elif higher_than "$v" "$FLOOR"; then
      echo "NOTE: vendor runtime $(basename "$f") requires macOS $v (> $FLOOR) — it will not load below that. If it is linked at build time (Sony's Cr_Core) that is the release's real floor; a dlopen'd one (Nikon) just leaves its backend unavailable."
    fi
  done
done <<EOF
$(find "$DIR" -type f)
EOF

if [ "$fail" -ne 0 ]; then
  echo "ERROR: macOS minimum-version check failed — see MIN VERSION FAIL lines above"
  exit 1
fi

echo "macOS minimum-version check passed (floor: $FLOOR)"
