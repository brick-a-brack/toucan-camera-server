# toucan-camera-server — WebAssembly

The camera API of toucan-camera-server, running in a web page. No install and
no native server: the browser is the platform.

- **Webcams** through `getUserMedia` (backend `webcam-browser`).
- **DSLR / mirrorless bodies** through **libgphoto2 compiled to WebAssembly**,
  talking to the camera over **WebUSB** (backend `gphoto2`, the same id and
  the same parameter curation as the native gphoto2 backend).

It is the same API: opaque device IDs, `CameraParameter` JSON, the routes and
their status codes, frame averaging (`?frames=N`). Most of it is compiled from
the server's own sources, not copied from them (see [Layout](#layout)).

## Try it

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.130 --locked   # = the version pinned in Cargo.toml
wasm/build.sh            # Docker is needed for the libgphoto2 part
node wasm/serve.mjs      # http://localhost:8080/
```

Open <http://localhost:8080/> in **Chrome or Edge** to get the test page:

1. **Allow webcams** grants camera access (until then the browser lists no webcam).
2. **Add a USB camera…** opens the WebUSB picker filtered to still cameras (PTP).
3. Pick a camera in the list, then **Connect**. The parameters panel fills itself,
   **Start live view** streams frames, and **Capture** takes a still. Set
   *Frames* > 1 to average N shots.
4. The **API console** sends HTTP-style requests (`GET /cameras`,
   `PUT /cameras/{id}/parameters`…) to the in-page server and shows the
   response exactly as the native server would return it.

`SKIP_GPHOTO2=1 wasm/build.sh` skips the libgphoto2 build (and reuses
`wasm/gphoto2/out/` when it exists). Without it, the page runs with the webcam
backend only. The CI job `build-wasm` publishes the built site as
`toucan-camera-server-wasm.zip` (unzip it, then run `node serve.mjs --dir .`).

### Browser requirements

| Need | Why |
|---|---|
| Secure context (`https://` or `http://localhost`) | `getUserMedia` and WebUSB require it |
| Cross-origin isolation (`COOP: same-origin` + `COEP: require-corp`) | libusb's WebUSB backend waits on shared memory (`Atomics.waitAsync`), so the module is built with pthreads → `SharedArrayBuffer` |
| Chromium (Chrome, Edge, Opera…) for USB cameras | WebUSB is not in Firefox/Safari; webcams work everywhere |

`serve.mjs` sets the headers. Any other host must send them too.

### USB cameras: operating-system caveats

WebUSB can only claim an interface that no kernel driver holds:

