//! Webcams through the browser's media stack (`getUserMedia`), via a JS bridge.
//!
//! The bridge (`js/webcam-bridge.js`) owns the browser objects — streams, the
//! `<video>` elements frames are grabbed from — and only moves plain data across:
//! device lists, the track's `getCapabilities()` / `getSettings()`, JPEG bytes.
//! This side turns capabilities into the server's `CameraParameter`s and maps
//! writes back to constraints, so the API a client sees is the one every other
//! backend exposes.
//!
//! Capabilities map onto the parameters the native webcam backends expose
//! (`webcam_macos`, `webcam_windows`, `webcam_linux`):
//!
//! | constraint                                   | parameter                          |
//! |----------------------------------------------|------------------------------------|
//! | brightness, contrast, saturation, sharpness  | same name (range)                  |
//! | exposureMode (`continuous` ↔ `manual`)        | `exposure_auto`                    |
//! | exposureTime (100 µs units, as UVC)          | `exposure` (disabled while auto)   |
//! | exposureCompensation                         | `exposure_compensation`            |
//! | whiteBalanceMode                             | `white_balance_auto`               |
//! | colorTemperature (K)                         | `color_temperature` (disabled while auto) |
//! | focusMode                                    | `focus_auto`                       |
//! | focusDistance                                | `focus` (disabled while auto)      |
//! | iso, zoom, pan, tilt                         | same name (range)                  |
//! | width × height                               | `photo_resolution` (w*10000+h)     |
//!
//! The model's ranges are integers while some constraints are fractional (zoom
//! steps of 0.1, focus distance in metres): those are scaled by a power of ten
//! derived from the step (`scale_for`), and divided back on write.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize as _};
use serde_json::Value;
use wasm_bindgen::prelude::*;

use crate::backend::{LocalFuture, WebBackend};
use crate::camera::{
    CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterOption, ParameterType,
};
use crate::js_error_message;

pub const BACKEND_ID: &str = "webcam-browser";

#[wasm_bindgen]
extern "C" {
    /// The JS object built by `js/webcam-bridge.js`.
    pub type WebcamBridge;

    /// `[{ deviceId, label }]` for every `videoinput`.
    #[wasm_bindgen(method, catch, js_name = listDevices)]
    async fn list_devices(this: &WebcamBridge) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn open(this: &WebcamBridge, device_id: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn close(this: &WebcamBridge, device_id: &str) -> Result<JsValue, JsValue>;

    /// `{ capabilities, settings }` of the open track.
    #[wasm_bindgen(method, catch)]
    async fn capabilities(this: &WebcamBridge, device_id: &str) -> Result<JsValue, JsValue>;

    /// `applyConstraints` with `{ [name]: value }` (several keys for resolution).
    #[wasm_bindgen(method, catch, js_name = applyConstraints)]
    async fn apply_constraints(
        this: &WebcamBridge,
        device_id: &str,
        constraints: JsValue,
    ) -> Result<JsValue, JsValue>;

    /// One frame of the stream as JPEG (`Uint8Array`).
    #[wasm_bindgen(method, catch, js_name = grabFrame)]
    async fn grab_frame(
        this: &WebcamBridge,
        device_id: &str,
        quality: f64,
    ) -> Result<JsValue, JsValue>;

    /// A still at the best quality the browser offers (`Uint8Array`, JPEG).
    #[wasm_bindgen(method, catch, js_name = takePhoto)]
    async fn take_photo(this: &WebcamBridge, device_id: &str) -> Result<JsValue, JsValue>;
}

/// JPEG quality of live-view frames (0..1): speed over fidelity, like the
/// native webcam backends' preview encoding.
const LIVE_VIEW_QUALITY: f64 = 0.8;

pub struct WebcamBackend {
    bridge: WebcamBridge,
    connected: RefCell<HashSet<String>>,
}

impl WebcamBackend {
    pub fn new(bridge: WebcamBridge) -> Self {
        Self {
            bridge,
            connected: RefCell::new(HashSet::new()),
        }
    }

    fn require_connected(&self, native_id: &str) -> Result<(), CameraError> {
        if self.is_connected(native_id) {
            Ok(())
        } else {
            Err(CameraError::NotConnected)
        }
    }

    async fn track_state(&self, native_id: &str) -> Result<TrackState, CameraError> {
        let raw = self
            .bridge
            .capabilities(native_id)
            .await
            .map_err(backend_error)?;
        serde_wasm_bindgen::from_value(raw).map_err(|e| CameraError::Backend(e.to_string()))
    }
}

#[derive(Deserialize)]
struct BridgeDevice {
    #[serde(rename = "deviceId")]
    device_id: String,
    #[serde(default)]
    label: String,
}

/// `MediaTrackCapabilities` entry: a numeric range or a list of modes.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum Capability {
    Range {
        min: f64,
        max: f64,
        #[serde(default)]
        step: Option<f64>,
    },
    Modes(Vec<String>),
    Other(serde_json::Value),
}

