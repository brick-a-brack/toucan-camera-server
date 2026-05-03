use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::context;
use v4l::control::{self, MenuItem};
use v4l::format::FourCC;
use v4l::framesize::FrameSizeEnum;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

/// Pixel format we negotiate with the camera. Most modern UVC webcams expose
/// MJPG natively, which lets us stream JPEG frames straight out of the kernel
/// buffer without any encoding step on our side.
const MJPG: [u8; 4] = *b"MJPG";

/// Default resolution we try at connect time. The user can change it later via
/// the `VideoFormat` parameter.
const DEFAULT_WIDTH:  u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const BUFFER_COUNT:   u32 = 4;

/// Block at most this long when waiting for a frame from the kernel queue.
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// V4L2 control IDs — from linux/videodev2.h
// ---------------------------------------------------------------------------
const CID_BRIGHTNESS:             u32 = 0x00980900;
const CID_CONTRAST:               u32 = 0x00980901;
const CID_SATURATION:             u32 = 0x00980902;
const CID_HUE:                    u32 = 0x00980903;
const CID_AUTO_WHITE_BALANCE:     u32 = 0x0098090c;
const CID_GAMMA:                  u32 = 0x00980910;
const CID_GAIN:                   u32 = 0x00980913;
const CID_HUE_AUTO:               u32 = 0x00980919;
const CID_WHITE_BALANCE_TEMP:     u32 = 0x0098091a;
const CID_SHARPNESS:              u32 = 0x0098091b;
const CID_BACKLIGHT_COMPENSATION: u32 = 0x0098091c;
const CID_POWER_LINE_FREQUENCY:   u32 = 0x00980918;
const CID_EXPOSURE_AUTO:          u32 = 0x009a0901;
const CID_EXPOSURE_ABSOLUTE:      u32 = 0x009a0902;
const CID_PAN_ABSOLUTE:           u32 = 0x009a0908;
const CID_TILT_ABSOLUTE:          u32 = 0x009a0909;
const CID_FOCUS_ABSOLUTE:         u32 = 0x009a090a;
const CID_FOCUS_AUTO:             u32 = 0x009a090c;
const CID_ZOOM_ABSOLUTE:          u32 = 0x009a090d;

/// V4L2-backed webcam backend (Linux only).
pub struct WebcamLinuxBackend {
    connected: Mutex<HashMap<String, ConnectedDevice>>,
}

/// State for one open device.
///
/// `stream` is wrapped in `Option` so we can briefly take ownership and drop
/// the stream during a video-format switch (V4L2 requires REQBUFS=0 before
/// `set_format`, which is what the stream's Drop impl does). Outside of a
/// switch the field is always `Some`.
struct ConnectedDevice {
    /// SAFETY: see SAFETY block in `start_stream()`. The `'static` lifetime
    /// is phantom — the stream is fully self-sufficient via its internal
    /// `Arc<Handle>`, so we extend the borrow to escape the local `device`.
    stream: Option<MmapStream<'static>>,
    /// Kept alive so V4L2 control / format ioctls have a stable Device handle
    /// without needing a second open of the file (which some drivers reject
    /// while streaming).
    device: Device,
}

impl WebcamLinuxBackend {
    pub fn new() -> Result<Self, CameraError> {
        Ok(Self {
            connected: Mutex::new(HashMap::new()),
        })
    }
}

fn map_io(err: std::io::Error) -> CameraError {
    eprintln!("[webcam_linux] error: {err}");
    CameraError::SdkError(0)
}

