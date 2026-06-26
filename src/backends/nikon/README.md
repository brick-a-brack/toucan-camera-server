# Nikon backend — implementation notes

Working notes for `backend-nikon` (Nikon "Remote SDK v2.0.0", MAID3-based,
package `S-SDKZ-200BF-ALLIN`). Kept separate from `CLAUDE.md` because the SDK is
git-ignored (`external/NIKON/`) and these details aren't derivable from the repo.

Status: **work in progress.** Compiles (cfg-gated to macOS + feature) and unit
tests pass; `external/NIKON/runtime/` is populated and `build.rs` stages it next
to the binary. Not yet validated against a real camera. Sections flagged
**[VALIDATE]** need a hardware/runtime check.

Done so far: dlopen loader + actor thread, single-camera session,
list/connect/disconnect/live-view/capture wiring, parameter reader with
`ExposureMode` decoded to labels, `ExposureComp` as a RangeSelect over its steps,
and `IsoControl` auto-ISO split (ISO disabled while auto).

Robustness/instrumentation added for the hardware test (items 1 & 2):
- **Capture** filters on `kNkMAIDEvent_ImageSaved` (=8) for the saved path, and
  falls back to the newest file in a freshly-emptied temp dir if no event fires.
- **Live view** accepts only payloads starting with the JPEG SOI marker
  (`FF D8 FF`) and logs the first frame's size + head bytes once — a failed
  marker means the `NkMaidLiveViewData` header size (884) is wrong.
- **Parameter labels**: `Aperture` (f-number ×100), `ISO` (direct), `ShutterSpeed`
  (packed num/den) now decode with plausibility guards + raw fallback. These are
  HEURISTIC — set `NIKON_DUMP_PARAMS=1` to log the real codes and confirm/correct.

See the hardware test checklist at the bottom.

---

## 1. What the SDK gives us

Supported bodies: Z9, Z8, Z6III, Z7II, Z6II, Z7, Z6, Z5II, Z5, Zf, Z50II, Z50,
Z30, Zfc, ZR. Platforms: **Windows + macOS only (no Linux)** — on Linux, Nikon
stays on the gphoto2 (ptp2) backend.

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
This directory is now populated (unzipped from `TestApp.zip` and rearranged; the
zip nests the files under `TestApp/TestApp/` and `TestApp/Frameworks/`).
`build.rs::copy_nikon_runtime()` copies it next to the binary on every build.

The 3 `.config` files must end up in `~/Library/Preferences/Nikon/NXTether/`
(hardcoded path inside the SDK). The backend deploys them at startup from the
files placed next to the binary.

---

## 2. Hard constraints

- **Single camera only** (ReadMe + the CS-Layer session is global: no device
  handle on `ConnectDevice`/`StartLiveView`/`StartShooting`). The backend keeps
  one `connected: Option<(native_id, device_id_u32)>` and refuses a 2nd connect.
- **Event loop**: callbacks (events, live view, shoot completion) are delivered
  while the SDK is pumped. We run everything on a dedicated `nikon-sdk` OS thread
  (actor pattern, like the Canon backend) and pump in the recv loop.
- **Coexistence (server-level dedup, not in any backend)**: the dedup is a server
  policy in `routes/cameras.rs::dedup_devices`, not gphoto2-specific logic.
  - Each `DeviceInfo` carries an optional `dedup_key` = `camera::dedup_key(usb_vendor,
    model)` (`"04b0:z5ii"`), built by any backend that can identify the body.
  - `CameraBackend::dedup_priority()` ranks backends; SDK backends return 10,
    generic ones 0.
  - `list_cameras` gathers every backend's devices and `dedup_devices` keeps, per
    `dedup_key`, only the highest-priority one. Keyless devices are never deduped.
  - The Nikon SDK / Canon EDSDK set the key from their model name + known vendor.
    gphoto2 sets it from the real USB vendor (nusb) + its model (or the USB product
    string when libgphoto2 only knows the body generically, so a Z5II shown as
    "USB PTP Class Camera" still keys to "04b0:z5ii").
  - **Key property**: SDK backends only enumerate models they support, so a key
    only ever collides for an SDK-driven body. Older Nikon/Canon (no SDK entry)
    have no collision and **stay on gphoto2 automatically** — no model lists, and
    **gphoto2 knows nothing about the Canon/Nikon backends**. Adding a vendor SDK =
    new backend sets its `dedup_key` + `dedup_priority`; nothing else changes.
  Build with BOTH features: `--features backend-nikon,backend-gphoto2`. Conflicts
  with NX Tether / Camera Control Pro / Nikon Transfer.

