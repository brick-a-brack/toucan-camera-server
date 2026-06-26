# Nikon backend — implementation notes

Working notes for `backend-nikon` (Nikon "Remote SDK v2.0.0", MAID3-based,
package `S-SDKZ-200BF-ALLIN`). Kept separate from `CLAUDE.md` because the SDK is
git-ignored (`external/NIKON/`) and these details aren't derivable from the repo.

**Status: working, validated on a Z5II** (detection, connect, parameters, live
view, capture). macOS + the `backend-nikon` feature only. Builds with the
`backend-gphoto2` / `backend-canon` features alongside it (the three coexist —
see §2 and §6). Body-specific items still relying on heuristics are noted inline.

What it does: dlopen CS-Layer loader + `nikon-sdk` actor thread, single-camera
session, list/connect/disconnect/live-view/capture, and a curated parameter set —
`ExposureMode` decoded to labels, `ExposureComp` as a RangeSelect over its steps,
`IsoControl` auto-ISO split (ISO disabled while auto), and a JPEG-only
`ImageQuality` (RAW / RAW+JPEG options hidden).

Key runtime facts confirmed on hardware:
- **Enum capabilities come back as `PackedString`** (`NkMAIDEnum.ul_type == 7`):
  `p_data` is NUL-separated label strings, so the SDK hands us human labels for
  free (e.g. `"JPEG Fine"`, `"1/250"`, `"F5.6"`, `"ISO 100"`). The numeric
  `decode_*` functions are a fallback for bodies that report raw codes instead.
- **Live view** payloads start with the JPEG SOI marker (`FF D8 FF`); the
  `NkMaidLiveViewData` header size (884) is correct — `p_image_data` lands at
  offset 896 (asserted by the `live_view_data_layout` unit test).
- **Capture** routes the image to SDRAM and reads it back from a temp dir. The
  `ImageSaved` event reports a *bare* filename (e.g. `"SImage.001.jpg"`), so the
  path is resolved against the save dir, with a newest-file fallback. Capture is
  JPEG-only: a RAW body is forced to a JPEG image quality first, and a non-JPEG
  result is refused.

---

## 1. What the SDK gives us

Supported bodies: Z9, Z8, Z6III, Z7II, Z6II, Z7, Z6, Z5II, Z5, Zf, Z50II, Z50,
Z30, Zfc, ZR. Platforms: **Windows + macOS only (no Linux)** — on Linux, Nikon
stays on the gphoto2 (ptp2) backend. This backend is macOS-only for now.

Two API layers exist; we use the **CS Layer (Simplified API)**, a flat set of C
functions, not the low-level MAID3 entry point.

macOS runtime artifacts live inside `external/NIKON/S-SDKZ-200BF-ALLIN/Module/Mac/BinaryFile/TestApp.zip`:
- `TypeCommon Module.bundle` — the CS-Layer module (exports `InitializeSDK`,
  `EnumDevices`, `ConnectDevice`, `StartLiveView`, `StartShooting`,
  `GetCapability`, `SetCapability`, `FreeSDK`, … in **C linkage**, no `MAID` prefix).
- `Frameworks/libNkPTPDriver2.dylib`, `Frameworks/Royalmile.framework`.
- 3 config files: `MaidLayer.config` (~16 MB), `RangeValue.config`, `DC_PTP_Config.config`.

All binaries are universal (x86_64 + arm64). The bundle hardcodes
`@executable_path/../Frameworks/libNkPTPDriver2.dylib`; the PTP driver pulls
`@rpath/Royalmile.framework`.

### Expected layout for the build
`build.rs` copies from a **stable, unzipped** directory:
```
external/NIKON/runtime/
  TypeCommon Module.bundle/        (whole bundle)
  Frameworks/libNkPTPDriver2.dylib
  Frameworks/Royalmile.framework/
  config/DC_PTP_Config.config
  config/MaidLayer.config
  config/RangeValue.config
```
Populated by unzipping `TestApp.zip` and rearranging it (the zip nests files
under `TestApp/TestApp/` and `TestApp/Frameworks/`).
`build.rs::copy_nikon_runtime()` copies it next to the binary on every build, then
`fixup_nikon_runtime()` rewrites the install names / rpaths and re-signs (see §6).