#[derive(Deserialize, Debug, Default)]
pub struct TrackState {
    #[serde(default)]
    pub capabilities: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    pub settings: serde_json::Map<String, serde_json::Value>,
}

impl TrackState {
    fn capability(&self, name: &str) -> Option<Capability> {
        self.capabilities
            .get(name)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }

    fn setting_f64(&self, name: &str) -> Option<f64> {
        self.settings.get(name).and_then(|v| v.as_f64())
    }

    fn setting_str(&self, name: &str) -> Option<&str> {
        self.settings.get(name).and_then(|v| v.as_str())
    }
}

/// Numeric constraints exposed as plain ranges, in display order.
const RANGES: &[(&str, ParameterType)] = &[
    ("brightness", ParameterType::Brightness),
    ("contrast", ParameterType::Contrast),
    ("saturation", ParameterType::Saturation),
    ("sharpness", ParameterType::Sharpness),
    ("exposureCompensation", ParameterType::ExposureCompensation),
    ("iso", ParameterType::Iso),
    ("zoom", ParameterType::Zoom),
    ("pan", ParameterType::Pan),
    ("tilt", ParameterType::Tilt),
];

/// Numeric constraints paired with an auto/manual mode constraint: the value is
/// only writable in manual mode, so it is `disabled` while the mode is auto.
const MODE_PAIRS: &[(&str, ParameterType, &str, ParameterType)] = &[
    (
        "exposureMode",
        ParameterType::ExposureAuto,
        "exposureTime",
        ParameterType::Exposure,
    ),
    (
        "whiteBalanceMode",
        ParameterType::WhiteBalanceAuto,
        "colorTemperature",
        ParameterType::ColorTemperature,
    ),
    (
        "focusMode",
        ParameterType::FocusAuto,
        "focusDistance",
        ParameterType::Focus,
    ),
];

