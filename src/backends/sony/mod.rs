//! Sony camera backend over the Camera Remote SDK (CrSDK).
//!
//! The CrSDK is C++ with asynchronous callbacks, so all SDK access goes through a
//! small C++ shim (`bridge.cpp`) that exposes a flat, synchronous C API. This
//! module drives that shim from a single dedicated OS thread (the "sony-sdk"
//! actor) — mirroring the Canon backend — because the SDK holds process-global
//! state and its session handles must not be touched concurrently.
//!
//! Property values cross the FFI as raw SDK integers; the human-readable labels
//! (f/5.6, 1/500, ISO 400, …) are decoded here, the same split as the Canon
//! backend's decode tables.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc;
use std::time::Duration;

use crate::camera::{
    dedup_key, CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

// ---------------------------------------------------------------------------
// C bridge constants / types — must match bridge.h
// ---------------------------------------------------------------------------

const SN_MAX_MODEL: usize = 128;
const SN_MAX_ID: usize = 256;
const SN_MAX_CONN: usize = 32;
const SN_MAX_DEVICES: usize = 32;
const SN_MAX_PARAMS: usize = 256;
const SN_MAX_OPTIONS: usize = 512;

const SN_OK: c_int = 0;
const SN_NOT_READY: c_int = 2;

/// Sony's USB vendor id — used to build the cross-backend dedup key so the same
/// body seen by gphoto2 (PTP) is recognised as this SDK's device.
const SONY_VENDOR_ID: u16 = 0x054c;

#[repr(C)]
struct SnDeviceInfo {
    model: [c_char; SN_MAX_MODEL],
    id: [c_char; SN_MAX_ID],
    conn_type: [c_char; SN_MAX_CONN],
}

#[repr(C)]
struct SnParam {
    code: u32,
    current: u64,
    writable: i32,
    value_type: u32,
    num_options: i32,
    options: [i64; SN_MAX_OPTIONS],
}

extern "C" {
    fn sn_init() -> c_int;
    fn sn_release();
    fn sn_list_devices(out: *mut SnDeviceInfo, capacity: c_int) -> c_int;
    fn sn_connect(native_id: *const c_char) -> *mut c_void;
    fn sn_disconnect(cam: *mut c_void);
    fn sn_get_parameters(cam: *mut c_void, out: *mut SnParam, capacity: c_int) -> c_int;
    fn sn_set_parameter(cam: *mut c_void, code: u32, value: u64) -> c_int;
    fn sn_get_live_view(cam: *mut c_void, out: *mut *mut u8, size: *mut u32) -> c_int;
    fn sn_capture(cam: *mut c_void, out: *mut *mut u8, size: *mut u32) -> c_int;
    fn sn_free(p: *mut u8);
}

// ---------------------------------------------------------------------------
// Sony property codes (CrDevicePropertyCode) we expose
// ---------------------------------------------------------------------------

const CODE_FNUMBER: u32 = 0x0100;
const CODE_EXPOSURE_BIAS: u32 = 0x0101;
const CODE_SHUTTER_SPEED: u32 = 0x0103;
const CODE_ISO: u32 = 0x0104;
const CODE_STILL_QUALITY: u32 = 0x0107;
const CODE_WHITE_BALANCE: u32 = 0x0108;
const CODE_FOCUS_MODE: u32 = 0x0109;

fn code_to_param_type(code: u32) -> Option<ParameterType> {
    match code {
        CODE_FNUMBER => Some(ParameterType::Aperture),
        CODE_EXPOSURE_BIAS => Some(ParameterType::ExposureCompensation),
        CODE_SHUTTER_SPEED => Some(ParameterType::ShutterSpeed),
        CODE_ISO => Some(ParameterType::Iso),
        CODE_STILL_QUALITY => Some(ParameterType::ImageQuality),
        CODE_WHITE_BALANCE => Some(ParameterType::WhiteBalance),
        CODE_FOCUS_MODE => Some(ParameterType::FocusMode),
        _ => None,
    }
}

fn param_type_to_code(pt: ParameterType) -> Option<u32> {
    match pt {
        ParameterType::Aperture => Some(CODE_FNUMBER),
        ParameterType::ExposureCompensation => Some(CODE_EXPOSURE_BIAS),
        ParameterType::ShutterSpeed => Some(CODE_SHUTTER_SPEED),
        ParameterType::Iso => Some(CODE_ISO),
        ParameterType::ImageQuality => Some(CODE_STILL_QUALITY),
        ParameterType::WhiteBalance => Some(CODE_WHITE_BALANCE),
        ParameterType::FocusMode => Some(CODE_FOCUS_MODE),
        _ => None,
    }
}

/// Value parameters (numerically ordered) render as `range_select`; enum choices
/// render as `select`.
fn is_range_select(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::Aperture
            | ParameterType::ShutterSpeed
            | ParameterType::Iso
            | ParameterType::ExposureCompensation
    )
}