The 3 `.config` files must end up in `~/Library/Preferences/Nikon/NXTether/`
(hardcoded path inside the SDK). `deploy_config_files()` copies them there at
startup from the files staged next to the binary.

---

## 2. Hard constraints

- **Single camera only** (the CS-Layer session is global: no device handle on
  `ConnectDevice`/`StartLiveView`/`StartShooting`). The backend keeps one
  `Session { native_id, live_view_running }` and refuses a 2nd connect.
- **Event loop**: callbacks (events, live view, shoot completion) are delivered
  while the SDK is pumped. Everything runs on a dedicated `nikon-sdk` OS thread
  (actor pattern over `std::sync::mpsc`, like the Canon backend).
- **Lazy, nusb-gated init**: `InitializeSDK` (and especially `EnumDevices`) probe
  the USB/PTP bus and can take ~10–18 s when a non-Nikon PTP body is also
  attached. So `new()` returns immediately and the SDK is initialized only once a
  Nikon-vendor (`0x04B0`) USB device is actually present (`nikon_usb_present()`
  via `nusb`); `list_devices` fires a one-shot background warm-up and reports
  empty until ready. Enumeration is cached (refreshed only when idle and stale,
  never while a session is live — re-probing mid-session breaks the running Nikon
  live view).
- **Coexistence** with the Canon EDSDK and gphoto2 backends — see §6 (ObjC class
  clash) and §3 (server-level dedup). Build with the features together, e.g.
  `--features backend-nikon,backend-gphoto2`. Conflicts with NX Tether / Camera
  Control Pro / Nikon Transfer (quit them first).

---

## 3. Mapping to the `CameraBackend` trait

| trait method | CS Layer |
|---|---|
| `backend_id` | `"nikon"` |
| `dedup_priority` | `10` (SDK backend wins dedup over gphoto2 for the same body) |
| `list_devices` | `EnumDevices` → `NkMAIDEnumDevices.pDeviceData[]` (`ID:u32`, `Name`) |
| `connect` | `ConnectDevice(id_u32)` (+ single-camera guard) |
| `disconnect` | `StopLiveView` then `DisconnectDevice()` |
| `get_parameters` | `GetCapability(id, SupportedValueArray)` → options; `ulValue` = current **index** |
| `set_parameter` | read enum (mode 0) for a valid struct, set `ulValue = index`, `pData = NULL`, `SetCapability(id, &enum, EnumPtr)` |
| `get_live_view_frame` | `StartLiveView` once; JPEG arrives via `LiveViewDataProc`; kept in a global latest-frame cell |
| `capture_photo` | SaveMedia=SDRAM + `SetImageVideoSavePath(tmp)` + `StartShooting(Single)`; read the file from the `ImageSaved` event (newest-file fallback) |

**Enum semantics (important):** `NkMAIDEnum.ulValue` is an **index** into the
supported-values array, NOT a raw code (confirmed in Nikon's sample: the menu
lists `index - label` and set assigns the chosen index to `ulValue`). Options must
be read with mode 1 (`SupportedValueArray`) — mode 0 (`Value`) does not fill the
array, which produced duplicate/garbage options. So `option.value = index`,
`label = decode(rawCode[index])` (or the PackedString label), `current = ulValue`.

Parameters are curated like the Canon/gphoto2 backends: ISO is split into an
`IsoAuto` boolean + an `Iso` selector (disabled while auto is on); `ExposureComp`
is a RangePtr exposed as a RangeSelect over its discrete steps; `ImageQuality`
hides RAW / RAW+JPEG options (capture is JPEG-only).

### Native ID
`EnumDevices` returns a numeric `ID:u32` + a `Name`. We use
`native_id = "<ID>|<Name>"` so the opaque ID survives re-enumeration by name and
we can still recover the numeric `ID` for `ConnectDevice`.

