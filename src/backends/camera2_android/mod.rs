use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io::Cursor;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::mpsc;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo,
    ParameterOption, ParameterType,
};

// ---------------------------------------------------------------------------
// C bridge constants — must match bridge.h
// ---------------------------------------------------------------------------

const AC_MAX_STR: usize = 256;
const AC_MAX_DEVICES: usize = 16;
const AC_MAX_PARAMS: usize = 32;
const AC_MAX_OPTIONS: usize = 64;
const AC_MAX_KIND: usize = 64;
const AC_MAX_LABEL: usize = 64;

// ---------------------------------------------------------------------------
// C bridge types
// ---------------------------------------------------------------------------

#[repr(C)]
struct AcDeviceInfo {
    camera_id: [c_char; AC_MAX_STR],
    name:      [c_char; AC_MAX_STR],
}

#[repr(C)]
struct AcParamOption {
    value: i32,
    label: [c_char; AC_MAX_LABEL],
}

#[repr(C)]
struct AcParamDesc {
    kind:        [c_char; AC_MAX_KIND],
    current:     i32,
    is_range:    i32,
    min:         i32,
    max:         i32,
    step:        i32,
    num_options: i32,
    options:     [AcParamOption; AC_MAX_OPTIONS],
}

// ---------------------------------------------------------------------------
// C bridge FFI
// ---------------------------------------------------------------------------

extern "C" {
    fn ac_list_devices(out: *mut AcDeviceInfo, capacity: c_int) -> c_int;
    fn ac_open_session(camera_id: *const c_char) -> *mut c_void;
    fn ac_close_session(handle: *mut c_void);
    fn ac_capture_frame(
        handle:     *mut c_void,
        out_data:   *mut *mut u8,
        out_size:   *mut usize,
        out_width:  *mut i32,
        out_height: *mut i32,
    ) -> c_int;
    fn ac_capture_photo(
        handle:     *mut c_void,
        out_data:   *mut *mut u8,
        out_size:   *mut usize,
        out_width:  *mut i32,
        out_height: *mut i32,
        out_is_jpeg: *mut i32,
    ) -> c_int;
    fn ac_free_frame(data: *mut u8);
    fn ac_get_parameters(handle: *mut c_void, out: *mut AcParamDesc, capacity: c_int) -> c_int;
    fn ac_set_parameter(handle: *mut c_void, kind: *const c_char, value: i32) -> c_int;
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
    Shutdown,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct Camera2AndroidBackend {
    tx: mpsc::Sender<Command>,
}

impl Camera2AndroidBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (tx, rx) = mpsc::channel::<Command>();

        std::thread::Builder::new()
            .name("camera2-android".to_string())
            .spawn(move || actor_thread(rx))
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;

        Ok(Self { tx })
    }
}