/// Common capture sizes offered for `photo_resolution`, within the camera's range.
const RESOLUTIONS: &[(u32, u32)] = &[
    (640, 480),
    (800, 600),
    (1280, 720),
    (1280, 960),
    (1600, 1200),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

/// Power of ten that turns `step` into an integer (0.1 → 10, 0.01 → 100), so a
/// fractional constraint fits the model's integer ranges. Capped at 1000.
pub fn scale_for(step: Option<f64>) -> f64 {
    let Some(step) = step.filter(|s| *s > 0.0) else {
        return 1.0;
    };
    let mut scale = 1.0;
    while scale < 1000.0 && ((step * scale) - (step * scale).round()).abs() > 1e-6 {
        scale *= 10.0;
    }
    scale
}

fn range_param(
    param_type: ParameterType,
    cap: &Capability,
    current: Option<f64>,
    disabled: bool,
) -> Option<CameraParameter> {
    let Capability::Range { min, max, step } = cap else {
        return None;
    };
    if max <= min {
        return None; // a fixed value is not a control
    }
    let scale = scale_for(*step);
    let to_int = |v: f64| (v * scale).round() as i32;
    Some(CameraParameter::Range {
        param_type,
        current: to_int(current.unwrap_or(*min).clamp(*min, *max)),
        min: to_int(*min),
        max: to_int(*max),
        step: step.map(to_int).filter(|s| *s > 0).unwrap_or(1),
        disabled,
    })
}

/// Whether a `*Mode` value means "the camera decides" — `continuous` for every
/// mode constraint; `single-shot` is a one-off adjustment, so it counts as manual.
fn is_auto_mode(mode: &str) -> bool {
    mode == "continuous"
}

/// The camera's parameters, from its track capabilities and settings.
pub fn parameters_from(state: &TrackState) -> Vec<CameraParameter> {
    let mut params = Vec::new();

    for (mode_name, auto_type, value_name, value_type) in MODE_PAIRS {
        let auto = state.setting_str(mode_name).map(is_auto_mode);
        let modes = match state.capability(mode_name) {
            Some(Capability::Modes(modes)) => modes,
            _ => Vec::new(),
        };
        // A toggle needs both an auto and a manual mode to switch between.
        let can_toggle =
            modes.iter().any(|m| is_auto_mode(m)) && modes.iter().any(|m| m == "manual");
        if can_toggle {
            params.push(CameraParameter::Boolean {
                param_type: *auto_type,
                current: auto.unwrap_or(false),
                disabled: false,
            });
        }
        if let Some(cap) = state.capability(value_name) {
            let disabled = can_toggle && auto.unwrap_or(false);
            if let Some(p) = range_param(*value_type, &cap, state.setting_f64(value_name), disabled)
            {
                params.push(p);
            }
        }
    }

    for (name, param_type) in RANGES {
        if let Some(cap) = state.capability(name) {
            if let Some(p) = range_param(*param_type, &cap, state.setting_f64(name), false) {
                params.push(p);
            }
        }
    }

    if let Some(p) = resolution_param(state) {
        params.push(p);
    }
    params
}

fn resolution_param(state: &TrackState) -> Option<CameraParameter> {
    let (
        Some(Capability::Range {
            max: max_w,
            min: min_w,
            ..
        }),
        Some(Capability::Range {
            max: max_h,
            min: min_h,
            ..
        }),
    ) = (state.capability("width"), state.capability("height"))
    else {
        return None;
    };
    let current_w = state.setting_f64("width")? as u32;
    let current_h = state.setting_f64("height")? as u32;

    let mut sizes: Vec<(u32, u32)> = RESOLUTIONS
        .iter()
        .copied()
        .filter(|(w, h)| {
            (min_w..=max_w).contains(&(*w as f64)) && (min_h..=max_h).contains(&(*h as f64))
        })
        .collect();
    if !sizes.contains(&(current_w, current_h)) {
        sizes.push((current_w, current_h));
        sizes.sort();
    }
    if sizes.len() < 2 {
        return None;
    }
    let encode = |(w, h): (u32, u32)| (w * 10000 + h).to_string();
    Some(CameraParameter::Select {
        param_type: ParameterType::PhotoResolution,
        current: encode((current_w, current_h)),
        options: sizes
            .into_iter()
            .map(|s| ParameterOption {
                label: format!("{}×{}", s.0, s.1),
                value: encode(s),
            })
            .collect(),
        disabled: false,
    })
}

/// The constraints to apply for a parameter write, as a JSON object.
pub fn constraints_for(
    state: &TrackState,
    param_type: ParameterType,
    value: &str,
) -> Result<Value, CameraError> {
    if param_type == ParameterType::PhotoResolution {
        let encoded: u32 = value
            .trim()
            .parse()
            .map_err(|_| CameraError::NotSupported)?;
        let (w, h) = (encoded / 10000, encoded % 10000);
        return Ok(object(&[("width", w.into()), ("height", h.into())]));
    }

    if let Some((mode_name, ..)) = MODE_PAIRS.iter().find(|(_, auto, ..)| *auto == param_type) {
        let mode = if is_truthy(value) {
            "continuous"
        } else {
            "manual"
        };
        return Ok(object(&[(mode_name, mode.into())]));
    }

    let name = MODE_PAIRS
        .iter()
        .find(|(_, _, _, vt)| *vt == param_type)
        .map(|(_, _, name, _)| *name)
        .or_else(|| {
            RANGES
                .iter()
                .find(|(_, pt)| *pt == param_type)
                .map(|(name, _)| *name)
        })
        .ok_or(CameraError::NotSupported)?;
    let Some(Capability::Range { step, .. }) = state.capability(name) else {
        return Err(CameraError::NotSupported);
    };
    let raw: f64 = value
        .trim()
        .parse()
        .map_err(|_| CameraError::NotSupported)?;
    let value = raw / scale_for(step);

    // A value constraint is ignored while its mode is auto: switch to manual
    // first, which is what a client moving the slider means.
    if let Some((mode_name, ..)) = MODE_PAIRS.iter().find(|(_, _, n, _)| *n == name) {
        if state.setting_str(mode_name).is_some_and(is_auto_mode) {
            return Ok(object(&[
                (mode_name, "manual".into()),
                (name, value.into()),
            ]));
        }
    }
    Ok(object(&[(name, value.into())]))
}

fn object(entries: &[(&str, Value)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    )
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "on" | "yes"
    )
}