/// Negotiates MJPG at (width, height) on the device, allocates buffers, and
/// returns a started stream with its lifetime extended to `'static`.
///
/// SAFETY: `MmapStream<'a>`'s `'a` is purely phantom — see v4l 0.14
/// src/io/mmap/{stream,arena}.rs. The stream owns:
///   - an `Arc<Handle>` cloned from `dev.handle()` (refcounted fd),
///   - an `Arena<'a>` whose `bufs: Vec<&'a mut [u8]>` point to kernel mmap'd
///     pages that `Arena::Drop` releases via `munmap`.
/// No reference into the local borrowed `device` survives the call.
unsafe fn start_stream(
    device: &Device,
    width:  u32,
    height: u32,
) -> Result<MmapStream<'static>, CameraError> {
    let mut wanted = device.format().map_err(map_io)?;
    wanted.fourcc = FourCC::new(&MJPG);
    wanted.width  = width;
    wanted.height = height;
    let actual = device.set_format(&wanted).map_err(map_io)?;
    if actual.fourcc.repr != MJPG {
        eprintln!(
            "[webcam_linux] camera does not support MJPG (driver chose {:?})",
            actual.fourcc,
        );
        return Err(CameraError::NotSupported);
    }
    eprintln!(
        "[webcam_linux] negotiated format: MJPG {}x{}",
        actual.width, actual.height,
    );

    let mut stream =
        MmapStream::with_buffers(device, Type::VideoCapture, BUFFER_COUNT).map_err(map_io)?;
    stream.set_timeout(FRAME_TIMEOUT);

    Ok(std::mem::transmute(stream))
}

impl CameraBackend for WebcamLinuxBackend {
    fn backend_id(&self) -> &str {
        "webcam_linux"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let mut devices = Vec::new();
        let connected = self.connected.lock().expect("webcam_linux mutex poisoned");

        for node in context::enum_devices() {
            let path = node.path();

            let device = match Device::with_path(path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[webcam_linux] cannot open {}: {e}", path.display());
                    continue;
                }
            };

            let caps = match device.query_caps() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[webcam_linux] query_caps failed for {}: {e}", path.display());
                    continue;
                }
            };

            let is_capture = caps.capabilities.contains(Flags::VIDEO_CAPTURE)
                || caps.capabilities.contains(Flags::VIDEO_CAPTURE_MPLANE);
            if !is_capture {
                continue;
            }

            let native_id = path.to_string_lossy().into_owned();
            let name = node.name().unwrap_or_else(|| caps.card.clone());

            devices.push(DeviceInfo {
                connected: connected.contains_key(&native_id),
                id: DeviceId::new("webcam_linux", &native_id).encode(),
                name,
            });
        }

        Ok(devices)
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        if self.is_connected(native_id) {
            return Ok(());
        }

        let device = Device::with_path(native_id).map_err(map_io)?;
        let stream = unsafe { start_stream(&device, DEFAULT_WIDTH, DEFAULT_HEIGHT)? };

        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        connected.insert(
            native_id.to_string(),
            ConnectedDevice { stream: Some(stream), device },
        );
        Ok(())
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        connected.remove(native_id);
        Ok(())
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.connected
            .lock()
            .expect("webcam_linux mutex poisoned")
            .contains_key(native_id)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        let dev = connected
            .get(native_id)
            .ok_or(CameraError::NotConnected)?;

        let mut params = Vec::new();

        // VideoFormat: list every MJPG (width, height) the device supports.
        if let Some(format_param) = build_video_format_param(&dev.device) {
            params.push(format_param);
        }

        // V4L2 controls (brightness, contrast, exposure, …).
        let descriptions = dev.device.query_controls().map_err(map_io)?;
        for desc in descriptions {
            let Some(param_type) = cid_to_param_type(desc.id) else {
                continue;
            };

            if desc.flags.contains(control::Flags::DISABLED)
                || desc.flags.contains(control::Flags::INACTIVE)
                || desc.flags.contains(control::Flags::READ_ONLY)
                || desc.flags.contains(control::Flags::WRITE_ONLY)
                || desc.typ == control::Type::Button
                || desc.typ == control::Type::CtrlClass
            {
                continue;
            }

            let current = match dev.device.control(desc.id) {
                Ok(c) => match c.value {
                    control::Value::Integer(v) => v,
                    control::Value::Boolean(b) => i64::from(b),
                    _ => continue,
                },
                Err(_) => continue,
            };

            let param = match desc.typ {
                control::Type::Integer | control::Type::Integer64 => CameraParameter::Range {
                    param_type,
                    current: current as i32,
                    min: desc.minimum as i32,
                    max: desc.maximum as i32,
                    step: if desc.step == 0 { 1 } else { desc.step as i32 },
                },
                control::Type::Boolean => CameraParameter::Select {
                    param_type,
                    current: current.to_string(),
                    options: vec![
                        ParameterOption { label: "Off".into(), value: "0".into() },
                        ParameterOption { label: "On".into(),  value: "1".into() },
                    ],
                },
                control::Type::Menu | control::Type::IntegerMenu => {
                    let options: Vec<ParameterOption> = desc
                        .items
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(idx, item)| ParameterOption {
                            label: match item {
                                MenuItem::Name(s) => s,
                                MenuItem::Value(v) => v.to_string(),
                            },
                            value: idx.to_string(),
                        })
                        .collect();
                    CameraParameter::Select {
                        param_type,
                        current: current.to_string(),
                        options,
                    }
                }
                _ => continue,
            };

            params.push(param);
        }

        Ok(params)
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        let dev = connected
            .get_mut(native_id)
            .ok_or(CameraError::NotConnected)?;

        let stream = dev.stream.as_mut().ok_or(CameraError::NotConnected)?;
        let (buf, _meta) = CaptureStream::next(stream).map_err(map_io)?;
        Ok(buf.to_vec())
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        self.get_live_view_frame(native_id)
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        let dev = connected
            .get_mut(native_id)
            .ok_or(CameraError::NotConnected)?;

        // VideoFormat is special: we have to stop the stream, change format,
        // then restart the stream. All other parameters map to V4L2 controls.
        if param_type == ParameterType::VideoFormat {
            let (width, height) = parse_resolution(value).ok_or(CameraError::NotSupported)?;

            // Drop the active stream first. V4L2 requires REQBUFS=0 (which the
            // Stream's Drop does) before another `set_format` is accepted.
            let _ = dev.stream.take();

            // Reconfigure and restart. If this fails, we leave `stream = None`
            // and surface the error — the user can retry with another format
            // or call disconnect/connect.
            let new_stream = unsafe { start_stream(&dev.device, width, height)? };
            dev.stream = Some(new_stream);
            return Ok(());
        }

        let cid = param_type_to_cid(param_type).ok_or(CameraError::NotSupported)?;
        let int_value: i64 = value.parse().map_err(|_| CameraError::NotSupported)?;
        dev.device
            .set_control(control::Control {
                id:    cid,
                value: control::Value::Integer(int_value),
            })
            .map_err(map_io)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// VideoFormat enumeration