impl Drop for Camera2AndroidBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for Camera2AndroidBackend {
    fn backend_id(&self) -> &str {
        "camera2-android"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::ListDevices { reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Connect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(Command::IsConnected {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetParameters {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::SetParameter {
                device_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetLiveViewFrame {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::CapturePhoto {
                device_id: native_id.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }
}

// ---------------------------------------------------------------------------
// Actor thread
// ---------------------------------------------------------------------------

struct SessionHandle(*mut c_void);
unsafe impl Send for SessionHandle {}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        unsafe { ac_close_session(self.0) };
    }
}

fn actor_thread(rx: mpsc::Receiver<Command>) {
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
            Ok(Command::SetParameter { device_id, param_type, value, reply }) => {
                let _ = reply.send(set_parameter_impl(&device_id, param_type, &value, &sessions));
            }
            Ok(Command::GetLiveViewFrame { device_id, reply }) => {
                let _ = reply.send(capture_frame_impl(&device_id, &sessions));
            }
            Ok(Command::CapturePhoto { device_id, reply }) => {
                let _ = reply.send(capture_photo_impl(&device_id, &sessions));
            }
            Ok(Command::Shutdown) | Err(_) => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge wrappers
// ---------------------------------------------------------------------------

fn list_devices_impl(sessions: &HashMap<String, SessionHandle>) -> Result<Vec<DeviceInfo>, CameraError> {
    let mut buf: Vec<AcDeviceInfo> = (0..AC_MAX_DEVICES)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();
    let count = unsafe { ac_list_devices(buf.as_mut_ptr(), AC_MAX_DEVICES as c_int) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let devices = buf
        .iter()
        .map(|d| {
            let native_id = unsafe { CStr::from_ptr(d.camera_id.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let name = unsafe { CStr::from_ptr(d.name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let id = DeviceId::new("camera2-android", &native_id).encode();
            let connected = sessions.contains_key(&native_id);
            DeviceInfo { id, name, connected, dedup_key: None }
        })
        .collect();

    Ok(devices)
}

fn connect_impl(
    device_id: &str,
    sessions: &mut HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    if sessions.contains_key(device_id) {
        return Ok(());
    }

    let c_id = CString::new(device_id).map_err(|_| CameraError::InvalidDeviceId)?;
    let handle = unsafe { ac_open_session(c_id.as_ptr()) };

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
    Ok(())
}

fn get_parameters_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut buf: Vec<AcParamDesc> = (0..AC_MAX_PARAMS)
        .map(|_| unsafe { std::mem::zeroed() })
        .collect();

    let count = unsafe { ac_get_parameters(handle, buf.as_mut_ptr(), AC_MAX_PARAMS as c_int) };
    if count < 0 {
        return Err(CameraError::SdkError(0xFFFF_FFFF));
    }
    buf.truncate(count as usize);

    let mut params: Vec<CameraParameter> = buf
        .iter()
        .filter_map(|d| {
            let c_kind = unsafe { CStr::from_ptr(d.kind.as_ptr()) }.to_string_lossy();
            let param_type = c_kind_to_param_type(&c_kind)?;

            if is_boolean_param(param_type) {
                return Some(CameraParameter::Boolean {
                    param_type,
                    current:  d.current != 0,
                    disabled: false,
                });
            }

            if d.is_range != 0 {
                Some(CameraParameter::Range {
                    param_type,
                    current: d.current,
                    min:     d.min,
                    max:     d.max,
                    step:    if d.step > 0 { d.step } else { 1 },
                    disabled: false,
                })
            } else {
                let num_opts = d.num_options as usize;
                let options: Vec<ParameterOption> = d.options[..num_opts]
                    .iter()
                    .map(|o| {
                        let label = unsafe { CStr::from_ptr(o.label.as_ptr()) }
                            .to_string_lossy()
                            .into_owned();
                        ParameterOption { label, value: o.value.to_string() }
                    })
                    .collect();
                if param_type == ParameterType::ShutterSpeed {
                    Some(CameraParameter::RangeSelect {
                        param_type,
                        current: d.current.to_string(),
                        options,
                        disabled: false,
                    })
                } else {
                    Some(CameraParameter::Select {
                        param_type,
                        current: d.current.to_string(),
                        options,
                        disabled: false,
                    })
                }
            }
        })
        .collect();

    let iso_auto_on = params.iter().any(|p| {
        matches!(p, CameraParameter::Boolean { param_type: ParameterType::IsoAuto, current: true, .. })
    });
    let shutter_auto_on = params.iter().any(|p| {
        matches!(p, CameraParameter::Boolean { param_type: ParameterType::ShutterSpeedAuto, current: true, .. })
    });
    let wb_auto_on = params.iter().any(|p| {
        matches!(p, CameraParameter::Boolean { param_type: ParameterType::WhiteBalanceAuto, current: true, .. })
    });
    let focus_auto_on = params.iter().any(|p| {
        matches!(p, CameraParameter::Boolean { param_type: ParameterType::FocusAuto, current: true, .. })
    });

    for p in &mut params {
        match p {
            CameraParameter::Range    { param_type: ParameterType::Iso,          disabled, .. }
            | CameraParameter::Select { param_type: ParameterType::Iso,          disabled, .. } => {
                *disabled = iso_auto_on;
            }
            CameraParameter::RangeSelect { param_type: ParameterType::ShutterSpeed, disabled, .. } => {
                *disabled = shutter_auto_on;
            }
            CameraParameter::Range    { param_type: ParameterType::WhiteBalance, disabled, .. } => {
                *disabled = wb_auto_on;
            }
            CameraParameter::Range    { param_type: ParameterType::Focus,        disabled, .. } => {
                *disabled = focus_auto_on;
            }
            _ => {}
        }
    }

    Ok(params)
}

fn set_parameter_impl(
    device_id: &str,
    param_type: ParameterType,
    value: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<(), CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;
    let c_kind = param_type_to_c_kind(param_type).ok_or(CameraError::NotSupported)?;
    let c_kind = CString::new(c_kind).map_err(|_| CameraError::NotSupported)?;

    let int_val: i32 = match value {
        "true"  => 1,
        "false" => 0,
        v       => v.parse().map_err(|_| CameraError::NotSupported)?,
    };

    let ret = unsafe { ac_set_parameter(handle, c_kind.as_ptr(), int_val) };
    if ret != 0 { Err(CameraError::NotSupported) } else { Ok(()) }
}

fn capture_frame_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size:   usize = 0;
    let mut width:  i32   = 0;
    let mut height: i32   = 0;

    let ret = unsafe { ac_capture_frame(handle, &mut data_ptr, &mut size, &mut width, &mut height) };
    if ret != 0 || data_ptr.is_null() {
        // Reuse the Canon "not ready" code so the capture loop skips this frame
        // instead of breaking the stream (frames may be temporarily unavailable).
        return Err(CameraError::SdkError(0x0000_A102));
    }

    let rgb = unsafe { std::slice::from_raw_parts(data_ptr, size).to_vec() };
    unsafe { ac_free_frame(data_ptr) };

    rgb24_to_jpeg(rgb, width as u32, height as u32)
}

fn capture_photo_impl(
    device_id: &str,
    sessions: &HashMap<String, SessionHandle>,
) -> Result<Vec<u8>, CameraError> {
    let handle = sessions.get(device_id).ok_or(CameraError::NotConnected)?.0;

    let mut data_ptr: *mut u8 = std::ptr::null_mut();
    let mut size:    usize = 0;
    let mut width:   i32   = 0;
    let mut height:  i32   = 0;
    let mut is_jpeg: i32   = 0;

    let ret = unsafe {
        ac_capture_photo(handle, &mut data_ptr, &mut size, &mut width, &mut height, &mut is_jpeg)
    };
    if ret != 0 || data_ptr.is_null() {
        return Err(CameraError::SdkError(0xFFFF_FFFD));
    }

    let bytes = unsafe { std::slice::from_raw_parts(data_ptr, size).to_vec() };
    unsafe { ac_free_frame(data_ptr) };

    if is_jpeg != 0 {
        Ok(bytes)
    } else {
        rgb24_to_jpeg(bytes, width as u32, height as u32)
    }
}

// ---------------------------------------------------------------------------
// JPEG encoding
// ---------------------------------------------------------------------------

fn rgb24_to_jpeg(rgb: Vec<u8>, width: u32, height: u32) -> Result<Vec<u8>, CameraError> {
    let img = image::RgbImage::from_raw(width, height, rgb)
        .ok_or(CameraError::SdkError(0xDEAD_0001))?;
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Jpeg)
        .map_err(|_| CameraError::SdkError(0xDEAD_0002))?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Parameter type mappings
// ---------------------------------------------------------------------------

fn c_kind_to_param_type(kind: &str) -> Option<ParameterType> {
    match kind {
        "iso_auto"              => Some(ParameterType::IsoAuto),
        "shutter_auto"          => Some(ParameterType::ShutterSpeedAuto),
        "wb_auto"               => Some(ParameterType::WhiteBalanceAuto),
        "focus_auto"            => Some(ParameterType::FocusAuto),
        "iso"                   => Some(ParameterType::Iso),
        "shutter_us"            => Some(ParameterType::ShutterSpeed),
        "aperture"              => Some(ParameterType::Aperture),
        "color_temperature"     => Some(ParameterType::WhiteBalance),
        "ev_compensation"       => Some(ParameterType::ExposureCompensation),
        "focus_distance_x100"   => Some(ParameterType::Focus),
        "zoom_x100"             => Some(ParameterType::Zoom),
        "photo_resolution"      => Some(ParameterType::PhotoResolution),
        _ => None,
    }
}

fn param_type_to_c_kind(pt: ParameterType) -> Option<&'static str> {
    match pt {
        ParameterType::IsoAuto              => Some("iso_auto"),
        ParameterType::ShutterSpeedAuto     => Some("shutter_auto"),
        ParameterType::WhiteBalanceAuto     => Some("wb_auto"),
        ParameterType::FocusAuto            => Some("focus_auto"),
        ParameterType::Iso                  => Some("iso"),
        ParameterType::ShutterSpeed         => Some("shutter_us"),
        ParameterType::Aperture             => Some("aperture"),
        ParameterType::WhiteBalance          => Some("color_temperature"),
        ParameterType::ExposureCompensation => Some("ev_compensation"),
        ParameterType::Focus                => Some("focus_distance_x100"),
        ParameterType::Zoom                 => Some("zoom_x100"),
        ParameterType::PhotoResolution      => Some("photo_resolution"),
        _ => None,
    }
}

fn is_boolean_param(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::IsoAuto
            | ParameterType::ShutterSpeedAuto
            | ParameterType::WhiteBalanceAuto
            | ParameterType::FocusAuto
    )
}
