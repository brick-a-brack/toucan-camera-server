use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;

// The `#[implement]` macro emits `::windows_core::...` paths, so windows_core
// must be resolvable as a top-level crate name.
extern crate windows_core;

use windows::core::{implement, Interface, GUID, PWSTR};
use windows::Win32::Media::DirectShow::{
    CameraControlProperty, IAMCameraControl, IAMVideoProcAmp,
    IBaseFilter, IEnumPins, IPin,
    VideoProcAmpProperty,
    CameraControl_Exposure, CameraControl_Flags_Auto, CameraControl_Flags_Manual,
    CameraControl_Focus, CameraControl_Pan, CameraControl_Roll, CameraControl_Tilt,
    CameraControl_Zoom,
    VideoProcAmp_BacklightCompensation, VideoProcAmp_Brightness, VideoProcAmp_Contrast,
    VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual, VideoProcAmp_Gain, VideoProcAmp_Gamma,
    VideoProcAmp_Hue, VideoProcAmp_Saturation, VideoProcAmp_Sharpness, VideoProcAmp_WhiteBalance,
};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFCaptureSink, IMFCaptureEngine,
    IMFCaptureEngineClassFactory, IMFCaptureEngineOnEventCallback,
    IMFCaptureEngineOnEventCallback_Impl, IMFCaptureEngineOnSampleCallback,
    IMFCaptureEngineOnSampleCallback_Impl, IMFCapturePhotoSink, IMFCaptureSource,
    IMFMediaEvent, IMFMediaSource, IMFMediaType, IMFSample, IMFSourceReader,
    MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromMediaSource,
    MFEnumDeviceSources, MFShutdown, MFStartup,
    MF_CAPTURE_ENGINE_INITIALIZED,
    MF_CAPTURE_ENGINE_SINK_TYPE_PHOTO,
    MF_CAPTURE_ENGINE_STREAM_CATEGORY_PHOTO_INDEPENDENT,
    MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
    MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MFImageFormat_JPEG,
    MFMediaType_Image, MFVideoFormat_MJPG, MFVideoFormat_YUY2,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    CLSID_MFCaptureEngineClassFactory,
};
use windows::Win32::Media::KernelStreaming::IKsPropertySet;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
};

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption,
    ParameterType,
};

// MF_VERSION = (MF_SDK_VERSION << 16 | MF_API_VERSION) = (0x0002 << 16 | 0x0070)
const MF_SDK_VERSION_VALUE: u32 = 0x0002_0070;

// Power line frequency accessed via IKsPropertySet on the DirectShow capture pin.
// IAMVideoProcAmp only exposes IDs 0–9; ID 13 must go through IKsPropertySet.
// ksproxy.ax routes the call to the correct processing-unit topology node automatically.
const PROPSETID_VIDCAP_VIDEOPROCAMP: GUID = GUID {
    data1: 0xC6E1_3360,
    data2: 0x30AC,
    data3: 0x11D0,
    data4: [0xA1, 0x8C, 0x00, 0xA0, 0xC9, 0x11, 0x89, 0x56],
};
// KSPROPERTY_VIDEOPROCAMP_POWERLINE_FREQUENCY = 13 in ksmedia.h (sequential enum 0–16).
const KSPROP_VIDPROCAMP_POWERLINE: u32 = 13;
const KSPROPERTY_TYPE_SET: u32 = 0x0000_0002;

// KSPROPERTY_VIDEOPROCAMP_S (36 bytes) — layout used by IKsPropertySet for VIDEOPROCAMP.
// The Property header (first 24 bytes) is written by the driver on GET; on SET we fill
// it in so the kernel IOCTL input buffer is complete (matching how IAMVideoProcAmp works).
#[repr(C)]
struct KsVideoProcAmpS {
    prop_set:     GUID,  // KSPROPERTY.Set (16 bytes)
    prop_id:      u32,   // KSPROPERTY.Id
    prop_flags:   u32,   // KSPROPERTY.Flags
    value:        i32,
    vpa_flags:    u32,   // 1 = auto, 2 = manual
    capabilities: u32,
}

// IMFSourceReader stream-flags (MFSTREAMSINK_MARKER_FLAG)
const MF_SOURCE_READERF_ERROR: u32 = 0x0001;
const MF_SOURCE_READERF_STREAMTICK: u32 = 0x0100;

// Convenience: cast MF_SOURCE_READER_CONSTANTS to u32 for all calls.
fn video_stream() -> u32 {
    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32
}

// ---------------------------------------------------------------------------
// COM callback: IMFCaptureEngine init event (used only for Path A photo)
// ---------------------------------------------------------------------------

#[implement(IMFCaptureEngineOnEventCallback)]
struct EngineEventCallback {
    init_tx: Mutex<Option<mpsc::SyncSender<Result<(), CameraError>>>>,
}

