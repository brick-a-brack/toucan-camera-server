# Sony backend (Camera Remote SDK / CrSDK)

Controls Sony bodies (α / ILCE / ILME / ZV / DSC …) over USB or IP through Sony's
**Camera Remote SDK** (CrSDK, `Cr_Core`). The SDK is C++ with asynchronous
callbacks, so all access goes through a small C++ shim (`bridge.cpp`) that exposes
a flat, synchronous C API; the Rust side (`mod.rs`) drives it from one dedicated
`sony-sdk` OS thread (the actor pattern, like the Canon EDSDK backend).

- `backend_id()` = `"sony"`; feature flag `backend-sony`; targets Windows / macOS / Linux.
- `dedup_priority()` = 10 and each device emits a `dedup_key` (Sony USB vendor
  `0x054c` + model), so a Sony body also seen by gphoto2 is dropped in favour of
  this SDK.
- Capture: still is routed to the PC (`StillImageStoreDestination = HostPC + card`),
  saved by the SDK to a temp dir, read back as JPEG and deleted. Put the body in a
  JPEG image-quality mode — RAW-only capture yields a RAW file.
- The camera must be in **PC Remote** USB connection mode (menu:
  Network → USB → USB Connection Mode → PC Remote, or Setup → USB) to enumerate.

## Vendoring the SDK (not committed — `external/` is git-ignored)

Download the Camera Remote SDK (v2.02.00 used here) from Sony and lay it out as:

```
external/SONY/CrSDK/
  include/CRSDK/*.h                              # headers (shared, from the Win64 package)
  windows/x64/  Cr_Core.lib Cr_Core.dll monitor_protocol.dll monitor_protocol_pf.dll  CrAdapter/
  macos/        libCr_Core.dylib libmonitor_protocol.dylib libmonitor_protocol_pf.dylib  CrAdapter/
  linux/x64/    libCr_Core.so  libmonitor_protocol.so  libmonitor_protocol_pf.so         CrAdapter/
```

Each platform package's libraries live inside its `RemoteCli.zip` under
`RemoteCli/external/crsdk/` (`Cr_Core*` at the top, transport plugins in
`CrAdapter/`). Copy them into the layout above; the headers come from the Win64
package's `app/CRSDK/`.

`build.rs` compiles `bridge.cpp`, links `Cr_Core`, and copies the runtime
libraries + `CrAdapter/` next to the produced binary (Windows: exe dir; Linux:
`$ORIGIN`; macOS: `@loader_path`). A missing SDK only prints a warning, so builds
without the feature — or without the vendored SDK — keep working.

## Build

```
cargo build --features backend-sony
```
