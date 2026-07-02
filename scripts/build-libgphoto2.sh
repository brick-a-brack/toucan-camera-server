#!/usr/bin/env bash
#
# Build libgphoto2 from a specific git ref into a self-contained prefix.
#
# The release pipeline compiles libgphoto2 itself instead of installing the OS
# package (which is frequently outdated), then points PKG_CONFIG_PATH at the
# resulting prefix. build.rs (`copy_gphoto2_bundle`) discovers it via pkg-config
# and bundles libgphoto2 + libgphoto2_port + the camlibs/iolibs plugins next to
# the binary as usual. Only the two core libs come from source; their small
# dependencies (libusb, libexif, libltdl) still come from the OS.
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
# pkg-config / libtool autoconf macros live under the Homebrew prefix. Pick the
# prefix matching the running architecture (x86_64 slice runs under Rosetta at
# /usr/local, arm64 natively at /opt/homebrew). On Linux none of these exist and
# the loop is a no-op — the -dev packages already put everything on the default
# search paths.
case "$(uname -m)" in
  x86_64) BREW=/usr/local ;;
  arm64)  BREW=/opt/homebrew ;;
  *)      BREW="" ;;
esac
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