impl IMFCaptureEngineOnEventCallback_Impl for EngineEventCallback_Impl {
    fn OnEvent(&self, pevent: Option<&IMFMediaEvent>) -> windows::core::Result<()> {
        let Some(event) = pevent else { return Ok(()) };
        let event_type = unsafe { event.GetExtendedType() }
            .unwrap_or(windows::core::GUID::zeroed());

        if event_type == MF_CAPTURE_ENGINE_INITIALIZED {
            let hr = unsafe { event.GetStatus() }.unwrap_or(windows::core::HRESULT(-1));
            let result = hr.ok().map_err(|e| CameraError::SdkError(e.code().0 as u32));
            if let Ok(mut guard) = self.init_tx.lock() {
                if let Some(tx) = guard.take() {
                    let _ = tx.send(result);
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// COM callback: photo sink sample (Path A — PHOTO_INDEPENDENT)
// ---------------------------------------------------------------------------

type PhotoReply = mpsc::SyncSender<Result<Vec<u8>, CameraError>>;
type PendingPhoto = Arc<Mutex<Option<PhotoReply>>>;

#[implement(IMFCaptureEngineOnSampleCallback)]
struct PhotoSampleCallback {
    pending: PendingPhoto,
}

impl IMFCaptureEngineOnSampleCallback_Impl for PhotoSampleCallback_Impl {
    fn OnSample(&self, psample: Option<&IMFSample>) -> windows::core::Result<()> {
        let result = match psample {
            Some(sample) => extract_raw_buffer(sample),
            None => Err(CameraError::SdkError(0xDEAD_0010)),
        };
        if let Ok(mut guard) = self.pending.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(result);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Commands
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
    Shutdown,
}

// ---------------------------------------------------------------------------
// Video format descriptors — enumerated once at connect time
// ---------------------------------------------------------------------------

struct VideoFormatInfo {
    media_type: IMFMediaType,
    is_mjpeg:   bool,
    width:      u32,
    height:     u32,
    fps_num:    u32,
    fps_den:    u32,
}

impl VideoFormatInfo {
    fn label(&self) -> String {
        let codec = if self.is_mjpeg { "MJPEG" } else { "YUV" };
        let fps = if self.fps_den > 0 {
            format!(" {:.0}fps", self.fps_num as f64 / self.fps_den as f64)
        } else {
            String::new()
        };
        format!("{}×{} {}{}", self.width, self.height, codec, fps)
    }
}

// ---------------------------------------------------------------------------
// Photo stream info for Path A (PHOTO_INDEPENDENT)
// ---------------------------------------------------------------------------

struct PhotoStreamInfo {
    /// Stream index within the device (used by IMFCaptureEngine).
    stream_idx:  u32,
    /// Best available photo media type (highest resolution).
    media_type:  IMFMediaType,
    width:       u32,
    height:      u32,
}

// ---------------------------------------------------------------------------
// Per-device state — lives exclusively on the SDK thread
// ---------------------------------------------------------------------------

struct DeviceState {
    /// SourceReader used for live view (polling-based, proven reliable).
    reader:              IMFSourceReader,
    /// Underlying source kept for IAMVideoProcAmp / IAMCameraControl.
    source:              IMFMediaSource,
    video_proc_amp:      Option<IAMVideoProcAmp>,
    camera_control:      Option<IAMCameraControl>,
    /// IKsPropertySet from the underlying DirectShow capture pin.
    /// Used for UVC properties beyond IAMVideoProcAmp's 0–9 range (e.g. powerline freq).
    ks_prop_set:         Option<IKsPropertySet>,
    formats:             Vec<VideoFormatInfo>,
    current_format_idx:  usize,
    is_mjpeg:            bool,
    width:               u32,
    height:              u32,
    /// Path A: dedicated photo stream available (None → use Path B).
    photo_stream:        Option<PhotoStreamInfo>,
    /// The IMFActivate used to open this device (re-used for Path A engine).
    activate:            IMFActivate,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct WebcamWindowsBackend {
    tx: mpsc::Sender<Command>,
}

impl WebcamWindowsBackend {
    pub fn new() -> Result<Self, CameraError> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), CameraError>>();

        std::thread::Builder::new()
            .name("webcam-windows-sdk".to_string())
            .spawn(move || sdk_thread(cmd_rx, init_tx))
            .expect("failed to spawn webcam-windows-sdk thread");

        let init_result = init_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));
        init_result?;

        Ok(Self { tx: cmd_tx })
    }
}

impl Drop for WebcamWindowsBackend {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

impl CameraBackend for WebcamWindowsBackend {
    fn backend_id(&self) -> &str {
        "webcam-windows"
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
            .send(Command::Connect { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::Disconnect { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn is_connected(&self, native_id: &str) -> bool {
        let (reply_tx, reply_rx) = mpsc::channel();
        if self
            .tx
            .send(Command::IsConnected { native_id: native_id.to_string(), reply: reply_tx })
            .is_err()
        {
            return false;
        }
        reply_rx.recv().unwrap_or(false)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetParameters { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::GetLiveViewFrame { native_id: native_id.to_string(), reply: reply_tx })
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
                native_id: native_id.to_string(),
                param_type,
                value: value.to_string(),
                reply: reply_tx,
            })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx
            .send(Command::CapturePhoto { native_id: native_id.to_string(), reply: reply_tx })
            .map_err(|_| CameraError::SdkError(0xFFFF_FFFF))?;
        reply_rx.recv().unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)))
    }
}

// ---------------------------------------------------------------------------
// SDK thread
// ---------------------------------------------------------------------------

fn sdk_thread(rx: mpsc::Receiver<Command>, init_tx: mpsc::Sender<Result<(), CameraError>>) {
    // Initialize COM in a single-threaded apartment (STA). Many webcam drivers
    // are STA COM components and will not enumerate under MTA (COINIT_MULTITHREADED).
    // S_FALSE (already initialized) is also fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    if let Err(e) = unsafe { MFStartup(MF_SDK_VERSION_VALUE, 1 /* MFSTARTUP_NOSOCKET */) } {
        let _ = init_tx.send(Err(CameraError::SdkError(e.code().0 as u32)));
        unsafe { CoUninitialize() };
        return;
    }

    let _ = init_tx.send(Ok(()));
    drop(init_tx);

    // Device state lives exclusively on this thread.
    let mut connected: HashMap<String, DeviceState> = HashMap::new();

    // STA COM requires this thread to pump Windows messages so that inter-apartment
    // calls and driver callbacks can be dispatched. We poll for commands with a short
    // timeout and pump the message queue between iterations.
    loop {
        // Pump all pending Windows messages before blocking on the next command.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        match rx.recv_timeout(std::time::Duration::from_millis(16)) {
            Ok(Command::ListDevices { reply }) => {
                let _ = reply.send(list_devices_impl(&connected));
            }
            Ok(Command::Connect { native_id, reply }) => {
                let _ = reply.send(connect_impl(&native_id, &mut connected));
            }
            Ok(Command::Disconnect { native_id, reply }) => {
                let _ = reply.send(disconnect_impl(&native_id, &mut connected));
            }
            Ok(Command::IsConnected { native_id, reply }) => {
                let alive = connected
                    .get(&native_id)
                    .map(|s| is_source_alive(&s.source))
                    .unwrap_or(false);
                if !alive {
                    force_disconnect(&native_id, &mut connected);
                }
                let _ = reply.send(alive);
            }
            Ok(Command::GetParameters { native_id, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(get_parameters_impl);
                let _ = reply.send(result);
            }
            Ok(Command::GetLiveViewFrame { native_id, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(get_live_view_frame_impl);
                if result.is_err() {
                    // Probe the source only on failure to avoid per-frame overhead.
                    let dead = connected
                        .get(&native_id)
                        .map(|s| !is_source_alive(&s.source))
                        .unwrap_or(false);
                    if dead {
                        force_disconnect(&native_id, &mut connected);
                    }
                }
                let _ = reply.send(result);
            }
            Ok(Command::SetParameter { native_id, param_type, value, reply }) => {
                let result = connected
                    .get_mut(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(|state| set_parameter_impl(state, param_type, &value));
                let _ = reply.send(result);
            }
            Ok(Command::CapturePhoto { native_id, reply }) => {
                let result = connected
                    .get(&native_id)
                    .ok_or(CameraError::NotConnected)
                    .and_then(capture_photo_impl);
                let _ = reply.send(result);
            }
            Ok(Command::Shutdown) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }

    for (_, state) in connected.drain() {
        unsafe { let _ = state.source.Shutdown(); }
    }

    let _ = unsafe { MFShutdown() };
    unsafe { CoUninitialize() };
}

// ---------------------------------------------------------------------------
// SDK operations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn win_err(e: windows::core::Error) -> CameraError {
    CameraError::SdkError(e.code().0 as u32)
}

/// Returns false if the underlying media source is no longer functional
/// (device unplugged, system sleep, driver reset, etc.).
/// `GetCharacteristics` is a lightweight COM call that fails immediately
/// when the source has been invalidated.
fn is_source_alive(source: &IMFMediaSource) -> bool {
    unsafe { source.GetCharacteristics() }.is_ok()
}

/// Removes a device from `connected` and shuts down its source.
fn force_disconnect(native_id: &str, connected: &mut HashMap<String, DeviceState>) {
    if let Some(state) = connected.remove(native_id) {
        unsafe { let _ = state.source.Shutdown(); }
    }
}

fn list_devices_impl(
    connected: &HashMap<String, DeviceState>,
) -> Result<Vec<DeviceInfo>, CameraError> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(win_err)?;
        let attrs = attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
        attrs
            .SetGUID(
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
            )
            .map_err(win_err)?;

        let mut devices_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFEnumDeviceSources(&attrs, &mut devices_ptr, &mut count).map_err(win_err)?;

        let mut result = Vec::with_capacity(count as usize);

        for i in 0..count as usize {
            // Take ownership of the activate pointer from the CoTask-allocated array.
            // Replacing with None prevents double-Release when the array is freed.
            let activate = match std::ptr::replace(devices_ptr.add(i), None) {
                Some(a) => a,
                None => continue,
            };

            let name = read_string_attr(&activate, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME)
                .unwrap_or_else(|| "Unknown".to_string());

            let native_id = match read_string_attr(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) {
                Some(s) if !s.is_empty() => s,
                _ => continue, // activate dropped here, calls Release
            };

            let id = DeviceId::new("webcam-windows", &native_id).encode();
            let is_connected = connected.contains_key(&native_id);
            result.push(DeviceInfo { id, name, connected: is_connected });
            // activate dropped here, calls Release
        }

        CoTaskMemFree(Some(devices_ptr.cast()));
        Ok(result)
    }
}

/// Returns the first `IKsPropertySet` found on any pin of the DirectShow
/// capture filter underlying the MF source.  The MF source wraps a DS filter
/// via COM aggregation, so `QueryInterface(IBaseFilter)` usually succeeds.
fn ks_prop_set_from_source(source: &IMFMediaSource) -> Option<IKsPropertySet> {
    let base_filter: IBaseFilter = source.cast().ok()?;
    let pin_enum: IEnumPins = unsafe { base_filter.EnumPins() }.ok()?;
    loop {
        let mut pin_buf = [None::<IPin>; 1];
        let mut fetched = 0u32;
        let hr = unsafe { pin_enum.Next(&mut pin_buf, Some(&mut fetched)) };
        if fetched == 0 { break; }
        if let Some(Some(pin)) = pin_buf.first() {
            if let Ok(pks) = pin.cast::<IKsPropertySet>() {
                return Some(pks);
            }
        }
        if hr.is_err() { break; }
    }
    None
}

fn connect_impl(
    native_id: &str,
    connected: &mut HashMap<String, DeviceState>,
) -> Result<(), CameraError> {
    if connected.contains_key(native_id) {
        return Ok(()); // idempotent
    }

    unsafe {
        let activate = find_activate(native_id)?;

        // Create IMFMediaSource from the activate object.
        let source: IMFMediaSource = activate.ActivateObject().map_err(win_err)?;

        // Build source reader with video processing enabled for format conversion.
        let mut reader_attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut reader_attrs, 1).map_err(win_err)?;
        let reader_attrs = reader_attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
        reader_attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .map_err(win_err)?;

        let reader: IMFSourceReader =
            MFCreateSourceReaderFromMediaSource(&source, &reader_attrs).map_err(win_err)?;

        let formats = enumerate_video_formats(&reader);
        if formats.is_empty() {
            return Err(CameraError::SdkError(0xA102_0003)); // no usable formats
        }
        let best_idx = select_best_format_index(&formats);
        let mt = &formats[best_idx].media_type;
        reader.SetCurrentMediaType(video_stream(), None, mt).map_err(win_err)?;
        let is_mjpeg = formats[best_idx].is_mjpeg;
        let width    = formats[best_idx].width;
        let height   = formats[best_idx].height;

        let video_proc_amp = source.cast::<IAMVideoProcAmp>().ok();
        let camera_control = source.cast::<IAMCameraControl>().ok();
        let ks_prop_set    = ks_prop_set_from_source(&source);

        // Detect PHOTO_INDEPENDENT stream for Path A photo capture.
        // We probe via a temporary IMFCaptureEngine on the same activate.
        // This must happen after ActivateObject — on some drivers the activate
        // can only be used once, so we probe BEFORE calling ActivateObject a
        // second time (we actually call DetachObject to reset it first).
        let photo_stream = probe_photo_stream(&activate);

        connected.insert(
            native_id.to_string(),
            DeviceState {
                reader,
                source,
                video_proc_amp,
                camera_control,
                ks_prop_set,
                formats,
                current_format_idx: best_idx,
                is_mjpeg,
                width,
                height,
                photo_stream,
                activate,
            },
        );
        Ok(())
    }
}

/// Attempts to probe for a PHOTO_INDEPENDENT stream using IMFCaptureEngine.
/// Returns `None` silently on any failure (probe is best-effort).
fn probe_photo_stream(activate: &IMFActivate) -> Option<PhotoStreamInfo> {
    // Some drivers disallow a second ActivateObject while the first session is
    // open. We accept that: if probing fails, we fall back to Path B.
    let result = unsafe { init_capture_engine_for_photo(activate) };
    result.ok()
}

/// Creates a temporary IMFCaptureEngine, waits for init, finds the best
/// PHOTO_INDEPENDENT media type, then immediately shuts the engine down.
unsafe fn init_capture_engine_for_photo(activate: &IMFActivate) -> Result<PhotoStreamInfo, CameraError> {
    let factory: IMFCaptureEngineClassFactory =
        CoCreateInstance(&CLSID_MFCaptureEngineClassFactory, None, CLSCTX_INPROC_SERVER)
            .map_err(win_err)?;
    let engine: IMFCaptureEngine = factory
        .CreateInstance::<IMFCaptureEngine>(&CLSID_MFCaptureEngineClassFactory)
        .map_err(win_err)?;

    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), CameraError>>(1);
    let event_cb: IMFCaptureEngineOnEventCallback = EngineEventCallback {
        init_tx: Mutex::new(Some(init_tx)),
    }
    .into();

