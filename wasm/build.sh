#!/bin/sh
# Builds the WebAssembly version of toucan-camera-server into wasm/dist/:
#
#   dist/index.html            test page
#   dist/toucan.js             entry point (createServer, fetch wrapper)
#   dist/webcam-bridge.js      getUserMedia bridge
#   dist/gphoto2-bridge.js     WebUSB/libgphoto2 bridge
#   dist/pkg/                  the Rust crate (wasm-bindgen --target web)
#   dist/gphoto2/              libgphoto2 + libusb compiled with Emscripten
#
# Needs: rustup target wasm32-unknown-unknown, wasm-bindgen-cli at the exact
# version pinned in Cargo.toml, and Docker for the libgphoto2 part.
# SKIP_GPHOTO2=1 skips the (slow) libgphoto2 build and reuses gphoto2/out/ if
# present — the page then runs with the webcam backend only.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
DIST="$HERE/dist"

WASM_BINDGEN_VERSION=$(sed -n 's/^wasm-bindgen = "=\(.*\)"/\1/p' "$HERE/Cargo.toml")
if ! wasm-bindgen --version 2>/dev/null | grep -q "$WASM_BINDGEN_VERSION"; then
	echo "wasm-bindgen $WASM_BINDGEN_VERSION is required:" >&2
	echo "  cargo install wasm-bindgen-cli --version $WASM_BINDGEN_VERSION --locked" >&2
	exit 1
fi

cargo build --release --target wasm32-unknown-unknown --manifest-path "$HERE/Cargo.toml"
mkdir -p "$DIST/pkg" "$DIST/gphoto2"
wasm-bindgen --target web --out-dir "$DIST/pkg" \
	"$HERE/target/wasm32-unknown-unknown/release/toucan_camera_wasm.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
	wasm-opt -Os --enable-bulk-memory --enable-nontrapping-float-to-int \
		-o "$DIST/pkg/toucan_camera_wasm_bg.wasm" "$DIST/pkg/toucan_camera_wasm_bg.wasm"
fi

cp "$HERE/js/toucan.js" "$HERE/js/webcam-bridge.js" "$HERE/js/gphoto2-bridge.js" "$DIST/"
cp "$HERE/www/index.html" "$DIST/"

if [ "${SKIP_GPHOTO2:-0}" != 1 ]; then
	"$HERE/gphoto2/build.sh"
fi
if [ -f "$HERE/gphoto2/out/toucan_gphoto2.mjs" ]; then
	cp "$HERE/gphoto2/out/toucan_gphoto2.mjs" "$HERE/gphoto2/out/toucan_gphoto2.wasm" "$DIST/gphoto2/"
else
	echo "warning: no libgphoto2 module (wasm/gphoto2/out/) — the gphoto2 backend will fail to load" >&2
fi

echo "Built $DIST — serve it with: node $HERE/serve.mjs"
