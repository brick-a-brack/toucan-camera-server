use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use v4l::buffer::Type;
use v4l::capability::Flags;
use v4l::context;
use v4l::control::{self, MenuItem};
use v4l::format::FourCC;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

/// Pixel formats we negotiate with the camera. MJPG is preferred (the camera
/// already emits JPEG, so frames stream straight out of the kernel buffer).
/// YUYV is the fallback for resolutions a camera only offers uncompressed; we
/// transcode those frames to JPEG ourselves, just like the Windows backend.
const MJPG: [u8; 4] = *b"MJPG";
const YUYV: [u8; 4] = *b"YUYV";

/// Default resolution we try at connect time. The user can change it later via
/// the `VideoFormat` parameter.
const DEFAULT_WIDTH:  u32 = 1280;
const DEFAULT_HEIGHT: u32 = 720;
const BUFFER_COUNT:   u32 = 4;

/// Block at most this long when waiting for a frame from the kernel queue.
/// Generous because high-bandwidth uncompressed modes (large YUYV) can take a
/// while to deliver their first frame after STREAMON while the USB isochronous
/// bandwidth is negotiated, especially at low frame rates (e.g. 2 fps).
const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

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

// V4L2_CID_EXPOSURE_AUTO is a 4-entry menu (enum v4l2_exposure_auto_type), but
// the macOS and Windows backends model auto-exposure as a simple on/off toggle.
// We present it the same way: MANUAL means "auto off"; APERTURE_PRIORITY is the
// value UVC cameras accept for "auto on".
const V4L2_EXPOSURE_MANUAL:            i64 = 1;
const V4L2_EXPOSURE_APERTURE_PRIORITY: i64 = 3;

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
    /// True when the negotiated format is MJPG (frames are already JPEG). When
    /// false the format is YUYV and frames are transcoded to JPEG per capture.
    is_mjpg: bool,
    /// Negotiated frame dimensions — needed to transcode YUYV frames.
    width:  u32,
    height: u32,
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

/// A started capture stream plus the format the driver actually negotiated.
struct StartedStream {
    /// SAFETY: lifetime extended to `'static` — see the SAFETY block below.
    stream:  MmapStream<'static>,
    is_mjpg: bool,
    width:   u32,
    height:  u32,
}