    engine.Initialize(&event_cb, None, None, activate).map_err(win_err)?;

    // Pump messages while waiting for MF_CAPTURE_ENGINE_INITIALIZED.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match init_rx.try_recv() {
            Ok(Ok(())) => break,
            Ok(Err(e)) => return Err(e),
            Err(mpsc::TryRecvError::Disconnected) => return Err(CameraError::SdkError(0xDEAD_0020)),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if std::time::Instant::now() >= deadline {
            return Err(CameraError::SdkError(0xDEAD_0020));
        }
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let cap_source: IMFCaptureSource = engine.GetSource().map_err(win_err)?;
    let stream_count = cap_source.GetDeviceStreamCount().map_err(win_err)?;

    let mut photo_idx: Option<u32> = None;
    for i in 0..stream_count {
        if let Ok(cat) = cap_source.GetDeviceStreamCategory(i) {
            if cat == MF_CAPTURE_ENGINE_STREAM_CATEGORY_PHOTO_INDEPENDENT {
                photo_idx = Some(i);
                break;
            }
        }
    }

    let stream_idx = photo_idx.ok_or(CameraError::NotSupported)?;
    let info = build_photo_stream_info(&cap_source, stream_idx)?;

    // Engine drops here, releasing the device for IMFSourceReader.
    drop(cap_source);
    drop(engine);
    drop(factory);

