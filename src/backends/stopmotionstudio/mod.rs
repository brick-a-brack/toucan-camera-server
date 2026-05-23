//! Stop Motion Studio backend — relays cameras exposed by Stop Motion Studio
//! "remote camera" servers over HTTP.
//!
//! A remote camera server (the Stop Motion Studio Remote Camera app running on
//! another device) speaks the protocol documented in
//! `docs/remote-camera-protocol.md`: plain HTTP, every endpoint is a `POST`,
//! parameters travel in the query string, and `/status` returns the camera
//! state as JSON. This backend drives that protocol as a client so the remote
//! camera appears in `/cameras` like any local device.
//!
//! ## Identity scheme
//! One remote camera server exposes one camera, so a native ID is simply the
//! server's normalized base URL (e.g. `http://192.168.1.14:2222`). The route
//! layer wraps it as `base64url("stopmotionstudio:<url>")`.
//!
//! ## Sync trait over async HTTP
//! Same pattern as the [`remote`] backend: a dedicated multi-threaded tokio
//! runtime, with each synchronous trait method spawning an owned future and
//! blocking on a `std::sync::mpsc` reply.
//!
//! ## Live view
//! There is no MJPEG stream — `POST /preview` returns a single JPEG per call.
//! `get_live_view_frame` therefore performs one request per frame; the shared
//! capture loop polls it (capped at 30 fps).
//!
//! [`remote`]: crate::backends::remote

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tokio::runtime::Runtime;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterType,
};
use crate::peers::{PeerKind, PeerRegistry};

/// "Not ready yet" code reused from the Canon EVF path. The shared capture loop
/// treats it as a skippable frame instead of a fatal error.
const NOT_READY: CameraError = CameraError::SdkError(0x0000_A102);

const TIMEOUT_STATUS: Duration = Duration::from_secs(5);
const TIMEOUT_CONTROL: Duration = Duration::from_secs(10);
const TIMEOUT_PREVIEW: Duration = Duration::from_secs(10);

/// Fractional camera values (zoom factor, lens position) are exposed as integer
/// `Range` parameters by multiplying with this scale, and divided back before
/// being sent to the camera.
const FLOAT_SCALE: f64 = 1000.0;

/// Default preview resolution used until `/status` reports the real one.
const DEFAULT_RESOLUTION: (u32, u32) = (1280, 720);

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct StopMotionStudioBackend {
    rt: Runtime,
    client: reqwest::Client,
    registry: Arc<PeerRegistry>,
    /// Native IDs (server URLs) we have an open session with.
    connected: Mutex<HashSet<String>>,
    /// Last known capture resolution per native ID, used to size preview requests.
    resolutions: Mutex<HashMap<String, (u32, u32)>>,
}

impl StopMotionStudioBackend {
    pub fn new(registry: Arc<PeerRegistry>) -> Result<Self, CameraError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("stopmotion-backend")
            .build()
            .map_err(|e| CameraError::Remote(format!("failed to build runtime: {e}")))?;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .build()
            .map_err(|e| CameraError::Remote(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            rt,
            client,
            registry,
            connected: Mutex::new(HashSet::new()),
            resolutions: Mutex::new(HashMap::new()),
        })
    }

    /// Spawns an owned future on the dedicated runtime and blocks until it
    /// resolves. Drives async HTTP from synchronous trait methods.
    fn block_on<T, F>(&self, fut: F) -> Result<T, CameraError>
    where
        T: Send + 'static,
        F: std::future::Future<Output = Result<T, CameraError>> + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv()
            .unwrap_or_else(|_| Err(CameraError::Remote("backend task did not complete".into())))
    }

    /// Returns `(base_url, token)` for a native ID (which is the server URL).
    fn route(&self, native_id: &str) -> (String, Option<String>) {
        let token = self.registry.token_for(native_id);
        (native_id.to_string(), token)
    }

    /// Caches the capture resolution reported by a status payload.
    fn cache_resolution(&self, native_id: &str, status: &SmsStatus) {
        if status.width > 0 && status.height > 0 {
            self.resolutions
                .lock()
                .unwrap()
                .insert(native_id.to_string(), (status.width, status.height));
        }
    }

    fn resolution(&self, native_id: &str) -> (u32, u32) {
        self.resolutions
            .lock()
            .unwrap()
            .get(native_id)
            .copied()
            .unwrap_or(DEFAULT_RESOLUTION)
    }
}