// ---------------------------------------------------------------------------
// Label decoding (raw SDK code → human label)
// ---------------------------------------------------------------------------

/// Byte width of one element for a CrDataType (0 = unsupported / string).
fn element_width(value_type: u32) -> u32 {
    match value_type & 0x000F {
        0x0001 => 1,
        0x0002 => 2,
        0x0003 => 4,
        0x0004 => 8,
        _ => 0,
    }
}

fn decode_label(pt: ParameterType, raw: u64) -> String {
    match pt {
        ParameterType::Aperture => fmt_aperture(raw),
        ParameterType::ShutterSpeed => fmt_shutter(raw),
        ParameterType::Iso => fmt_iso(raw),
        ParameterType::ExposureCompensation => fmt_exposure_comp(raw),
        ParameterType::WhiteBalance => fmt_white_balance(raw),
        ParameterType::FocusMode => fmt_focus_mode(raw),
        ParameterType::ImageQuality => fmt_image_quality(raw),
        _ => raw.to_string(),
    }
}

/// FNumber: raw = f-number × 100.
fn fmt_aperture(raw: u64) -> String {
    let f = raw as f64 / 100.0;
    if (f.fract()).abs() < 0.05 {
        format!("f/{}", f.round() as i64)
    } else {
        format!("f/{f:.1}")
    }
}

/// ShutterSpeed: upper 16 bits numerator, lower 16 bits denominator; 0 = Bulb.
fn fmt_shutter(raw: u64) -> String {
    let u = raw as u32;
    if u == 0 {
        return "Bulb".to_string();
    }
    if u == 0xFFFF_FFFF {
        return "—".to_string();
    }
    let num = (u >> 16) & 0xFFFF;
    let den = u & 0xFFFF;
    if den == 0 || num == 0 {
        return "Bulb".to_string();
    }
    if num >= den {
        if num.is_multiple_of(den) {
            format!("{}\"", num / den)
        } else {
            format!("{:.1}\"", num as f64 / den as f64)
        }
    } else {
        format!("1/{}", (den as f64 / num as f64).round() as u32)
    }
}

/// IsoSensitivity: bits 0-23 ISO value, bits 24-27 mode, bits 28-31 extension;
/// 0xFFFFFF = AUTO.
fn fmt_iso(raw: u64) -> String {
    let u = raw as u32;
    let iso = u & 0x00FF_FFFF;
    let ext = (u >> 28) & 0xF;
    if iso == 0x00FF_FFFF {
        return "ISO AUTO".to_string();
    }
    if ext != 0 {
        format!("ISO {iso} (ext)")
    } else {
        format!("ISO {iso}")
    }
}

/// ExposureBiasCompensation: signed 16-bit, value = EV × 1000.
fn fmt_exposure_comp(raw: u64) -> String {
    let ev = (raw as u16) as i16 as f64 / 1000.0;
    format!("{ev:+.1} EV")
}