    Ok(info)
}

/// Finds the highest-resolution media type on a PHOTO_INDEPENDENT stream.
unsafe fn build_photo_stream_info(
    cap_source: &IMFCaptureSource,
    stream_idx: u32,
) -> Result<PhotoStreamInfo, CameraError> {
    let mut best_mt: Option<IMFMediaType> = None;
    let mut best_pixels: u64 = 0;
    let mut best_w = 0u32;
    let mut best_h = 0u32;

    let mut type_idx = 0u32;
    loop {
        let mut mt: Option<IMFMediaType> = None;
        if cap_source
            .GetAvailableDeviceMediaType(stream_idx, type_idx, Some(&mut mt))
            .is_err()
        {
            break;
        }
        type_idx += 1;
        let Some(mt) = mt else { continue };
        let (w, h) = frame_size(&mt);
        let pixels = w as u64 * h as u64;
        if pixels > best_pixels {
            best_pixels = pixels;
            best_w = w;
            best_h = h;
            best_mt = Some(mt);
        }
    }

    let media_type = best_mt.ok_or(CameraError::SdkError(0xA102_0005))?;
    Ok(PhotoStreamInfo { stream_idx, media_type, width: best_w, height: best_h })
}

fn disconnect_impl(
    native_id: &str,
    connected: &mut HashMap<String, DeviceState>,
) -> Result<(), CameraError> {
    let state = connected
        .remove(native_id)
        .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;

    unsafe { let _ = state.source.Shutdown(); }
    // All COM interfaces in state are released via Drop.
    Ok(())
}

fn get_live_view_frame_impl(state: &DeviceState) -> Result<Vec<u8>, CameraError> {
    // ReadSample blocks until a frame is available (~33ms for 30fps).
    // Retry up to 10 times to skip stream ticks (gaps without payload).
    for _ in 0..10 {
        let mut flags: u32 = 0;
        let mut sample: Option<IMFSample> = None;

        unsafe {
            state
                .reader
                .ReadSample(video_stream(), 0, None, Some(&mut flags), None, Some(&mut sample))
                .map_err(win_err)?;
        }

        if flags & MF_SOURCE_READERF_ERROR != 0 {
            return Err(CameraError::SdkError(0xA102_0001));
        }
        if flags & MF_SOURCE_READERF_STREAMTICK != 0 {
            continue;
        }

        let Some(sample) = sample else { continue };

        let data = unsafe {
            let buffer = sample.ConvertToContiguousBuffer().map_err(win_err)?;
            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut current_len: u32 = 0;
            buffer.Lock(&mut data_ptr, None, Some(&mut current_len)).map_err(win_err)?;
            let bytes = std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec();
            let _ = buffer.Unlock();
            bytes
        };

        if state.is_mjpeg {
            return Ok(data);
        }
        return yuyv_to_jpeg(&data, state.width, state.height);
    }

    Err(CameraError::SdkError(0xA102_0002))
}

fn capture_photo_impl(state: &DeviceState) -> Result<Vec<u8>, CameraError> {
    if let Some(photo_stream) = &state.photo_stream {
        capture_photo_path_a(state, photo_stream)
    } else {
        // Path B: grab the current preview frame.
        get_live_view_frame_impl(state)
    }
}