impl CameraBackend for StopMotionStudioBackend {
    fn backend_id(&self) -> &str {
        "stopmotionstudio"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let connected = self.connected.lock().unwrap();
        Ok(self
            .registry
            .routing_snapshot(PeerKind::Stopmotion)
            .into_iter()
            .map(|(url, _token)| DeviceInfo {
                id: DeviceId::new("stopmotionstudio", &url).encode(),
                name: format!("Stop Motion Studio Camera ({})", host_port(&url)),
                connected: connected.contains(&url),
            })
            .collect())
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        // Verify the camera is reachable and learn its capture resolution.
        let (base, token) = self.route(native_id);
        let client = self.client.clone();
        let status = self.block_on(fetch_status(client, base, token))?;
        self.cache_resolution(native_id, &status);
        self.connected.lock().unwrap().insert(native_id.to_string());
        Ok(())
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        self.connected.lock().unwrap().remove(native_id);
        Ok(())
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.connected.lock().unwrap().contains(native_id)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (base, token) = self.route(native_id);
        let client = self.client.clone();
        let status = self.block_on(fetch_status(client, base, token))?;
        self.cache_resolution(native_id, &status);
        Ok(status.to_parameters())
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let query = build_set_query(param_type, value)?;
        let (base, token) = self.route(native_id);
        let url = format!("{base}{query}");
        let client = self.client.clone();
        self.block_on(async move {
            let resp = auth(client.post(&url), &token)
                .timeout(TIMEOUT_CONTROL)
                .send()
                .await
                .map_err(|e| CameraError::Remote(e.to_string()))?;
            status_error(resp.status())
        })
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (base, token) = self.route(native_id);
        let (w, h) = self.resolution(native_id);
        let client = self.client.clone();
        match self.block_on(fetch_preview(client, base, token, w, h)) {
            Ok(bytes) => Ok(bytes),
            // Treat any transient failure as "not ready" so the capture loop
            // skips the frame instead of tearing down the stream.
            Err(_) => Err(NOT_READY),
        }
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        // There is no dedicated capture endpoint; a full-resolution preview is
        // the captured frame.
        let (base, token) = self.route(native_id);
        let (w, h) = self.resolution(native_id);
        let client = self.client.clone();
        self.block_on(fetch_preview(client, base, token, w, h))
    }
}

// ---------------------------------------------------------------------------
// HTTP helpers — owned-argument async functions so spawned futures are 'static
// ---------------------------------------------------------------------------

fn auth(builder: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => builder.bearer_auth(t),
        None => builder,
    }
}

/// Maps a non-success status to a `CameraError`; returns `Ok(())` on success.
fn status_error(status: reqwest::StatusCode) -> Result<(), CameraError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(CameraError::Remote(format!(
            "camera returned HTTP {}",
            status.as_u16()
        )))
    }
}

async fn fetch_status(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
) -> Result<SmsStatus, CameraError> {
    let resp = auth(client.post(format!("{base}/status")), &token)
        .timeout(TIMEOUT_STATUS)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    status_error(resp.status())?;
    resp.json::<SmsStatus>()
        .await
        .map_err(|e| CameraError::Remote(format!("decoding status failed: {e}")))
}

async fn fetch_preview(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, CameraError> {
    let url = format!("{base}/preview?Width={width}&Height={height}&Format=JPG");
    let resp = auth(client.post(&url), &token)
        .timeout(TIMEOUT_PREVIEW)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    status_error(resp.status())?;
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| CameraError::Remote(e.to_string()))
}

/// Strips the scheme from a normalized URL, leaving `host:port`.
fn host_port(url: &str) -> &str {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url)
}

// ---------------------------------------------------------------------------
// set_parameter — map a ParameterType + value to a remote-camera query string
// ---------------------------------------------------------------------------