---

## 4. Constants (verified from headers)

`kNkMAIDCapability_VendorBase = 0x8000`, `VendorBaseDX2 = 0x8100`.

| capability | id |
|---|---|
| FileType | 0x810f |
| CompressionLevel | 0x8110 |
| ExposureMode | 0x8111 |
| ShutterSpeed | 0x8112 |
| Aperture | 0x8113 |
| ExposureComp | 0x8115 |
| MeteringMode | 0x8116 |
| Sensitivity (ISO) | 0x8117 |
| WBMode | 0x8118 |
| IsoControl | 0x816c |
| SaveMedia | 0x8305 |

`eNkMAIDSaveMedia`: Card=0, SDRAM=1, Card_SDRAM=2.
`eNkSDKGetSettingRequestType`: Value=0, SupportedValueArray=1, DefaultValue=2, CapabilityInfo=3.
`eNkMAIDArrayType`: PackedString=7 (the form the Z bodies use for enum caps).
`eNkMAIDDataType`: BooleanPtr=4, UnsignedPtr=6, RangePtr=14, ArrayPtr=15, **EnumPtr=16**.
`eNkMAIDResult`: NoError=0, Pending=+1, Waiting_2ndRelease=168, StartLiveViewFailed=-109, LiveViewAlreadyStarted=-112.
`eNkSDKShootingType`: Single=1, Continuous=2, Interval=3, SelfTimer=4, BULB=5, …
`eNkMAIDEvent`: `ImageSaved = 8` (mac: event `data` is a `char*` to the saved path).

### Live view data layout
`NkMAIDLiveViewData { u32 ulLvImageSize; u16 wPhysicalBytes; u16 wLogicalBits;
NKMAIDLiveViewHeader header; void* pImageData; }`. The header is modelled as
`[u8; 884]` (size derived field-by-field from `NkTypes.h`; AFAREASIZE=96 → four
96×2-byte arrays = 768 of it). With the trailing pointer this puts `pImageData` at
offset 896 — matching the C struct, and asserted by the `live_view_data_layout`
unit test plus the runtime JPEG-SOI check on every frame.

The `LiveViewDataProc` callback **owns** the data: copy the JPEG out, then `free`
`pImageData` and the struct (matches the sample). Our `AllocateMemory` /
`FreeMemory` passed to `InitializeSDK` are `malloc` / `free`.

`InitializeSDK` requires **all five** `NkMAIDCSCallback` procs to be non-null
(UIRequest, Event, Progress, Data, LiveView) — leaving any null returns
`-93 = kNkMAIDAPIResult_InvalidArguments`. We supply no-op stubs for the three we
don't otherwise need (UIRequest auto-answers with the request's default button).

`SetLoggingLevel(2)` (Error) is used by default; `NIKON_SDK_DEBUG=1` raises it to
Debug (3) to trace the (chatty) SDK — useful when diagnosing a slow init.

---

## 5. Server-level dedup (Nikon ↔ gphoto2 ↔ Canon)

The dedup is a server policy in `routes/cameras.rs::dedup_devices`, not
backend-specific logic.
- Each `DeviceInfo` carries an optional `dedup_key` = `camera::dedup_key(usb_vendor,
  model)` (`"04b0:z5ii"`), built by any backend that can identify the body.
- `CameraBackend::dedup_priority()` ranks backends; SDK backends return 10, generic
  ones 0. `list_cameras` gathers every backend's devices and keeps, per
  `dedup_key`, only the highest-priority one. Keyless devices are never deduped.
- The Nikon SDK / Canon EDSDK set the key from their model name + known vendor.
  gphoto2 sets it from the real USB vendor (nusb) + its model (or the USB product
  string when libgphoto2 only knows the body generically, so a Z5II shown as
  "USB PTP Class Camera" still keys to "04b0:z5ii").