/// Path A: spin up a temporary IMFCaptureEngine on the PHOTO_INDEPENDENT
/// stream, fire TakePhoto(), collect the JPEG, then shut the engine down.
///
/// The IMFSourceReader (live view) stays running on the VIDEO stream;
/// the engine only touches the PHOTO_INDEPENDENT stream. Both can coexist
/// on devices that expose separate streams.
fn capture_photo_path_a(state: &DeviceState, photo: &PhotoStreamInfo) -> Result<Vec<u8>, CameraError> {
    unsafe {
        let factory: IMFCaptureEngineClassFactory =
            CoCreateInstance(&CLSID_MFCaptureEngineClassFactory, None, CLSCTX_INPROC_SERVER)
                .map_err(win_err)?;
        let engine: IMFCaptureEngine = factory
            .CreateInstance::<IMFCaptureEngine>(&CLSID_MFCaptureEngineClassFactory)
            .map_err(win_err)?;

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), CameraError>>(1);
        let event_cb: IMFCaptureEngineOnEventCallback = EngineEventCallback {
            init_tx: Mutex::new(Some(init_tx)),
        }
        .into();

        engine.Initialize(&event_cb, None, None, &state.activate).map_err(win_err)?;

        // Pump messages while waiting for init.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match init_rx.try_recv() {
                Ok(Ok(())) => break,
                Ok(Err(e)) => return Err(e),
                Err(mpsc::TryRecvError::Disconnected) => return Err(CameraError::SdkError(0xDEAD_0020)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if std::time::Instant::now() >= deadline {
                return Err(CameraError::SdkError(0xDEAD_0020));
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Configure source to use the photo stream's media type.
        let cap_source: IMFCaptureSource = engine.GetSource().map_err(win_err)?;
        cap_source
            .SetCurrentDeviceMediaType(photo.stream_idx, &photo.media_type)
            .map_err(win_err)?;

        // Build the JPEG sink media type.
        let sink_mt: IMFMediaType = MFCreateMediaType().map_err(win_err)?;
        sink_mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Image).map_err(win_err)?;
        sink_mt.SetGUID(&MF_MT_SUBTYPE, &MFImageFormat_JPEG).map_err(win_err)?;
        let frame_size_packed = ((photo.width as u64) << 32) | (photo.height as u64);
        sink_mt.SetUINT64(&MF_MT_FRAME_SIZE, frame_size_packed).map_err(win_err)?;

        // Configure photo sink.
        let sink_raw: IMFCaptureSink =
            engine.GetSink(MF_CAPTURE_ENGINE_SINK_TYPE_PHOTO).map_err(win_err)?;
        let photo_sink: IMFCapturePhotoSink = sink_raw.cast().map_err(win_err)?;
        photo_sink.RemoveAllStreams().map_err(win_err)?;

        let mut dw_index: u32 = 0;
        photo_sink
            .AddStream(photo.stream_idx, &sink_mt, None, Some(&mut dw_index))
            .map_err(win_err)?;

        let pending: PendingPhoto = Arc::new(Mutex::new(None));
        let (photo_tx, photo_rx) = mpsc::sync_channel::<Result<Vec<u8>, CameraError>>(1);
        *pending.lock().unwrap() = Some(photo_tx);

        let photo_cb: IMFCaptureEngineOnSampleCallback = PhotoSampleCallback {
            pending: pending.clone(),
        }
        .into();
        photo_sink.SetSampleCallback(&photo_cb).map_err(win_err)?;

        engine.TakePhoto().map_err(win_err)?;

        // Pump messages while waiting for the photo callback (up to 10s).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let result = loop {
            match photo_rx.try_recv() {
                Ok(result) => break result,
                Err(mpsc::TryRecvError::Disconnected) => break Err(CameraError::SdkError(0xDEAD_0011)),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if std::time::Instant::now() >= deadline {
                break Err(CameraError::SdkError(0xDEAD_0021));
            }
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };

        // Engine drops here, releasing the PHOTO_INDEPENDENT stream.
        drop(photo_cb);
        drop(photo_sink);
        drop(sink_raw);
        drop(cap_source);
        drop(engine);

        result
    }
}

