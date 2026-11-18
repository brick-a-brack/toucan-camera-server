# Toolchain for the libgphoto2 WebAssembly module (see wasm/README.md).
# Emscripten 3.1.73 + libusb 1.0.27 is the combination web-gphoto2 ships with.
FROM emscripten/emsdk:3.1.73
RUN apt-get update \
 && apt-get install -qqy --no-install-recommends autoconf automake autopoint gettext pkg-config libtool libtool-bin patch bzip2 \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src/wasm/gphoto2
