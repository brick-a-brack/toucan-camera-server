use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;

// The `#[implement]` macro emits `::windows_core::...` paths, so windows_core
// must be resolvable as a top-level crate name.
extern crate windows_core;

use windows::core::{implement, Interface, BSTR, GUID, HRESULT, PCWSTR, PWSTR, VARIANT};
use windows::Win32::Media::{IReferenceClock};
use windows::Win32::Media::DirectShow::{
    CameraControlProperty, IAMCameraControl, IAMStreamConfig, IAMVideoProcAmp,
    IBaseFilter, IBaseFilter_Impl, ICreateDevEnum, IEnumMediaTypes, IEnumPins,
    IEnumPins_Impl, IFilterGraph, IGraphBuilder, IMediaControl, IMediaFilter,
    IMediaFilter_Impl, IMediaSample, IMemAllocator, IMemInputPin, IMemInputPin_Impl,
    IPin, IPin_Impl,
    VideoProcAmpProperty,
    CameraControl_Exposure, CameraControl_Flags_Auto, CameraControl_Flags_Manual,
    CameraControl_Focus, CameraControl_Pan, CameraControl_Roll, CameraControl_Tilt,
    CameraControl_Zoom,
    VideoProcAmp_BacklightCompensation, VideoProcAmp_Brightness, VideoProcAmp_Contrast,
    VideoProcAmp_Flags_Auto, VideoProcAmp_Flags_Manual, VideoProcAmp_Gain, VideoProcAmp_Gamma,
    VideoProcAmp_Hue, VideoProcAmp_Saturation, VideoProcAmp_Sharpness, VideoProcAmp_WhiteBalance,
    ALLOCATOR_PROPERTIES, FILTER_INFO, FILTER_STATE, PIN_INFO, PIN_DIRECTION,
    PINDIR_INPUT, PINDIR_OUTPUT,
    State_Paused, State_Running, State_Stopped,
    VFW_E_ALREADY_CONNECTED, VFW_E_NOT_CONNECTED, VFW_E_TYPE_NOT_ACCEPTED,
};
use windows::Win32::Media::MediaFoundation::{
    AM_MEDIA_TYPE, CLSID_FilterGraph, CLSID_SystemDeviceEnum, CLSID_VideoInputDeviceCategory,
    FORMAT_VideoInfo, MEDIATYPE_Video, VIDEOINFOHEADER,
};

use windows::Win32::Foundation::{BOOL, E_FAIL, E_NOTIMPL, E_UNEXPECTED, S_FALSE, S_OK};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, IBindCtx, IEnumMoniker, IMoniker, IPersist, IPersist_Impl,
};
use windows::Win32::System::Com::StructuredStorage::IPropertyBag;
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
    MFMediaType_Image, MFVideoFormat_MJPG, MFVideoFormat_NV12, MFVideoFormat_YUY2,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum VideoCodec { Mjpeg, Yuy2, Nv12 }

struct VideoFormatInfo {
    media_type: IMFMediaType,
    codec:      VideoCodec,
    width:      u32,
    height:     u32,
    fps_num:    u32,
    fps_den:    u32,
}