/// Builds the `"/endpoint?param=value"` query for a parameter write, or
/// `NotSupported` for parameters this camera does not expose.
fn build_set_query(param_type: ParameterType, value: &str) -> Result<String, CameraError> {
    let as_bool = || value == "true";
    let as_int = || {
        value
            .parse::<i64>()
            .map_err(|_| CameraError::Remote(format!("invalid integer value: {value}")))
    };
    // Reverse the FLOAT_SCALE applied when the parameter was read.
    let as_scaled = || as_int().map(|v| v as f64 / FLOAT_SCALE);

    Ok(match param_type {
        ParameterType::ExposureAuto => {
            // Locked (0) = manual exposure, AutoExpose (1) = auto.
            let mode = if as_bool() { 1 } else { 0 };
            format!("/exposuremode?AVCaptureExposureMode={mode}")
        }
        ParameterType::Iso => format!("/setISO?iso={}", as_int()?),
        // ShutterSpeed is exposed in microseconds, the unit the camera expects.
        ParameterType::ShutterSpeed => format!("/setExposureDuration?duration={}", as_int()?),
        ParameterType::ExposureCompensation => {
            format!("/changeExposureTargetBiasTo?bias={}", as_int()?)
        }
        ParameterType::WhiteBalanceAuto => {
            // Locked (0) = manual, ContinuousAutoWhiteBalance (2) = auto.
            let mode = if as_bool() { 2 } else { 0 };
            format!("/whitebalancemode?AVCaptureWhiteBalanceMode={mode}")
        }
        ParameterType::ColorTemperature => format!("/setWhiteBalanceGains?gains={}", as_int()?),
        ParameterType::FocusAuto => {
            // Locked (0) = manual focus, AutoFocus (1) = auto.
            let mode = if as_bool() { 1 } else { 0 };
            format!("/focusmode?AVCaptureFocusMode={mode}")
        }
        ParameterType::Focus => format!("/setLensPosition?position={}", as_scaled()?),
        ParameterType::Zoom => format!("/setZoomFactor?zoom={}", as_scaled()?),
        _ => return Err(CameraError::NotSupported),
    })
}

// ---------------------------------------------------------------------------
// Status payload → parameters
// ---------------------------------------------------------------------------

fn one() -> f64 {
    1.0
}

/// Subset of the `/status` JSON this backend needs. Unknown fields are ignored
/// and missing fields fall back to defaults so partial payloads don't fail.
#[derive(Debug, Clone, Deserialize)]
struct SmsStatus {
    #[serde(rename = "minISO", default)]
    min_iso: f64,
    #[serde(rename = "maxISO", default)]
    max_iso: f64,
    #[serde(rename = "currentISO", default)]
    current_iso: f64,

    #[serde(rename = "minExposureDuration", default)]
    min_exposure_duration: f64,
    #[serde(rename = "maxExposureDuration", default)]
    max_exposure_duration: f64,
    #[serde(rename = "currentExposureDuration", default)]
    current_exposure_duration: f64,

    #[serde(rename = "minExposureTargetBias", default)]
    min_bias: f64,
    #[serde(rename = "maxExposureTargetBias", default)]
    max_bias: f64,
    #[serde(rename = "exposureTargetBias", default)]
    current_bias: f64,

    #[serde(rename = "AVCaptureExposureMode", default)]
    exposure_mode: i64,
    #[serde(rename = "AVCaptureWhiteBalanceMode", default)]
    wb_mode: i64,
    #[serde(rename = "AVCaptureFocusMode", default)]
    focus_mode: i64,

    #[serde(rename = "minWhitebalanceGains", default)]
    min_wb_gains: f64,
    #[serde(rename = "maxWhitebalanceGains", default)]
    max_wb_gains: f64,
    #[serde(rename = "currentWhitebalanceGains", default)]
    current_wb_gains: f64,

    #[serde(rename = "minFocusLensPosition", default)]
    min_focus: f64,
    #[serde(rename = "maxFocusLensPosition", default)]
    max_focus: f64,
    #[serde(rename = "currentFocusLensPosition", default)]
    current_focus: f64,

    #[serde(rename = "currentZoomFactor", default = "one")]
    current_zoom: f64,
    #[serde(rename = "maxZoomFactor", default = "one")]
    max_zoom: f64,

    #[serde(rename = "CAPTURE_RESOLUTION_WIDTH", default)]
    width: u32,
    #[serde(rename = "CAPTURE_RESOLUTION_HEIGHT", default)]
    height: u32,
}

impl SmsStatus {
    /// AVFoundation auto exposure (AutoExpose / ContinuousAutoExposure); Locked
    /// and Custom are treated as manual.
    fn exposure_auto(&self) -> bool {
        matches!(self.exposure_mode, 1 | 2)
    }