---

## 3. Mapping to the `CameraBackend` trait

| trait method | CS Layer |
|---|---|
| `backend_id` | `"nikon"` |
| `list_devices` | `EnumDevices` → `NkMAIDEnumDevices.pDeviceData[]` (`ID:u32`, `Name`, `Availability`) |
| `connect` | `ConnectDevice(id_u32)` (+ single-camera guard) |
| `disconnect` | `DisconnectDevice()` |
| `get_parameters` | `GetCapability(id, kNkSDKGetSettingSupportedValueArray)` → the supported-codes array; `ulValue` = current **index** |
| `set_parameter` | read enum (mode 0) for a valid struct, set `ulValue = index`, `pData = NULL`, `SetCapability(id, &enum, EnumPtr)` |

**Enum semantics (important):** `NkMAIDEnum.ulValue` is an **index** into the
supported-values array, NOT a raw code (confirmed in Nikon's sample: the menu
lists `index - label` and set assigns the chosen index to `ulValue`). Options
must be read with mode 1 (`SupportedValueArray`) — mode 0 (`Value`) does not fill
the array, which produced duplicate/garbage options. So `option.value = index`,
`label = decode(rawCode[index])`, `current = ulValue`.
| `get_live_view_frame` | `StartLiveView` once; JPEG arrives via `LiveViewDataProc`; kept in a global latest-frame cell |
| `capture_photo` | SaveMedia=SDRAM + `SetImageVideoSavePath(tmp)` + `StartShooting(Single)`; read the file from the `ImageSaved` event |

### Native ID
`EnumDevices` returns a numeric `ID:u32` + a `Name`. We use
`native_id = "<ID>|<Name>"` so the opaque ID survives re-enumeration by name and
we can still recover the numeric `ID` for `ConnectDevice`. **[VALIDATE]** whether
`ID` is stable across reconnects; if not, match by `Name`.

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
| LiveViewStatus | 0x823e |
| GetLiveViewImage | 0x8247 |
| LiveViewProhibit | 0x825e |
| SaveMedia | 0x8305 |
| LiveViewSelector | 0x8334 |

`eNkMAIDSaveMedia`: Card=0, SDRAM=1, Card_SDRAM=2.
`eNkSDKGetSettingRequestType`: Value=0, SupportedValueArray=1, DefaultValue=2, CapabilityInfo=3.
`eNkMAIDDataType`: UnsignedPtr=6, RangePtr=14, ArrayPtr=15, **EnumPtr=16**.
`eNkMAIDResult`: NoError=0, Pending=+1, Waiting_2ndRelease=168.
`eNkSDKShootingType`: Single=1, Continuous=2, Interval=3, SelfTimer=4, BULB=5, …
`eNkMAIDEvent`: ImageSaved index = `CapChangeValueOnly(6) + 1 + 1`… use the value
from `Maid3.h` enum order (AddChild=0 … ImageSaved). **[VALIDATE]** numeric value.

### Live view data layout
`NkMAIDLiveViewData { u32 ulLvImageSize; u16 wPhysicalBytes; u16 wLogicalBits;
NKMAIDLiveViewHeader header; void* pImageData; }`. The header is modelled as
`[u8; 884]` (size derived field-by-field from `NkTypes.h`; AFAREASIZE=96 → four
96×2-byte arrays = 768 of it). With the trailing pointer this gives the same
padding as C (pImageData at offset 896). **[VALIDATE]** the 884 figure on the
real struct (e.g. `sizeof` check in a tiny C probe) before trusting live view —
a wrong size means reading `pImageData` from garbage.

The `LiveViewDataProc` callback **owns** the data: copy the JPEG out, then `free`
`pImageData` and the struct (matches the sample). Our `AllocateMemory`/`FreeMemory`
passed to `InitializeSDK` are `malloc`/`free`.

`InitializeSDK` requires **all five** `NkMAIDCSCallback` procs to be non-null
(UIRequest, Event, Progress, Data, LiveView) — leaving any null returns
`-93 = kNkMAIDAPIResult_InvalidArguments`. We supply no-op stubs for the three we
don't otherwise need (UIRequest auto-answers with the request's default button).

