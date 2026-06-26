//! Nikon backend (Remote SDK v2.0.0, MAID3 "CS Layer").
//!
//! The Nikon SDK is delivered as a macOS CFBundle (`TypeCommon Module.bundle`)
//! whose CS-Layer functions are exported in plain C linkage. We `dlopen` the
//! bundle's binary at runtime and resolve the function pointers by name — there
//! is no build-time link step.
//!
//! Like the Canon EDSDK backend, every SDK call runs on a single dedicated OS
//! thread (`nikon-sdk`) using the actor pattern over `std::sync::mpsc`. The SDK
//! exposes a **single global session** (no per-device handle on its calls), so
//! this backend controls **one camera at a time** — `connect` refuses a second
//! device while one is already connected.
//!
//! See `README.md` in this directory for the integration notes and constants.

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

// ---------------------------------------------------------------------------
// libdl / libc (always available in libSystem on macOS — no extra crate)
// ---------------------------------------------------------------------------

extern "C" {
    fn dlopen(filename: *const c_char, flag: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

const RTLD_NOW: c_int = 0x2;

// ---------------------------------------------------------------------------
// MAID constants (see README.md and Maid3d1.h)
// ---------------------------------------------------------------------------

/// Nikon's USB vendor id — used to build the cross-backend dedup key so gphoto2
/// recognises the same body and yields it to this (higher-priority) backend.
const USB_VENDOR_NIKON: u16 = 0x04B0;

const CAP_FILE_TYPE: u32 = 0x810f; // image format (JPEG/NEF/…) — codes camera-specific
const CAP_COMPRESSION_LEVEL: u32 = 0x8110; // JPEG compression (Basic/Normal/Fine)
const CAP_EXPOSURE_MODE: u32 = 0x8111;
const CAP_SHUTTER_SPEED: u32 = 0x8112;
const CAP_APERTURE: u32 = 0x8113;
const CAP_EXPOSURE_COMP: u32 = 0x8115; // RangePtr (computed value), not an enum
const CAP_SENSITIVITY: u32 = 0x8117; // ISO
const CAP_WB_MODE: u32 = 0x8118;
const CAP_ISO_CONTROL: u32 = 0x816c; // boolean: auto-ISO on/off
const CAP_SAVE_MEDIA: u32 = 0x8305;

// eNkSDKGetSettingRequestType
const GET_SETTING_VALUE: i32 = 0;
const GET_SETTING_SUPPORTED_VALUE_ARRAY: i32 = 1;

// eNkMAIDArrayType (NkMAIDEnum.ul_type): how the supported-values array is encoded.
const ARRAY_TYPE_PACKED_STRING: u32 = 7;

// eNkMAIDDataType
const DATATYPE_BOOLEAN_PTR: i32 = 4;
const DATATYPE_UNSIGNED_PTR: i32 = 6;
const DATATYPE_RANGE_PTR: i32 = 14;
const DATATYPE_ENUM_PTR: i32 = 16;

// eNkMAIDResult
const RESULT_NO_ERROR: i32 = 0;
const RESULT_WAITING_2ND_RELEASE: i32 = 168;
// Live-view (re)start results (eNkMAIDResult negative codes).
const RESULT_LIVE_VIEW_ALREADY_STARTED: i32 = -112;

// eNkMAIDEvent (Maid3.h enum order): the camera finished saving a transferred
// image; on macOS the event `data` is a `char*` to the saved file path.
const EVENT_IMAGE_SAVED: u32 = 8;

// eNkMAIDSaveMedia
const SAVE_MEDIA_SDRAM: u32 = 1;

// eNkSDKShootingType
const SHOOTING_TYPE_SINGLE: i32 = 1;

/// Capture wait budget once the shutter has fired.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// FFI structs (mirror NkTypes.h / Maid3.h — see README.md)
// ---------------------------------------------------------------------------

#[repr(C)]
struct NkMaidDeviceInfo {
    id: u32,
    name: [c_char; 64],
    availability: u8, // C++ bool, 1 byte
    ul_connected_pid: u32,
    version: [c_char; 64],
}

#[repr(C)]
struct NkMaidEnumDevices {
    ul_elements: u32,
    ul_value: u32,
    p_device_data: *mut NkMaidDeviceInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NkMaidEnum {
    ul_type: u32,         // one of eNkMAIDArrayType
    ul_elements: u32,     // number of options
    ul_value: u32,        // current index INTO the supported-values array
    ul_default: u32,      // default index
    w_physical_bytes: i16, // SWORD: bytes per element
    p_data: *mut c_void,  // array of `ul_elements` values, each `w_physical_bytes` wide
}

/// Mirrors `NkMAIDRange` (used by ExposureComp). `lfValue`/`lfLower`/`lfUpper`
/// are the value and its bounds; when `ul_steps >= 2` the value is the discrete
/// step `ul_value_index` (value = lfLower + idx*(lfUpper-lfLower)/(ulSteps-1)).
#[repr(C)]
struct NkMaidRange {
    lf_value: f64,
    lf_default: f64,
    ul_value_index: u32,
    ul_default_index: u32,
    lf_lower: f64,
    lf_upper: f64,
    ul_steps: u32,
}

/// Live view payload. The header is opaque (`[u8; 884]`, size derived from
/// `NkTypes.h`); only `ul_lv_image_size` and `p_image_data` are read. The
/// trailing pointer reproduces the same padding as the C struct (`p_image_data`
/// at offset 896, asserted by the `live_view_data_layout` unit test).
#[repr(C)]
struct NkMaidLiveViewData {
    ul_lv_image_size: u32,
    w_physical_bytes: u16,
    w_logical_bits: u16,
    st_live_view_header: [u8; 884],
    p_image_data: *mut c_void,
}

#[repr(C)]
struct NkMaidCsCallback {
    p_ui_req_proc: *mut c_void,
    pfn_event_proc: *mut c_void,
    p_progress_proc: *mut c_void,
    p_data_proc: *mut c_void,
    p_live_view_data_proc: *mut c_void,
    ref_proc: *mut c_void,
}

/// Mirrors `MAIDShootingStructure` (non-Windows variant: `char ImageSavePath[1024]`).
#[repr(C)]
struct MaidShootingStructure {
    shooting_type: i32,
    ul_continuous_interval_num_shots: u32,
    ul_bulb_exposure_duration: u32,
    ul_shooting_start_time_from_now: u32,
    ul_interval_time: u32,
    b_auto_focus: u8, // C++ bool
    image_save_path: [c_char; 1024],
    p_output_reference: *mut c_void,
}

impl MaidShootingStructure {
    fn zeroed() -> Self {
        // SAFETY: all fields are POD / valid when zeroed (paths become "", ptr null).
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// CS-Layer function pointer types
// ---------------------------------------------------------------------------

type AllocFn = unsafe extern "C" fn(usize) -> *mut c_void;
type FreeFn = unsafe extern "C" fn(*mut c_void);

type InitializeSdkFn = unsafe extern "C" fn(
    AllocFn,
    FreeFn,
    *mut NkMaidCsCallback,
    *mut *mut NkMaidEnumDevices,
    *mut *mut c_void, // ppEnumCapInfo (unused here)
) -> i32;
type FreeSdkFn = unsafe extern "C" fn() -> i32;
type EnumDevicesFn =
    unsafe extern "C" fn(*mut *mut NkMaidEnumDevices, *mut c_void, *mut c_void) -> i32;
type ConnectDeviceFn = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type DisconnectDeviceFn = unsafe extern "C" fn() -> i32;
type StartLiveViewFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type StopLiveViewFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> i32;
type StartShootingFn =
    unsafe extern "C" fn(*mut MaidShootingStructure, *mut c_void, *mut c_void) -> i32;
type GetCapabilityFn = unsafe extern "C" fn(u32, i32, *mut *mut c_void, *mut i32) -> i32;
type SetCapabilityFn = unsafe extern "C" fn(u32, *mut c_void, i32) -> i32;
type SetImageVideoSavePathFn = unsafe extern "C" fn(*const c_char, *const c_char) -> i32;
type SetLoggingLevelFn = unsafe extern "C" fn(i32) -> i32;

/// Resolved CS-Layer entry points. Lives only on the `nikon-sdk` thread.
struct Sdk {
    free_sdk: FreeSdkFn,
    enum_devices: EnumDevicesFn,
    connect_device: ConnectDeviceFn,
    disconnect_device: DisconnectDeviceFn,
    start_live_view: StartLiveViewFn,
    stop_live_view: StopLiveViewFn,
    start_shooting: StartShootingFn,
    get_capability: GetCapabilityFn,
    set_capability: SetCapabilityFn,
    set_image_video_save_path: SetImageVideoSavePathFn,
}

// ---------------------------------------------------------------------------
// Globals fed by SDK callbacks. The SDK is single-session, so a global is the
// natural fit (and avoids passing Rust refs through C callbacks).
// ---------------------------------------------------------------------------

/// Latest JPEG frame delivered by `LiveViewDataProc`.
static LATEST_LV_FRAME: Mutex<Option<Vec<u8>>> = Mutex::new(None);
/// Full path of the most recent saved image, from the `ImageSaved` event.
static LAST_SAVED_PATH: Mutex<Option<String>> = Mutex::new(None);

unsafe extern "C" fn alloc_memory(size: usize) -> *mut c_void {
    malloc(size)
}

unsafe extern "C" fn free_memory(ptr: *mut c_void) {
    free(ptr)
}

// `InitializeSDK` rejects the call (InvalidArguments) unless all five callbacks
// are non-null, so we provide no-op stubs for the ones we don't otherwise use.

/// UI request callback. We have no UI, so auto-answer with the request's own
/// default button (the 2nd `ULONG` of `NkMAIDUIRequestInfo`, `ulDefault`).
unsafe extern "C" fn ui_request_proc(_ref: *mut c_void, ui_request: *mut c_void) -> u32 {
    if ui_request.is_null() {
        return 1; // kNkMAIDUIRequestResult_Ok
    }
    *(ui_request as *const u32).add(1)
}

/// Progress callback during lengthy operations — ignored.
unsafe extern "C" fn progress_proc(
    _ul_command: u32,
    _ul_param: u32,
    _ref: *mut c_void,
    _ul_done: u32,
    _ul_total: u32,
) {
}

/// Data callback (image/sound/file delivery). Capture uses the file path from
/// the ImageSaved event instead, so this is a no-op returning success.
unsafe extern "C" fn data_proc(
    _ref: *mut c_void,
    _p_info: *mut c_void,
    _p_data: *mut c_void,
) -> i32 {
    RESULT_NO_ERROR
}

/// Receives MAID events. We record the saved file path on `ImageSaved`
/// (mac: `data` is a `char*` to the path).
unsafe extern "C" fn event_proc(_ref_client: *mut c_void, ul_event: u32, data: u64) {
    if ul_event == EVENT_IMAGE_SAVED && data != 0 {
        let ptr = data as *const c_char;
        if let Ok(s) = CStr::from_ptr(ptr).to_str() {
            if !s.is_empty() {
                if let Ok(mut g) = LAST_SAVED_PATH.lock() {
                    *g = Some(s.to_string());
                }
            }
        }
    }
}

/// Receives a live view JPEG. Owns the data: copy it out, then free as the SDK
/// sample does (`free(pImageData)` + `free(struct)`).
///
/// We only accept payloads starting with the JPEG SOI marker (`FF D8 FF`), which
/// also guards against a wrong `NkMaidLiveViewData` header size (`p_image_data`
/// would then point at garbage that fails the marker check).
unsafe extern "C" fn live_view_data_proc(_ref: *mut c_void, data: *mut NkMaidLiveViewData) -> i32 {
    if data.is_null() {
        return RESULT_NO_ERROR;
    }
    let lv = &*data;
    if lv.ul_lv_image_size > 0 && !lv.p_image_data.is_null() {
        let bytes = std::slice::from_raw_parts(
            lv.p_image_data as *const u8,
            lv.ul_lv_image_size as usize,
        );
        let is_jpeg = bytes.len() >= 3 && bytes[0] == 0xFF && bytes[1] == 0xD8 && bytes[2] == 0xFF;
        if is_jpeg {
            if let Ok(mut g) = LATEST_LV_FRAME.lock() {
                *g = Some(bytes.to_vec());
            }
        }
        free(lv.p_image_data);
    }
    free(data as *mut c_void);
    RESULT_NO_ERROR
}

// ---------------------------------------------------------------------------
// Actor commands
// ---------------------------------------------------------------------------

enum Command {
    ListDevices {
        reply: mpsc::Sender<Result<Vec<DeviceInfo>, CameraError>>,
    },
    Connect {
        native_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    Disconnect {
        native_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    IsConnected {
        native_id: String,
        reply: mpsc::Sender<bool>,
    },
    GetParameters {
        native_id: String,
        reply: mpsc::Sender<Result<Vec<CameraParameter>, CameraError>>,
    },
    GetLiveViewFrame {
        native_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    SetParameter {
        native_id: String,
        param_type: ParameterType,
        value: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    CapturePhoto {
        native_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    /// Fire-and-forget: initialize the SDK in the background (pre-warm) so the
    /// first real command doesn't pay the ~10 s InitializeSDK cost inline.
    Warmup,
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct NikonBackend {
    tx: mpsc::Sender<Command>,
    /// Set once the SDK has finished initializing (on the sdk thread).
    ready: Arc<AtomicBool>,
    /// Ensures we only fire one background warm-up.
    warming: Arc<AtomicBool>,
}

impl NikonBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let ready = Arc::new(AtomicBool::new(false));

        // The SDK is initialized lazily/asynchronously (see sdk_thread):
        // InitializeSDK probes USB devices and can take ~10 s when a non-Nikon
        // camera is attached, so we never pay that cost inline. `new()` returns
        // immediately; warm-up happens in the background once a Nikon appears.
        let ready_thread = ready.clone();
        std::thread::Builder::new()
            .name("nikon-sdk".to_string())
            .spawn(move || sdk_thread(cmd_rx, ready_thread))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        Ok(Self {
            tx: cmd_tx,
            ready,
            warming: Arc::new(AtomicBool::new(false)),
        })
    }

    fn call<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<T>) -> Command,
        on_err: T,
    ) -> T {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(make(reply_tx)).is_err() {
            return on_err;
        }
        reply_rx.recv().unwrap_or(on_err)
    }
}

impl Drop for NikonBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for NikonBackend {
    fn backend_id(&self) -> &str {
        "nikon"
    }

    /// Above the generic backends: the Nikon SDK gives native live view and the
    /// full parameter set, so it wins dedup over gphoto2 for the same body.
    fn dedup_priority(&self) -> i32 {
        10
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        // The SDK's EnumDevices can take ~18 s probing non-Nikon bodies (e.g. a
        // Canon also plugged in). Skip it entirely unless a Nikon-vendor USB device
        // is actually present — keeps /cameras fast when no Nikon is connected.
        if !nikon_usb_present() {
            return Ok(Vec::new());
        }
        // A Nikon is present. If the SDK isn't initialized yet, kick off a one-shot
        // background warm-up and report empty for now — it'll appear on a later poll
        // once ready. This keeps list_devices non-blocking (never pays the ~10 s
        // InitializeSDK cost inline).
        if !self.ready.load(Ordering::Relaxed) {
            if !self.warming.swap(true, Ordering::Relaxed) {
                let _ = self.tx.send(Command::Warmup);
            }
            return Ok(Vec::new());
        }
        self.call(
            |reply| Command::ListDevices { reply },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Connect {
                native_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Disconnect {
                native_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.call(
            |reply| Command::IsConnected {
                native_id: native_id.to_string(),
                reply,
            },
            false,
        )
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        self.call(
            |reply| Command::GetParameters {
                native_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::GetLiveViewFrame {
                native_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        self.call(
            |reply| Command::SetParameter {
                native_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::CapturePhoto {
                native_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }
}

// ---------------------------------------------------------------------------
// Native-ID helpers
//
// EnumDevices returns a numeric `ID:u32` and a `Name`. We encode the native id
// as "<ID>|<Name>" so we keep the numeric ID needed by ConnectDevice while the
// opaque device id stays human-meaningful.
// ---------------------------------------------------------------------------

fn make_native_id(id: u32, name: &str) -> String {
    format!("{id}|{name}")
}

fn parse_device_id(native_id: &str) -> Option<u32> {
    native_id.split('|').next()?.parse().ok()
}

// ---------------------------------------------------------------------------
// Connected-session state (single camera)
// ---------------------------------------------------------------------------

struct Session {
    native_id: String,
    live_view_running: bool,
}

// ---------------------------------------------------------------------------
// SDK thread
// ---------------------------------------------------------------------------

fn sdk_thread(rx: mpsc::Receiver<Command>, ready: Arc<AtomicBool>) {
    // The SDK is initialized lazily, on the first command that needs it (or a
    // background Warmup). The Nikon driver (libNkPTPDriver2.dylib) coexists with
    // the Canon EDSDK because build.rs renames its clashing ObjC PTP classes.
    let mut sdk: Option<Sdk> = None;
    let mut session: Option<Session> = None;
    // Cached enumeration so we don't re-probe the USB/PTP bus (which disturbs other
    // attached cameras, e.g. a Canon) on every /cameras poll. Only refreshed when
    // idle and stale; never while a session is live.
    let mut cached: Vec<(String, String)> = Vec::new();
    let mut last_enum: Option<std::time::Instant> = None;

    /// Ensures the SDK is loaded; on failure replies the error and skips the command.
    macro_rules! sdk {
        ($reply:expr) => {
            match ensure_sdk(&mut sdk) {
                Some(s) => s,
                None => {
                    let _ = $reply.send(Err(CameraError::SdkError(0xFFFF_FFFF)));
                    continue;
                }
            }
        };
    }

    loop {
        match rx.recv() {
            Ok(Command::ListDevices { reply }) => {
                // Re-enumerate ONLY when idle (no live session) and the cache is
                // empty or stale. While connected, reuse the cache — calling
                // EnumDevices mid-session re-probes other PTP devices and breaks the
                // running Nikon live view.
                let idle = session.is_none();
                let stale = last_enum
                    .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(15));
                if idle && (cached.is_empty() || stale) {
                    let s = sdk!(reply);
                    match enumerate_nikon(s) {
                        Ok(raw) => {
                            cached = raw;
                            last_enum = Some(std::time::Instant::now());
                        }
                        Err(e) => {
                            let _ = reply.send(Err(e));
                            ready.store(sdk.is_some(), Ordering::Relaxed);
                            continue;
                        }
                    }
                }
                let devices = cached
                    .iter()
                    .map(|(nid, name)| nikon_device_info(nid, name, &session))
                    .collect();
                let _ = reply.send(Ok(devices));
            }
            // Pure session state — never needs the SDK (so it won't trigger init).
            Ok(Command::IsConnected { native_id, reply }) => {
                let connected = session
                    .as_ref()
                    .map(|s| s.native_id == native_id)
                    .unwrap_or(false);
                let _ = reply.send(connected);
            }
            Ok(Command::Connect { native_id, reply }) => {
                let s = sdk!(reply);
                let _ = reply.send(connect_impl(s, &native_id, &mut session));
            }
            Ok(Command::Disconnect { native_id, reply }) => {
                let s = sdk!(reply);
                let result = disconnect_impl(s, &native_id, &mut session);
                // Allow a fresh enumeration next time (device set may have changed).
                cached.clear();
                last_enum = None;
                let _ = reply.send(result);
            }
            Ok(Command::GetParameters { native_id, reply }) => {
                let s = sdk!(reply);
                let _ = reply.send(get_parameters_impl(s, &native_id, &session));
            }
            Ok(Command::GetLiveViewFrame { native_id, reply }) => {
                let s = sdk!(reply);
                let _ = reply.send(get_live_view_frame_impl(s, &native_id, &mut session));
            }
            Ok(Command::SetParameter {
                native_id,
                param_type,
                value,
                reply,
            }) => {
                let s = sdk!(reply);
                let _ = reply.send(set_parameter_impl(s, &native_id, param_type, &value, &session));
            }
            Ok(Command::CapturePhoto { native_id, reply }) => {
                let s = sdk!(reply);
                let _ = reply.send(capture_photo_impl(s, &native_id, &session));
            }
            // Background pre-warm: initialize the SDK (no reply expected).
            Ok(Command::Warmup) => {
                let _ = ensure_sdk(&mut sdk);
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
        // Reflect init state so list_devices knows when the SDK is usable.
        ready.store(sdk.is_some(), Ordering::Relaxed);
    }

    if let Some(sdk) = sdk {
        if session.take().is_some() {
            unsafe { (sdk.stop_live_view)(std::ptr::null_mut(), std::ptr::null_mut()) };
            unsafe { (sdk.disconnect_device)() };
        }
        unsafe { (sdk.free_sdk)() };
    }
}

/// Lazily loads and initializes the Nikon SDK, caching it in `slot`. Returns the
/// loaded SDK, or `None` if initialization failed.
fn ensure_sdk(slot: &mut Option<Sdk>) -> Option<&Sdk> {
    if slot.is_none() {
        match load_and_init_sdk() {
            Ok(sdk) => *slot = Some(sdk),
            Err(e) => {
                eprintln!("[nikon] SDK init failed: {e:?}");
                return None;
            }
        }
    }
    slot.as_ref()
}

// ---------------------------------------------------------------------------
// Loading & init
// ---------------------------------------------------------------------------

/// Resolves the bundle binary next to the running executable, dlopens it, wires
/// the symbols, deploys the `.config` files, and calls `InitializeSDK`.
fn load_and_init_sdk() -> Result<Sdk, CameraError> {
    deploy_config_files();

    let bundle_bin = bundle_binary_path().ok_or(CameraError::SdkError(0xFFFF_FFF0))?;
    let c_path = CString::new(bundle_bin.to_string_lossy().as_bytes())
        .map_err(|_| CameraError::SdkError(0xFFFF_FFF1))?;

    let handle = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        eprintln!("[nikon] dlopen failed for {}", bundle_bin.display());
        return Err(CameraError::SdkError(0xFFFF_FFF2));
    }

    // Resolve a symbol or bail. Symbols are plain C names (no `MAID` prefix).
    macro_rules! sym {
        ($name:literal, $ty:ty) => {{
            let cname = CString::new($name).unwrap();
            let p = unsafe { dlsym(handle, cname.as_ptr()) };
            if p.is_null() {
                eprintln!("[nikon] missing symbol: {}", $name);
                return Err(CameraError::SdkError(0xFFFF_FFF3));
            }
            unsafe { std::mem::transmute::<*mut c_void, $ty>(p) }
        }};
    }

    let initialize_sdk: InitializeSdkFn = sym!("InitializeSDK", InitializeSdkFn);
    let set_logging_level: SetLoggingLevelFn = sym!("SetLoggingLevel", SetLoggingLevelFn);
    let sdk = Sdk {
        free_sdk: sym!("FreeSDK", FreeSdkFn),
        enum_devices: sym!("EnumDevices", EnumDevicesFn),
        connect_device: sym!("ConnectDevice", ConnectDeviceFn),
        disconnect_device: sym!("DisconnectDevice", DisconnectDeviceFn),
        start_live_view: sym!("StartLiveView", StartLiveViewFn),
        stop_live_view: sym!("StopLiveView", StopLiveViewFn),
        start_shooting: sym!("StartShooting", StartShootingFn),
        get_capability: sym!("GetCapability", GetCapabilityFn),
        set_capability: sym!("SetCapability", SetCapabilityFn),
        set_image_video_save_path: sym!("SetImageVideoSavePath", SetImageVideoSavePathFn),
    };

    // Error level only — the SDK is chatty in Debug. NIKON_SDK_DEBUG=1 raises it to
    // Debug (3) to trace what InitializeSDK does (e.g. why it's slow probing a
    // non-Nikon PTP device that's also attached). 1=SystemError, 2=Error, 3=Debug.
    let log_level = if std::env::var_os("NIKON_SDK_DEBUG").is_some() {
        3
    } else {
        2
    };
    unsafe { set_logging_level(log_level) };

    let mut callback = NkMaidCsCallback {
        p_ui_req_proc: ui_request_proc as *mut c_void,
        pfn_event_proc: event_proc as *mut c_void,
        p_progress_proc: progress_proc as *mut c_void,
        p_data_proc: data_proc as *mut c_void,
        p_live_view_data_proc: live_view_data_proc as *mut c_void,
        ref_proc: std::ptr::null_mut(),
    };
    let mut device_list: *mut NkMaidEnumDevices = std::ptr::null_mut();

    let err = unsafe {
        initialize_sdk(
            alloc_memory,
            free_memory,
            &mut callback,
            &mut device_list,
            std::ptr::null_mut(),
        )
    };
    if err != RESULT_NO_ERROR {
        eprintln!("[nikon] InitializeSDK failed: {err}");
        return Err(CameraError::SdkError(err as u32));
    }

    Ok(sdk)
}

/// Path to the CFBundle's executable next to the running binary:
/// `<exe_dir>/TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module`.
fn bundle_binary_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let p = dir
        .join("TypeCommon Module.bundle")
        .join("Contents/MacOS/TypeCommon Module");
    p.exists().then_some(p)
}

/// Copies the 3 `.config` files (shipped next to the binary) into
/// `~/Library/Preferences/Nikon/NXTether/`, where the SDK expects them.
/// Best-effort: missing files are logged, not fatal.
fn deploy_config_files() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let dest = std::path::Path::new(&home).join("Library/Preferences/Nikon/NXTether");
    if std::fs::create_dir_all(&dest).is_err() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let Some(src_dir) = exe.parent() else {
        return;
    };
    for name in ["DC_PTP_Config.config", "MaidLayer.config", "RangeValue.config"] {
        let src = src_dir.join(name);
        let dst = dest.join(name);
        if src.exists() && !dst.exists() {
            if let Err(e) = std::fs::copy(&src, &dst) {
                eprintln!("[nikon] failed to deploy {name}: {e}");
            }
        }
    }
}

/// True if any Nikon-vendor (0x04B0) USB device is connected. Used to skip the
/// SDK's slow `EnumDevices` when no Nikon body is present. If the USB scan fails,
/// returns `true` so we fall back to asking the SDK.
fn nikon_usb_present() -> bool {
    use nusb::MaybeFuture;
    match nusb::list_devices().wait() {
        Ok(devices) => devices.into_iter().any(|d| d.vendor_id() == USB_VENDOR_NIKON),
        Err(_) => true,
    }
}

// ---------------------------------------------------------------------------
// SDK operations (run exclusively on the nikon-sdk thread)
// ---------------------------------------------------------------------------

/// Raw SDK enumeration: `(native_id, name)` for each Nikon body the SDK can drive.
///
/// WARNING: `EnumDevices` probes the USB/PTP bus and, when another PTP device is
/// attached (e.g. a Canon), it tries to talk to it — which is slow and disrupts an
/// in-progress Nikon session. Callers MUST cache this and never call it while a
/// live session is active (see the ListDevices handler in `sdk_thread`).
fn enumerate_nikon(sdk: &Sdk) -> Result<Vec<(String, String)>, CameraError> {
    let mut list: *mut NkMaidEnumDevices = std::ptr::null_mut();
    let err = unsafe { (sdk.enum_devices)(&mut list, std::ptr::null_mut(), std::ptr::null_mut()) };
    if err != RESULT_NO_ERROR || list.is_null() {
        return Err(CameraError::SdkError(err as u32));
    }

    let mut out = Vec::new();
    unsafe {
        let count = (*list).ul_elements as usize;
        let data = (*list).p_device_data;
        for i in 0..count {
            let info = &*data.add(i);
            let name = CStr::from_ptr(info.name.as_ptr())
                .to_string_lossy()
                .into_owned();
            let native_id = make_native_id(info.id, &name);
            out.push((native_id, name));
        }
        // The SDK allocated the list with our allocator; free it.
        free((*list).p_device_data as *mut c_void);
        free(list as *mut c_void);
    }
    Ok(out)
}

/// Builds the API [`DeviceInfo`] for a cached `(native_id, name)`, setting the live
/// `connected` flag from the current session.
fn nikon_device_info(native_id: &str, name: &str, session: &Option<Session>) -> DeviceInfo {
    let connected = session
        .as_ref()
        .map(|s| s.native_id == native_id)
        .unwrap_or(false);
    // The SDK only enumerates bodies it supports, so this dedup key only ever
    // collides with gphoto2 for cameras the SDK drives — older Nikons (no SDK
    // entry) are left to gphoto2 automatically.
    DeviceInfo {
        id: DeviceId::new("nikon", native_id).encode(),
        dedup_key: Some(crate::camera::dedup_key(USB_VENDOR_NIKON, name)),
        name: name.to_string(),
        connected,
    }
}

fn connect_impl(
    sdk: &Sdk,
    native_id: &str,
    session: &mut Option<Session>,
) -> Result<(), CameraError> {
    if let Some(s) = session.as_ref() {
        // Single-camera SDK: connecting the same device is a no-op; a different
        // one is rejected until the current is disconnected.
        if s.native_id == native_id {
            return Ok(());
        }
        return Err(CameraError::NotSupported);
    }

    let device_id = parse_device_id(native_id)
        .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;

    let mut cap_info: *mut c_void = std::ptr::null_mut();
    let err = unsafe { (sdk.connect_device)(device_id, &mut cap_info) };
    if !cap_info.is_null() {
        // We don't keep the capability snapshot; free it.
        unsafe { free(cap_info) };
    }
    if err != RESULT_NO_ERROR {
        return Err(CameraError::SdkError(err as u32));
    }

    *session = Some(Session {
        native_id: native_id.to_string(),
        live_view_running: false,
    });
    Ok(())
}

fn disconnect_impl(
    sdk: &Sdk,
    native_id: &str,
    session: &mut Option<Session>,
) -> Result<(), CameraError> {
    let is_current = session
        .as_ref()
        .map(|s| s.native_id == native_id)
        .unwrap_or(false);
    if !is_current {
        return Err(CameraError::DeviceNotFound(native_id.to_string()));
    }

    // Always stop live view before disconnecting (not just when our flag says it
    // is running): a StartLiveView that failed mid-recovery can leave the SDK with
    // live view on, which would make the next session's StartLiveView fail.
    // StopLiveView when nothing is running is harmless (AlreadyStopped).
    unsafe { (sdk.stop_live_view)(std::ptr::null_mut(), std::ptr::null_mut()) };
    let err = unsafe { (sdk.disconnect_device)() };
    *session = None;
    *LATEST_LV_FRAME.lock().unwrap() = None;
    if err != RESULT_NO_ERROR {
        return Err(CameraError::SdkError(err as u32));
    }
    Ok(())
}

fn require_connected<'a>(
    native_id: &str,
    session: &'a Option<Session>,
) -> Result<&'a Session, CameraError> {
    match session.as_ref() {
        Some(s) if s.native_id == native_id => Ok(s),
        _ => Err(CameraError::NotConnected),
    }
}

fn get_live_view_frame_impl(
    sdk: &Sdk,
    native_id: &str,
    session: &mut Option<Session>,
) -> Result<Vec<u8>, CameraError> {
    {
        let s = require_connected(native_id, session)?;
        let _ = s;
    }

    // Start the stream on first poll. The SDK then pushes frames via
    // LiveViewDataProc into LATEST_LV_FRAME.
    let need_start = session
        .as_ref()
        .map(|s| !s.live_view_running)
        .unwrap_or(false);
    if need_start {
        start_live_view(sdk)?;
        if let Some(s) = session.as_mut() {
            s.live_view_running = true;
        }
    }

    if let Some(frame) = LATEST_LV_FRAME.lock().unwrap().clone() {
        Ok(frame)
    } else {
        // Not ready yet — mirror the Canon/remote "object not ready" code so the
        // route layer keeps polling.
        Err(CameraError::SdkError(0x0000_A102))
    }
}

/// Starts live view, recovering from stale SDK state. After a disconnect/reconnect
/// the SDK often rejects `StartLiveView` with `StartLiveViewFailed` (-109) because
/// it still holds the previous session's live-view state; a `StopLiveView` resets
/// it. We also retry a few times to cover the body not being ready right after
/// connect. `LiveViewAlreadyStarted` is treated as success.
fn start_live_view(sdk: &Sdk) -> Result<(), CameraError> {
    let mut err =
        unsafe { (sdk.start_live_view)(std::ptr::null_mut(), std::ptr::null_mut()) };
    if err == RESULT_NO_ERROR || err == RESULT_LIVE_VIEW_ALREADY_STARTED {
        return Ok(());
    }
    for _ in 0..5 {
        unsafe { (sdk.stop_live_view)(std::ptr::null_mut(), std::ptr::null_mut()) };
        std::thread::sleep(Duration::from_millis(200));
        err = unsafe { (sdk.start_live_view)(std::ptr::null_mut(), std::ptr::null_mut()) };
        if err == RESULT_NO_ERROR || err == RESULT_LIVE_VIEW_ALREADY_STARTED {
            return Ok(());
        }
    }
    eprintln!("[nikon] StartLiveView failed after retries: {err}");
    Err(CameraError::SdkError(err as u32))
}

fn capture_photo_impl(
    sdk: &Sdk,
    native_id: &str,
    session: &Option<Session>,
) -> Result<Vec<u8>, CameraError> {
    require_connected(native_id, session)?;

    // Switch a RAW body to JPEG so the transferred file is servable as image/jpeg.
    ensure_jpeg_quality(sdk);

    // Route the image to the host (SDRAM) so the SDK transfers + saves it.
    let media = SAVE_MEDIA_SDRAM;
    let media_err = unsafe {
        (sdk.set_capability)(
            CAP_SAVE_MEDIA,
            &media as *const u32 as *mut c_void,
            DATATYPE_UNSIGNED_PTR,
        )
    };
    if media_err != RESULT_NO_ERROR {
        eprintln!("[nikon] capture: SetCapability(SaveMedia=SDRAM) -> {media_err}");
    }

    // Save into a fresh, empty temp dir so any file appearing afterwards is
    // unambiguously the one we just shot (fallback when no ImageSaved event).
    let tmp_dir = std::env::temp_dir().join("toucan-nikon-capture");
    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::create_dir_all(&tmp_dir);
    let c_dir = CString::new(tmp_dir.to_string_lossy().as_bytes())
        .map_err(|_| CameraError::NotSupported)?;
    unsafe { (sdk.set_image_video_save_path)(c_dir.as_ptr(), c_dir.as_ptr()) };

    *LAST_SAVED_PATH.lock().unwrap() = None;

    let mut shoot = MaidShootingStructure::zeroed();
    shoot.shooting_type = SHOOTING_TYPE_SINGLE;
    let err =
        unsafe { (sdk.start_shooting)(&mut shoot, std::ptr::null_mut(), std::ptr::null_mut()) };
    if err != RESULT_NO_ERROR && err != RESULT_WAITING_2ND_RELEASE {
        return Err(CameraError::SdkError(err as u32));
    }

    // Prefer the path reported by the ImageSaved event; otherwise fall back to
    // the newest file that appeared in the temp dir. The event often reports a
    // bare filename (e.g. "SImage.001.nef"), so resolve it against the save dir.
    let deadline = std::time::Instant::now() + CAPTURE_TIMEOUT;
    loop {
        let from_event = LAST_SAVED_PATH.lock().unwrap().clone();
        if let Some(file) = resolve_capture_file(from_event.as_deref(), &tmp_dir) {
            let bytes = std::fs::read(&file).map_err(|_| CameraError::SdkError(0x0000_0002))?;
            let _ = std::fs::remove_file(&file);

            // JPEG only — a RAW/NEF file can't be served as image/jpeg.
            if bytes.len() < 3 || bytes[0] != 0xFF || bytes[1] != 0xD8 || bytes[2] != 0xFF {
                eprintln!(
                    "[nikon] capture: '{}' is not a JPEG ({} bytes) — set the camera's \
                     image quality to JPEG (RAW/NEF is not supported).",
                    file.display(),
                    bytes.len()
                );
                return Err(CameraError::NotSupported);
            }
            return Ok(bytes);
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "[nikon] capture: timed out — no ImageSaved event and no file in {}. \
                 Check SaveMedia support and that the body actually released.",
                tmp_dir.display()
            );
            return Err(CameraError::SdkError(0x0000_0001)); // capture timeout
        }
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// Resolves the captured file: the ImageSaved event path (absolute, or relative
/// to the save dir / CWD), else the newest file in the save dir.
fn resolve_capture_file(
    event_path: Option<&str>,
    save_dir: &std::path::Path,
) -> Option<std::path::PathBuf> {
    if let Some(p) = event_path {
        let pb = std::path::Path::new(p);
        if pb.is_absolute() && pb.exists() {
            return Some(pb.to_path_buf());
        }
        let in_dir = save_dir.join(p);
        if in_dir.exists() {
            return Some(in_dir);
        }
        if pb.exists() {
            return Some(pb.to_path_buf());
        }
    }
    newest_file_in(save_dir)
}

/// Newest regular file in `dir` by modification time (capture fallback).
fn newest_file_in(dir: &std::path::Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, path)| path)
}

// ---------------------------------------------------------------------------
// Parameters
//
// Enum capabilities are read into Select / RangeSelect params. `ExposureMode`
// has a known value enum so it gets human labels; `ShutterSpeed` / `Aperture` /
// `Sensitivity` / `WBMode` carry raw camera codes (the SDK ships no value→label
// table — even Nikon's own sample prints them raw), so their labels are the raw
// codes for now, to be decoded empirically against hardware (see docs).
//
// `IsoControl` (auto-ISO) is surfaced as an `IsoAuto` boolean; `Sensitivity` is
// disabled while it is on. `ExposureComp` is a RangePtr exposed as a RangeSelect
// over its discrete steps (value = step index), mirroring Nikon's own formula.
// ---------------------------------------------------------------------------

/// One enum-backed parameter: its trait type, MAID capability id, whether its
/// values form an ordered progression (RangeSelect vs Select), and a label
/// decoder for the raw code.
struct EnumSpec {
    param_type: ParameterType,
    cap_id: u32,
    ordered: bool,
    decode: fn(i64) -> String,
}

fn raw_label(v: i64) -> String {
    v.to_string()
}

/// `eNkMAIDExposureMode` (Maid3d1.h). Unknown codes fall back to the raw value.
fn decode_exposure_mode(v: i64) -> String {
    match v {
        0 => "Program (P)",
        1 => "Aperture priority (A)",
        2 => "Shutter priority (S)",
        3 => "Manual (M)",
        4 => "Disable",
        5 => "Auto",
        6 => "Portrait",
        7 => "Landscape",
        8 => "Close-up",
        9 => "Sports",
        10 => "Night portrait",
        11 => "Night view",
        12 => "Child",
        13 => "Flash off",
        14 => "Scene",
        17 => "Effects",
        _ => return v.to_string(),
    }
    .to_string()
}

// The decoders below are a numeric fallback only: on the validated Z bodies the
// SDK returns these capabilities as PackedString (human labels used directly).
// They use Nikon's common numeric conventions and are HEURISTIC — the CS-Layer
// enum codes are undocumented and may differ per body, so each falls back to the
// raw code outside a plausible range.

/// Aperture (f-number). Nikon's PTP convention encodes the f-number ×100
/// (e.g. 560 → f/5.6, 1400 → f/14).
fn decode_aperture(v: i64) -> String {
    if !(100..=13000).contains(&v) {
        return v.to_string();
    }
    let f = v as f64 / 100.0;
    if (f.fract()).abs() < 0.05 {
        format!("f/{f:.0}")
    } else {
        format!("f/{f:.1}")
    }
}

/// ISO sensitivity. Nikon encodes the actual ISO value (e.g. 100, 6400).
fn decode_iso(v: i64) -> String {
    if (25..=6_553_600).contains(&v) {
        format!("ISO {v}")
    } else {
        v.to_string()
    }
}

/// Shutter speed. Nikon packs numerator in the high 16 bits and denominator in
/// the low 16 bits (e.g. 1/250 s → 0x0001_00FA, 4 s → 0x0004_0001). Produces
/// "1/250", "4\"" or "1.3\"" with plausibility guards; raw otherwise.
fn decode_shutter_speed(v: i64) -> String {
    let num = ((v >> 16) & 0xFFFF) as u32;
    let den = (v & 0xFFFF) as u32;
    if num == 0 || den == 0 {
        return v.to_string();
    }
    if num == 1 && den <= 16000 {
        return format!("1/{den}");
    }
    if den == 1 && num <= 900 {
        return format!("{num}\"");
    }
    let s = num as f64 / den as f64;
    if s > 0.0 && s <= 900.0 {
        return format!("{s:.1}\"");
    }
    v.to_string()
}

const ENUM_PARAMS: &[EnumSpec] = &[
    EnumSpec { param_type: ParameterType::Exposure, cap_id: CAP_EXPOSURE_MODE, ordered: false, decode: decode_exposure_mode },
    EnumSpec { param_type: ParameterType::ShutterSpeed, cap_id: CAP_SHUTTER_SPEED, ordered: true, decode: decode_shutter_speed },
    EnumSpec { param_type: ParameterType::Aperture, cap_id: CAP_APERTURE, ordered: true, decode: decode_aperture },
    EnumSpec { param_type: ParameterType::Iso, cap_id: CAP_SENSITIVITY, ordered: true, decode: decode_iso },
    // WBMode has no documented value enum — raw codes until decoded on hardware.
    EnumSpec { param_type: ParameterType::WhiteBalance, cap_id: CAP_WB_MODE, ordered: false, decode: raw_label },
];

fn type_to_cap(param_type: ParameterType) -> Option<u32> {
    ENUM_PARAMS
        .iter()
        .find(|s| s.param_type == param_type)
        .map(|s| s.cap_id)
}

fn get_parameters_impl(
    sdk: &Sdk,
    native_id: &str,
    session: &Option<Session>,
) -> Result<Vec<CameraParameter>, CameraError> {
    require_connected(native_id, session)?;

    let iso_auto = read_bool_cap(sdk, CAP_ISO_CONTROL);

    let mut result = Vec::new();
    for spec in ENUM_PARAMS {
        // ISO is greyed out while auto-ISO is on, like the Canon/gphoto2 backends.
        let disabled = spec.param_type == ParameterType::Iso && iso_auto == Some(true);
        if let Some(param) = read_enum_param(sdk, spec, disabled) {
            result.push(param);
        }
    }

    if let Some(auto) = iso_auto {
        result.push(CameraParameter::Boolean {
            param_type: ParameterType::IsoAuto,
            current: auto,
            disabled: false,
        });
    }

    if let Some(ec) = read_exposure_comp(sdk) {
        result.push(ec);
    }

    // JPEG quality (CompressionLevel, JPEG-only options).
    if let Some(q) = read_image_quality(sdk) {
        result.push(q);
    }

    Ok(result)
}

/// Reads one enum capability into a Select / RangeSelect parameter. Returns
/// `None` when the capability is unsupported in the current camera state.
///
/// Options come from `GetSettingSupportedValueArray` (mode 1). `ulValue` is the
/// **index** of the current option (what `SetCapability` expects), so
/// `option.value = index`. The label depends on the array type:
/// - `PackedString`: `pData` is NUL-separated strings — the SDK's own labels are
///   used directly (e.g. "JPEG Fine", "1/250", "F5.6", "ISO 100").
/// - numeric (`Unsigned`/`Integer`): each element is a raw code passed through
///   `spec.decode`.
fn read_enum_param(sdk: &Sdk, spec: &EnumSpec, disabled: bool) -> Option<CameraParameter> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err = unsafe {
        (sdk.get_capability)(
            spec.cap_id,
            GET_SETTING_SUPPORTED_VALUE_ARRAY,
            &mut data,
            &mut data_type,
        )
    };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_ENUM_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return None;
    }

    let param = unsafe {
        let en = &*(data as *const NkMaidEnum);
        let labels: Vec<String> = if en.ul_type == ARRAY_TYPE_PACKED_STRING {
            parse_packed_strings(en.p_data, en.ul_elements as usize)
        } else {
            let elem_bytes = en.w_physical_bytes.max(1) as usize;
            (0..en.ul_elements as usize)
                .map(|i| read_enum_element(en.p_data, i, elem_bytes))
                .map(spec.decode)
                .collect()
        };

        let options: Vec<ParameterOption> = labels
            .into_iter()
            .enumerate()
            .map(|(i, label)| ParameterOption { label, value: i.to_string() })
            .collect();
        let current = en.ul_value.to_string(); // current index into the array

        // Free the SDK-allocated value array and the enum struct.
        if !en.p_data.is_null() {
            free(en.p_data);
        }
        if spec.ordered {
            CameraParameter::RangeSelect { param_type: spec.param_type, current, options, disabled }
        } else {
            CameraParameter::Select { param_type: spec.param_type, current, options, disabled }
        }
    };
    unsafe { free(data) };
    Some(param)
}

/// Reads element `idx` (1, 2 or 4 bytes wide) from an SDK value array as i64.
unsafe fn read_enum_element(base: *const c_void, idx: usize, elem_bytes: usize) -> i64 {
    if base.is_null() {
        return 0;
    }
    let p = (base as *const u8).add(idx * elem_bytes);
    match elem_bytes {
        1 => *p as i64,
        2 => i16::from_ne_bytes([*p, *p.add(1)]) as i64,
        _ => i32::from_ne_bytes([*p, *p.add(1), *p.add(2), *p.add(3)]) as i64,
    }
}

/// Parses a MAID `PackedString` array: `byte_len` bytes of NUL-separated strings
/// at `base`. Each string is one option, indexed by its position (the index that
/// `ulValue` / `SetCapability` use). The trailing terminator is dropped.
unsafe fn parse_packed_strings(base: *const c_void, byte_len: usize) -> Vec<String> {
    if base.is_null() || byte_len == 0 {
        return Vec::new();
    }
    let bytes = std::slice::from_raw_parts(base as *const u8, byte_len);
    let mut out: Vec<String> = bytes
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    // `split` yields a trailing empty element after the final NUL terminator.
    if out.last().is_some_and(|s| s.is_empty()) {
        out.pop();
    }
    out
}

/// Reads a boolean capability (`BooleanPtr`). `None` if unsupported.
fn read_bool_cap(sdk: &Sdk, cap_id: u32) -> Option<bool> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err = unsafe { (sdk.get_capability)(cap_id, GET_SETTING_VALUE, &mut data, &mut data_type) };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_BOOLEAN_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return None;
    }
    let on = unsafe { *(data as *const u8) != 0 };
    unsafe { free(data) };
    Some(on)
}

/// Reads `ExposureComp` (a RangePtr) as a RangeSelect over its discrete steps.
/// Returns `None` if unsupported or continuous (`ulSteps < 2`).
fn read_exposure_comp(sdk: &Sdk) -> Option<CameraParameter> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err =
        unsafe { (sdk.get_capability)(CAP_EXPOSURE_COMP, GET_SETTING_VALUE, &mut data, &mut data_type) };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_RANGE_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return None;
    }

    let param = unsafe {
        let r = &*(data as *const NkMaidRange);
        if r.ul_steps < 2 {
            None
        } else {
            let steps = r.ul_steps;
            let span = r.lf_upper - r.lf_lower;
            let options = (0..steps)
                .map(|i| {
                    let ev = r.lf_lower + (i as f64) * span / ((steps - 1) as f64);
                    ParameterOption {
                        label: format!("{ev:+.1} EV"),
                        value: i.to_string(),
                    }
                })
                .collect();
            Some(CameraParameter::RangeSelect {
                param_type: ParameterType::ExposureCompensation,
                current: r.ul_value_index.to_string(),
                options,
                disabled: false,
            })
        }
    };
    unsafe { free(data) };
    param
}

fn set_parameter_impl(
    sdk: &Sdk,
    native_id: &str,
    param_type: ParameterType,
    value: &str,
    session: &Option<Session>,
) -> Result<(), CameraError> {
    require_connected(native_id, session)?;

    match param_type {
        ParameterType::IsoAuto => return set_bool_cap(sdk, CAP_ISO_CONTROL, value == "true"),
        ParameterType::ExposureCompensation => {
            let idx: u32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            return set_exposure_comp(sdk, idx);
        }
        ParameterType::ImageQuality => {
            let idx: u32 = value.parse().map_err(|_| CameraError::NotSupported)?;
            return set_enum_index(sdk, CAP_COMPRESSION_LEVEL, idx);
        }
        _ => {}
    }

    let cap_id = type_to_cap(param_type).ok_or(CameraError::NotSupported)?;
    // `value` is the option index produced by read_enum_param.
    let idx: u32 = value.parse().map_err(|_| CameraError::NotSupported)?;
    set_enum_index(sdk, cap_id, idx)
}

/// Sets an enum capability to the given option index: fetch the current enum to
/// obtain a valid struct (ulType / wPhysicalBytes), copy it out, free the SDK
/// buffers, point `ulValue` at the index, null `pData`, write it back. Mirrors
/// Nikon's sample.
fn set_enum_index(sdk: &Sdk, cap_id: u32, idx: u32) -> Result<(), CameraError> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err = unsafe { (sdk.get_capability)(cap_id, GET_SETTING_VALUE, &mut data, &mut data_type) };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_ENUM_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return Err(CameraError::NotSupported);
    }

    let mut en = unsafe {
        let en = *(data as *const NkMaidEnum);
        if !en.p_data.is_null() {
            free(en.p_data);
        }
        free(data);
        en
    };
    en.ul_value = idx;
    en.p_data = std::ptr::null_mut();

    let r = unsafe {
        (sdk.set_capability)(
            cap_id,
            &mut en as *mut NkMaidEnum as *mut c_void,
            DATATYPE_ENUM_PTR,
        )
    };
    if r == RESULT_NO_ERROR {
        Ok(())
    } else {
        Err(CameraError::SdkError(r as u32))
    }
}

/// Reads `CompressionLevel`'s option labels and current index (PackedString).
fn read_compression_options(sdk: &Sdk) -> Option<(Vec<String>, u32)> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err = unsafe {
        (sdk.get_capability)(
            CAP_COMPRESSION_LEVEL,
            GET_SETTING_SUPPORTED_VALUE_ARRAY,
            &mut data,
            &mut data_type,
        )
    };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_ENUM_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return None;
    }
    let res = unsafe {
        let en = &*(data as *const NkMaidEnum);
        let r = (en.ul_type == ARRAY_TYPE_PACKED_STRING)
            .then(|| (parse_packed_strings(en.p_data, en.ul_elements as usize), en.ul_value));
        if !en.p_data.is_null() {
            free(en.p_data);
        }
        r
    };
    unsafe { free(data) };
    res
}

/// Exposes `CompressionLevel` as a JPEG-only `ImageQuality` Select. Capture only
/// serves JPEG, so RAW / RAW+JPEG options are hidden. `current` reflects what
/// captures will use: the current setting if it is already JPEG, otherwise the
/// best JPEG option that capture would force.
fn read_image_quality(sdk: &Sdk) -> Option<CameraParameter> {
    let (labels, cur) = read_compression_options(sdk)?;

    let mut options = Vec::new();
    let mut best: Option<((u8, bool), usize)> = None;
    for (i, label) in labels.iter().enumerate() {
        if let Some(rank) = jpeg_label_rank(label) {
            options.push(ParameterOption { value: i.to_string(), label: label.clone() });
            if best.is_none_or(|(br, _)| rank < br) {
                best = Some((rank, i));
            }
        }
    }
    let best_idx = best?.1; // None → no JPEG option, hide the parameter
    if options.is_empty() {
        return None;
    }

    let cur_is_jpeg = labels
        .get(cur as usize)
        .is_some_and(|l| jpeg_label_rank(l).is_some());
    let current = if cur_is_jpeg { cur as usize } else { best_idx };

    Some(CameraParameter::Select {
        param_type: ParameterType::ImageQuality,
        current: current.to_string(),
        options,
        disabled: false,
    })
}

/// Ranks a `CompressionLevel`/`FileType` option label for JPEG-only capture.
/// Lower is better. `None` rejects the option (RAW/NEF/TIFF, or RAW+JPEG combos).
/// Prefers Fine > Normal > Basic, and the non-`*` ("optimal quality") variant.
fn jpeg_label_rank(label: &str) -> Option<(u8, bool)> {
    let l = label.trim().to_ascii_uppercase();
    // Must be JPEG-only: combos are listed as "RAW + JPEG …" and start with RAW.
    if !l.starts_with("JPEG") {
        return None;
    }
    let quality = if l.contains("FINE") {
        0
    } else if l.contains("NORMAL") {
        1
    } else if l.contains("BASIC") {
        2
    } else {
        3
    };
    Some((quality, l.ends_with('*')))
}

/// Reads an image-quality enum cap's options and returns the index of the best
/// JPEG-only option, if any.
fn find_jpeg_index(sdk: &Sdk, cap_id: u32) -> Option<u32> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err = unsafe {
        (sdk.get_capability)(cap_id, GET_SETTING_SUPPORTED_VALUE_ARRAY, &mut data, &mut data_type)
    };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_ENUM_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return None;
    }

    let labels = unsafe {
        let en = &*(data as *const NkMaidEnum);
        let labels = if en.ul_type == ARRAY_TYPE_PACKED_STRING {
            parse_packed_strings(en.p_data, en.ul_elements as usize)
        } else {
            Vec::new() // image quality is a PackedString on Z bodies
        };
        if !en.p_data.is_null() {
            free(en.p_data);
        }
        labels
    };
    unsafe { free(data) };

    labels
        .iter()
        .enumerate()
        .filter_map(|(i, l)| jpeg_label_rank(l).map(|rank| (rank, i as u32)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, i)| i)
}

