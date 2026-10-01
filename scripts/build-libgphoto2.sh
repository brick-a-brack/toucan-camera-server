#!/usr/bin/env bash
#
# Build libgphoto2 from a specific git ref into a self-contained prefix.
#
# The release pipeline compiles libgphoto2 itself instead of installing the OS
# package (which is frequently outdated), then points PKG_CONFIG_PATH at the
# resulting prefix. build.rs (`copy_gphoto2_bundle`) discovers it via pkg-config
# and bundles libgphoto2 + libgphoto2_port + the camlibs/iolibs plugins next to
# the binary as usual.
#
# Its small dependencies (libltdl, libusb, libexif) come from the OS on Linux. On
# macOS the release pipeline builds them from source too — see
# scripts/build-macos-deps.sh — and passes them in through PKG_CONFIG_PATH plus
# LTDLINCL/LIBLTDL (libltdl has no .pc file).
#
# Usage: build-libgphoto2.sh <git-ref> <install-prefix> [extra configure args...]
#
# <git-ref> may be a branch, tag, or full commit SHA (fetched shallowly).
set -euo pipefail

REF="$1"
PREFIX="$2"
shift 2

REPO="${LIBGPHOTO2_REPO:-https://github.com/gphoto/libgphoto2.git}"
SRC="${LIBGPHOTO2_SRC:-${PREFIX}.src}"

# --- Toolchain discovery (macOS Homebrew keg-only tools) -------------------
# On macOS, gettext (autopoint) and libtool (glibtoolize) are keg-only, and the
# pkg-config / libtool autoconf macros live under the Homebrew prefix.
#
# Always /opt/homebrew, even when this runs as an x86_64 process under Rosetta for
# the universal binary's Intel slice: Homebrew 7.0.0 (2026-09) moved macOS x86_64
# to Tier 3 and there is no Intel Homebrew at /usr/local any more. These are build
# tools — generators and .pc readers — so running the arm64 ones from an x86_64
# process is fine, and the aclocal m4 files are architecture-independent. What
# does have to match the architecture is the libraries, which the caller supplies
# per arch (see the header).
#
# On Linux the prefix does not exist and the loop is a no-op — the -dev packages
# already put everything on the default search paths.
if [ "$(uname -s)" = "Darwin" ]; then
  BREW=/opt/homebrew
else
  BREW=""
fi
if [ -n "$BREW" ]; then
  for tool in gettext libtool; do
    [ -d "$BREW/opt/$tool/bin" ] && PATH="$BREW/opt/$tool/bin:$PATH"
  done
  [ -d "$BREW/share/aclocal" ] && ACLOCAL_PATH="$BREW/share/aclocal:${ACLOCAL_PATH:-}"
  export PATH ACLOCAL_PATH
fi

# autoreconf calls `libtoolize`; Homebrew ships it as `glibtoolize`.
if ! command -v libtoolize >/dev/null 2>&1 && command -v glibtoolize >/dev/null 2>&1; then
  export LIBTOOLIZE=glibtoolize
fi

# --- Fetch the requested ref (branch / tag / SHA) --------------------------
rm -rf "$SRC"
mkdir -p "$SRC"
git -C "$SRC" init -q
git -C "$SRC" remote add origin "$REPO"
git -C "$SRC" fetch -q --depth 1 origin "$REF"
git -C "$SRC" checkout -q FETCH_HEAD
echo "libgphoto2 @ $(git -C "$SRC" rev-parse HEAD) (ref: $REF)"

# --- Configure / build / install -------------------------------------------
cd "$SRC"
# A git checkout ships no ./configure — generate the autotools build system.
autoreconf -i -f

# --disable-static: ship only shared libs (what the bundle relinks).
# --without-libgd: build.rs drops the libgd-only toy-camera camlibs anyway, so
#   avoid dragging in the gd/codec tree.
# --disable-nls:   the server forces LC_ALL=C at runtime for stable ASCII labels,
#   so translations are dead weight (and this drops the libintl runtime dep).
./configure --prefix="$PREFIX" \
  --disable-static \
  --without-libgd \
  --disable-nls \
  "$@"

make -j"$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
make install

echo "libgphoto2 installed into $PREFIX"