---

## 5. Open questions / TODO

- **[VALIDATE] capture file delivery**: confirm SaveMedia=SDRAM makes the SDK
  write the JPEG to `SetImageVideoSavePath` and fire `ImageSaved` (event 8) with
  the full path (mac: `data` is `char*`). Now filtered on event 8 with a
  newest-file-in-temp-dir fallback, so it should work either way — but verify the
  event actually carries the path (vs. the fallback doing all the work).
- **[VALIDATE] decode heuristics**: `Aperture`=code/100, `ISO`=code, `ShutterSpeed`
  =packed num/den. Run with `NIKON_DUMP_PARAMS=1`, compare the logged codes to the
  camera's displayed values, and correct the decoders / add the `WBMode` table.
- **`libMCARecLib3.dylib`** mentioned in docs but absent from the zip — check at
  runtime whether the bundle needs it.
- **`Sensitivity` vs `ISOControlSensitivity`**: confirm 0x8117 is the right ISO
  capability for stills (there are several ISO-related caps).

---

## 6b. Canon EDSDK + Nikon SDK coexistence (ObjC class clash)

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

Verified structurally (rename applied in both arch slices, dylib loads + signs).
**[VALIDATE]** on hardware that a Canon (EDSDK) and a Nikon work simultaneously
with no `objc[..]: Class … implemented in both …` warnings and no Canon deadlock.
If new clashing class names appear in the warning, add them to the `RENAMES` list.
Small residual risk: if libNkPTPDriver2 ever looks these classes up by name
(`objc_getClass`), the rename would break that lookup — not observed.

The Nikon SDK is loaded eagerly at startup (the class rename removed the reason to
defer it); it no longer needs `nusb`.

## 7. Hardware test checklist (real Z body)

1. `external/NIKON/runtime/` is populated; `cargo build --features backend-nikon`
   stages the bundle/Frameworks/configs next to the binary. Quit NX Tether /
   Camera Control Pro / Nikon Transfer first.
2. Run with the dump on, e.g.
   `NIKON_DUMP_PARAMS=1 BIND_ADDR=127.0.0.1:8040 ./toucan-camera-server`
   (token via the usual mechanism). `GET /cameras` → the Z should appear under
   backend `nikon`.
3. `PUT /cameras/{id}/connect`, then `GET /cameras/{id}/parameters`. Check stderr
   for `[nikon] cap 0x…: codes=[…]` lines; compare to the body's UI and fix the
   `decode_*` tables (especially ShutterSpeed and add WBMode).
4. `GET /cameras/{id}/liveview`. Expect one `[nikon] first live view frame: …
   jpeg=true` log. If `jpeg=false`, the `NkMaidLiveViewData` header size is wrong.
5. `POST /cameras/{id}/capture` → should return a JPEG. Note in the log whether
   the path came from the ImageSaved event or the temp-dir fallback.
6. Report back the dumped codes + any errors so the tables/flows can be finalised.
- **dlopen + install_name fixup (done)**: we `dlopen` the bundle's inner binary.
  The shipped bundle is wired for a `.app` layout (`@executable_path/../Frameworks`)
  and `libNkPTPDriver2.dylib` references `@rpath/Royalmile.framework` with no rpath,
  plus a download quarantine — a plain copy fails to load. `build.rs::fixup_nikon_runtime()`
  now rewrites both to `@loader_path`-relative paths, adds the rpath, strips
  quarantine, and ad-hoc re-signs the modified Mach-Os. Verified: dlopen succeeds.
- **macOS CI packaging**: the build.rs fixup needs `install_name_tool`, `codesign`,
  `xattr` (standard on macOS runners). Also lipo/relink the Nikon dylibs like the
  gphoto2 closure, and ship the config files. For a real signed/notarised build,
  re-sign with a real identity instead of ad-hoc.

---

## 6. Files touched
- `Cargo.toml` — `backend-nikon` feature.
- `src/backends/mod.rs` — module decl (cfg macos + feature).
- `src/backends/nikon/mod.rs` — backend (this work).
- `build.rs` — `copy_nikon_runtime()`.
- `src/lib.rs` — registration in `build_backends()`.
- `src/backends/gphoto2/mod.rs` — `owned_by_other_backend()` now also defers Nikon.
