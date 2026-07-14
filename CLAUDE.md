# Claude Rules for toucan-camera-server

## Language
- All code, comments, commit messages, variable names, and documentation must be in English.

## Project overview
REST API to control cameras (DSLR and webcams) from multiple vendors and operating systems.
The API is protected by a bearer token (`auth.rs`: `Authorization: Bearer <token>` or `?token=`). It binds to `127.0.0.1` by default (loopback only); pass `--expose` to bind `0.0.0.0` (LAN). On Android it always binds `0.0.0.0`. `BIND_ADDR` overrides the bind address on any platform.

## Code style
- Follow standard Rust conventions (`rustfmt`, `clippy`).
- No `unwrap()` in production paths — use proper error handling (`?`, `Result`, `thiserror`).
- Keep handlers thin: business logic belongs in dedicated modules, not in route handlers.
- Prefer explicit types over inference when it aids readability.

## Architecture

### Device IDs
- All device IDs exposed by the API are **opaque, URL-safe base64url strings**.
- Format: `base64url("<backend_id>:<native_id>")` — e.g. `base64url("canon:USB:0,1,0")`.
- Encoding/decoding is handled by `DeviceId` in `src/camera/mod.rs`.
- Backends work exclusively with **native IDs** (e.g. Canon port names). They never see or produce opaque IDs directly, except in `list_devices` where they call `DeviceId::new(...).encode()` to build the `DeviceInfo.id` field.
- Opaque IDs allow direct backend routing without trying all backends: decode → read `backend` field → look up in `BackendState` HashMap.

### CameraBackend trait
- Every backend must implement the `CameraBackend` trait defined in `src/camera/mod.rs`.
- Current trait methods: `backend_id`, `list_devices`, `connect`, `disconnect`, `is_connected`, `get_parameters`, `get_live_view_frame`, `set_parameter`, `capture_photo`.
- `backend_id()` returns the backend's unique name (e.g. `"canon"`). It is used to build opaque device IDs and to key the backend registry.
- Route handlers must only interact with the `CameraBackend` trait — never with a concrete backend type.

### Backend registry & app state
- `BackendState` is `Arc<HashMap<String, Arc<dyn CameraBackend>>>`, keyed by `backend_id()`.
- The axum app state is `AppState` (in `src/routes/cameras.rs`), which wraps `BackendState`, `LiveViewSenders`, the auth token (`Arc<RwLock<String>>`), and — when `backend-remote` is enabled — the shared peer registry.
- `FromRef<AppState> for BackendState` is implemented so handlers that only need backends can extract `State<BackendState>` directly.
- Backends are registered at startup in `build_backends()` in `lib.rs` (returns `BuiltBackends`, which also carries the peer registry when `backend-remote` is on).
- If a backend fails to initialize (e.g. SDK DLL not found), it is skipped with an error log — the server starts anyway.
- Each backend is gated behind a Cargo feature flag: `backend-canon`, `backend-sony`, `backend-gphoto2`, `backend-webcam-macos`, `backend-webcam-windows`, `backend-camera2-android`, `backend-remote`.
- Backend code lives in `src/backends/<name>/mod.rs` (`#[cfg(feature = "backend-<name>")]`).
- Active backends: `backend-canon` (Windows / macOS / Linux), `backend-sony` (Windows / macOS / Linux), `backend-gphoto2` (macOS / Linux), `backend-webcam-windows` (Windows), `backend-webcam-macos` (macOS), `backend-camera2-android` (Android), `backend-remote` (all platforms).
- `backend-canon` and `backend-gphoto2` can be built together (release builds do, on macOS/Linux): EDSDK owns Canon bodies, gphoto2 hides them and covers other vendors — see the gphoto2 backend section.