/// Negotiates (width, height) on the device — MJPG when the camera offers that
/// resolution compressed, otherwise YUYV — allocates buffers, and returns a
/// started stream with its lifetime extended to `'static`.
///
/// SAFETY: `MmapStream<'a>`'s `'a` is purely phantom — see v4l 0.14
/// src/io/mmap/{stream,arena}.rs. The stream owns:
///   - an `Arc<Handle>` cloned from `dev.handle()` (refcounted fd),
///   - an `Arena<'a>` whose `bufs: Vec<&'a mut [u8]>` point to kernel mmap'd
///     pages that `Arena::Drop` releases via `munmap`.
///
/// No reference into the local borrowed `device` survives the call.
unsafe fn start_stream(
    device: &Device,
    width:  u32,
    height: u32,
) -> Result<StartedStream, CameraError> {
    // Prefer MJPG when the camera offers this resolution compressed; otherwise
    // fall back to YUYV (transcoded to JPEG on capture).
    let fourcc = if discrete_sizes(device, MJPG).contains(&(width, height)) {
        MJPG
    } else {
        YUYV
    };

    let mut wanted = device.format().map_err(map_io)?;
    wanted.fourcc = FourCC::new(&fourcc);
    wanted.width  = width;
    wanted.height = height;
    let actual = device.set_format(&wanted).map_err(map_io)?;

    let is_mjpg = actual.fourcc.repr == MJPG;
    if !is_mjpg && actual.fourcc.repr != YUYV {
        eprintln!(
            "[webcam_linux] camera produced an unsupported format ({:?})",
            actual.fourcc,
        );
        return Err(CameraError::NotSupported);
    }
    eprintln!(
        "[webcam_linux] negotiated format: {} {}x{}",
        if is_mjpg { "MJPG" } else { "YUYV" },
        actual.width, actual.height,
    );

    let mut stream =
        MmapStream::with_buffers(device, Type::VideoCapture, BUFFER_COUNT).map_err(map_io)?;
    stream.set_timeout(FRAME_TIMEOUT);

    Ok(StartedStream {
        stream:  std::mem::transmute::<MmapStream<'_>, MmapStream<'static>>(stream),
        is_mjpg,
        width:  actual.width,
        height: actual.height,
    })
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
                dedup_key: None,
            });
        }

        Ok(devices)
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        if self.is_connected(native_id) {
            return Ok(());
        }

        let device = Device::with_path(native_id).map_err(map_io)?;
        let started = unsafe { start_stream(&device, DEFAULT_WIDTH, DEFAULT_HEIGHT)? };

        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        connected.insert(
            native_id.to_string(),
            ConnectedDevice {
                stream:  Some(started.stream),
                device,
                is_mjpg: started.is_mjpg,
                width:   started.width,
                height:  started.height,
            },
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

            // DISABLED / read-only / write-only / button / control-class entries
            // are never settable, so they are dropped entirely. INACTIVE entries
            // (e.g. exposure time while auto-exposure is on) are kept but rendered
            // disabled, matching the macOS and Windows backends which always
            // surface a value parameter and only grey it out.
            if desc.flags.contains(control::Flags::DISABLED)
                || desc.flags.contains(control::Flags::READ_ONLY)
                || desc.flags.contains(control::Flags::WRITE_ONLY)
                || desc.typ == control::Type::Button
                || desc.typ == control::Type::CtrlClass
            {
                continue;
            }
            let inactive = desc.flags.contains(control::Flags::INACTIVE);

            let current = match dev.device.control(desc.id) {
                Ok(c) => match c.value {
                    control::Value::Integer(v) => v,
                    control::Value::Boolean(b) => i64::from(b),
                    _ => continue,
                },
                Err(_) => continue,
            };

            // Auto-exposure is a V4L2 menu; expose it as a boolean toggle like the
            // other backends. Auto is on unless the camera reports MANUAL.
            if param_type == ParameterType::ExposureAuto {
                params.push(CameraParameter::Boolean {
                    param_type,
                    current:  current != V4L2_EXPOSURE_MANUAL,
                    disabled: inactive,
                });
                continue;
            }

            // White balance / hue / focus auto are plain boolean V4L2 controls.
            if is_boolean_param(param_type) {
                params.push(CameraParameter::Boolean {
                    param_type,
                    current:  current != 0,
                    disabled: inactive,
                });
                continue;
            }

            let param = match desc.typ {
                control::Type::Integer | control::Type::Integer64 => CameraParameter::Range {
                    param_type,
                    current: current as i32,
                    min: desc.minimum as i32,
                    max: desc.maximum as i32,
                    step: if desc.step == 0 { 1 } else { desc.step as i32 },
                    disabled: inactive,
                },
                control::Type::Boolean => CameraParameter::Boolean {
                    param_type,
                    current:  current != 0,
                    disabled: inactive,
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
                        disabled: inactive,
                    }
                }
                _ => continue,
            };

            params.push(param);
        }

        Ok(finalize_disabled(params))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let mut connected = self.connected.lock().expect("webcam_linux mutex poisoned");
        let dev = connected
            .get_mut(native_id)
            .ok_or(CameraError::NotConnected)?;

        let is_mjpg = dev.is_mjpg;
        let (width, height) = (dev.width, dev.height);

        let stream = dev.stream.as_mut().ok_or(CameraError::NotConnected)?;

        // A complete YUYV frame is exactly 2 bytes/pixel (UVC adds no row padding).
        let expected_yuyv = width as usize * height as usize * 2;

        // Read one frame per call (like the macOS / Windows backends), skipping any
        // torn/incomplete frame, until we get a complete one or hit the retry cap.
        //
        // We deliberately do NOT manually drain the V4L2 queue: a poll()-based drain
        // can't distinguish POLLIN from POLLERR/POLLHUP (the v4l crate only exposes
        // the fd count, not the revents), so under the heavy bandwidth of high-res
        // YUYV it would mistake an error condition for "buffer ready", DQBUF a
        // not-cleanly-ready buffer, and hand back stale/partial pixels — the exact
        // "blocks of the previous frame" artifact (and sometimes EINVAL). DQBUF on
        // its own only ever returns complete buffers, and the capture loop's own
        // cadence keeps latency bounded.
        const MAX_READS: u32 = 4;
        for _ in 0..MAX_READS {
            let (buf, meta) = CaptureStream::next(stream).map_err(map_io)?;
            if meta.flags.contains(v4l::buffer::Flags::ERROR) {
                continue; // torn frame — discard, try the next
            }
            let used = (meta.bytesused as usize).min(buf.len());
            if is_mjpg {
                // MJPG frames are already JPEG — return them tight, like macOS / Windows.
                return Ok(buf[..used].to_vec());
            } else if used >= expected_yuyv {
                // YUYV is uncompressed — transcode the complete frame to JPEG.
                let frame = buf[..expected_yuyv].to_vec();
                return yuyv_to_jpeg(&frame, width, height);
            }
            // A short YUYV buffer is an incomplete frame — discard and retry.
        }

        // Only torn/incomplete frames this round — signal "not ready" so the live
        // view loop skips and retries instead of tearing down the stream.
        Err(CameraError::SdkError(0x0000_A102))
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
        if param_type == ParameterType::VideoStreamFormat {
            let (width, height) = parse_resolution(value).ok_or(CameraError::NotSupported)?;

            // Drop the active stream first. V4L2 requires REQBUFS=0 (which the
            // Stream's Drop does) before another `set_format` is accepted.
            let _ = dev.stream.take();

            // Reconfigure and restart. If this fails, we leave `stream = None`
            // and surface the error — the user can retry with another format
            // or call disconnect/connect.
            let started = unsafe { start_stream(&dev.device, width, height)? };
            dev.stream  = Some(started.stream);
            dev.is_mjpg = started.is_mjpg;
            dev.width   = started.width;
            dev.height  = started.height;
            return Ok(());
        }

        let cid = param_type_to_cid(param_type).ok_or(CameraError::NotSupported)?;

        // Auto-exposure is a menu: translate the on/off toggle the other backends
        // send onto APERTURE_PRIORITY (auto) / MANUAL (manual). Every other
        // parameter takes either a boolean ("true"/"false") or an integer value.
        let int_value: i64 = if param_type == ParameterType::ExposureAuto {
            if value == "true" { V4L2_EXPOSURE_APERTURE_PRIORITY } else { V4L2_EXPOSURE_MANUAL }
        } else {
            match value {
                "true"  => 1,
                "false" => 0,
                v       => v.parse().map_err(|_| CameraError::NotSupported)?,
            }
        };

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