    /// AVFoundation auto white balance (Auto / ContinuousAuto).
    fn wb_auto(&self) -> bool {
        matches!(self.wb_mode, 1 | 2)
    }

    /// AVFoundation auto focus (anything but Locked).
    fn focus_auto(&self) -> bool {
        self.focus_mode != 0
    }

    fn to_parameters(&self) -> Vec<CameraParameter> {
        let mut out = Vec::new();

        out.push(boolean(ParameterType::ExposureAuto, self.exposure_auto(), false));
        push_range(
            &mut out,
            ParameterType::Iso,
            self.current_iso,
            self.min_iso,
            self.max_iso,
            1.0,
            self.exposure_auto(),
        );
        // Exposure duration is reported in seconds; expose it in microseconds.
        push_range(
            &mut out,
            ParameterType::ShutterSpeed,
            self.current_exposure_duration,
            self.min_exposure_duration,
            self.max_exposure_duration,
            1_000_000.0,
            self.exposure_auto(),
        );
        push_range(
            &mut out,
            ParameterType::ExposureCompensation,
            self.current_bias,
            self.min_bias,
            self.max_bias,
            1.0,
            false,
        );

        out.push(boolean(ParameterType::WhiteBalanceAuto, self.wb_auto(), false));
        push_range(
            &mut out,
            ParameterType::ColorTemperature,
            self.current_wb_gains,
            self.min_wb_gains,
            self.max_wb_gains,
            1.0,
            self.wb_auto(),
        );

        out.push(boolean(ParameterType::FocusAuto, self.focus_auto(), false));
        push_range(
            &mut out,
            ParameterType::Focus,
            self.current_focus,
            self.min_focus,
            self.max_focus,
            FLOAT_SCALE,
            self.focus_auto(),
        );
        push_range(
            &mut out,
            ParameterType::Zoom,
            self.current_zoom,
            // The protocol reports no minimum zoom; 1.0 is the floor.
            1.0,
            self.max_zoom,
            FLOAT_SCALE,
            false,
        );

        out
    }
}

fn boolean(param_type: ParameterType, current: bool, disabled: bool) -> CameraParameter {
    CameraParameter::Boolean {
        param_type,
        current,
        disabled,
    }
}