fn get_parameters_impl(state: &DeviceState) -> Result<Vec<CameraParameter>, CameraError> {
    let mut params = Vec::new();

    // Video format selection.
    if state.formats.len() > 1 {
        let options: Vec<ParameterOption> = state
            .formats
            .iter()
            .enumerate()
            .map(|(i, f)| ParameterOption { label: f.label(), value: i.to_string() })
            .collect();
        params.push(CameraParameter::Select {
            param_type: ParameterType::VideoStreamFormat,
            current:    state.current_format_idx.to_string(),
            options,
            disabled:   false,
        });
    }

    let exposure_is_auto = state.camera_control.as_ref().map_or(false, |cc| {
        let mut val = 0i32; let mut flags = 0i32;
        unsafe { cc.Get(CameraControl_Exposure.0, &mut val, &mut flags) }.is_ok()
            && flags & CameraControl_Flags_Auto.0 != 0
    });

    if let Some(vpa) = &state.video_proc_amp {
        let specs: &[(VideoProcAmpProperty, ParameterType, Option<ParameterType>)] = &[
            (VideoProcAmp_Brightness,           ParameterType::Brightness,           Some(ParameterType::BrightnessAuto)),
            (VideoProcAmp_Contrast,             ParameterType::Contrast,             Some(ParameterType::ContrastAuto)),
            (VideoProcAmp_Hue,                  ParameterType::Hue,                  Some(ParameterType::HueAuto)),
            (VideoProcAmp_Saturation,           ParameterType::Saturation,           Some(ParameterType::SaturationAuto)),
            (VideoProcAmp_Sharpness,            ParameterType::Sharpness,            None),
            (VideoProcAmp_Gamma,                ParameterType::Gamma,                None),
            (VideoProcAmp_WhiteBalance,         ParameterType::WhiteBalance,         Some(ParameterType::WhiteBalanceAuto)),
            // BacklightCompensation handled separately below (boolean per UVC spec).
            (VideoProcAmp_Gain,                 ParameterType::Gain,                 Some(ParameterType::GainAuto)),
        ];

        for &(prop, param_type, auto_type) in specs {
            let mut min = 0i32; let mut max = 0i32;
            let mut step = 0i32; let mut default = 0i32; let mut caps = 0i32;
            if unsafe { vpa.GetRange(prop.0, &mut min, &mut max, &mut step, &mut default, &mut caps) }.is_err() {
                continue;
            }
            let mut cur_value = 0i32; let mut cur_flags = 0i32;
            let current = if unsafe { vpa.Get(prop.0, &mut cur_value, &mut cur_flags) }.is_ok() {
                cur_value
            } else { default };

            let is_auto = auto_type.is_some()
                && caps & VideoProcAmp_Flags_Auto.0 != 0
                && cur_flags & VideoProcAmp_Flags_Auto.0 != 0;

            params.push(CameraParameter::Range {
                param_type, current, min, max, step,
                disabled: is_auto || (param_type == ParameterType::Gain && exposure_is_auto),
            });

            if let Some(auto_param_type) = auto_type {
                if caps & VideoProcAmp_Flags_Auto.0 != 0 {
                    params.push(CameraParameter::Boolean {
                        param_type: auto_param_type,
                        current:    is_auto,
                        disabled:   false,
                    });
                }
            }
        }

        // BacklightCompensation is boolean (0/1) per the UVC spec.
        let mut cur = 0i32; let mut flags = 0i32;
        if unsafe { vpa.Get(VideoProcAmp_BacklightCompensation.0, &mut cur, &mut flags) }.is_ok() {
            params.push(CameraParameter::Boolean {
                param_type: ParameterType::BacklightCompensation,
                current:    cur != 0,
                disabled:   false,
            });
        }
    }

    if let Some(cc) = &state.camera_control {
        let specs: &[(CameraControlProperty, ParameterType, Option<ParameterType>)] = &[
            (CameraControl_Pan,      ParameterType::Pan,      Some(ParameterType::PanAuto)),
            (CameraControl_Tilt,     ParameterType::Tilt,     Some(ParameterType::TiltAuto)),
            (CameraControl_Roll,     ParameterType::Roll,     Some(ParameterType::RollAuto)),
            (CameraControl_Zoom,     ParameterType::Zoom,     Some(ParameterType::ZoomAuto)),
            (CameraControl_Exposure, ParameterType::Exposure, Some(ParameterType::ExposureAuto)),
            (CameraControl_Focus,    ParameterType::Focus,    Some(ParameterType::FocusAuto)),
        ];

        let cc_start = params.len();

        for &(prop, param_type, auto_type) in specs {
            let mut min = 0i32; let mut max = 0i32;
            let mut step = 0i32; let mut default = 0i32; let mut caps = 0i32;
            if unsafe { cc.GetRange(prop.0, &mut min, &mut max, &mut step, &mut default, &mut caps) }.is_err() {
                continue;
            }
            let mut cur_value = 0i32; let mut cur_flags = 0i32;
            let current = if unsafe { cc.Get(prop.0, &mut cur_value, &mut cur_flags) }.is_ok() {
                cur_value
            } else { default };

            let is_auto = auto_type.is_some()
                && caps & CameraControl_Flags_Auto.0 != 0
                && cur_flags & CameraControl_Flags_Auto.0 != 0;

            params.push(CameraParameter::Range {
                param_type, current, min, max, step,
                disabled: is_auto,
            });

            if let Some(auto_param_type) = auto_type {
                if caps & CameraControl_Flags_Auto.0 != 0 {
                    params.push(CameraParameter::Boolean {
                        param_type: auto_param_type,
                        current:    is_auto,
                        disabled:   false,
                    });
                }
            }
        }

        // Disable Pan/Tilt/Roll when zoom is at its minimum (no room to pan/tilt).
        // Computed from the loop values to avoid a separate driver call.
        let zoom_is_min = params[cc_start..].iter().any(|p| {
            matches!(p, CameraParameter::Range {
                param_type: ParameterType::Zoom, current, min, ..
            } if current <= min)
        });
        if zoom_is_min {
            for p in &mut params[cc_start..] {
                if let CameraParameter::Range { param_type, disabled, .. } = p {
                    if matches!(param_type, ParameterType::Pan | ParameterType::Tilt | ParameterType::Roll) {
                        *disabled = true;
                    }
                }
            }
        }
    }

    // Power line frequency (anti-flicker) — ID 13 in PROPSETID_VIDCAP_VIDEOPROCAMP.
    // IAMVideoProcAmp only covers IDs 0–9; use IKsPropertySet on the DS capture pin.
    // ksproxy.ax routes the call to the correct processing-unit topology node.
    if let Some(pks) = &state.ks_prop_set {
        let mut s = KsVideoProcAmpS {
            prop_set: GUID::zeroed(), prop_id: 0, prop_flags: 0,
            value: 0, vpa_flags: 0, capabilities: 0,
        };
        let mut bytes_returned = 0u32;
        let hr = unsafe {
            pks.Get(
                &PROPSETID_VIDCAP_VIDEOPROCAMP,
                KSPROP_VIDPROCAMP_POWERLINE,
                std::ptr::null_mut(),
                0,
                &mut s as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<KsVideoProcAmpS>() as u32,
                &mut bytes_returned,
            )
        };
        if hr.is_ok() {
            let options = vec![
                ParameterOption { label: "Disabled".to_string(), value: "0".to_string() },
                ParameterOption { label: "50 Hz".to_string(),    value: "1".to_string() },
                ParameterOption { label: "60 Hz".to_string(),    value: "2".to_string() },
                ParameterOption { label: "Auto".to_string(),     value: "3".to_string() },
            ];
            params.push(CameraParameter::Select {
                param_type: ParameterType::PowerLineFrequency,
                current:    s.value.to_string(),
                options,
                disabled:   false,
            });
        }
    }

    Ok(params)
}