// ---------------------------------------------------------------------------

/// Builds the `VideoFormat` parameter from the device's enumerable framesizes
/// for MJPG. Returns `None` if the camera doesn't expose any MJPG mode.
fn build_video_format_param(device: &Device) -> Option<CameraParameter> {
    let framesizes = device.enum_framesizes(FourCC::new(&MJPG)).ok()?;

    // Expand stepwise ranges to discrete sizes; deduplicate; sort by total
    // pixels descending (highest quality first).
    let mut sizes: Vec<(u32, u32)> = framesizes
        .into_iter()
        .flat_map(|fs| {
            // `to_discrete` flattens both Discrete and Stepwise. For Stepwise
            // ranges this can produce many entries; UVC cameras almost always
            // report Discrete though.
            let _ = matches!(fs.size, FrameSizeEnum::Discrete(_));
            fs.size.to_discrete().into_iter().map(|d| (d.width, d.height))
        })
        .collect();
    sizes.sort_unstable();
    sizes.dedup();
    sizes.sort_by_key(|(w, h)| std::cmp::Reverse((*w as u64) * (*h as u64)));

    if sizes.is_empty() {
        return None;
    }

    // Cap at 50 entries to keep the UI sane on cameras that report stepwise
    // ranges expanding to thousands of options.
    sizes.truncate(50);

    let current_format = device.format().ok()?;
    let current = format!("{}x{}", current_format.width, current_format.height);

    let options = sizes
        .into_iter()
        .map(|(w, h)| ParameterOption {
            label: format!("{}\u{00d7}{}", w, h), // U+00D7 MULTIPLICATION SIGN
            value: format!("{}x{}", w, h),
        })
        .collect();

    Some(CameraParameter::Select {
        param_type: ParameterType::VideoFormat,
        current,
        options,
    })
}

fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

// ---------------------------------------------------------------------------
// V4L2 CID ↔ ParameterType mapping
// ---------------------------------------------------------------------------

fn cid_to_param_type(cid: u32) -> Option<ParameterType> {
    match cid {
        CID_BRIGHTNESS             => Some(ParameterType::Brightness),
        CID_CONTRAST               => Some(ParameterType::Contrast),
        CID_SATURATION             => Some(ParameterType::Saturation),
        CID_HUE                    => Some(ParameterType::Hue),
        CID_HUE_AUTO               => Some(ParameterType::HueMode),
        CID_AUTO_WHITE_BALANCE     => Some(ParameterType::WhiteBalanceMode),
        CID_GAMMA                  => Some(ParameterType::Gamma),
        CID_GAIN                   => Some(ParameterType::Gain),
        CID_WHITE_BALANCE_TEMP     => Some(ParameterType::ColorTemperature),
        CID_SHARPNESS              => Some(ParameterType::Sharpness),
        CID_BACKLIGHT_COMPENSATION => Some(ParameterType::BacklightCompensation),
        CID_POWER_LINE_FREQUENCY   => Some(ParameterType::PowerLineFrequency),
        CID_EXPOSURE_AUTO          => Some(ParameterType::ExposureMode),
        CID_EXPOSURE_ABSOLUTE      => Some(ParameterType::Exposure),
        CID_PAN_ABSOLUTE           => Some(ParameterType::Pan),
        CID_TILT_ABSOLUTE          => Some(ParameterType::Tilt),
        CID_FOCUS_ABSOLUTE         => Some(ParameterType::Focus),
        CID_FOCUS_AUTO             => Some(ParameterType::FocusMode),
        CID_ZOOM_ABSOLUTE          => Some(ParameterType::Zoom),
        _ => None,
    }
}

fn param_type_to_cid(pt: ParameterType) -> Option<u32> {
    match pt {
        ParameterType::Brightness            => Some(CID_BRIGHTNESS),
        ParameterType::Contrast              => Some(CID_CONTRAST),
        ParameterType::Saturation            => Some(CID_SATURATION),
        ParameterType::Hue                   => Some(CID_HUE),
        ParameterType::HueMode               => Some(CID_HUE_AUTO),
        ParameterType::WhiteBalanceMode      => Some(CID_AUTO_WHITE_BALANCE),
        ParameterType::Gamma                 => Some(CID_GAMMA),
        ParameterType::Gain                  => Some(CID_GAIN),
        ParameterType::ColorTemperature      => Some(CID_WHITE_BALANCE_TEMP),
        ParameterType::Sharpness             => Some(CID_SHARPNESS),
        ParameterType::BacklightCompensation => Some(CID_BACKLIGHT_COMPENSATION),
        ParameterType::PowerLineFrequency    => Some(CID_POWER_LINE_FREQUENCY),
        ParameterType::ExposureMode          => Some(CID_EXPOSURE_AUTO),
        ParameterType::Exposure              => Some(CID_EXPOSURE_ABSOLUTE),
        ParameterType::Pan                   => Some(CID_PAN_ABSOLUTE),
        ParameterType::Tilt                  => Some(CID_TILT_ABSOLUTE),
        ParameterType::Focus                 => Some(CID_FOCUS_ABSOLUTE),
        ParameterType::FocusMode             => Some(CID_FOCUS_AUTO),
        ParameterType::Zoom                  => Some(CID_ZOOM_ABSOLUTE),
        _ => None,
    }
}
