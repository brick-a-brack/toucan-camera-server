#!/bin/sh
# Builds the libgphoto2 WebAssembly module (wasm/gphoto2/out/toucan_gphoto2.{mjs,wasm})
# inside the Emscripten Docker image. Usage: wasm/gphoto2/build.sh [make args...]
#
# The sources (libgphoto2 at a pinned ref, libusb, libtool) are downloaded by the
# Makefile from inside the container. Behind an HTTP proxy, the host's proxy
# variables are forwarded and the container shares the host network; a custom CA
# bundle named by SSL_CERT_FILE is mounted and trusted by curl and git.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)

docker build -q -t toucan-gphoto2-wasm - < "$HERE/Dockerfile" >/dev/null
mkdir -p "$HERE/work/emcache"

NET_OPTS=""
for var in HTTPS_PROXY https_proxy HTTP_PROXY http_proxy NO_PROXY no_proxy; do
	eval "value=\${$var:-}"
	if [ -n "$value" ]; then
		NET_OPTS="$NET_OPTS -e $var=$value"
	fi
done
if [ -n "$NET_OPTS" ]; then
	NET_OPTS="$NET_OPTS --network host"
fi
if [ -n "${SSL_CERT_FILE:-}" ] && [ -f "$SSL_CERT_FILE" ]; then
	NET_OPTS="$NET_OPTS -v $SSL_CERT_FILE:/etc/toucan-ca.crt:ro -e CURL_CA_BUNDLE=/etc/toucan-ca.crt -e GIT_SSL_CAINFO=/etc/toucan-ca.crt"
fi

# shellcheck disable=SC2086 # NET_OPTS is a list of options
docker run --rm \
	-u "$(id -u):$(id -g)" \
	-e HOME=/tmp \
	$NET_OPTS \
	-v "$REPO:/src" \
	-v "$HERE/work/emcache:/emsdk/upstream/emscripten/cache" \
	toucan-gphoto2-wasm \
	sh -c 'emmake make -j"$(nproc)" "$@"' make "$@"