fn set_parameter_impl(
    state: &mut DeviceState,
    param_type: ParameterType,
    value: &str,
) -> Result<(), CameraError> {
    // Format switch — value is the format index as a string.
    if param_type == ParameterType::VideoStreamFormat {
        let idx: usize = value.parse().map_err(|_| CameraError::NotSupported)?;
        let fmt = state.formats.get(idx).ok_or(CameraError::NotSupported)?;
        unsafe {
            state.reader.SetCurrentMediaType(video_stream(), None, &fmt.media_type).map_err(win_err)?;
            let _ = state.reader.Flush(video_stream());
        }
        state.current_format_idx = idx;
        state.is_mjpeg = fmt.is_mjpeg;
        state.width    = fmt.width;
        state.height   = fmt.height;
        return Ok(());
    }

    // Power line frequency — set via IKsPropertySet on the DirectShow capture pin.
    if param_type == ParameterType::PowerLineFrequency {
        let int_val: i32 = value.parse().map_err(|_| CameraError::NotSupported)?;
        let pks = state.ks_prop_set.as_ref().ok_or(CameraError::NotSupported)?;
        let mut s = KsVideoProcAmpS {
            prop_set:   PROPSETID_VIDCAP_VIDEOPROCAMP,
            prop_id:    KSPROP_VIDPROCAMP_POWERLINE,
            prop_flags: KSPROPERTY_TYPE_SET,
            value:      int_val,
            vpa_flags:  2, // manual
            capabilities: 0,
        };
        unsafe {
            pks.Set(
                &PROPSETID_VIDCAP_VIDEOPROCAMP,
                KSPROP_VIDPROCAMP_POWERLINE,
                std::ptr::null_mut(),
                0,
                &mut s as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<KsVideoProcAmpS>() as u32,
            )
        }.map_err(win_err)?;
        return Ok(());
    }

    // Boolean (auto/manual) toggles — value "true" = auto, "false" = manual.
    let auto = value == "true";
    match param_type {
        ParameterType::BrightnessAuto   => return set_auto_vpa(state, VideoProcAmp_Brightness,           auto),
        ParameterType::ContrastAuto     => return set_auto_vpa(state, VideoProcAmp_Contrast,             auto),
        ParameterType::HueAuto          => return set_auto_vpa(state, VideoProcAmp_Hue,                  auto),
        ParameterType::SaturationAuto   => return set_auto_vpa(state, VideoProcAmp_Saturation,           auto),
        ParameterType::WhiteBalanceAuto => return set_auto_vpa(state, VideoProcAmp_WhiteBalance,         auto),
        ParameterType::GainAuto         => return set_auto_vpa(state, VideoProcAmp_Gain,                 auto),
        ParameterType::ExposureAuto     => return set_auto_cc(state,  CameraControl_Exposure,            auto),
        ParameterType::FocusAuto        => return set_auto_cc(state,  CameraControl_Focus,               auto),
        ParameterType::PanAuto          => return set_auto_cc(state,  CameraControl_Pan,                 auto),
        ParameterType::TiltAuto         => return set_auto_cc(state,  CameraControl_Tilt,                auto),
        ParameterType::RollAuto         => return set_auto_cc(state,  CameraControl_Roll,                auto),
        ParameterType::ZoomAuto         => return set_auto_cc(state,  CameraControl_Zoom,                auto),
        ParameterType::BacklightCompensation => {
            let vpa = state.video_proc_amp.as_ref().ok_or(CameraError::NotSupported)?;
            let val = if auto { 1i32 } else { 0i32 };
            return unsafe { vpa.Set(VideoProcAmp_BacklightCompensation.0, val, VideoProcAmp_Flags_Manual.0) }.map_err(win_err);
        }
        _ => {}
    }

    // Range params — value is a stringified integer.
    let int_val: i32 = value.parse().map_err(|_| CameraError::NotSupported)?;

    if let Some(vpa) = &state.video_proc_amp {
        if let Some(prop) = vpa_prop(param_type) {
            unsafe { vpa.Set(prop.0, int_val, VideoProcAmp_Flags_Manual.0) }.map_err(win_err)?;
            return Ok(());
        }
    }
    if let Some(cc) = &state.camera_control {
        if let Some(prop) = cc_prop(param_type) {
            unsafe { cc.Set(prop.0, int_val, CameraControl_Flags_Manual.0) }.map_err(win_err)?;
            return Ok(());
        }
    }

    Err(CameraError::NotSupported)
}

fn set_auto_vpa(state: &DeviceState, prop: VideoProcAmpProperty, auto: bool) -> Result<(), CameraError> {
    let vpa = state.video_proc_amp.as_ref().ok_or(CameraError::NotSupported)?;
    let mut cur_value = 0i32; let mut cur_flags = 0i32;
    unsafe { vpa.Get(prop.0, &mut cur_value, &mut cur_flags) }.ok();
    let flags = if auto { VideoProcAmp_Flags_Auto.0 } else { VideoProcAmp_Flags_Manual.0 };
    unsafe { vpa.Set(prop.0, cur_value, flags) }.map_err(win_err)
}

fn set_auto_cc(state: &DeviceState, prop: CameraControlProperty, auto: bool) -> Result<(), CameraError> {
    let cc = state.camera_control.as_ref().ok_or(CameraError::NotSupported)?;
    let mut cur_value = 0i32; let mut cur_flags = 0i32;
    unsafe { cc.Get(prop.0, &mut cur_value, &mut cur_flags) }.ok();
    let flags = if auto { CameraControl_Flags_Auto.0 } else { CameraControl_Flags_Manual.0 };
    unsafe { cc.Set(prop.0, cur_value, flags) }.map_err(win_err)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Finds the IMFActivate for the device whose symbolic link matches `native_id`.
unsafe fn find_activate(native_id: &str) -> Result<IMFActivate, CameraError> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).map_err(win_err)?;
    let attrs = attrs.ok_or(CameraError::SdkError(0xFFFF_FFFF))?;
    attrs
        .SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )
        .map_err(win_err)?;

    let mut devices_ptr: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFEnumDeviceSources(&attrs, &mut devices_ptr, &mut count).map_err(win_err)?;

    let mut found: Option<IMFActivate> = None;

    for i in 0..count as usize {
        let activate = match std::ptr::replace(devices_ptr.add(i), None) {
            Some(a) => a,
            None => continue,
        };

        if found.is_none() {
            if let Some(link) = read_string_attr(
                &activate,
                &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK,
            ) {
                if link == native_id {
                    found = Some(activate);
                    continue; // moved into found — not dropped
                }
            }
        }
        // activate dropped here for non-matching devices
    }

    CoTaskMemFree(Some(devices_ptr.cast()));
    found.ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))
}