/// Pushes a scaled integer `Range` parameter, skipping it when the camera
/// reports a degenerate range (`max <= min`), which means the value is fixed or
/// unavailable in the current mode.
fn push_range(
    out: &mut Vec<CameraParameter>,
    param_type: ParameterType,
    current: f64,
    min: f64,
    max: f64,
    scale: f64,
    disabled: bool,
) {
    let to_i = |v: f64| (v * scale).round() as i32;
    let (min_i, max_i) = (to_i(min), to_i(max));
    if max_i <= min_i {
        return;
    }
    out.push(CameraParameter::Range {
        param_type,
        current: to_i(current).clamp(min_i, max_i),
        min: min_i,
        max: max_i,
        step: 1,
        disabled,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> SmsStatus {
        serde_json::from_value(serde_json::json!({
            "minISO": 100, "maxISO": 6400, "currentISO": 2498,
            "minExposureDuration": 0.0001, "maxExposureDuration": 32, "currentExposureDuration": 0.065,
            "minExposureTargetBias": -12, "maxExposureTargetBias": 12, "exposureTargetBias": 0,
            "AVCaptureExposureMode": 0, "AVCaptureWhiteBalanceMode": 9, "AVCaptureFocusMode": 0,
            "minWhitebalanceGains": 3000, "maxWhitebalanceGains": 8000, "currentWhitebalanceGains": 8000,
            "minFocusLensPosition": 0.1, "maxFocusLensPosition": 12.5, "currentFocusLensPosition": 11.36,
            "currentZoomFactor": 1.5, "maxZoomFactor": 10,
            "CAPTURE_RESOLUTION_WIDTH": 1280, "CAPTURE_RESOLUTION_HEIGHT": 720
        }))
        .unwrap()
    }

    fn find<'a>(params: &'a [CameraParameter], pt: ParameterType) -> Option<&'a CameraParameter> {
        params.iter().find(|p| match p {
            CameraParameter::Boolean { param_type, .. }
            | CameraParameter::Range { param_type, .. }
            | CameraParameter::Select { param_type, .. }
            | CameraParameter::RangeSelect { param_type, .. } => *param_type == pt,
        })
    }

    #[test]
    fn shutter_speed_is_exposed_in_microseconds() {
        let params = sample_status().to_parameters();
        let p = find(&params, ParameterType::ShutterSpeed).expect("shutter speed present");
        match p {
            CameraParameter::Range { current, min, max, .. } => {
                assert_eq!(*current, 65_000); // 0.065 s
                assert_eq!(*min, 100); // 0.0001 s
                assert_eq!(*max, 32_000_000); // 32 s
            }
            _ => panic!("expected a range"),
        }
    }

    #[test]
    fn zoom_and_focus_are_scaled_by_a_thousand() {
        let params = sample_status().to_parameters();
        match find(&params, ParameterType::Zoom).unwrap() {
            CameraParameter::Range { current, min, max, .. } => {
                assert_eq!(*current, 1500);
                assert_eq!(*min, 1000);
                assert_eq!(*max, 10_000);
            }
            _ => panic!("expected a range"),
        }
        match find(&params, ParameterType::Focus).unwrap() {
            CameraParameter::Range { current, min, max, .. } => {
                assert_eq!(*current, 11_360);
                assert_eq!(*min, 100);
                assert_eq!(*max, 12_500);
            }
            _ => panic!("expected a range"),
        }
    }

    #[test]
    fn manual_modes_report_auto_off_and_enable_dependent_params() {
        let params = sample_status().to_parameters();
        // Exposure mode 0 (Locked) → not auto; ISO enabled.
        assert!(matches!(
            find(&params, ParameterType::ExposureAuto).unwrap(),
            CameraParameter::Boolean { current: false, .. }
        ));
        assert!(matches!(
            find(&params, ParameterType::Iso).unwrap(),
            CameraParameter::Range { disabled: false, .. }
        ));
        // Focus mode 0 (Locked) → not auto; lens position enabled.
        assert!(matches!(
            find(&params, ParameterType::FocusAuto).unwrap(),
            CameraParameter::Boolean { current: false, .. }
        ));
    }

    #[test]
    fn auto_exposure_disables_iso_and_shutter() {
        let mut s = sample_status();
        s.exposure_mode = 1; // AutoExpose
        let params = s.to_parameters();
        assert!(matches!(
            find(&params, ParameterType::Iso).unwrap(),
            CameraParameter::Range { disabled: true, .. }
        ));
        assert!(matches!(
            find(&params, ParameterType::ShutterSpeed).unwrap(),
            CameraParameter::Range { disabled: true, .. }
        ));
    }

    #[test]
    fn build_set_query_maps_each_parameter() {
        assert_eq!(
            build_set_query(ParameterType::Iso, "800").unwrap(),
            "/setISO?iso=800"
        );
        assert_eq!(
            build_set_query(ParameterType::ShutterSpeed, "40000").unwrap(),
            "/setExposureDuration?duration=40000"
        );
        assert_eq!(
            build_set_query(ParameterType::ExposureAuto, "true").unwrap(),
            "/exposuremode?AVCaptureExposureMode=1"
        );
        assert_eq!(
            build_set_query(ParameterType::ExposureAuto, "false").unwrap(),
            "/exposuremode?AVCaptureExposureMode=0"
        );
        assert_eq!(
            build_set_query(ParameterType::WhiteBalanceAuto, "true").unwrap(),
            "/whitebalancemode?AVCaptureWhiteBalanceMode=2"
        );
        assert_eq!(
            build_set_query(ParameterType::FocusAuto, "false").unwrap(),
            "/focusmode?AVCaptureFocusMode=0"
        );
        // Scaled params are divided back to a float before sending.
        assert_eq!(
            build_set_query(ParameterType::Zoom, "1500").unwrap(),
            "/setZoomFactor?zoom=1.5"
        );
        assert_eq!(
            build_set_query(ParameterType::Focus, "11360").unwrap(),
            "/setLensPosition?position=11.36"
        );
        assert!(matches!(
            build_set_query(ParameterType::Pan, "1"),
            Err(CameraError::NotSupported)
        ));
    }

    #[test]
    fn host_port_strips_scheme() {
        assert_eq!(host_port("http://192.168.1.14:2222"), "192.168.1.14:2222");
        assert_eq!(host_port("https://host:2222"), "host:2222");
    }
}
