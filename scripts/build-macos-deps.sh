#!/usr/bin/env bash
#
# Build libgphoto2's small dependencies from source into a self-contained prefix:
# libltdl (from libtool), libusb and libexif.
#
# Why from source: these used to be installed with Homebrew, one prefix per arch
# (/opt/homebrew for arm64, /usr/local for the Intel slice under Rosetta).
# Homebrew 7.0.0 (2026-09) moved macOS x86_64 to Tier 3 — it publishes no Intel
# bottles any more and its installer refuses to run at all on x86_64, Rosetta
# included ("Homebrew on macOS is only supported on Apple Silicon processors!").
# So the universal binary's x86_64 slice has no Homebrew to get them from.
# Building from pinned sources fixes that and makes both slices use identical
# versions, which the `lipo` merge did not guarantee before.
#
# ARCHITECTURE: whatever this script runs as. Call it natively for the arm64
# slice and under `arch -x86_64` for the Intel one — configure and config.guess
# then see a plain native build, so nothing enters cross-compilation mode. The
# autotools themselves still come from the native (arm64) Homebrew; an x86_64
# process can exec those arm64 binaries without trouble.
#
# These end up in the shipped artifact: build.rs walks libgphoto2's dependency
# closure (`copy_gphoto2_bundle`) and copies whatever it links next to the
# binary, which scripts/macos-bundle-gphoto2.sh then lipo-merges and relinks to
# @executable_path. That is exactly what happened with the Homebrew copies, so
# the artifact's shape is unchanged.
#
# Usage: build-macos-deps.sh <install-prefix>
set -euo pipefail

if [ "$#" -lt 1 ]; then
  echo "usage: $0 <install-prefix>" >&2
  exit 2
fi

PREFIX="$1"
WORK="${MACOS_DEPS_SRC:-${PREFIX}.src}"

# Pinned like LIBGPHOTO2_REF in the workflow: a CI build must not change because
# an upstream release landed. These match what Homebrew shipped when the Intel
# path was dropped, so the artifact keeps the same library versions as before.
LIBTOOL_VERSION="${LIBTOOL_VERSION:-2.6.2}"
LIBUSB_VERSION="${LIBUSB_VERSION:-1.0.30}"
LIBEXIF_VERSION="${LIBEXIF_VERSION:-0.6.26}"

echo "==> building libgphoto2 deps for $(uname -m) into $PREFIX"

mkdir -p "$PREFIX" "$WORK"

# Builds one release tarball. Release tarballs ship a ./configure, so no
# autoreconf (and no autotools version matching) is needed here.
#
#   build <name> <url> [configure args...]
build() {
  local name="$1" url="$2"
  shift 2

  local tarball="$WORK/$(basename "$url")"
  local src="$WORK/$name"

  echo "--> $name: $url"
  curl -fsSL --retry 3 --retry-delay 2 "$url" -o "$tarball"
  rm -rf "$src"
  mkdir -p "$src"
  tar -xf "$tarball" -C "$src" --strip-components=1

  (
    cd "$src"
    # --disable-static:              only the dylibs are bundled, so skip the .a.
    # --disable-dependency-tracking: single-shot build, no need for .deps files.
    # Anything package-specific comes from the caller — configure only *warns* on
    # an option it does not know, which would quietly hide a real typo.
    ./configure --prefix="$PREFIX" \
      --disable-static \
      --disable-dependency-tracking \
      "$@"
    make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
    make install
  )
}

# libltdl — libgphoto2 loads its camlibs/iolibs through it and requires an
# external copy (see GP_LIBLTDL in libgphoto2_port/gphoto-m4/gp-libltdl.m4: it
# deliberately does not ship its own). It comes as part of the libtool tarball;
# the libtool driver scripts it also installs are unused here, which is harmless.
build libtool \
  "https://ftp.gnu.org/gnu/libtool/libtool-${LIBTOOL_VERSION}.tar.xz"

# libusb — the USB transport behind libgphoto2_port's usb1 iolib.
build libusb \
  "https://github.com/libusb/libusb/releases/download/v${LIBUSB_VERSION}/libusb-${LIBUSB_VERSION}.tar.bz2"

# libexif — optional but default-on in libgphoto2 (thumbnail extraction from the
# CameraFilesystem). Built so both slices have the same feature set.
#
# --disable-nls keeps libintl out of the link: it would come from Homebrew, i.e.
# the wrong architecture for the x86_64 slice, and the server forces LC_ALL=C at
# runtime anyway (see the gphoto2 backend). --disable-docs avoids needing doxygen.
# libtool and libusb have neither option (they use no gettext and ship no docs
# target), hence passing it only here.
build libexif \
  "https://github.com/libexif/libexif/releases/download/v${LIBEXIF_VERSION}/libexif-${LIBEXIF_VERSION}.tar.bz2" \
  --disable-nls \
  --disable-docs

echo "==> deps installed into $PREFIX"
# A missing lib here surfaces much later as a confusing libgphoto2 configure
# failure, so fail now with the actual file list.
for lib in libltdl libusb-1.0 libexif; do
  if ! ls "$PREFIX/lib/${lib}"*.dylib >/dev/null 2>&1; then
    echo "ERROR: $lib was not installed into $PREFIX/lib" >&2
    ls -la "$PREFIX/lib" >&2 || true
    exit 1
  fi
done
file "$PREFIX/lib/libusb-1.0.0.dylib" 2>/dev/null || true