fn fmt_white_balance(raw: u64) -> String {
    let label = match raw as u16 {
        0x0000 => "Auto",
        0x0001 => "Underwater Auto",
        0x0011 => "Daylight",
        0x0012 => "Shade",
        0x0013 => "Cloudy",
        0x0014 => "Incandescent",
        0x0020 => "Fluorescent",
        0x0021 => "Fluorescent: Warm White",
        0x0022 => "Fluorescent: Cool White",
        0x0023 => "Fluorescent: Day White",
        0x0024 => "Fluorescent: Daylight",
        0x0030 => "Flash",
        0x0100 => "Color Temperature",
        0x0101 => "Custom 1",
        0x0102 => "Custom 2",
        0x0103 => "Custom 3",
        0x0104 => "Custom",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

fn fmt_focus_mode(raw: u64) -> String {
    let label = match raw as u16 {
        0x0001 => "Manual",
        0x0002 => "AF-S",
        0x0003 => "AF-C",
        0x0004 => "AF-A",
        0x0005 => "AF-D",
        0x0006 => "DMF",
        0x0007 => "PF",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

fn fmt_image_quality(raw: u64) -> String {
    let label = match raw as u16 {
        0x0001 => "Light",
        0x0002 => "Standard",
        0x0003 => "Fine",
        0x0004 => "Extra Fine",
        _ => return format!("0x{:04X}", raw as u16),
    };
    label.to_string()
}

// ---------------------------------------------------------------------------
// Actor commands
// ---------------------------------------------------------------------------

enum Command {
    ListDevices {
        reply: mpsc::Sender<Result<Vec<DeviceInfo>, CameraError>>,
    },
    Connect {
        device_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    Disconnect {
        device_id: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    IsConnected {
        device_id: String,
        reply: mpsc::Sender<bool>,
    },
    GetParameters {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<CameraParameter>, CameraError>>,
    },
    SetParameter {
        device_id: String,
        param_type: ParameterType,
        value: String,
        reply: mpsc::Sender<Result<(), CameraError>>,
    },
    GetLiveViewFrame {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    CapturePhoto {
        device_id: String,
        reply: mpsc::Sender<Result<Vec<u8>, CameraError>>,
    },
    PrepareExit {
        ack: mpsc::Sender<()>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct SonyBackend {
    tx: mpsc::Sender<Command>,
}

impl SonyBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (tx, rx) = mpsc::channel::<Command>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CameraError>>();

        std::thread::Builder::new()
            .name("sony-sdk".to_string())
            .spawn(move || actor_thread(rx, init_tx))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        init_rx
            .recv()
            .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))?;

        Ok(Self { tx })
    }

    fn call<T>(&self, make: impl FnOnce(mpsc::Sender<T>) -> Command, on_err: T) -> T {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.tx.send(make(reply_tx)).is_err() {
            return on_err;
        }
        reply_rx.recv().unwrap_or(on_err)
    }
}

impl Drop for SonyBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for SonyBackend {
    fn backend_id(&self) -> &str {
        "sony"
    }

    /// Above the generic backends (like Canon's EDSDK): the Sony SDK gives native
    /// live view and the full parameter set, so it wins dedup over gphoto2 for the
    /// same body.
    fn dedup_priority(&self) -> i32 {
        10
    }

    /// Releases the SDK before the process exits: closes all sessions and calls
    /// Release on the SDK thread, bounded so an abrupt Ctrl-C never hangs.
    fn shutdown(&self) {
        let (ack_tx, ack_rx) = mpsc::channel();
        if self.tx.send(Command::PrepareExit { ack: ack_tx }).is_ok() {
            let _ = ack_rx.recv_timeout(Duration::from_secs(3));
        }
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        self.call(
            |reply| Command::ListDevices { reply },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Connect {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        self.call(
            |reply| Command::Disconnect {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.call(
            |reply| Command::IsConnected {
                device_id: native_id.to_string(),
                reply,
            },
            false,
        )
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        self.call(
            |reply| Command::GetParameters {
                device_id: native_id.to_string(),
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
                device_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::GetLiveViewFrame {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.call(
            |reply| Command::CapturePhoto {
                device_id: native_id.to_string(),
                reply,
            },
            Err(CameraError::SdkError(0xFFFF_FFFF)),
        )
    }
}

// ---------------------------------------------------------------------------
// Actor thread — the only place SDK / bridge calls happen
// ---------------------------------------------------------------------------

/// An open session handle. Raw pointers live exclusively on the SDK thread.
struct SessionHandle(*mut c_void);
unsafe impl Send for SessionHandle {}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        unsafe { sn_disconnect(self.0) };
    }
}

fn actor_thread(rx: mpsc::Receiver<Command>, init_tx: mpsc::Sender<Result<(), CameraError>>) {
    let init = unsafe { sn_init() };
    if init != SN_OK {
        let _ = init_tx.send(Err(CameraError::SdkError(0xFFFF_FFFF)));
        return;
    }
    let _ = init_tx.send(Ok(()));

    let mut sessions: HashMap<String, SessionHandle> = HashMap::new();

    loop {
        match rx.recv() {
            Ok(Command::ListDevices { reply }) => {
                let _ = reply.send(list_devices_impl(&sessions));
            }
            Ok(Command::IsConnected { device_id, reply }) => {
                let _ = reply.send(sessions.contains_key(&device_id));
            }
            Ok(Command::Connect { device_id, reply }) => {
                let _ = reply.send(connect_impl(&device_id, &mut sessions));
            }
            Ok(Command::Disconnect { device_id, reply }) => {
                let _ = reply.send(disconnect_impl(&device_id, &mut sessions));
            }
            Ok(Command::GetParameters { device_id, reply }) => {
                let _ = reply.send(get_parameters_impl(&device_id, &sessions));
            }
            Ok(Command::SetParameter {
                device_id,
                param_type,
                value,
                reply,
            }) => {
                let _ = reply.send(set_parameter_impl(
                    &device_id, param_type, &value, &sessions,
                ));
            }
            Ok(Command::GetLiveViewFrame { device_id, reply }) => {
                let _ = reply.send(get_live_view_impl(&device_id, &sessions));
            }
            Ok(Command::CapturePhoto { device_id, reply }) => {
                let _ = reply.send(capture_photo_impl(&device_id, &sessions));
            }
            // Release everything here (SDK calls are only valid on this thread),
            // ack, then return so the process can exit cleanly.
            Ok(Command::PrepareExit { ack }) => {
                sessions.clear(); // SessionHandle::drop closes each session
                unsafe { sn_release() };
                let _ = ack.send(());
                return;
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }

    sessions.clear();
    unsafe { sn_release() };
}

// ---------------------------------------------------------------------------
// Implementations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn list_devices_impl(
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<DeviceInfo>, CameraError> {
    let mut buf: Vec<SnDeviceInfo> = (0..SN_MAX_DEVICES)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();

    let count = unsafe { sn_list_devices(buf.as_mut_ptr(), SN_MAX_DEVICES as c_int) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let devices = buf
        .iter()
        .map(|d| {
            let native_id = cstr(&d.id);
            let model = cstr(&d.model);
            let id = DeviceId::new("sony", &native_id).encode();
            let connected = sessions.contains_key(&native_id);
            DeviceInfo {
                id,
                name: format!("Sony {model}"),
                connected,
                dedup_key: Some(dedup_key(SONY_VENDOR_ID, &model)),
            }
        })
        .collect();

    Ok(devices)
}

fn connect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    if sessions.contains_key(device_id) {
        return Ok(()); // idempotent
    }
    let c_id = CString::new(device_id).map_err(|_| CameraError::InvalidDeviceId)?;
    let handle = unsafe { sn_connect(c_id.as_ptr()) };
    if handle.is_null() {
        return Err(CameraError::DeviceNotFound(device_id.to_string()));
    }
    sessions.insert(device_id.to_string(), SessionHandle(handle));
    Ok(())
}

fn disconnect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    sessions
        .remove(device_id)
        .ok_or_else(|| CameraError::DeviceNotFound(device_id.to_string()))?;
    // SessionHandle::drop calls sn_disconnect.
    Ok(())
}

fn get_parameters_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut buf: Vec<SnParam> = (0..SN_MAX_PARAMS)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    let count = unsafe { sn_get_parameters(handle, buf.as_mut_ptr(), SN_MAX_PARAMS as c_int) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let params = buf.iter().filter_map(build_parameter).collect();
    Ok(params)
}

/// Turns one raw SDK property into a CameraParameter, or `None` if we don't map
/// the code or it has fewer than two selectable options (no real choice).
fn build_parameter(p: &SnParam) -> Option<CameraParameter> {
    let param_type = code_to_param_type(p.code)?;
    if p.num_options < 2 {
        return None;
    }

    let width = element_width(p.value_type);
    let mask = if width == 0 || width >= 8 {
        u64::MAX
    } else {
        (1u64 << (8 * width)) - 1
    };

    let options: Vec<ParameterOption> = p.options[..p.num_options as usize]
        .iter()
        .map(|&raw| {
            let v = raw as u64 & mask;
            ParameterOption {
                label: decode_label(param_type, v),
                value: v.to_string(),
            }
        })
        .collect();

    let current = (p.current & mask).to_string();
    let disabled = p.writable == 0;

    Some(if is_range_select(param_type) {
        CameraParameter::RangeSelect {
            param_type,
            current,
            options,
            disabled,
        }
    } else {
        CameraParameter::Select {
            param_type,
            current,
            options,
            disabled,
        }
    })
}

fn set_parameter_impl(
    device_id: &str,
    param_type: ParameterType,
    value: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;
    let code = param_type_to_code(param_type).ok_or(CameraError::NotSupported)?;
    let raw: u64 = value.parse().map_err(|_| CameraError::NotSupported)?;

    let ret = unsafe { sn_set_parameter(handle, code, raw) };
    if ret != SN_OK {
        return Err(CameraError::SdkError(ret as u32));
    }
    Ok(())
}

fn get_live_view_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size: u32 = 0;
    let ret = unsafe { sn_get_live_view(handle, &mut data_ptr, &mut size) };

    if ret == SN_NOT_READY {
        // Same "not ready" convention as the other backends.
        return Err(CameraError::SdkError(0x0000_A102));
    }
    if ret != SN_OK || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFE));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size as usize).to_vec() };
    unsafe { sn_free(data_ptr) };
    Ok(bytes)
}

fn capture_photo_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size: u32 = 0;
    let ret = unsafe { sn_capture(handle, &mut data_ptr, &mut size) };

    if ret != SN_OK || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFD));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size as usize).to_vec() };
    unsafe { sn_free(data_ptr) };
    Ok(bytes)
}

/// Reads a NUL-terminated C char array into an owned String (lossy UTF-8).
fn cstr(buf: &[c_char]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aperture_labels() {
        assert_eq!(fmt_aperture(280), "f/2.8");
        assert_eq!(fmt_aperture(800), "f/8");
        assert_eq!(fmt_aperture(180), "f/1.8");
        assert_eq!(fmt_aperture(560), "f/5.6");
    }

    #[test]
    fn shutter_labels() {
        // 1/500 → num=1, den=500
        assert_eq!(fmt_shutter((1 << 16) | 500), "1/500");
        // 2" → num=2, den=1
        assert_eq!(fmt_shutter((2 << 16) | 1), "2\"");
        // 1.3" → num=13, den=10
        assert_eq!(fmt_shutter((13 << 16) | 10), "1.3\"");
        assert_eq!(fmt_shutter(0), "Bulb");
    }

    #[test]
    fn iso_labels() {
        assert_eq!(fmt_iso(400), "ISO 400");
        assert_eq!(fmt_iso(0x00FF_FFFF), "ISO AUTO");
    }

    #[test]
    fn exposure_comp_labels() {
        assert_eq!(fmt_exposure_comp(300), "+0.3 EV");
        // -1.0 EV = -1000 stored as signed 16-bit
        assert_eq!(fmt_exposure_comp((-1000i16) as u16 as u64), "-1.0 EV");
        assert_eq!(fmt_exposure_comp(0), "+0.0 EV");
    }

    #[test]
    fn enum_labels() {
        assert_eq!(fmt_white_balance(0x0000), "Auto");
        assert_eq!(fmt_white_balance(0x0011), "Daylight");
        assert_eq!(fmt_focus_mode(0x0002), "AF-S");
        assert_eq!(fmt_image_quality(0x0003), "Fine");
    }

    #[test]
    fn code_mapping_roundtrip() {
        for pt in [
            ParameterType::Aperture,
            ParameterType::ShutterSpeed,
            ParameterType::Iso,
            ParameterType::ExposureCompensation,
            ParameterType::WhiteBalance,
            ParameterType::FocusMode,
            ParameterType::ImageQuality,
        ] {
            let code = param_type_to_code(pt).unwrap();
            assert_eq!(code_to_param_type(code), Some(pt));
        }
    }
}