/// Reads and frees an allocated string attribute from an IMFAttributes object.
unsafe fn read_string_attr(attrs: &IMFAttributes, key: &GUID) -> Option<String> {
    let mut ptr = PWSTR(std::ptr::null_mut());
    let mut len: u32 = 0;
    attrs.GetAllocatedString(key, &mut ptr, &mut len).ok()?;
    let s = ptr.to_string().ok();
    CoTaskMemFree(Some(ptr.0.cast()));
    s
}

/// Enumerates all MJPEG and YUY2 native types for the first video stream.
/// Deduplicates by (codec, width, height, fps).
unsafe fn enumerate_video_formats(reader: &IMFSourceReader) -> Vec<VideoFormatInfo> {
    let mut formats: Vec<VideoFormatInfo> = Vec::new();
    let mut index = 0u32;
    loop {
        let Ok(mt) = reader.GetNativeMediaType(video_stream(), index) else { break };
        index += 1;

        let subtype = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or(GUID::zeroed());
        let is_mjpeg = subtype == MFVideoFormat_MJPG;
        let is_yuv   = subtype == MFVideoFormat_YUY2;
        if !is_mjpeg && !is_yuv {
            continue;
        }

        let (width, height) = frame_size(&mt);
        let fps_packed = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let fps_num    = (fps_packed >> 32) as u32;
        let fps_den    = (fps_packed & 0xFFFF_FFFF) as u32;

        // Skip exact duplicates (same codec, resolution, fps).
        let is_dup = formats.iter().any(|f| {
            f.is_mjpeg == is_mjpeg
                && f.width   == width
                && f.height  == height
                && f.fps_num == fps_num
                && f.fps_den == fps_den
        });
        if !is_dup {
            formats.push(VideoFormatInfo { media_type: mt, is_mjpeg, width, height, fps_num, fps_den });
        }
    }

    // Sort: resolution descending, then MJPEG before YUV, then fps descending.
    formats.sort_by(|a, b| {
        let res_a = a.width * a.height;
        let res_b = b.width * b.height;
        res_b.cmp(&res_a)
            .then_with(|| b.is_mjpeg.cmp(&a.is_mjpeg))
            .then_with(|| {
                let fps_a = if a.fps_den > 0 { a.fps_num / a.fps_den } else { 0 };
                let fps_b = if b.fps_den > 0 { b.fps_num / b.fps_den } else { 0 };
                fps_b.cmp(&fps_a)
            })
    });

    // Keep only the first format per resolution (after sort, this is MJPEG > YUV, highest fps).
    let mut seen_resolutions: Vec<(u32, u32)> = Vec::new();
    formats.retain(|f| {
        if seen_resolutions.contains(&(f.width, f.height)) {
            false
        } else {
            seen_resolutions.push((f.width, f.height));
            true
        }
    });

    formats
}

/// Returns the index of the highest-resolution MJPEG format in an already-sorted
/// list, falling back to index 0 (highest-res YUV) if no MJPEG is present.
fn select_best_format_index(formats: &[VideoFormatInfo]) -> usize {
    formats.iter().position(|f| f.is_mjpeg).unwrap_or(0)
}

/// Extracts (width, height) from an MF_MT_FRAME_SIZE attribute (width<<32 | height).
unsafe fn frame_size(mt: &IMFMediaType) -> (u32, u32) {
    let packed = mt.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0x0000_0280_0000_01E0);
    let w = (packed >> 32) as u32;
    let h = (packed & 0xFFFF_FFFF) as u32;
    (w.max(1), h.max(1))
}

fn extract_raw_buffer(sample: &IMFSample) -> Result<Vec<u8>, CameraError> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer().map_err(win_err)?;
        let mut data_ptr: *mut u8 = std::ptr::null_mut();
        let mut current_len: u32 = 0;
        buffer.Lock(&mut data_ptr, None, Some(&mut current_len)).map_err(win_err)?;
        let bytes = std::slice::from_raw_parts(data_ptr, current_len as usize).to_vec();
        let _ = buffer.Unlock();
        Ok(bytes)
    }
}

/// Converts a YUY2 (YUYV) frame to a JPEG buffer.
///
/// YUY2 packs two pixels into 4 bytes: Y0 U0 Y1 V0.
fn yuyv_to_jpeg(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CameraError> {
    let mut rgb: Vec<u8> = Vec::with_capacity((width * height * 3) as usize);

    for chunk in data.chunks_exact(4) {
        let y0 = chunk[0] as f32;
        let u = chunk[1] as f32 - 128.0;
        let y1 = chunk[2] as f32;
        let v = chunk[3] as f32 - 128.0;

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

fn vpa_prop(pt: ParameterType) -> Option<VideoProcAmpProperty> {
    match pt {
        ParameterType::Brightness           => Some(VideoProcAmp_Brightness),
        ParameterType::Contrast             => Some(VideoProcAmp_Contrast),
        ParameterType::Hue                  => Some(VideoProcAmp_Hue),
        ParameterType::Saturation           => Some(VideoProcAmp_Saturation),
        ParameterType::Sharpness            => Some(VideoProcAmp_Sharpness),
        ParameterType::Gamma                => Some(VideoProcAmp_Gamma),
        ParameterType::WhiteBalance         => Some(VideoProcAmp_WhiteBalance),
        // BacklightCompensation is boolean — handled in the boolean SET block above.
        ParameterType::Gain                 => Some(VideoProcAmp_Gain),
        _ => None,
    }
}

fn cc_prop(pt: ParameterType) -> Option<CameraControlProperty> {
    match pt {
        ParameterType::Pan      => Some(CameraControl_Pan),
        ParameterType::Tilt     => Some(CameraControl_Tilt),
        ParameterType::Roll     => Some(CameraControl_Roll),
        ParameterType::Zoom     => Some(CameraControl_Zoom),
        ParameterType::Exposure => Some(CameraControl_Exposure),
        ParameterType::Focus    => Some(CameraControl_Focus),
        _ => None,
    }
}