impl VideoFormatInfo {
    fn label(&self) -> String {
        let codec = match self.codec {
            VideoCodec::Mjpeg => "MJPEG",
            VideoCodec::Yuy2  => "YUY2",
            VideoCodec::Nv12  => "NV12",
        };
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
    codec:               VideoCodec,
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
    let mut connected_ds: HashMap<String, DsState> = HashMap::new();

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
                let _ = reply.send(list_devices_impl(&connected, &connected_ds));
            }
            Ok(Command::Connect { native_id, reply }) => {
                if native_id.starts_with(DS_PREFIX) {
                    let _ = reply.send(ds_connect_impl(&native_id, &mut connected_ds));
                } else {
                    let _ = reply.send(connect_impl(&native_id, &mut connected));
                }
            }
            Ok(Command::Disconnect { native_id, reply }) => {
                if native_id.starts_with(DS_PREFIX) {
                    let _ = reply.send(ds_disconnect_impl(&native_id, &mut connected_ds));
                } else {
                    let _ = reply.send(disconnect_impl(&native_id, &mut connected));
                }
            }
            Ok(Command::IsConnected { native_id, reply }) => {
                let alive = if native_id.starts_with(DS_PREFIX) {
                    connected_ds.contains_key(&native_id)
                } else {
                    let alive = connected
                        .get(&native_id)
                        .map(|s| is_source_alive(&s.source))
                        .unwrap_or(false);
                    if !alive {
                        force_disconnect(&native_id, &mut connected);
                    }
                    alive
                };
                let _ = reply.send(alive);
            }
            Ok(Command::GetParameters { native_id, reply }) => {
                let result = if native_id.starts_with(DS_PREFIX) {
                    connected_ds
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(ds_get_parameters_impl)
                } else {
                    connected
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(get_parameters_impl)
                };
                let _ = reply.send(result);
            }
            Ok(Command::GetLiveViewFrame { native_id, reply }) => {
                let result = if native_id.starts_with(DS_PREFIX) {
                    connected_ds
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(ds_get_live_view_frame)
                } else {
                    let r = connected
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(get_live_view_frame_impl);
                    if r.is_err() {
                        let dead = connected
                            .get(&native_id)
                            .map(|s| !is_source_alive(&s.source))
                            .unwrap_or(false);
                        if dead { force_disconnect(&native_id, &mut connected); }
                    }
                    r
                };
                let _ = reply.send(result);
            }
            Ok(Command::SetParameter { native_id, param_type, value, reply }) => {
                let result = if native_id.starts_with(DS_PREFIX) {
                    connected_ds
                        .get_mut(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(|s| ds_set_parameter_impl(s, param_type, &value))
                } else {
                    connected
                        .get_mut(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(|state| set_parameter_impl(state, param_type, &value))
                };
                let _ = reply.send(result);
            }
            Ok(Command::CapturePhoto { native_id, reply }) => {
                let result = if native_id.starts_with(DS_PREFIX) {
                    connected_ds
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(ds_capture_photo)
                } else {
                    connected
                        .get(&native_id)
                        .ok_or(CameraError::NotConnected)
                        .and_then(capture_photo_impl)
                };
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
    for (_, state) in connected_ds.drain() {
        unsafe { let _ = state.control.Stop(); }
        drop(state);
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
    connected_ds: &HashMap<String, DsState>,
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
        let mut mf_native_ids: Vec<String> = Vec::with_capacity(count as usize);

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
            mf_native_ids.push(native_id);
            // activate dropped here, calls Release
        }

        CoTaskMemFree(Some(devices_ptr.cast()));

        // Append DirectShow-only cameras not exposed by Media Foundation.
        if let Ok(ds_devices) = ds_list_devices(connected_ds, &mf_native_ids) {
            result.extend(ds_devices);
        }

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
            return Err(CameraError::SdkError(0xA102_0003));
        }
        let best_idx = select_best_format_index(&formats);
        let mt = &formats[best_idx].media_type;
        reader.SetCurrentMediaType(video_stream(), None, mt).map_err(win_err)?;
        let codec  = formats[best_idx].codec;
        let width  = formats[best_idx].width;
        let height = formats[best_idx].height;

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
                codec,
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

        return match state.codec {
            VideoCodec::Mjpeg => Ok(data),
            VideoCodec::Yuy2  => yuyv_to_jpeg(&data, state.width, state.height),
            VideoCodec::Nv12  => nv12_to_jpeg(&data, state.width, state.height),
        };
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
        state.codec = fmt.codec;
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

/// Enumerates all MJPEG, YUY2, and NV12 native types for the first video stream.
/// Deduplicates by (codec, width, height, fps).
unsafe fn enumerate_video_formats(reader: &IMFSourceReader) -> Vec<VideoFormatInfo> {
    let mut formats: Vec<VideoFormatInfo> = Vec::new();
    let mut index = 0u32;
    loop {
        let Ok(mt) = reader.GetNativeMediaType(video_stream(), index) else { break };
        index += 1;

        let subtype = mt.GetGUID(&MF_MT_SUBTYPE).unwrap_or(GUID::zeroed());
        let (width, height) = frame_size(&mt);
        let fps_packed = mt.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(0);
        let fps_num    = (fps_packed >> 32) as u32;
        let fps_den    = (fps_packed & 0xFFFF_FFFF) as u32;
        let codec = if subtype == MFVideoFormat_MJPG {
            VideoCodec::Mjpeg
        } else if subtype == MFVideoFormat_YUY2 {
            VideoCodec::Yuy2
        } else if subtype == MFVideoFormat_NV12 {
            VideoCodec::Nv12
        } else {
            continue;
        };

        // Skip exact duplicates (same codec, resolution, fps).
        let is_dup = formats.iter().any(|f| {
            f.codec  == codec
                && f.width   == width
                && f.height  == height
                && f.fps_num == fps_num
                && f.fps_den == fps_den
        });
        if !is_dup {
            formats.push(VideoFormatInfo { media_type: mt, codec, width, height, fps_num, fps_den });
        }
    }

    // Codec priority: MJPEG=0, YUY2=1, NV12=2 (lower = preferred).
    let codec_rank = |c: VideoCodec| match c {
        VideoCodec::Mjpeg => 0u8,
        VideoCodec::Yuy2  => 1,
        VideoCodec::Nv12  => 2,
    };

    // Sort: resolution descending, then codec priority, then fps descending.
    formats.sort_by(|a, b| {
        let res_a = a.width * a.height;
        let res_b = b.width * b.height;
        res_b.cmp(&res_a)
            .then_with(|| codec_rank(a.codec).cmp(&codec_rank(b.codec)))
            .then_with(|| {
                let fps_a = if a.fps_den > 0 { a.fps_num / a.fps_den } else { 0 };
                let fps_b = if b.fps_den > 0 { b.fps_num / b.fps_den } else { 0 };
                fps_b.cmp(&fps_a)
            })
    });

    // Keep only the first format per resolution (best codec, highest fps).
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
/// list, falling back to index 0 (highest-res YUY2/NV12) if no MJPEG is present.
fn select_best_format_index(formats: &[VideoFormatInfo]) -> usize {
    formats.iter().position(|f| f.codec == VideoCodec::Mjpeg).unwrap_or(0)
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
    let pixel_count = (width * height) as usize;
    let mut rgb = vec![0u8; pixel_count * 3];
    let mut dst = 0usize;

    // Fixed-point YCbCr→RGB (BT.601 full-range), shift = 14 bits.
    // Coefficients: 1.402→22970, 0.344136→5638, 0.714136→11700, 1.772→29032
    for chunk in data.chunks_exact(4) {
        let y0 = chunk[0] as i32;
        let cb = chunk[1] as i32 - 128;
        let y1 = chunk[2] as i32;
        let cr = chunk[3] as i32 - 128;
        for y in [y0, y1] {
            rgb[dst]     = (y + ((22970 * cr) >> 14)).clamp(0, 255) as u8;
            rgb[dst + 1] = (y - ((5638  * cb) >> 14) - ((11700 * cr) >> 14)).clamp(0, 255) as u8;
            rgb[dst + 2] = (y + ((29032 * cb) >> 14)).clamp(0, 255) as u8;
            dst += 3;
        }
    }

    encode_rgb_to_jpeg(width, height, rgb)
}

/// Converts an NV12 frame to a JPEG buffer.
///
/// NV12 layout: Y plane (width×height bytes) followed by interleaved UV plane
/// (width×height/2 bytes, one U+V pair per 2×2 block).
fn nv12_to_jpeg(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, CameraError> {
    let (w, h) = (width as usize, height as usize);
    let y_size = w * h;
    if data.len() < y_size + w * h / 2 {
        return Err(CameraError::SdkError(0xDEAD_0003));
    }
    let y_plane  = &data[..y_size];
    let uv_plane = &data[y_size..];

    let mut rgb = vec![0u8; w * h * 3];
    let mut dst = 0usize;
    for row in 0..h {
        for col in 0..w {
            let y  = y_plane[row * w + col] as i32;
            let uv = (row / 2) * w + (col & !1);
            let cb = uv_plane[uv]     as i32 - 128;
            let cr = uv_plane[uv + 1] as i32 - 128;
            rgb[dst]     = (y + ((22970 * cr) >> 14)).clamp(0, 255) as u8;
            rgb[dst + 1] = (y - ((5638  * cb) >> 14) - ((11700 * cr) >> 14)).clamp(0, 255) as u8;
            rgb[dst + 2] = (y + ((29032 * cb) >> 14)).clamp(0, 255) as u8;
            dst += 3;
        }
    }

    encode_rgb_to_jpeg(width, height, rgb)
}

fn encode_rgb_to_jpeg(width: u32, height: u32, rgb: Vec<u8>) -> Result<Vec<u8>, CameraError> {
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

// ============================================================================
// DirectShow backend for cameras not exposed by Media Foundation
// ============================================================================

const DS_PREFIX: &str = "ds|";

// Unique CLSID for our custom sink filter (Chromium's sink filter GUID).
const CLSID_DS_SINK_FILTER: GUID =
    GUID::from_u128(0x88cdbbdc_a73b_4afa_acbf_15d5e2ce12c3);

// Standard DirectShow user-mode memory allocator (from uuids.h).
const CLSID_MEMORY_ALLOCATOR: GUID =
    GUID::from_u128(0x1e651cc0_b199_11d0_8212_00c04fc32c45);

// ---------------------------------------------------------------------------
// DirectShow format descriptor
// ---------------------------------------------------------------------------

#[allow(dead_code)]
struct DsFormat {
    codec:     VideoCodec,
    width:     u32,
    height:    u32,
    fps_num:   u32,
    fps_den:   u32,
    cap_index: i32,
}

impl DsFormat {
    #[allow(dead_code)]
    fn label(&self) -> String {
        let codec = match self.codec {
            VideoCodec::Mjpeg => "MJPEG",
            VideoCodec::Yuy2  => "YUY2",
            VideoCodec::Nv12  => "NV12",
        };
        let fps = if self.fps_den > 0 {
            format!(" {:.0}fps", self.fps_num as f64 / self.fps_den as f64)
        } else {
            String::new()
        };
        format!("{}×{} {}{}", self.width, self.height, codec, fps)
    }

    fn rank(&self) -> u64 {
        let codec_rank: u64 = match self.codec {
            VideoCodec::Mjpeg => 0,
            VideoCodec::Yuy2  => 1,
            VideoCodec::Nv12  => 2,
        };
        // Lower rank = preferred. Penalise higher codec index; favour higher resolution.
        codec_rank * 1_000_000_000_000
            + (u32::MAX as u64).saturating_sub((self.width * self.height) as u64)
    }
}

// ---------------------------------------------------------------------------
// Shared state between the custom COM filter and its input pin
// ---------------------------------------------------------------------------

struct DsSinkInner {
    // Most recent frame, already JPEG-encoded, written by the converter thread.
    latest_jpeg:   Mutex<Option<Vec<u8>>>,
    // Channel to the per-device JPEG converter thread.
    // Set to None on disconnect so the thread exits cleanly.
    raw_tx:        Mutex<Option<std::sync::mpsc::SyncSender<Vec<u8>>>>,
    // Written once in ReceiveConnection (during ConnectDirect), before streaming starts.
    codec:         Mutex<VideoCodec>,
    width:         Mutex<u32>,
    height:        Mutex<u32>,
    filter_state:  Mutex<FILTER_STATE>,
    clock:         Mutex<Option<IReferenceClock>>,
    connected_pin: Mutex<Option<IPin>>,
    // Set in JoinFilterGraph(Some); cleared in JoinFilterGraph(None) to break COM cycles.
    // Some source filters validate that the sink is in the same graph before connecting.
    graph:         Mutex<Option<IFilterGraph>>,
    parent_filter: Mutex<Option<IBaseFilter>>,
    sink_pin:      Mutex<Option<IPin>>,
}

// SAFETY: DsSinkInner lives inside Arc; all fields are accessed while holding
// their respective Mutex. COM interface pointers inside the Mutexes are only
// touched on the SDK thread.
unsafe impl Send for DsSinkInner {}
unsafe impl Sync for DsSinkInner {}

// ---------------------------------------------------------------------------
// IEnumPins — enumerates our single input pin
// ---------------------------------------------------------------------------

#[implement(IEnumPins)]
struct DsSinkEnumPins {
    pin: IPin,
    pos: Mutex<u32>,
}

impl IEnumPins_Impl for DsSinkEnumPins_Impl {
    fn Next(&self, cpins: u32, pppins: *mut Option<IPin>, pcfetched: *mut u32) -> HRESULT {
        let mut pos = self.pos.lock().unwrap();
        let buf = unsafe { std::slice::from_raw_parts_mut(pppins, cpins as usize) };
        let mut fetched = 0u32;
        for slot in buf.iter_mut() {
            if *pos == 0 {
                *slot = Some(self.pin.clone());
                *pos += 1;
                fetched += 1;
            } else {
                break;
            }
        }
        if !pcfetched.is_null() {
            unsafe { *pcfetched = fetched; }
        }
        if fetched == cpins { S_OK } else { S_FALSE }
    }

    fn Skip(&self, cpins: u32) -> windows::core::Result<()> {
        let mut pos = self.pos.lock().unwrap();
        let available = 1u32.saturating_sub(*pos);
        *pos += cpins.min(available);
        Ok(())
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.pos.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumPins> {
        let new_enum = DsSinkEnumPins {
            pin: self.pin.clone(),
            pos: Mutex::new(*self.pos.lock().unwrap()),
        };
        Ok(new_enum.into())
    }
}

// ---------------------------------------------------------------------------
// IPin + IMemInputPin — our single input pin
// ---------------------------------------------------------------------------

#[implement(IPin, IMemInputPin)]
struct DsSinkPinCom {
    inner: Arc<DsSinkInner>,
}

impl IPin_Impl for DsSinkPinCom_Impl {
    fn Connect(
        &self,
        _preceivepin: Option<&IPin>,
        _pmt: *const AM_MEDIA_TYPE,
    ) -> windows::core::Result<()> {
        // Input pins don't initiate connections.
        Err(windows::core::Error::from(E_UNEXPECTED))
    }

    fn ReceiveConnection(
        &self,
        pconnector: Option<&IPin>,
        pmt: *const AM_MEDIA_TYPE,
    ) -> windows::core::Result<()> {
        let mut guard = self.inner.connected_pin.lock().unwrap();
        if guard.is_some() {
            return Err(windows::core::Error::from(VFW_E_ALREADY_CONNECTED));
        }
        if pmt.is_null() {
            return Err(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED));
        }
        let mt = unsafe { &*pmt };
        if mt.majortype != MEDIATYPE_Video {
            return Err(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED));
        }
        // Validate subtype and extract dimensions from the format block.
        if mt.formattype != FORMAT_VideoInfo || mt.pbFormat.is_null() {
            return Err(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED));
        }
        let codec = if mt.subtype == MFVideoFormat_MJPG       { VideoCodec::Mjpeg }
                    else if mt.subtype == MFVideoFormat_YUY2   { VideoCodec::Yuy2  }
                    else if mt.subtype == MFVideoFormat_NV12   { VideoCodec::Nv12  }
                    else { return Err(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED)); };
        let vih = unsafe { &*(mt.pbFormat as *const VIDEOINFOHEADER) };
        let w = vih.bmiHeader.biWidth as u32;
        let h = vih.bmiHeader.biHeight.unsigned_abs();
        if w == 0 || h == 0 {
            return Err(windows::core::Error::from(VFW_E_TYPE_NOT_ACCEPTED));
        }
        *self.inner.codec.lock().unwrap()  = codec;
        *self.inner.width.lock().unwrap()  = w;
        *self.inner.height.lock().unwrap() = h;
        *guard = pconnector.map(|p| p.clone());
        Ok(())
    }

    fn Disconnect(&self) -> windows::core::Result<()> {
        let mut guard = self.inner.connected_pin.lock().unwrap();
        if guard.is_none() {
            return Err(windows::core::Error::from(VFW_E_NOT_CONNECTED));
        }
        *guard = None;
        Ok(())
    }

    fn ConnectedTo(&self) -> windows::core::Result<IPin> {
        self.inner.connected_pin.lock().unwrap()
            .clone()
            .ok_or_else(|| windows::core::Error::from(VFW_E_NOT_CONNECTED))
    }

    fn ConnectionMediaType(&self, _pmt: *mut AM_MEDIA_TYPE) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn QueryPinInfo(&self, pinfo: *mut PIN_INFO) -> windows::core::Result<()> {
        unsafe {
            let info = &mut *pinfo;
            info.dir = PINDIR_INPUT;
            info.achName = [0u16; 128];
            let name: Vec<u16> = "In\0".encode_utf16().collect();
            let len = name.len().min(128);
            info.achName[..len].copy_from_slice(&name[..len]);
            info.pFilter = std::mem::ManuallyDrop::new(
                self.inner.parent_filter.lock().unwrap().clone(),
            );
        }
        Ok(())
    }

    fn QueryDirection(&self) -> windows::core::Result<PIN_DIRECTION> {
        Ok(PINDIR_INPUT)
    }

    fn QueryId(&self) -> windows::core::Result<PWSTR> {
        let wide: Vec<u16> = "In\0".encode_utf16().collect();
        let nbytes = wide.len() * 2;
        unsafe {
            let ptr = CoTaskMemAlloc(nbytes) as *mut u16;
            if ptr.is_null() {
                return Err(windows::core::Error::from(E_FAIL));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            Ok(PWSTR(ptr))
        }
    }

    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT {
        if pmt.is_null() { return E_FAIL; }
        let mt = unsafe { &*pmt };
        if mt.majortype != MEDIATYPE_Video { return S_FALSE; }
        if mt.subtype == MFVideoFormat_MJPG
            || mt.subtype == MFVideoFormat_YUY2
            || mt.subtype == MFVideoFormat_NV12
        {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn EnumMediaTypes(&self) -> windows::core::Result<IEnumMediaTypes> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn QueryInternalConnections(
        &self,
        _appin: *mut Option<IPin>,
        _npin: *mut u32,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn EndOfStream(&self) -> windows::core::Result<()> { Ok(()) }
    fn BeginFlush(&self) -> windows::core::Result<()> { Ok(()) }
    fn EndFlush(&self) -> windows::core::Result<()> { Ok(()) }
    fn NewSegment(&self, _tstart: i64, _tstop: i64, _drate: f64) -> windows::core::Result<()> { Ok(()) }
}

impl IMemInputPin_Impl for DsSinkPinCom_Impl {
    fn GetAllocator(&self) -> windows::core::Result<IMemAllocator> {
        // KS proxy (WDM camera) drivers require a real allocator here; returning
        // E_NOTIMPL causes them to fail the connection with a driver-specific error
        // instead of falling back to their own allocator.
        unsafe {
            CoCreateInstance(&CLSID_MEMORY_ALLOCATOR, None, CLSCTX_INPROC_SERVER)
        }
    }

    fn NotifyAllocator(
        &self,
        _pallocator: Option<&IMemAllocator>,
        _breadonly: BOOL,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetAllocatorRequirements(&self) -> windows::core::Result<ALLOCATOR_PROPERTIES> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }

    fn Receive(&self, psample: Option<&IMediaSample>) -> windows::core::Result<()> {
        let sample = match psample {
            Some(s) => s,
            None => return Ok(()),
        };
        // Keep Receive fast: just copy the raw pixels and hand them off.
        // JPEG conversion runs on the dedicated ds-jpeg-converter thread so
        // neither the DS streaming thread nor the SDK actor thread is blocked.
        unsafe {
            let ptr = match sample.GetPointer() {
                Ok(p) if !p.is_null() => p,
                _ => return Ok(()),
            };
            let len = sample.GetActualDataLength();
            if len <= 0 { return Ok(()); }
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            // try_send: if the converter hasn't finished the previous frame yet,
            // drop this one rather than block the streaming thread.
            if let Some(ref tx) = *self.inner.raw_tx.lock().unwrap() {
                let _ = tx.try_send(data);
            }
        }
        Ok(())
    }

    fn ReceiveMultiple(
        &self,
        psamples: *const Option<IMediaSample>,
        nsamples: i32,
    ) -> windows::core::Result<i32> {
        let slice = unsafe { std::slice::from_raw_parts(psamples, nsamples as usize) };
        for sample in slice {
            let _ = self.Receive(sample.as_ref());
        }
        Ok(nsamples)
    }

    fn ReceiveCanBlock(&self) -> windows::core::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// IBaseFilter + IMediaFilter + IPersist — our custom sink filter
// ---------------------------------------------------------------------------

#[implement(IBaseFilter, IMediaFilter, IPersist)]
struct DsSinkFilterCom {
    inner: Arc<DsSinkInner>,
    pin:   IPin,
}

impl IPersist_Impl for DsSinkFilterCom_Impl {
    fn GetClassID(&self) -> windows::core::Result<GUID> {
        Ok(CLSID_DS_SINK_FILTER)
    }
}

impl IMediaFilter_Impl for DsSinkFilterCom_Impl {
    fn Stop(&self) -> windows::core::Result<()> {
        *self.inner.filter_state.lock().unwrap() = State_Stopped;
        Ok(())
    }

    fn Pause(&self) -> windows::core::Result<()> {
        *self.inner.filter_state.lock().unwrap() = State_Paused;
        Ok(())
    }

    fn Run(&self, _tstart: i64) -> windows::core::Result<()> {
        *self.inner.filter_state.lock().unwrap() = State_Running;
        Ok(())
    }

    fn GetState(&self, _dwmillisecstimeout: u32) -> windows::core::Result<FILTER_STATE> {
        Ok(*self.inner.filter_state.lock().unwrap())
    }

    fn SetSyncSource(&self, pclock: Option<&IReferenceClock>) -> windows::core::Result<()> {
        *self.inner.clock.lock().unwrap() = pclock.map(|c| c.clone());
        Ok(())
    }

    fn GetSyncSource(&self) -> windows::core::Result<IReferenceClock> {
        self.inner.clock.lock().unwrap()
            .clone()
            .ok_or_else(|| windows::core::Error::from(E_FAIL))
    }
}

impl IBaseFilter_Impl for DsSinkFilterCom_Impl {
    fn EnumPins(&self) -> windows::core::Result<IEnumPins> {
        Ok(DsSinkEnumPins { pin: self.pin.clone(), pos: Mutex::new(0) }.into())
    }

    fn FindPin(&self, id: &PCWSTR) -> windows::core::Result<IPin> {
        let id_str = unsafe { id.to_string().unwrap_or_default() };
        if id_str == "In" {
            Ok(self.pin.clone())
        } else {
            Err(windows::core::Error::from(E_FAIL))
        }
    }

    fn QueryFilterInfo(&self, pinfo: *mut FILTER_INFO) -> windows::core::Result<()> {
        unsafe {
            let info = &mut *pinfo;
            info.achName = [0u16; 128];
            let name: Vec<u16> = "ToucanSink\0".encode_utf16().collect();
            let len = name.len().min(128);
            info.achName[..len].copy_from_slice(&name[..len]);
            // Return the current graph (caller must Release). ManuallyDrop hands off
            // the AddRef'd clone without running Rust's drop.
            info.pGraph = std::mem::ManuallyDrop::new(
                self.inner.graph.lock().unwrap().clone(),
            );
        }
        Ok(())
    }

    fn JoinFilterGraph(
        &self,
        pgraph: Option<&IFilterGraph>,
        _pname: &PCWSTR,
    ) -> windows::core::Result<()> {
        if pgraph.is_none() {
            // Graph is being torn down — clear all back-references to break COM cycles.
            *self.inner.graph.lock().unwrap()         = None;
            *self.inner.parent_filter.lock().unwrap() = None;
            *self.inner.sink_pin.lock().unwrap()      = None;
        } else {
            *self.inner.graph.lock().unwrap() = pgraph.map(|g| g.clone());
        }
        Ok(())
    }

    fn QueryVendorInfo(&self) -> windows::core::Result<PWSTR> {
        Err(windows::core::Error::from(E_NOTIMPL))
    }
}

// ---------------------------------------------------------------------------
// Per-device DirectShow connection state
// ---------------------------------------------------------------------------

struct DsState {
    #[allow(dead_code)] // kept alive to maintain graph lifetime
    graph:          IGraphBuilder,
    control:        IMediaControl,
    sink_inner:     Arc<DsSinkInner>,
    video_proc_amp: Option<IAMVideoProcAmp>,
    camera_control: Option<IAMCameraControl>,
}

// DsState lives exclusively on the SDK thread; COM pointers don't need Send here.
// The sink_inner Arc<DsSinkInner> may be shared with DirectShow's streaming thread
// but DsSinkInner declares unsafe Send/Sync already.

// ---------------------------------------------------------------------------
// DirectShow helper functions
// ---------------------------------------------------------------------------

/// Create a DsSinkFilterCom + DsSinkPinCom pair sharing the given DsSinkInner.
/// Codec/width/height are populated later by ReceiveConnection during ConnectDirect.
/// Spawns a dedicated JPEG converter thread that runs until the channel is closed
/// (i.e. until `DsSinkInner::raw_tx` is set to None in ds_disconnect_impl).
fn create_ds_sink() -> (IBaseFilter, Arc<DsSinkInner>) {
    // Bounded at 1: Receive drops a raw frame rather than block the DS streaming
    // thread when the converter is still busy with the previous frame.
    let (raw_tx, raw_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);

    let inner = Arc::new(DsSinkInner {
        latest_jpeg:   Mutex::new(None),
        raw_tx:        Mutex::new(Some(raw_tx)),
        codec:         Mutex::new(VideoCodec::Yuy2),
        width:         Mutex::new(0),
        height:        Mutex::new(0),
        filter_state:  Mutex::new(State_Stopped),
        clock:         Mutex::new(None),
        connected_pin: Mutex::new(None),
        graph:         Mutex::new(None),
        parent_filter: Mutex::new(None),
        sink_pin:      Mutex::new(None),
    });

    // Converter thread: receives raw frames, encodes to JPEG, stores result.
    // Exits automatically when raw_tx is dropped (disconnect).
    {
        let inner2 = inner.clone();
        std::thread::Builder::new()
            .name("ds-jpeg-converter".into())
            .spawn(move || {
                while let Ok(raw) = raw_rx.recv() {
                    let codec  = *inner2.codec.lock().unwrap();
                    let width  = *inner2.width.lock().unwrap();
                    let height = *inner2.height.lock().unwrap();
                    let jpeg = match codec {
                        VideoCodec::Mjpeg => raw,
                        VideoCodec::Yuy2  => match yuyv_to_jpeg(&raw, width, height) {
                            Ok(j) => j, Err(_) => continue,
                        },
                        VideoCodec::Nv12  => match nv12_to_jpeg(&raw, width, height) {
                            Ok(j) => j, Err(_) => continue,
                        },
                    };
                    *inner2.latest_jpeg.lock().unwrap() = Some(jpeg);
                }
            })
            .ok();
    }
    let pin: IPin = DsSinkPinCom { inner: inner.clone() }.into();
    let filter: IBaseFilter = DsSinkFilterCom { inner: inner.clone(), pin: pin.clone() }.into();
    // Set back-references (cleared later in JoinFilterGraph(None)).
    *inner.parent_filter.lock().unwrap() = Some(filter.clone());
    *inner.sink_pin.lock().unwrap() = Some(pin);
    (filter, inner)
}

/// Read a string property from an IMoniker via IPropertyBag.
fn moniker_read_string(moniker: &IMoniker, prop: PCWSTR) -> Option<String> {
    let prop_bag: IPropertyBag =
        unsafe { moniker.BindToStorage(None::<&IBindCtx>, None::<&IMoniker>) }.ok()?;
    let mut var = VARIANT::default();
    unsafe { prop_bag.Read(prop, &mut var, None) }.ok()?;
    BSTR::try_from(&var).map(|b| b.to_string()).ok()
}

/// Returns the moniker's display name (e.g. `@device:sw:{...}\{...}`).
/// Used as a fallback device identifier for software virtual cameras that
/// don't register a DevicePath property (e.g. OBS Virtual Camera).
fn moniker_display_name(moniker: &IMoniker) -> Option<String> {
    let ptr = unsafe { moniker.GetDisplayName(None::<&IBindCtx>, None::<&IMoniker>) }.ok()?;
    let s = unsafe { ptr.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(ptr.0.cast())) };
    s
}

/// Enumerate DirectShow video-input devices that are NOT already exposed by MF.
/// mf_native_ids: symbolic links of devices already listed by MF (for deduplication).
fn ds_list_devices(
    connected_ds: &HashMap<String, DsState>,
    mf_native_ids: &[String],
) -> Result<Vec<DeviceInfo>, CameraError> {
    unsafe {
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
                .map_err(win_err)?;

        let mut enum_moniker: Option<IEnumMoniker> = None;
        // S_FALSE means no devices in category — not an error.
        let _ = dev_enum.CreateClassEnumerator(
            &CLSID_VideoInputDeviceCategory,
            &mut enum_moniker,
            0,
        );
        let enum_moniker = match enum_moniker {
            Some(e) => e,
            None => return Ok(Vec::new()),
        };

        let mut result = Vec::new();
        loop {
            let mut buf = [None::<IMoniker>; 1];
            let mut fetched = 0u32;
            let hr = enum_moniker.Next(&mut buf, Some(&mut fetched));
            if fetched == 0 { break; }
            let moniker = match buf[0].take() { Some(m) => m, None => break };

            // Software virtual cameras (e.g. OBS Virtual Camera) often don't
            // register a DevicePath property. Fall back to the display name
            // (e.g. `@device:sw:{860BB310-...}\{A3FCE0F5-...}`), which is always
            // unique and stable for a registered filter.
            let device_path = match moniker_read_string(&moniker, windows::core::w!("DevicePath"))
                .filter(|s| !s.is_empty())
                .or_else(|| moniker_display_name(&moniker).filter(|s| !s.is_empty()))
            {
                Some(s) => s,
                None => {
                    if hr == S_FALSE { break; }
                    continue;
                }
            };

            // Skip devices already enumerated by MF (same device instance).
            let ds_prefix = device_instance_prefix(&device_path);
            if mf_native_ids.iter().any(|id| {
                device_instance_prefix(id).eq_ignore_ascii_case(&ds_prefix)
            }) {
                if hr == S_FALSE { break; }
                continue;
            }

            let name = moniker_read_string(&moniker, windows::core::w!("FriendlyName"))
                .unwrap_or_else(|| "Unknown".to_string());

            let native_id = format!("{}{}", DS_PREFIX, device_path);
            let id = DeviceId::new("webcam-windows", &native_id).encode();
            let connected = connected_ds.contains_key(&native_id);
            result.push(DeviceInfo { id, name, connected });

            if hr == S_FALSE { break; }
        }
        Ok(result)
    }
}

/// Extract the device-instance portion of a symbolic link / device path —
/// the part before the last category GUID (`#{...}`).  Used for cross-API
/// deduplication between MF symbolic links and DS DevicePath values.
fn device_instance_prefix(path: &str) -> String {
    let lower = path.to_lowercase();
    if let Some(pos) = lower.rfind("#{") {
        path[..pos].to_lowercase()
    } else {
        lower
    }
}

/// Enumerate supported video formats via IAMStreamConfig.
fn ds_enumerate_formats(stream_config: &IAMStreamConfig) -> Vec<DsFormat> {
    let mut formats = Vec::new();
    unsafe {
        let mut count = 0i32;
        let mut caps_size = 0i32;
        if stream_config.GetNumberOfCapabilities(&mut count, &mut caps_size).is_err() {
            return formats;
        }
        let mut caps_buf: Vec<u8> = vec![0u8; caps_size.max(0) as usize];

        for i in 0..count {
            let mut pmt: *mut AM_MEDIA_TYPE = std::ptr::null_mut();
            if stream_config
                .GetStreamCaps(i, &mut pmt, caps_buf.as_mut_ptr())
                .is_err()
            {
                continue;
            }
            if pmt.is_null() { continue; }

            let result = (|| {
                let mt = &*pmt;
                if mt.majortype != MEDIATYPE_Video { return None; }
                let codec = if mt.subtype == MFVideoFormat_MJPG      { VideoCodec::Mjpeg }
                            else if mt.subtype == MFVideoFormat_YUY2  { VideoCodec::Yuy2  }
                            else if mt.subtype == MFVideoFormat_NV12  { VideoCodec::Nv12  }
                            else { return None; };
                if mt.formattype != FORMAT_VideoInfo || mt.pbFormat.is_null() { return None; }
                let vih = &*(mt.pbFormat as *const VIDEOINFOHEADER);
                let w = vih.bmiHeader.biWidth as u32;
                let h = vih.bmiHeader.biHeight.unsigned_abs();
                if w == 0 || h == 0 { return None; }
                let avg_tpf = vih.AvgTimePerFrame as u32;
                let (fps_num, fps_den) = if avg_tpf > 0 { (10_000_000u32, avg_tpf) } else { (0u32, 1u32) };
                Some(DsFormat { codec, width: w, height: h, fps_num, fps_den, cap_index: i })
            })();

            ds_free_media_type(pmt);

            if let Some(f) = result { formats.push(f); }
        }
    }
    formats.sort_by_key(|f| f.rank());
    formats
}

/// Free an AM_MEDIA_TYPE returned by IAMStreamConfig::GetStreamCaps.
unsafe fn ds_free_media_type(pmt: *mut AM_MEDIA_TYPE) {
    if pmt.is_null() { return; }
    let mt = &*pmt;
    if !mt.pbFormat.is_null() {
        CoTaskMemFree(Some(mt.pbFormat.cast()));
    }
    CoTaskMemFree(Some(pmt.cast()));
}

/// Apply the given format to the capture pin via IAMStreamConfig::SetFormat.
fn ds_apply_format(
    stream_config: &IAMStreamConfig,
    fmt: &DsFormat,
) -> Result<(), CameraError> {
    unsafe {
        let mut count = 0i32;
        let mut caps_size = 0i32;
        stream_config.GetNumberOfCapabilities(&mut count, &mut caps_size).map_err(win_err)?;
        let mut caps_buf: Vec<u8> = vec![0u8; caps_size.max(0) as usize];

        let mut pmt: *mut AM_MEDIA_TYPE = std::ptr::null_mut();
        stream_config
            .GetStreamCaps(fmt.cap_index, &mut pmt, caps_buf.as_mut_ptr())
            .map_err(win_err)?;
        if pmt.is_null() { return Err(CameraError::SdkError(0xDEAD_0040)); }

        let result = stream_config.SetFormat(pmt as *const _).map_err(win_err);
        ds_free_media_type(pmt);
        result
    }
}

/// Find the first output pin on a DirectShow filter.
fn ds_first_output_pin(filter: &IBaseFilter) -> Result<IPin, CameraError> {
    let pin_enum = unsafe { filter.EnumPins() }.map_err(win_err)?;
    loop {
        let mut buf = [None::<IPin>; 1];
        let mut fetched = 0u32;
        let hr = unsafe { pin_enum.Next(&mut buf, Some(&mut fetched)) };
        if fetched == 0 { break; }
        if let Some(Some(pin)) = buf.first() {
            if let Ok(dir) = unsafe { pin.QueryDirection() } {
                if dir == PINDIR_OUTPUT { return Ok(pin.clone()); }
            }
        }
        if hr == S_FALSE { break; }
    }
    Err(CameraError::SdkError(0xDEAD_0041))
}

/// Find a DirectShow moniker for the camera with the given device path.
fn ds_find_moniker(device_path: &str) -> Result<IMoniker, CameraError> {
    unsafe {
        let dev_enum: ICreateDevEnum =
            CoCreateInstance(&CLSID_SystemDeviceEnum, None, CLSCTX_INPROC_SERVER)
                .map_err(win_err)?;

        let mut enum_moniker: Option<IEnumMoniker> = None;
        let _ = dev_enum.CreateClassEnumerator(
            &CLSID_VideoInputDeviceCategory,
            &mut enum_moniker,
            0,
        );
        let enum_moniker = enum_moniker.ok_or(CameraError::SdkError(0xFFFF_FFFE))?;

        loop {
            let mut buf = [None::<IMoniker>; 1];
            let mut fetched = 0u32;
            let hr = enum_moniker.Next(&mut buf, Some(&mut fetched));
            if fetched == 0 { break; }
            let moniker = match buf[0].take() { Some(m) => m, None => break };

            let candidate = moniker_read_string(&moniker, windows::core::w!("DevicePath"))
                .filter(|s| !s.is_empty())
                .or_else(|| moniker_display_name(&moniker).filter(|s| !s.is_empty()));
            if let Some(path) = candidate {
                if path.eq_ignore_ascii_case(device_path) {
                    return Ok(moniker);
                }
            }
            if hr == S_FALSE { break; }
        }
        Err(CameraError::SdkError(0xFFFF_FFFD))
    }
}

// ---------------------------------------------------------------------------
// DirectShow device operations (run exclusively on the SDK thread)
// ---------------------------------------------------------------------------

fn ds_connect_impl(
    native_id: &str,
    connected_ds: &mut HashMap<String, DsState>,
) -> Result<(), CameraError> {
    if connected_ds.contains_key(native_id) { return Ok(()); }

    let device_path = native_id.strip_prefix(DS_PREFIX).unwrap_or(native_id);

    unsafe {
        let moniker = ds_find_moniker(device_path)?;
        let capture_filter: IBaseFilter =
            moniker.BindToObject(None::<&IBindCtx>, None::<&IMoniker>).map_err(win_err)?;

        // Create the graph and add the capture filter FIRST so the driver's
        // JoinFilterGraph fires before we enumerate pins or call SetFormat.
        // Some drivers recreate their pins on JoinFilterGraph, which would
        // invalidate a pin reference obtained before AddFilter.
        let graph: IGraphBuilder =
            CoCreateInstance(&CLSID_FilterGraph, None, CLSCTX_INPROC_SERVER).map_err(win_err)?;
        graph.AddFilter(&capture_filter, windows::core::w!("Capture")).map_err(win_err)?;

        // Now that the filter is in the graph, enumerate its output pin.
        let output_pin = ds_first_output_pin(&capture_filter)?;
        let stream_config: IAMStreamConfig = output_pin.cast().map_err(win_err)?;

        let formats = ds_enumerate_formats(&stream_config);
        if formats.is_empty() {
            return Err(CameraError::SdkError(0xA102_0003));
        }

        // Try to pre-select the preferred format. Some drivers reject SetFormat —
        // that is non-fatal; the actual format is captured in ReceiveConnection.
        let _ = ds_apply_format(&stream_config, &formats[0]);

        let video_proc_amp = capture_filter.cast::<IAMVideoProcAmp>().ok();
        let camera_control = capture_filter.cast::<IAMCameraControl>().ok();

        // Codec/width/height are populated by ReceiveConnection during ConnectDirect.
        let (sink_filter, sink_inner) = create_ds_sink();

        graph.AddFilter(&sink_filter, windows::core::w!("Sink")).map_err(win_err)?;

        let sink_pin = sink_inner.sink_pin.lock().unwrap().clone()
            .ok_or(CameraError::SdkError(0xDEAD_0042))?;

        graph.ConnectDirect(&output_pin, &sink_pin, None).map_err(win_err)?;

        let control: IMediaControl = graph.cast().map_err(win_err)?;
        control.Run().map_err(win_err)?;

        connected_ds.insert(native_id.to_string(), DsState {
            graph, control, sink_inner,
            video_proc_amp, camera_control,
        });
        Ok(())
    }
}

fn ds_disconnect_impl(
    native_id: &str,
    connected_ds: &mut HashMap<String, DsState>,
) -> Result<(), CameraError> {
    let state = connected_ds.remove(native_id).ok_or(CameraError::NotConnected)?;
    // Close the raw frame channel before stopping the graph so the converter
    // thread exits cleanly rather than waiting for more frames that will never come.
    *state.sink_inner.raw_tx.lock().unwrap() = None;
    unsafe {
        let _ = state.control.Stop();
    }
    drop(state); // releases graph → triggers JoinFilterGraph(None) on our filter
    Ok(())
}

fn ds_get_live_view_frame(state: &DsState) -> Result<Vec<u8>, CameraError> {
    state.sink_inner.latest_jpeg.lock().unwrap()
        .clone()
        .ok_or(CameraError::SdkError(0x0000_A102)) // not ready yet
}

fn ds_capture_photo(state: &DsState) -> Result<Vec<u8>, CameraError> {
    // Capture the most recent frame from the live stream.
    ds_get_live_view_frame(state)
}

fn ds_get_parameters_impl(state: &DsState) -> Result<Vec<CameraParameter>, CameraError> {
    let mut params = Vec::new();
    if let Some(ref vpa) = state.video_proc_amp {
        ds_collect_vpa_params(vpa, &mut params);
    }
    if let Some(ref cc) = state.camera_control {
        ds_collect_cc_params(cc, &mut params);
    }
    Ok(params)
}

fn ds_collect_vpa_params(vpa: &IAMVideoProcAmp, out: &mut Vec<CameraParameter>) {
    let props = [
        (ParameterType::Brightness,   VideoProcAmp_Brightness),
        (ParameterType::Contrast,     VideoProcAmp_Contrast),
        (ParameterType::Hue,          VideoProcAmp_Hue),
        (ParameterType::Saturation,   VideoProcAmp_Saturation),
        (ParameterType::Sharpness,    VideoProcAmp_Sharpness),
        (ParameterType::Gamma,        VideoProcAmp_Gamma),
        (ParameterType::WhiteBalance, VideoProcAmp_WhiteBalance),
        (ParameterType::Gain,         VideoProcAmp_Gain),
    ];
    for (pt, prop) in props {
        unsafe {
            let mut min = 0i32; let mut max = 0i32; let mut step = 0i32;
            let mut def = 0i32; let mut caps = 0i32;
            if vpa.GetRange(prop.0, &mut min, &mut max, &mut step, &mut def, &mut caps).is_err() { continue; }
            let mut cur = 0i32; let mut flags = 0i32;
            if vpa.Get(prop.0, &mut cur, &mut flags).is_err() { continue; }
            let is_auto = caps & VideoProcAmp_Flags_Auto.0 != 0
                && flags & VideoProcAmp_Flags_Auto.0 != 0;
            out.push(CameraParameter::Range { param_type: pt, current: cur, min, max, step, disabled: is_auto });
        }
    }
}

fn ds_collect_cc_params(cc: &IAMCameraControl, out: &mut Vec<CameraParameter>) {
    let props = [
        (ParameterType::Pan,      CameraControl_Pan),
        (ParameterType::Tilt,     CameraControl_Tilt),
        (ParameterType::Roll,     CameraControl_Roll),
        (ParameterType::Zoom,     CameraControl_Zoom),
        (ParameterType::Exposure, CameraControl_Exposure),
        (ParameterType::Focus,    CameraControl_Focus),
    ];
    for (pt, prop) in props {
        unsafe {
            let mut min = 0i32; let mut max = 0i32; let mut step = 0i32;
            let mut def = 0i32; let mut caps = 0i32;
            if cc.GetRange(prop.0, &mut min, &mut max, &mut step, &mut def, &mut caps).is_err() { continue; }
            let mut cur = 0i32; let mut flags = 0i32;
            if cc.Get(prop.0, &mut cur, &mut flags).is_err() { continue; }
            let is_auto = caps & CameraControl_Flags_Auto.0 != 0
                && flags & CameraControl_Flags_Auto.0 != 0;
            out.push(CameraParameter::Range { param_type: pt, current: cur, min, max, step, disabled: is_auto });
        }
    }
}

fn ds_set_parameter_impl(
    state: &mut DsState,
    param_type: ParameterType,
    value: &str,
) -> Result<(), CameraError> {
    if let Some(ref vpa) = state.video_proc_amp {
        if let Some(prop) = vpa_prop(param_type) {
            let v: i32 = value.parse().map_err(|_| CameraError::SdkError(0x8007_0057))?;
            unsafe { vpa.Set(prop.0, v, VideoProcAmp_Flags_Manual.0).map_err(win_err)?; }
            return Ok(());
        }
    }
    if let Some(ref cc) = state.camera_control {
        if let Some(prop) = cc_prop(param_type) {
            let v: i32 = value.parse().map_err(|_| CameraError::SdkError(0x8007_0057))?;
            unsafe { cc.Set(prop.0, v, CameraControl_Flags_Manual.0).map_err(win_err)?; }
            return Ok(());
        }
    }
    Err(CameraError::NotSupported)
}
