#!/usr/bin/env bash
#
# Assemble and zip one macOS artifact from the two per-arch release dirs.
#
# Called twice by the release pipeline, because Sony's CrSDK sets the floor for
# the whole binary: libCr_Core.dylib is LINKED at build time, so dyld loads it at
# launch and the server cannot start on a macOS older than the SDK supports
# (12.1 at the time of writing).
#
#   standard — every backend, minimum macOS 12.1
#   legacy   — no Sony backend, minimum macOS 11.0 (Big Sur, the oldest macOS
#              arm64 exists on); for users on 11.x, who lose Sony and nothing else
#
# Usage: package-macos.sh <target-root> <out-dir> <floor> <with-sony|without-sony>
#   <target-root>  cargo target dir holding <triple>/release for both arches
#   <out-dir>      artifact directory to build; zipped to <out-dir>.zip
#   <floor>        minimum macOS version this artifact claims (must match the
#                  MACOSX_DEPLOYMENT_TARGET the binary was built with)

set -eu

TARGET_ROOT="${1%/}"
OUT="${2%/}"
FLOOR="$3"
SONY_MODE="$4"

X86_REL="$TARGET_ROOT/x86_64-apple-darwin/release"
ARM_REL="$TARGET_ROOT/aarch64-apple-darwin/release"

case "$SONY_MODE" in
  with-sony|without-sony) ;;
  *) echo "ERROR: fourth argument must be with-sony or without-sony"; exit 1 ;;
esac

for rel in "$X86_REL" "$ARM_REL"; do
  [ -x "$rel/toucan-camera-server" ] || { echo "ERROR: no binary in $rel"; exit 1; }
done

rm -rf "$OUT"
mkdir -p "$OUT"

lipo -create \
  "$X86_REL/toucan-camera-server" \
  "$ARM_REL/toucan-camera-server" \
  -output "$OUT/toucan-camera-server"

# Add a relative rpath so the binary finds EDSDK.framework next to itself when
# run outside the build tree.
install_name_tool -add_rpath @executable_path "$OUT/toucan-camera-server"

# Canon framework — only if present (SDK installed in CI)
FRAMEWORK_SRC="external/EDSDK/EDSDKv132010M/EDSDK.framework"
if [ -d "$FRAMEWORK_SRC" ]; then
  cp -R "$FRAMEWORK_SRC" "$OUT/"
fi

# gphoto2 runtime: lipo-merge the per-arch libs/plugins staged by build.rs and
# rewrite their install names to be relocatable, so the universal binary ships
# without a system libgphoto2.
bash scripts/macos-bundle-gphoto2.sh "$X86_REL" "$ARM_REL" "$OUT"

# Nikon runtime — staged and fixed-up by build.rs in the per-arch release dir; the
# SDK binaries are universal, so copy from one arch. The backend dlopen's
# "TypeCommon Module.bundle" next to the binary and resolves the sibling
# Frameworks/ via @loader_path, so both sit at the artifact root. Only present if
# the Nikon SDK was provided (see the Nikon staging step).
#
# It requires macOS 12.0, above the legacy floor — being dlopen'd, it simply fails
# to load there and the backend is skipped, so it ships in both artifacts.
if [ -d "$ARM_REL/TypeCommon Module.bundle" ]; then
  ditto "$ARM_REL/TypeCommon Module.bundle" "$OUT/TypeCommon Module.bundle"
  ditto "$ARM_REL/Frameworks" "$OUT/Frameworks"
  for cfg in DC_PTP_Config.config MaidLayer.config RangeValue.config; do
    [ -f "$ARM_REL/$cfg" ] && cp "$ARM_REL/$cfg" "$OUT/"
  done
  echo "Nikon runtime bundled into $OUT"
fi

if [ "$SONY_MODE" = "with-sony" ]; then
  # Sony CrSDK runtime — Cr_Core + monitor_protocol dylibs + CrAdapter/ transport
  # plugins staged next to the per-arch binary by build.rs. The dylibs are
  # universal, so copy from one arch; the binary resolves Cr_Core /
  # monitor_protocol via @loader_path (rpath added by build.rs).
  #
  # macOS CrAdapter placement is what actually makes cameras appear: Cr_Core
  # locates its transport plugins through NSBundle.mainBundle bundlePath +
  # "Contents/Frameworks/CrAdapter", NOT flat next to the binary. A flat
  # CrAdapter/ alone loads fine (SDK::Init succeeds) but EnumCameraObjects then
  # returns CrError_Adaptor_Create (0x8703) and the camera never appears.
  # build.rs stages BOTH the flat copy and the Contents/Frameworks/CrAdapter
  # bundle copy; ship the bundle one.
  #
  # Cr_Core is linked at build time — a successful build means the runtime must be
  # present. Fail hard rather than ship a zero-camera artifact.
  for lib in libCr_Core.dylib libmonitor_protocol.dylib libmonitor_protocol_pf.dylib; do
    [ -f "$ARM_REL/$lib" ] || { echo "ERROR: Sony runtime $lib missing in $ARM_REL"; exit 1; }
    cp "$ARM_REL/$lib" "$OUT/"
  done
  # Flat copy (harmless, keeps the layout uniform across platforms).
  [ -d "$ARM_REL/CrAdapter" ] || { echo "ERROR: Sony CrAdapter/ missing — camera would never enumerate"; exit 1; }
  cp -R "$ARM_REL/CrAdapter" "$OUT/"
  # Bundle-relative copy — the one Cr_Core actually reads on macOS.
  SONY_BUNDLE="$ARM_REL/Contents/Frameworks/CrAdapter"
  [ -n "$(ls -A "$SONY_BUNDLE" 2>/dev/null)" ] \
    || { echo "ERROR: Contents/Frameworks/CrAdapter missing/empty — camera would never enumerate (0x8703)"; exit 1; }
  mkdir -p "$OUT/Contents/Frameworks"
  cp -R "$SONY_BUNDLE" "$OUT/Contents/Frameworks/"
  echo "Sony CrSDK runtime bundled into $OUT"
else
  # Nothing to do, but make sure the build really left Sony out: a stray Cr_Core
  # would mean the feature was still on, and with it a 12.1 launch-time floor —
  # exactly what this artifact exists to avoid.
  if [ -f "$ARM_REL/libCr_Core.dylib" ]; then
    echo "ERROR: $ARM_REL contains libCr_Core.dylib — this build was supposed to"
    echo "       exclude backend-sony. Check the --features list and the target dir."
    exit 1
  fi
fi

# Every Mach-O we build must claim the floor, or it refuses to launch on the
# oldest macOS this artifact supports.
bash scripts/macos-check-min-version.sh "$OUT" "$FLOOR" "$ARM_REL/gphoto2-bundle.manifest"

(cd "$OUT" && zip -qr "../$(basename "$OUT").zip" .)
echo "packaged $(basename "$OUT").zip (minimum macOS $FLOOR, $SONY_MODE)"