/// Best-effort: switch a RAW body to a JPEG-only image quality before capture so
/// the transferred file is a JPEG. Tries `CompressionLevel` then `FileType`.
///
/// If the current `CompressionLevel` is already a JPEG-only option (e.g. the user
/// picked one via the `ImageQuality` parameter), it is left untouched so their
/// choice of Fine/Normal/Basic is respected.
fn ensure_jpeg_quality(sdk: &Sdk) {
    if let Some((labels, cur)) = read_compression_options(sdk) {
        if labels
            .get(cur as usize)
            .is_some_and(|l| jpeg_label_rank(l).is_some())
        {
            return; // already JPEG — keep the selected quality
        }
    }
    for cap in [CAP_COMPRESSION_LEVEL, CAP_FILE_TYPE] {
        if let Some(idx) = find_jpeg_index(sdk, cap) {
            if set_enum_index(sdk, cap, idx).is_ok() {
                return;
            }
        }
    }
    eprintln!(
        "[nikon] capture: no JPEG-only image-quality option found — capture may stay RAW \
         (send the logged options so the mapping can be fixed)"
    );
}

/// Sets a boolean capability (`BooleanPtr`).
fn set_bool_cap(sdk: &Sdk, cap_id: u32, on: bool) -> Result<(), CameraError> {
    let v: u8 = on as u8;
    let r = unsafe {
        (sdk.set_capability)(cap_id, &v as *const u8 as *mut c_void, DATATYPE_BOOLEAN_PTR)
    };
    if r == RESULT_NO_ERROR {
        Ok(())
    } else {
        Err(CameraError::SdkError(r as u32))
    }
}