fn backend_error(err: JsValue) -> CameraError {
    CameraError::Backend(js_error_message(&err))
}

fn bytes_from(value: JsValue) -> Vec<u8> {
    js_sys::Uint8Array::new(&value).to_vec()
}

impl WebBackend for WebcamBackend {
    fn backend_id(&self) -> &str {
        BACKEND_ID
    }

    fn list_devices(&self) -> LocalFuture<'_, Result<Vec<DeviceInfo>, CameraError>> {
        Box::pin(async move {
            let raw = self.bridge.list_devices().await.map_err(backend_error)?;
            let devices: Vec<BridgeDevice> = serde_wasm_bindgen::from_value(raw)
                .map_err(|e| CameraError::Backend(e.to_string()))?;
            let connected = self.connected.borrow();
            Ok(devices
                .into_iter()
                .enumerate()
                .map(|(i, d)| DeviceInfo {
                    id: DeviceId::new(BACKEND_ID, &d.device_id).encode(),
                    // Labels stay empty until the page holds camera permission.
                    name: if d.label.is_empty() {
                        format!("Webcam {}", i + 1)
                    } else {
                        d.label
                    },
                    connected: connected.contains(&d.device_id),
                    dedup_key: None,
                })
                .collect())
        })
    }

    fn connect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>> {
        Box::pin(async move {
            if self.is_connected(native_id) {
                return Ok(());
            }
            self.bridge.open(native_id).await.map_err(|e| {
                let message = js_error_message(&e);
                if message.contains("NotFoundError") || message.contains("OverconstrainedError") {
                    CameraError::DeviceNotFound(native_id.to_string())
                } else {
                    CameraError::Backend(message)
                }
            })?;
            self.connected.borrow_mut().insert(native_id.to_string());
            Ok(())
        })
    }

    fn disconnect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>> {
        Box::pin(async move {
            self.connected.borrow_mut().remove(native_id);
            self.bridge.close(native_id).await.map_err(backend_error)?;
            Ok(())
        })
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.connected.borrow().contains(native_id)
    }

    fn get_parameters<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<CameraParameter>, CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            Ok(parameters_from(&self.track_state(native_id).await?))
        })
    }

    fn get_live_view_frame<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            let frame = self
                .bridge
                .grab_frame(native_id, LIVE_VIEW_QUALITY)
                .await
                .map_err(backend_error)?;
            Ok(bytes_from(frame))
        })
    }

    fn set_parameter<'a>(
        &'a self,
        native_id: &'a str,
        param_type: ParameterType,
        value: &'a str,
    ) -> LocalFuture<'a, Result<(), CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            let state = self.track_state(native_id).await?;
            let constraints = constraints_for(&state, param_type, value)?;
            let constraints = constraints
                .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
                .map_err(|e| CameraError::Backend(e.to_string()))?;
            self.bridge
                .apply_constraints(native_id, constraints)
                .await
                .map_err(backend_error)?;
            Ok(())
        })
    }

    fn capture_photo<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            let photo = self
                .bridge
                .take_photo(native_id)
                .await
                .map_err(backend_error)?;
            Ok(bytes_from(photo))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(caps: serde_json::Value, settings: serde_json::Value) -> TrackState {
        serde_json::from_value(json!({ "capabilities": caps, "settings": settings })).unwrap()
    }

    fn find(params: &[CameraParameter], pt: ParameterType) -> Option<&CameraParameter> {
        params.iter().find(|p| match p {
            CameraParameter::Boolean { param_type, .. }
            | CameraParameter::Range { param_type, .. }
            | CameraParameter::Select { param_type, .. }
            | CameraParameter::RangeSelect { param_type, .. } => *param_type == pt,
        })
    }

    #[test]
    fn scale_turns_fractional_steps_into_integers() {
        assert_eq!(scale_for(None), 1.0);
        assert_eq!(scale_for(Some(1.0)), 1.0);
        assert_eq!(scale_for(Some(0.1)), 10.0);
        assert_eq!(scale_for(Some(0.01)), 100.0);
        assert_eq!(scale_for(Some(0.25)), 100.0);
        assert_eq!(scale_for(Some(1.0 / 3.0)), 1000.0, "capped");
    }

    /// A typical UVC webcam as Chrome reports it.
    fn uvc() -> TrackState {
        state(
            json!({
                "brightness": { "min": -64, "max": 64, "step": 1 },
                "exposureMode": ["manual", "continuous"],
                "exposureTime": { "min": 1, "max": 5000, "step": 1 },
                "whiteBalanceMode": ["manual", "continuous"],
                "colorTemperature": { "min": 2800, "max": 6500, "step": 10 },
                "focusMode": ["manual", "continuous"],
                "focusDistance": { "min": 0, "max": 2.5, "step": 0.01 },
                "zoom": { "min": 1, "max": 1, "step": 0.1 },
                "width": { "min": 1, "max": 1920 },
                "height": { "min": 1, "max": 1080 },
                "frameRate": { "min": 0, "max": 30 },
                "facingMode": [],
            }),
            json!({
                "brightness": 0, "exposureMode": "continuous", "exposureTime": 156,
                "whiteBalanceMode": "manual", "colorTemperature": 4600,
                "focusMode": "continuous", "focusDistance": 0.5,
                "width": 1280, "height": 720,
            }),
        )
    }

    #[test]
    fn capabilities_become_server_parameters() {
        let params = parameters_from(&uvc());
        let json = serde_json::to_value(&params).unwrap();
        // Serialized exactly like the native backends' parameters.
        assert!(json
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "brightness" && p["kind"] == "range"));

        assert!(matches!(
            find(&params, ParameterType::ExposureAuto),
            Some(CameraParameter::Boolean { current: true, .. })
        ));
        assert!(matches!(
            find(&params, ParameterType::Exposure),
            Some(CameraParameter::Range {
                current: 156,
                disabled: true,
                ..
            })
        ));
        assert!(matches!(
            find(&params, ParameterType::ColorTemperature),
            Some(CameraParameter::Range {
                current: 4600,
                disabled: false,
                step: 10,
                ..
            })
        ));
        // Focus distance in metres, step 0.01 → centimetres.
        assert!(matches!(
            find(&params, ParameterType::Focus),
            Some(CameraParameter::Range {
                current: 50,
                min: 0,
                max: 250,
                step: 1,
                disabled: true,
                ..
            })
        ));
        // A fixed zoom (min == max) is no control.
        assert!(find(&params, ParameterType::Zoom).is_none());
        match find(&params, ParameterType::PhotoResolution) {
            Some(CameraParameter::Select {
                current, options, ..
            }) => {
                assert_eq!(current, "12800720");
                assert!(options.iter().any(|o| o.value == "19201080"));
                assert!(
                    !options.iter().any(|o| o.value == "38402160"),
                    "above the camera's max"
                );
            }
            other => panic!("resolution: {other:?}"),
        }
    }

    #[test]
    fn writes_become_constraints() {
        let s = uvc();
        assert_eq!(
            constraints_for(&s, ParameterType::Brightness, "12").unwrap(),
            json!({ "brightness": 12.0 })
        );
        assert_eq!(
            constraints_for(&s, ParameterType::ExposureAuto, "false").unwrap(),
            json!({ "exposureMode": "manual" })
        );
        // Moving a value whose mode is auto switches the mode to manual too.
        assert_eq!(
            constraints_for(&s, ParameterType::Focus, "120").unwrap(),
            json!({ "focusMode": "manual", "focusDistance": 1.2 })
        );
        assert_eq!(
            constraints_for(&s, ParameterType::PhotoResolution, "19201080").unwrap(),
            json!({ "width": 1920, "height": 1080 })
        );
        assert!(matches!(
            constraints_for(&s, ParameterType::Pan, "1"),
            Err(CameraError::NotSupported)
        ));
        assert!(matches!(
            constraints_for(&s, ParameterType::Brightness, "bright"),
            Err(CameraError::NotSupported)
        ));
    }

    #[test]
    fn a_camera_without_controls_has_no_parameters() {
        assert!(parameters_from(&state(json!({}), json!({}))).is_empty());
    }
}
