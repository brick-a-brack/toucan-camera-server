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
## Camera + host setup (required to be detected)

A Sony body is only enumerable when it exposes a **PC Remote (PTP)** USB interface
*and* the host talks to it through libusb. Getting `/cameras` to list it:

1. **On the camera — enable PC Remote and stop it sleeping:**
   - `MENU → Network → Transfer/Remote → PC Remote Function → PC Remote: On`,
     `PC Remote Cnct Method: USB` (some models also expose `Setup → USB → USB
     Connection Mode → PC Remote`).
   - `MENU → Setup → Power Setting Option → Auto Power OFF Temp` / power-save →
     **long / off** while tethered. A body that sleeps drops off the USB bus and
     re-enumerates in a non–PC-Remote mode (different PID, HID class), and the SDK
     stops seeing it.
2. **On Windows — bind the libusbK driver (CrSDK talks to the camera via libusb,
   not the MTP/WPD driver):** the SDK package ships it in `Driver.zip`
   (`srcameradriver.inf`, `libusbK.sys`). In Device Manager, right-click the
   camera (it appears under *Portable Devices* as `ILCE-…` once in PC Remote) →
   *Update driver* → *Browse* → *Let me pick* → *Have Disk* → point at
   `srcameradriver.inf`. It then shows as **libusbK Usb Devices / Service:
   libusbK** ("Sony Remote Control Camera"). Until then it stays on `WUDFWpdMtp`
   (MTP) and the SDK can't open it.

   Verify the target state:
   ```powershell
   Get-PnpDevice -PresentOnly | ? { $_.InstanceId -match 'VID_054C' } | Select Class,Service,InstanceId
   ```
   You want `Service : libusbK` and a PC-Remote PID (`0x0CCC`–`0x1002`).

macOS / Linux use libusb directly (no per-device driver install); the camera just
needs to be in PC Remote mode.

### Detection is cached, not inline
The CrSDK USB scan (`EnumCameraObjects`) takes ~3 s and the `/cameras` route drops
a backend that exceeds a 3 s timeout, so `list_devices()` serves a cache the SDK
thread refreshes while idle (`mod.rs`). A freshly plugged/woken camera appears
within one refresh (~5 s), not on the very first poll.

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