/// All discrete (width, height) sizes the device offers for `fourcc`.
/// `to_discrete` flattens both Discrete and Stepwise framesizes; UVC cameras
/// almost always report Discrete. Returns empty if the format is unsupported.
fn discrete_sizes(device: &Device, fourcc: [u8; 4]) -> Vec<(u32, u32)> {
    device
        .enum_framesizes(FourCC::new(&fourcc))
        .map(|framesizes| {
            framesizes
                .into_iter()
                .flat_map(|fs| fs.size.to_discrete().into_iter().map(|d| (d.width, d.height)))
                .collect()
        })
        .unwrap_or_default()
}

/// Builds the `VideoStreamFormat` parameter from every resolution the camera
/// exposes, in MJPG and/or YUYV. MJPG is preferred when a resolution is offered
/// in both (already compressed); a resolution available only uncompressed is
/// offered as YUYV and transcoded to JPEG on capture. This mirrors the Windows
/// backend, which also enumerates both codecs. Returns `None` if fewer than two
/// distinct resolutions exist (no real choice).
fn build_video_format_param(device: &Device) -> Option<CameraParameter> {
    // resolution -> is_mjpg. Insert YUYV first, then MJPG so MJPG wins on overlap.
    let mut by_res: HashMap<(u32, u32), bool> = HashMap::new();
    for size in discrete_sizes(device, YUYV) {
        by_res.insert(size, false);
    }
    for size in discrete_sizes(device, MJPG) {
        by_res.insert(size, true);
    }

    // A selector with a single option is no real choice — hide it, mirroring the
    // Windows backend (`formats.len() > 1`) and the project-wide convention.
    if by_res.len() < 2 {
        return None;
    }

    // Sort by total pixels descending (highest quality first); cap the list to
    // keep the UI sane on cameras that report many stepwise sizes.
    let mut sizes: Vec<((u32, u32), bool)> = by_res.into_iter().collect();
    sizes.sort_by_key(|((w, h), _)| std::cmp::Reverse((*w as u64) * (*h as u64)));
    sizes.truncate(50);

    let current_format = device.format().ok()?;
    let current = format!("{}x{}", current_format.width, current_format.height);

    let options = sizes
        .into_iter()
        .map(|((w, h), is_mjpg)| ParameterOption {
            // U+00D7 MULTIPLICATION SIGN; codec suffix so YUV-only modes are clear.
            label: format!("{}\u{00d7}{} {}", w, h, if is_mjpg { "MJPEG" } else { "YUV" }),
            value: format!("{}x{}", w, h),
        })
        .collect();

    Some(CameraParameter::Select {
        param_type: ParameterType::VideoStreamFormat,
        current,
        options,
        disabled: false,
    })
}