/// Sets `ExposureComp` by its discrete step index (read-modify-write the range).
fn set_exposure_comp(sdk: &Sdk, idx: u32) -> Result<(), CameraError> {
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut data_type: i32 = 0;
    let err =
        unsafe { (sdk.get_capability)(CAP_EXPOSURE_COMP, GET_SETTING_VALUE, &mut data, &mut data_type) };
    if err != RESULT_NO_ERROR || data.is_null() || data_type != DATATYPE_RANGE_PTR {
        if !data.is_null() {
            unsafe { free(data) };
        }
        return Err(CameraError::NotSupported);
    }

    let result = unsafe {
        let r = &mut *(data as *mut NkMaidRange);
        if idx >= r.ul_steps {
            Err(CameraError::NotSupported)
        } else {
            r.ul_value_index = idx;
            let res = (sdk.set_capability)(CAP_EXPOSURE_COMP, data, DATATYPE_RANGE_PTR);
            if res == RESULT_NO_ERROR {
                Ok(())
            } else {
                Err(CameraError::SdkError(res as u32))
            }
        }
    };
    unsafe { free(data) };
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_id_roundtrip() {
        let id = make_native_id(3, "Nikon Z6");
        assert_eq!(id, "3|Nikon Z6");
        assert_eq!(parse_device_id(&id), Some(3));
    }

    #[test]
    fn parse_device_id_rejects_garbage() {
        assert_eq!(parse_device_id("notanumber|x"), None);
    }

    #[test]
    fn exposure_mode_labels() {
        assert_eq!(decode_exposure_mode(3), "Manual (M)");
        assert_eq!(decode_exposure_mode(0), "Program (P)");
        assert_eq!(decode_exposure_mode(17), "Effects");
        // Unknown codes fall back to the raw value.
        assert_eq!(decode_exposure_mode(99), "99");
    }

    #[test]
    fn aperture_labels() {
        assert_eq!(decode_aperture(560), "f/5.6");
        assert_eq!(decode_aperture(800), "f/8");
        assert_eq!(decode_aperture(1400), "f/14");
        // Implausible code falls back to raw.
        assert_eq!(decode_aperture(7), "7");
    }

    #[test]
    fn iso_labels() {
        assert_eq!(decode_iso(100), "ISO 100");
        assert_eq!(decode_iso(6400), "ISO 6400");
        assert_eq!(decode_iso(0), "0");
    }

    #[test]
    fn shutter_speed_labels() {
        assert_eq!(decode_shutter_speed(0x0001_00FA), "1/250"); // 1/250 s
        assert_eq!(decode_shutter_speed(0x0004_0001), "4\""); // 4 s
        assert_eq!(decode_shutter_speed(0x000D_000A), "1.3\""); // 13/10 s
        // num or den zero → raw.
        assert_eq!(decode_shutter_speed(0), "0");
    }

    #[test]
    fn packed_strings_parse() {
        // "A\0BB\0C\0" (trailing terminator dropped) → ["A","BB","C"].
        let bytes = b"A\0BB\0C\0";
        let v = unsafe { parse_packed_strings(bytes.as_ptr() as *const c_void, bytes.len()) };
        assert_eq!(v, vec!["A", "BB", "C"]);
    }

    #[test]
    fn jpeg_label_ranking() {
        assert_eq!(jpeg_label_rank("JPEG Fine"), Some((0, false)));
        assert_eq!(jpeg_label_rank("JPEG Fine*"), Some((0, true)));
        assert_eq!(jpeg_label_rank("JPEG Normal"), Some((1, false)));
        assert_eq!(jpeg_label_rank("JPEG Basic*"), Some((2, true)));
        // RAW-containing options are rejected (capture is JPEG-only).
        assert_eq!(jpeg_label_rank("RAW"), None);
        assert_eq!(jpeg_label_rank("RAW + JPEG Fine*"), None);
    }

    #[test]
    fn jpeg_index_picks_best_from_real_z5ii_list() {
        // The CompressionLevel option list reported by a Z5II (in order).
        let labels = [
            "RAW + JPEG Fine*", "RAW + JPEG Fine", "RAW + JPEG Normal*", "RAW + JPEG Normal",
            "RAW + JPEG Basic*", "RAW + JPEG Basic", "RAW", "JPEG Fine*", "JPEG Fine",
            "JPEG Normal*", "JPEG Normal", "JPEG Basic*", "JPEG Basic",
        ];
        let best = labels
            .iter()
            .enumerate()
            .filter_map(|(i, l)| jpeg_label_rank(l).map(|r| (r, i)))
            .min_by_key(|(r, _)| *r)
            .map(|(_, i)| i);
        // "JPEG Fine" (optimal quality, no '*') at index 8.
        assert_eq!(best, Some(8));
    }

    #[test]
    fn nikon_range_layout() {
        // Guards against accidental field reordering of the FFI range struct.
        assert_eq!(std::mem::offset_of!(NkMaidRange, ul_value_index), 16);
        assert_eq!(std::mem::offset_of!(NkMaidRange, ul_steps), 40);
    }

    #[test]
    fn live_view_data_layout() {
        // pImageData must land at offset 896 (header 884 + 8 prefix, padded to
        // the pointer's 8-byte alignment). A mismatch means a wrong header size.
        assert_eq!(
            std::mem::offset_of!(NkMaidLiveViewData, p_image_data),
            896
        );
    }
}