### Remote backend
- `backend-remote` relays cameras exposed by other toucan-camera-server instances ("peers") over HTTP, so they appear in `/cameras` and are controllable like local devices.
- `backend_id()` is `"remote"`. Native ID format: `"<peer_url>|<remote_opaque_id>"` where `peer_url` is the normalized peer base URL (e.g. `http://192.168.1.5:8040`) and `remote_opaque_id` is the peer's own opaque device ID. Neither part contains `|`, so the first `|` is the separator. The route layer then wraps it as the usual `base64url("remote:<peer_url>|<remote_opaque_id>")`.
- **Sync trait over async HTTP**: the backend owns a dedicated multi-threaded tokio runtime. Each synchronous `CameraBackend` method spawns an owned (`'static`) future on that runtime and blocks on a `std::sync::mpsc` reply — the same "block on a channel" pattern as the SDK-thread backends, with no nested `block_on`.
- **HTTP client**: `reqwest` with `default-features = false` (no TLS). Peers are reached over plain HTTP on the LAN. Per-request timeouts apply to control calls; the live-view stream is intentionally untimed.
- **Live view**: `get_live_view_frame` starts a per-device relay task that reads the peer's MJPEG stream and keeps the latest JPEG in a shared cell; polls return that frame (or `SdkError(0x0000_A102)` "not ready" while empty). The relay self-terminates ~2 s after polling stops, closing the upstream connection.
- **Connection state** is tracked locally (a `HashSet` of native IDs) — `connect`/`disconnect` proxy to the peer and update the set; `is_connected` and the `connected` flag in `list_devices` read from it.
- **Peers** are managed via `/peers` routes and held in an in-memory `PeerRegistry` (`Arc<RwLock<Vec<Peer>>>`) shared between the backend and the routes via `AppState.peers` (no on-disk persistence). Each peer has an id, normalized URL, and optional bearer token sent on every proxied request. The token is returned by the API (the server is local, so the UI can display it).
- **Add-time validation**: `POST /peers` calls `validate_peer` (hits the peer's `/health` with the given token) before registering. A peer that is unreachable, rejects the token, or is not a toucan-camera-server is refused with 502 and never stored.
- Code: `src/backends/remote/mod.rs` (backend + MJPEG relay) and `src/backends/remote/peers.rs` (registry).

### Sony backend (Camera Remote SDK / CrSDK)
- Source: `src/backends/sony/bridge.cpp` (+ `bridge.h`, a flat C shim over the C++ SDK) and `src/backends/sony/mod.rs` (Rust actor over `std::sync::mpsc`, like Canon). `backend_id()` is `"sony"`, gated behind `backend-sony`, built for Windows / macOS / Linux.
- **Why a C++ bridge**: the CrSDK is C++ with abstract classes (`ICrCameraObjectInfo`, `IDeviceCallback`) and asynchronous callbacks on SDK-owned threads. `bridge.cpp` wraps one session as a `SonyCamera : IDeviceCallback` and exposes a flat, synchronous C API (`sn_*`): connect blocks until `OnConnected`, capture blocks until `OnCompleteDownload` (condition variables). All `sn_*` calls run on the single `sony-sdk` actor thread; the SDK's callback threads only touch a session's mutex/condvar.
- **Header gotcha**: the `Cr` integer/char typedefs (`CrChar`, `CrInt8u`, `CrInt32`, `CrInt32u`, …) live at **global** scope (`CrTypes.h` has no namespace); only `CrDeviceHandle`, `CrError`, the enums and the classes are in `SCRSDK`. On Windows the DLL is built UNICODE so `CrChar` is `wchar_t` — build.rs defines `UNICODE`/`_UNICODE` and the bridge converts UTF-8 ↔ wide.
- **Values cross the FFI raw**; labels are decoded in `mod.rs` (Canon-style tables): FNumber = f×100, ShutterSpeed = numerator<<16 | denominator (0 = Bulb), ISO = bits 0-23 value / 24-27 mode / 28-31 ext (0xFFFFFF = AUTO), ExposureBiasCompensation = signed EV×1000, plus WhiteBalance / FocusMode / ImageQuality enum tables. Exposed codes: FNumber `0x0100`, ExposureBiasCompensation `0x0101`, ShutterSpeed `0x0103`, IsoSensitivity `0x0104`, StillImageQuality `0x0107`, WhiteBalance `0x0108`, FocusMode `0x0109`. **Verify codes/encodings in `external/SONY/CrSDK/include/CRSDK/CrDeviceProperty.h`.** Params with fewer than two options are hidden; read-only props are marked `disabled`.
- **Capture** (JPEG-only): at connect the bridge sets `StillImageStoreDestination = HostPC+MemoryCard`, calls `SetSaveInfo` to a temp dir, and enables live view. `sn_capture` sends `Release` Down → 35 ms → Up, waits for `OnCompleteDownload`, reads the saved JPEG and deletes it. Live view uses `GetLiveViewImageInfo` + `GetLiveViewImage`; "not ready" maps to `SdkError(0x0000_A102)`.
- **Dedup**: `dedup_priority()` = 10 and each device emits `dedup_key(0x054c, model)` (Sony USB vendor), so a Sony body also seen by gphoto2 is dropped in favour of the SDK.
- **Camera requirement**: the body must be in **PC Remote** USB connection mode to enumerate.
- **SDK vendoring** (git-ignored `external/`): `external/SONY/CrSDK/{include, windows/x64, macos, linux/x64}` — headers + `Cr_Core` + `monitor_protocol*` + `CrAdapter/`. build.rs compiles the bridge (C++17), links `Cr_Core`, and copies the runtime libs + `CrAdapter/` next to the binary (`$ORIGIN` on Linux, `@loader_path` on macOS). A missing SDK only warns. See `src/backends/sony/README.md`.

### Canon SDK thread
- The EDSDK relies on Windows messages internally and does not work on tokio worker threads.
- All Canon SDK calls run on a single dedicated OS thread (`"canon-sdk"`) that pumps `EdsGetEvent()` every 16 ms.
- Communication between the backend and its SDK thread uses `std::sync::mpsc` channels (actor pattern).
- The SDK thread holds all Canon-internal state (open session refs, etc.) — raw pointers never leave the thread.
- `EdsInitializeSDK` / `EdsTerminateSDK` are called on the SDK thread, not on the main thread.
- EVF (live view) output is enabled once at `connect` time via `EdsSetPropertyData(kEdsPropID_Evf_OutputDevice, kEdsEvfOutputDevice_PC)` — not on every frame.

### Camera parameters
- `get_parameters(native_id)` returns only the **currently settable** parameters for the connected device.
- Uses `EdsGetPropertyDesc` to get allowed values and access level per property. Properties with `access == 0` (read-only) or `num_elements == 0` are excluded.
- The allowed values depend on the camera's current exposure mode (e.g. Tv is read-only in Av mode).
- Response format: `[{ type, current, options: [{ label, value }] }]` where `value` is the raw SDK i32 code.
- Canon property IDs (from `EDSDKTypes.h`):
  - `kEdsPropID_DriveMode`           = `0x00000401`
  - `kEdsPropID_ISOSpeed`            = `0x00000402`
  - `kEdsPropID_MeteringMode`        = `0x00000403`
  - `kEdsPropID_AFMode`              = `0x00000404`
  - `kEdsPropID_Av`                  = `0x00000405`
  - `kEdsPropID_Tv`                  = `0x00000406`
  - `kEdsPropID_ExposureCompensation`= `0x00000407`
  - `kEdsPropID_WhiteBalance`        = `0x00000106`
  - `kEdsPropID_ColorTemperature`    = `0x00000107`
  - `kEdsPropID_Evf_OutputDevice`    = `0x00000500`
- **Do not guess property IDs** — always verify in `external/EDSDK/EDSDKv132010W/Windows/EDSDK/Header/EDSDKTypes.h`.

### Live view & streaming
- Live view is served as MJPEG over HTTP (`multipart/x-mixed-replace; boundary=frame`).
- `LiveViewSenders` = `Arc<Mutex<HashMap<String, broadcast::Sender<Arc<Bytes>>>>>` — one sender per active device (keyed by opaque device ID).
- Only one capture loop runs per device regardless of how many clients are connected. The loop starts when the first client subscribes and stops when the last one disconnects.
- `EDS_ERR_OBJECT_NOTREADY` (0x0000A102) during frame capture is skipped (continue), not fatal.
- The route checks `is_connected` via `spawn_blocking` **before** sending any HTTP headers — returns 409 if not connected.
- Broadcast buffer capacity: 4 frames (drops old frames if clients are slow).
- No frames are ever written to disk — everything is in-memory and streamed directly.

### Photo capture
- `POST /cameras/{id}/capture` returns the raw JPEG bytes directly in the HTTP response body.
- Response headers: `Content-Type: image/jpeg`, `Content-Length: <size>`.
- No base64, no JSON wrapper — raw binary only.
- Only JPEG output is supported for now (no RAW/CR3).

### HTTP layer
- Framework: `axum` (not actix-web).
- The server binds to `127.0.0.1` by default (loopback); the `--expose` flag binds `0.0.0.0` (LAN), and Android always does. `BIND_ADDR` overrides on any platform (highest precedence). See `resolve_bind_addr()` / `parse_args()` in `lib.rs`.
- Every route is wrapped by `auth::auth_middleware` (a `.layer()` on the whole router), so all endpoints — including `/`, `/health`, `/cameras`, and `/peers` — require the bearer token.
- All routes must be registered explicitly; no catch-all wildcards unless intentional.
- JSON is the response format for all non-binary endpoints.
- State changes (connect, disconnect) use `PUT` — they are idempotent.
- The device ID is always in the URL path, never in the request body.

### Current routes
```
GET  /                               — web UI (embedded HTML, served from binary via include_str!)
GET  /health                         — healthcheck JSON
GET  /cameras                        — list all devices across all active backends (includes connected: bool)
PUT  /cameras/{id}/connect           — open a session with a device
PUT  /cameras/{id}/disconnect        — close a session with a device
GET  /cameras/{id}/parameters        — list all parameters with current value, allowed options, and disabled flag (requires connected)
PUT  /cameras/{id}/parameters        — set a parameter value (requires connected)
GET  /cameras/{id}/liveview          — MJPEG stream (requires connected, returns 409 if not)
POST /cameras/{id}/capture           — capture a single JPEG photo, returns raw bytes (requires connected)

# Remote backend only (feature `backend-remote`)
GET    /peers                        — list registered peers (returns id, url, token — token surfaced for the local UI)
POST   /peers                        — register a peer { url, token? }; validates the peer's /health first (502 if unreachable, wrong token, or not a toucan instance), so dead peers are never stored. Idempotent per URL, returns 201
DELETE /peers/{id}                   — remove a peer (204, or 404 if unknown)
```

### Web UI
- Single-page HTML embedded in the binary via `include_str!("../static/index.html")`.
- Source file: `static/index.html` — no external assets, everything inline.
- Features: camera list with connected badge, connect/disconnect buttons, collapsible parameters panel, live view toggle.
- Auto-refreshes camera list every 5 seconds.

### AVFoundation backend (macOS webcams)

- Source: `src/backends/avfoundation/bridge.m` (Objective-C) + `src/backends/avfoundation/mod.rs` (Rust actor).
- Build: `build.rs` compiles `bridge.m` with `cc` and links `AVFoundation`, `CoreMedia`, `CoreVideo`, `CoreImage`, `Foundation`, `IOKit` frameworks.

#### Parameter reading
- Parameters are enumerated via **CoreMediaIO (CMIO)**: `wc_get_parameters` walks CMIO feature-control objects owned by the device.
- Ranges are read with `kCMIOFeatureControlPropertyNativeRange` / `kCMIOFeatureControlPropertyNativeValue` — these are read-only on most UVC cameras' CMIO drivers (that is fine, we only read).
- `exposure_time_absolute` values from CMIO `NativeValue` are already in 100µs units (same as UVC) for UVC cameras. No scaling is applied.
- Auto/manual toggles (exposure, white balance) are read via `kCMIOFeatureControlPropertyAutomaticManual`.

#### Parameter writing — IOKit direct UVC
- **Do NOT write via CMIO** (`kCMIOFeatureControlPropertyNativeValue`). On typical UVC webcams macOS marks this property as non-settable (`CMIOObjectIsPropertySettable` → false). Any write returns `kCMIOHardwareIllegalOperationError` (0x6E6F7065 = `'nope'`).
- **Do NOT call `[AVCaptureDevice lockForConfiguration]` + set `exposureMode`/`whiteBalanceMode`** inside `wc_set_parameter`. This permanently locks the AVFoundation mode and breaks subsequent CMIO reads.
- Writes go through **IOKit direct UVC `SET_CUR` requests** (`IOUSBDevRequest`, `bmRequestType=0x21`, `bRequest=0x01`).
- At `wc_open_session`: parse the uniqueID to extract the USB locationID, find the `IOUSBDevice` in the IOKit registry, parse the config descriptor to locate the VideoControl interface number + Processing Unit ID + Camera Terminal ID, then call `USBDeviceOpen`.
- At `wc_close_session` / dealloc: call `USBDeviceClose` + `Release`.

#### AVFoundation uniqueID format (USB cameras)
- Format: `"0x"` + up to 16 hex chars (leading zeros omitted) encoding `locationID(32) | vendorID(16) | productID(16)`.
- Example: `"0x130000046d082d"` → locationID=`0x00130000`, vendorID=`0x046d` (Logitech), productID=`0x082d`.
- Parse with `NSScanner scanHexLongLong`, then `locationID = (uint32_t)(combined >> 32)`.
- Minimum valid length is 10 chars (`"0x"` + 8 hex). Built-in cameras have a different format and will skip UVC silently.

#### UVC control table
| kind | unit | selector | size |
|---|---|---|---|
| backlight_compensation | PU | 0x01 | 2 |
| brightness | PU | 0x02 | 2 |
| contrast | PU | 0x03 | 2 |
| gain | PU | 0x04 | 2 |
| hue | PU | 0x06 | 2 |
| saturation | PU | 0x07 | 2 |
| sharpness | PU | 0x08 | 2 |
| white_balance_temperature | PU | 0x0A | 2 |
| white_balance_mode | PU | 0x0B | 1 |
| exposure_mode | CT | 0x02 | 1 (0→1=manual, 1→8=aperture priority) |
| exposure_time_absolute | CT | 0x04 | 4 (100µs units) |
| zoom_absolute | CT | 0x0B | 2 |

### Camera2 backend (Android)
- Source: `src/backends/camera2_android/bridge.c` (NDK Camera2 + Media NDK) + `src/backends/camera2_android/mod.rs` (Rust actor over `std::sync::mpsc`, like the other native backends).
- `backend_id()` is `"camera2-android"`. Compiled only for `target_os = "android"`, gated behind `backend-camera2-android`.
- Build: `build.rs` compiles `bridge.c` with the NDK clang toolchain (needs `ANDROID_NDK_HOME`/`NDK_HOME`, API level 24+) and links `camera2ndk`, `mediandk`, `android`, `log`.
- The crate is built as a `cdylib` loaded by the Kotlin `CameraServerService` via JNI. The `startServer` / `stopServer` / `setToken` entry points live in `lib.rs` (`android_jni` module).
- On Android the HTTP server binds to `0.0.0.0` by default (LAN-accessible) instead of `127.0.0.1`; `BIND_ADDR` overrides. The pairing token is supplied from Kotlin via `setToken()` and can change while running.

### gphoto2 backend (libgphoto2)
- Source: `src/backends/gphoto2/mod.rs`. Pure Rust over the `gphoto2` crate (libgphoto2 PTP/USB cameras). `backend_id()` is `"gphoto2"`. Compiled only for `target_os = "linux"` / `"macos"`, gated behind `backend-gphoto2`.
- **System dependency**: `libgphoto2` must be discoverable via `pkg-config` (`brew install libgphoto2 pkg-config` / `apt install libgphoto2-dev pkg-config`). Linked dynamically and **not bundled** — end users need libgphoto2 installed at runtime (like libusb on Linux). No actor thread: the `gphoto2` crate's `Camera`/`Context` are `Send + Sync` and serialize per-camera calls internally; open handles live in an `Arc<Mutex<HashMap>>`.
- **Coexistence with the Canon EDSDK backend**: when `backend-canon` is also compiled in, it owns Canon bodies (native EVF live view + zoom/pan/tilt, full property set). The gphoto2 backend hides Canon models via `owned_by_other_backend(model)` = `cfg!(feature = "backend-canon") && model.to_lowercase().starts_with("canon")`, filtering `list_devices` and guarding `connect`, so the same camera never appears under both backends nor has the two drivers contend for USB. Without `backend-canon`, gphoto2 handles Canon too.
- **Locale**: libgphoto2 localizes choice labels via gettext (e.g. French "Automatique"/"pose longue", decimal "0,5"). `GPhoto2Backend::new()` sets `LC_ALL=C` before the first gphoto2 call so labels and numeric formatting are stable English/ASCII (and option `value`s round-trip consistently to `set_choice`). Value classification is still locale-independent as defence (see below).
- **Parameter curation** (in `walk_widget` + `get_parameters`), mirroring the Canon backend's intent:
  - ISO is split into an `IsoAuto` boolean + an `Iso` selector; `Iso` is `disabled` when auto is on. The auto choice is detected as the only non-numeric one (`is_concrete_iso`).
  - Shutter speed drops the bulb entry — detected as the only choice with no digit (`is_real_shutter_speed`).
  - Image quality drops RAW / RAW+JPEG / cRAW formats (`is_jpeg_format`), since capture is JPEG-only.
  - `select`/`range_select` parameters with fewer than two options are hidden (no real choice — e.g. exposure compensation in Manual mode).
  - Read-only widgets are skipped. Config-key name → `ParameterType` mapping is in `param_type_for`; the reverse (for `set_parameter`) is `config_key_for`.
- **Keep-alive**: Canon bodies refuse to have `autopoweroff` disabled (PTP `0x2019` Device Busy), so a background `gphoto2-keepalive` thread (`spawn_keepalive`) pings every connected camera every 30 s via `camera.config()` — any PTP activity resets the body's idle timer (the EOS Utility trick), preventing it from sleeping and vanishing mid-session.
- **Live view / capture**: `get_live_view_frame` uses `capture_preview()` (no shutter actuation); `capture_photo` uses `capture_image()` + in-memory download (JPEG only — make sure the body is in a JPEG image-quality mode).
- Unit tests for the pure logic (key mapping, the auto/bulb/RAW heuristics incl. localized labels, Canon deferral) live in the file's `#[cfg(test)]` module; run with `cargo test --features backend-gphoto2`.

## Canon SDK
- SDK files live in `external/EDSDK/` (git-ignored).
- Windows 64-bit library: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library/EDSDK.lib`
- Windows 64-bit DLL: `external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Dll/EDSDK.dll`
- **Official API documentation (PDF)**: `external/EDSDK/EDSDKv132010W/Document/EDSDK_API_EN.pdf`
- **Header files** (property IDs, enums, structs): `external/EDSDK/EDSDKv132010W/Windows/EDSDK/Header/`
- `build.rs` links the SDK library and copies the DLLs to the build output directory automatically.
- Always verify property IDs, error codes, and struct layouts against the header files or PDF before implementing.

## File structure
```
src/
  main.rs             — binary entry point (macOS CFRunLoop pump; #[tokio::main] elsewhere)
  lib.rs              — run_server, build_backends, build_router, Android JNI entry points
  auth.rs             — bearer-token auth middleware (Authorization header or ?token=)
  camera/
    mod.rs            — CameraBackend trait, DeviceId, DeviceInfo, CameraError,
                        CameraParameter, ParameterOption
  backends/
    mod.rs            — feature-gated module declarations
    canon/
      mod.rs          — FFI bindings + impl CameraBackend for CanonBackend
                        (actor pattern, SDK thread, code→label decode tables)
    sony/
      mod.rs          — Sony CrSDK backend (actor pattern): FFI to the C++ bridge,
                        property code↔ParameterType mapping, label decode tables
      bridge.cpp      — flat C shim over the async C++ CrSDK (IDeviceCallback,
                        blocking connect/capture), bridge.h + README.md
    gphoto2/
      mod.rs          — libgphoto2 backend (PTP/USB cameras): param curation,
                        LC_ALL=C labels, keep-alive thread, Canon deferral to EDSDK
    webcam_windows/
      mod.rs          — MediaFoundation + DirectShow backend (Windows webcams)
    webcam_macos/
      mod.rs          — AVFoundation/CMIO/IOKit backend (macOS webcams), C bridge in bridge.m
    camera2_android/
      mod.rs          — Camera2 + Media NDK backend (Android), C bridge in bridge.c
    remote/
      mod.rs          — HTTP-proxying backend + MJPEG live-view relay (RemoteBackend)
      peers.rs        — in-memory PeerRegistry shared with the /peers routes
  routes/
    mod.rs
    cameras.rs        — AppState, BackendState, LiveViewSenders, route handlers
    peers.rs          — /peers management handlers (feature backend-remote)
static/
  index.html          — web UI source (embedded in binary at compile time)
build.rs              — SDK linking + DLL copy based on active features and target OS
```

## Dependencies
- Prefer well-maintained crates from the axum / tokio ecosystem.
- Do not add a dependency that can be replaced by a few lines of standard library code.
- Pin minor versions in `Cargo.toml` (e.g. `"1"` not `"*"`).

## Testing
- Unit tests live in the same file as the code under test (`#[cfg(test)]` module).
- Integration tests live under `tests/`.
- Every new route must have at least one integration test covering the happy path.
- Backend-specific tests must be gated behind the same feature flag as the backend.

## Git
- Commit messages use the imperative mood: "Add Canon live view route", not "Added" or "Adding".
- Never commit secrets, credentials, SDK license files, or local `.env` files.