- **Linux**: the device node must be accessible to the user (a udev rule such as
  libgphoto2's `print-udev-rules`), and nothing else may have the camera open.
  GNOME's gvfs gphoto2 monitor grabs cameras automatically, so stop it or
  unmount the camera first.
- **macOS**: close Image Capture / Photos / any app that grabbed the body.
  `ptpcamerad` may also hold the device: `killall -9 ptpcamerad`.
- **Windows**: the body needs the **WinUSB** driver (e.g. installed with Zadig).
  The default MTP/WPD driver keeps WebUSB out.
- The body must be in its PTP / "PC remote" USB mode, like with the native server.

## Using it from code

```js
import { createServer } from './toucan.js';

const toucan = await createServer();          // { webcam, gphoto2 } bridges when supported

// 1. The HTTP API, unchanged: swap fetch() for toucan.fetch()
const cameras = await (await toucan.fetch('/cameras')).json();
await toucan.fetch(`/cameras/${cameras[0].id}/connect`, { method: 'PUT' });
const jpeg = await (await toucan.fetch(`/cameras/${cameras[0].id}/capture?frames=4`, { method: 'POST' })).blob();
const mjpeg = await toucan.fetch(`/cameras/${cameras[0].id}/liveview`);   // multipart/x-mixed-replace stream

// 2. Or the typed API
const s = toucan.server;
await s.setParameter(id, 'focus_auto', 'false');
for await (const frame of toucan.frames(id, abortSignal)) { /* Uint8Array JPEG */ }
```

Errors from the typed API are `Error`s carrying the HTTP status the server
would have answered (`err.status`: 400, 404, 409, 500…).

There is no bearer token: nothing listens on the network, and the API is only
reachable from the page that created it.

## Layout

```
wasm/
  Cargo.toml, src/      Rust crate (wasm-bindgen): ToucanServer, router, backends
    lib.rs              JS API; #[path]-includes the server's shared sources:
                          src/camera/types.rs              DeviceId, CameraParameter, CameraError…
                          src/camera/averaging.rs          ?frames=N averaging
                          src/backends/gphoto2/curation.rs gphoto2 parameter curation
    api.rs              the server's routes, statuses and error messages
    backend.rs          WebBackend: the CameraBackend trait made async
    webcam.rs           capabilities ↔ parameters, writes → constraints
    gphoto2.rs          config tree → parameters (shared curation), vendor-key writes
  js/
    toucan.js           createServer(), fetch() wrapper, MJPEG stream, frames()
    webcam-bridge.js    getUserMedia, hidden <video>, frame grabs
    gphoto2-bridge.js   loads the Emscripten module, serialises calls, keep-alive
  gphoto2/              libgphoto2 → WebAssembly
    toucan_gphoto2.cpp  embind API: autodetect, sessions per port, config, capture,
                        plugin preload table, diagnostics, debug log
    Makefile, Dockerfile, build.sh
  www/index.html        test page
  serve.mjs             static server with COOP/COEP
```

### libgphoto2: upstream, unmodified, same version as the server

No fork and no patch. The Makefile checks out **upstream libgphoto2 at the
`LIBGPHOTO2_REF` of `.github/workflows/build.yml`**, the ref the native
releases are built from, so the browser and the server always run the same
libgphoto2. Bumping that one line moves both. To try another ref:
`wasm/gphoto2/build.sh LIBGPHOTO2_REF=<sha|tag|branch>`.

web-gphoto2 patched libgphoto2's sources for two reasons, both now handled
outside libgphoto2:

- **Plugin discovery.** libgphoto2 lists its camlib/iolib directories and
  `dlopen`s each plugin through libltdl, and WebAssembly has no `dlopen`. The
  ptp2 camlib and the usb1 iolib are linked in as static archives instead.
  `toucan_gphoto2.cpp` registers a libltdl preload table naming their six entry
  points (`camera_id`, `gp_port_library_type`…), and writes one `.la` stub per
  plugin into Emscripten's in-memory filesystem, where `CAMLIBS` / `IOLIBS`
  point. libltdl then resolves the "files" from the table. The table is written
  by hand because libtool's `-dlpreopen` prefixes every symbol with `lib` on an
  OS it does not know (Emscripten), which is what the patch's `_LTX_` renames
  worked around.
- **Threads.** libusb's WebUSB backend needs shared memory, and wasm objects
  with and without it cannot be linked together, so every library is compiled
  with `-pthread`: a build flag instead of the patch's `configure.ac` change.

`diagnostics()` (shown on the test page) reports what the build can drive:
2468 camera models for the pinned ref. 0 would mean plugin discovery failed.
Add `?debug` to the page URL to get libgphoto2's own log in the console.

### Why two WebAssembly modules

libgphoto2 is C with autotools, libltdl plugins and blocking USB I/O. Emscripten
handles all three: static `-dlpreopen` for the camlib/iolib plugins, and
Asyncify to suspend inside libusb's WebUSB backend while the browser's promise
resolves. The Rust crate targets `wasm32-unknown-unknown` with wasm-bindgen.
The two toolchains produce modules that cannot be linked together, so they
meet in JS (`gphoto2-bridge.js`) and exchange plain data only.

Asyncify allows one suspended call at a time across the whole module, so the
bridge queues every libgphoto2 call. Two cameras work, but their operations
interleave rather than overlap.

## Credits

The libgphoto2 port builds on the work referenced in
[gphoto/libgphoto2#1249](https://github.com/gphoto/libgphoto2/issues/1249):

- [GoogleChromeLabs/web-gphoto2](https://github.com/GoogleChromeLabs/web-gphoto2)
  (Ingvar Stepanyan), the original port, and its write-ups
  [Porting USB applications to the web: libusb](https://web.dev/articles/porting-libusb-to-webusb) /
  [gPhoto2](https://web.dev/articles/porting-gphoto2-to-the-web);
- [edward-io/web-gphoto2](https://github.com/edward-io/web-gphoto2), the fork
  updated to a current libgphoto2. Its patch showed what had to change; here
  that is done without modifying libgphoto2 (see above).

`toucan_gphoto2.cpp` is derived from web-gphoto2's `api.cpp`. Its changes:
several cameras by port instead of "the first one", bytes instead of Blobs, and
`{ error, code }` results instead of C++ exceptions. libgphoto2, libusb and
web-gphoto2 are LGPL-2.1-or-later. The Emscripten module is a separate,
replaceable file (`gphoto2/toucan_gphoto2.wasm`), built from the sources and
pinned refs above.