- **Key property**: SDK backends only enumerate models they support, so a key only
  ever collides for an SDK-driven body. Older Nikon/Canon (no SDK entry) have no
  collision and **stay on gphoto2 automatically** — no model lists, and gphoto2
  knows nothing about the Canon/Nikon backends. Adding a vendor SDK = a new backend
  that sets its `dedup_key` + `dedup_priority`; nothing else changes.

(Nikon's USB product string carries a "DSC" prefix the SDK name lacks — SDK name
"Z5_2" → "04b0:z5ii" but USB product "DSC Z5_2" → "04b0:dscz5ii" — so
`normalize_model` strips "dsc" alongside "nikon" to align them.)

---

## 6. Canon EDSDK + Nikon SDK coexistence (ObjC class clash)

Both `libNkPTPDriver2.dylib` (Nikon) and EDSDK (Canon) define Objective-C classes
with identical names (`PTPOperationRequest`, `PTPOperationResponse`, `PTPEvent`, +
their `…PrivateData` variants). The ObjC runtime keeps only one class per name
process-wide, so loading both SDKs in one process corrupts one driver (Canon
deadlock / connect failure). macOS has no per-dylib ObjC namespace (no `dlmopen`),
so to keep a **single process** `build.rs::patch_nikon_objc_classes()` renames
those classes in the staged `libNkPTPDriver2.dylib`:
- same-length byte patch `PTP…` → `NkP…`, matched as full NUL-terminated strings
  (so a prefix like `PTPEvent` doesn't hit `PTPEventPrivateData`);
- only the runtime registration NAME changes — exported `_OBJC_CLASS_$_*` symbols
  and internal class-ref pointers are untouched (two-level namespace binds them
  per-dylib), so Nikon keeps using its own classes, now non-colliding;
- the dylib is ad-hoc re-signed afterwards (verified: still dlopens, signature OK).

14 occurrences are renamed across both arch slices. Residual risk: if
libNkPTPDriver2 ever looks these classes up by name (`objc_getClass`), the rename
would break that lookup (not observed); if Nikon ships new clashing class names,
add them to the `RENAMES` list. gphoto2 is C (no ObjC), so it never clashed.

### dlopen + install_name fixup
We `dlopen` the bundle's inner binary. The shipped bundle is wired for a `.app`
layout (`@executable_path/../Frameworks`), `libNkPTPDriver2.dylib` references
`@rpath/Royalmile.framework` with no rpath, and a download-quarantine xattr is set
— a plain copy fails to load. `build.rs::fixup_nikon_runtime()` rewrites both to
`@loader_path`-relative paths, adds the rpath, strips quarantine (`xattr -cr`), and
ad-hoc re-signs the modified Mach-Os. Verified: dlopen succeeds.

**macOS CI packaging**: the fixup needs `install_name_tool`, `codesign`, `xattr`
(standard on macOS runners). lipo/relink the Nikon dylibs like the gphoto2 closure
and ship the config files. For a real signed/notarised build, re-sign with a real
identity instead of ad-hoc.

---

## 7. Remaining heuristics / per-body unknowns

- **Numeric `decode_*` fallback**: on the validated bodies enum caps are
  PackedString (labels for free), so `decode_aperture` (code/100), `decode_iso`
  (direct), `decode_shutter_speed` (packed num/den) and `decode_exposure_mode`
  only run on bodies that report raw numeric codes. They guard on plausible ranges
  and fall back to the raw value. `WBMode` has no numeric decoder (raw fallback).
- **`Sensitivity` (0x8117) vs other ISO caps**: confirmed adequate for stills on
  the Z5II; other bodies may expose ISO differently.

---

## 8. Files
- `Cargo.toml` — `backend-nikon` feature (pulls `nusb`).
- `src/backends/mod.rs` — module decl (cfg macos + feature).
- `src/backends/nikon/mod.rs` — the backend.
- `build.rs` — `copy_nikon_runtime()` / `fixup_nikon_runtime()` / `patch_nikon_objc_classes()`.
- `src/lib.rs` — registration in `build_backends()`.
- `src/routes/cameras.rs` — server-level `dedup_devices()`.
</content>
</invoke>