/// Converts a YUYV (a.k.a. YUY2) frame to a JPEG buffer. YUYV packs two pixels
/// into four bytes: Y0 U0 Y1 V0. Mirrors the Windows backend's conversion.
fn yuyv_to_jpeg(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CameraError> {
    let mut rgb: Vec<u8> = Vec::with_capacity((width * height * 3) as usize);

    for chunk in data.chunks_exact(4) {
        let y0 = chunk[0] as f32;
        let u  = chunk[1] as f32 - 128.0;
        let y1 = chunk[2] as f32;
        let v  = chunk[3] as f32 - 128.0;

        for y in [y0, y1] {
            let r = (y + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g = (y - 0.344_136 * u - 0.714_136 * v).clamp(0.0, 255.0) as u8;
            let b = (y + 1.772 * u).clamp(0.0, 255.0) as u8;
            rgb.extend_from_slice(&[r, g, b]);
        }
    }

    let img = image::RgbImage::from_raw(width, height, rgb)
        .ok_or(CameraError::SdkError(0xDEAD_0001))?;
    let mut jpeg_buf: Vec<u8> = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut jpeg_buf), image::ImageFormat::Jpeg)
        .map_err(|_| CameraError::SdkError(0xDEAD_0002))?;
    Ok(jpeg_buf)
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
        CID_HUE_AUTO               => Some(ParameterType::HueAuto),
        CID_AUTO_WHITE_BALANCE     => Some(ParameterType::WhiteBalanceAuto),
        CID_GAMMA                  => Some(ParameterType::Gamma),
        CID_GAIN                   => Some(ParameterType::Gain),
        CID_WHITE_BALANCE_TEMP     => Some(ParameterType::WhiteBalance),
        CID_SHARPNESS              => Some(ParameterType::Sharpness),
        CID_BACKLIGHT_COMPENSATION => Some(ParameterType::BacklightCompensation),
        CID_POWER_LINE_FREQUENCY   => Some(ParameterType::PowerLineFrequency),
        CID_EXPOSURE_AUTO          => Some(ParameterType::ExposureAuto),
        CID_EXPOSURE_ABSOLUTE      => Some(ParameterType::Exposure),
        CID_PAN_ABSOLUTE           => Some(ParameterType::Pan),
        CID_TILT_ABSOLUTE          => Some(ParameterType::Tilt),
        CID_FOCUS_ABSOLUTE         => Some(ParameterType::Focus),
        CID_FOCUS_AUTO             => Some(ParameterType::FocusAuto),
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
        ParameterType::HueAuto               => Some(CID_HUE_AUTO),
        ParameterType::WhiteBalanceAuto      => Some(CID_AUTO_WHITE_BALANCE),
        ParameterType::Gamma                 => Some(CID_GAMMA),
        ParameterType::Gain                  => Some(CID_GAIN),
        ParameterType::WhiteBalance          => Some(CID_WHITE_BALANCE_TEMP),
        ParameterType::Sharpness             => Some(CID_SHARPNESS),
        ParameterType::BacklightCompensation => Some(CID_BACKLIGHT_COMPENSATION),
        ParameterType::PowerLineFrequency    => Some(CID_POWER_LINE_FREQUENCY),
        ParameterType::ExposureAuto          => Some(CID_EXPOSURE_AUTO),
        ParameterType::Exposure              => Some(CID_EXPOSURE_ABSOLUTE),
        ParameterType::Pan                   => Some(CID_PAN_ABSOLUTE),
        ParameterType::Tilt                  => Some(CID_TILT_ABSOLUTE),
        ParameterType::Focus                 => Some(CID_FOCUS_ABSOLUTE),
        ParameterType::FocusAuto             => Some(CID_FOCUS_AUTO),
        ParameterType::Zoom                  => Some(CID_ZOOM_ABSOLUTE),
        _ => None,
    }
}

