#!/usr/bin/env bash
#
# Assemble the gphoto2 runtime bundle for the macOS *universal* binary.
#
# build.rs (`copy_gphoto2_bundle`) stages, in each per-arch target dir, the
# libgphoto2 dependency closure (flat dylibs, listed in `gphoto2-bundle.manifest`)
# plus the `camlibs/` and `iolibs/` plugin dirs. Build scripts run before the link
# and only ever see one arch, so they cannot lipo-merge the arches nor rewrite the
# binary's own Homebrew-baked absolute load commands. This script does both:
#   1. lipo-merges each staged dylib/plugin from the two arches into a fat file,
#   2. rewrites every install name to be relocatable (@executable_path for the
#      binary and the flat libs, @loader_path/.. for the plugins),
# so the universal binary ships standalone (users no longer need Homebrew).
#
# Usage: macos-bundle-gphoto2.sh <x86_64-release-dir> <arm64-release-dir> <out-dir>
# <out-dir> must already contain the lipo'd `toucan-camera-server` binary.

set -eu

X86_DIR="$1"
ARM_DIR="$2"
OUT="$3"

MANIFEST="$ARM_DIR/gphoto2-bundle.manifest"
if [ ! -f "$MANIFEST" ]; then
  echo "no gphoto2 bundle manifest in $ARM_DIR — gphoto2 not built, skipping"
  exit 0
fi

manifest_has() { grep -qx "$1" "$MANIFEST"; }

# relink <file> <prefix>: repoint every bundled-lib dependency of <file> at
# <prefix>/<basename>. Handles both arch slices (Homebrew uses /opt/homebrew on
# arm64 and /usr/local on x86_64).
relink() {
  file="$1"
  prefix="$2"
  chmod u+w "$file"
  deps=$(
    { otool -arch arm64 -L "$file" 2>/dev/null || true
      otool -arch x86_64 -L "$file" 2>/dev/null || true
    } | awk '/^[[:space:]]+\// { print $1 }' | sort -u
  )
  for dep in $deps; do
    base="$(basename "$dep")"
    if manifest_has "$base"; then
      install_name_tool -change "$dep" "$prefix/$base" "$file" || true
    fi
  done
  # lipo and install_name_tool invalidate the ad-hoc code signature, and Apple
  # Silicon refuses to load a Mach-O with a broken signature — re-sign ad-hoc.
  codesign --force --sign - "$file" 2>/dev/null || true
}

# 1. Flat libs: lipo the two arches, give a relocatable id, repoint sibling deps.
# `|| [ -n "$lib" ]` so the final manifest entry is processed even if the file
# has no trailing newline — otherwise `read` drops it at EOF and that lib never
# gets bundled while the binary is still relinked to it (dangling reference).
while IFS= read -r lib || [ -n "$lib" ]; do
  [ -n "$lib" ] || continue
  if [ -f "$X86_DIR/$lib" ] && [ -f "$ARM_DIR/$lib" ]; then
    lipo -create "$X86_DIR/$lib" "$ARM_DIR/$lib" -output "$OUT/$lib"
    chmod u+w "$OUT/$lib"
    install_name_tool -id "@executable_path/$lib" "$OUT/$lib"
    relink "$OUT/$lib" "@loader_path"
  else
    echo "WARNING: '$lib' is in the manifest but missing from a per-arch dir" \
         "(x86: $( [ -f "$X86_DIR/$lib" ] && echo yes || echo no )," \
         "arm: $( [ -f "$ARM_DIR/$lib" ] && echo yes || echo no )) — not bundled"
  fi
done < "$MANIFEST"

# 2. Plugins: lipo each, repoint deps one directory up (plugins live in subdirs).
for sub in camlibs iolibs; do
  [ -d "$ARM_DIR/$sub" ] || continue
  mkdir -p "$OUT/$sub"
  for so in "$ARM_DIR/$sub"/*.so; do
    [ -e "$so" ] || continue
    name="$(basename "$so")"
    if [ -f "$X86_DIR/$sub/$name" ]; then
      lipo -create "$X86_DIR/$sub/$name" "$ARM_DIR/$sub/$name" -output "$OUT/$sub/$name"
      relink "$OUT/$sub/$name" "@loader_path/.."
    fi
  done
done

# 3. The binary: point its bundled-lib dependencies next to itself.
relink "$OUT/toucan-camera-server" "@executable_path"

# 4. Verify the bundle is self-contained: every @executable_path / @loader_path
# dependency of the binary, the flat dylibs and the plugins must resolve to a
# file we actually shipped. This turns a silently-incomplete bundle (the kind
# that crashes at launch with "Library not loaded") into a hard CI failure.
verify_fail=0
verify_refs() {
  f="$1"
  refs=$(
    { otool -arch arm64 -L "$f" 2>/dev/null || true
      otool -arch x86_64 -L "$f" 2>/dev/null || true
    } | awk '/@executable_path\/|@loader_path\// { print $1 }' | sort -u
  )
  while IFS= read -r ref || [ -n "$ref" ]; do
    [ -n "$ref" ] || continue
    base="$(basename "$ref")"
    # Flat libs live in $OUT; plugins reference them one dir up (@loader_path/..).
    if [ ! -f "$OUT/$base" ] && [ ! -f "$(dirname "$f")/$base" ]; then
      echo "VERIFY FAIL: $(basename "$f") needs '$ref' but '$base' is not in the bundle"
      verify_fail=1
    fi
  done <<< "$refs"
}

verify_refs "$OUT/toucan-camera-server"
for dylib in "$OUT"/*.dylib; do [ -e "$dylib" ] && verify_refs "$dylib"; done
for sub in camlibs iolibs; do
  for so in "$OUT/$sub"/*.so; do [ -e "$so" ] && verify_refs "$so"; done
done

if [ "$verify_fail" -ne 0 ]; then
  echo "ERROR: gphoto2 macOS bundle is incomplete — see VERIFY FAIL lines above"
  exit 1
fi

echo "gphoto2 macOS bundle assembled in $OUT:"
echo "  $(echo "$OUT"/*.dylib | wc -w) dylibs, $(ls "$OUT"/camlibs 2>/dev/null | wc -l) camlibs, $(ls "$OUT"/iolibs 2>/dev/null | wc -l) iolibs"