/// Returns true for ParameterTypes presented as an on/off boolean: the
/// auto/manual toggles (plain boolean V4L2 controls) plus backlight compensation,
/// which is a 0/1 control on UVC cameras (matching the Windows backend).
/// Auto-exposure is intentionally excluded: it is a menu control handled
/// separately.
fn is_boolean_param(pt: ParameterType) -> bool {
    matches!(
        pt,
        ParameterType::WhiteBalanceAuto
            | ParameterType::HueAuto
            | ParameterType::FocusAuto
            | ParameterType::BacklightCompensation
    )
}

/// Applies the cross-backend "disabled" rules so the Linux backend behaves like
/// the macOS and Windows webcam backends:
///  - a value parameter is disabled while its `*_auto` toggle is active;
///  - gain is disabled while auto-exposure is active (mirrors the Windows backend);
///  - pan / tilt / roll are disabled while zoom is at its minimum.
///
/// Inactive controls were already flagged disabled at construction time; this
/// only ever sets `disabled = true`, never clears it.
fn finalize_disabled(mut params: Vec<CameraParameter>) -> Vec<CameraParameter> {
    // (value_type, auto_type): disable value_type when auto_type current == true.
    const PAIRS: &[(ParameterType, ParameterType)] = &[
        (ParameterType::WhiteBalance, ParameterType::WhiteBalanceAuto),
        (ParameterType::Exposure,     ParameterType::ExposureAuto),
        (ParameterType::Hue,          ParameterType::HueAuto),
        (ParameterType::Focus,        ParameterType::FocusAuto),
    ];

    // Auto toggles that are currently enabled.
    let active_autos: Vec<ParameterType> = params
        .iter()
        .filter_map(|p| match p {
            CameraParameter::Boolean { param_type, current: true, .. } => Some(*param_type),
            _ => None,
        })
        .collect();

    let exposure_is_auto = active_autos.contains(&ParameterType::ExposureAuto);

    // Zoom at minimum → no room to pan/tilt/roll (mirrors the Windows backend).
    let zoom_is_min = params.iter().any(|p| matches!(
        p,
        CameraParameter::Range { param_type: ParameterType::Zoom, current, min, .. } if current <= min
    ));

    for p in &mut params {
        let pt = match p {
            CameraParameter::Range { param_type, .. }
            | CameraParameter::Select { param_type, .. }
            | CameraParameter::RangeSelect { param_type, .. } => *param_type,
            CameraParameter::Boolean { .. } => continue,
        };

        let disabled_by_auto = PAIRS
            .iter()
            .any(|&(vt, at)| pt == vt && active_autos.contains(&at));
        let disabled_gain = pt == ParameterType::Gain && exposure_is_auto;
        let disabled_ptz = zoom_is_min
            && matches!(pt, ParameterType::Pan | ParameterType::Tilt | ParameterType::Roll);

        if disabled_by_auto || disabled_gain || disabled_ptz {
            match p {
                CameraParameter::Range { disabled, .. }
                | CameraParameter::Select { disabled, .. }
                | CameraParameter::RangeSelect { disabled, .. } => *disabled = true,
                CameraParameter::Boolean { .. } => {}
            }
        }
    }
    params
}
