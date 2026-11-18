//! libgphoto2 cameras over WebUSB, via a JS bridge to the Emscripten module.
//!
//! libgphoto2 itself is compiled with Emscripten (`wasm/gphoto2/`), libusb's
//! WebUSB backend standing in for the kernel USB stack. That module cannot be
//! linked into this crate (Emscripten and wasm-bindgen produce different
//! modules), so `js/gphoto2-bridge.js` loads it and exposes a small promise API:
//! autodetect, open a port, read the config tree or a single widget, write a
//! widget, capture a preview or a still.
//!
//! Everything above that is the native backend's logic: the same parameter
//! curation (`src/backends/gphoto2/curation.rs`, compiled from the same file),
//! the same vendor-key probing on writes, the same IsoAuto / FocusAuto
//! synthesis, the same focus-drive probe. A body therefore exposes the same
//! parameters in the browser as on the native server.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use wasm_bindgen::prelude::*;

use crate::backend::{LocalFuture, WebBackend};
use crate::camera::{CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterType};
use crate::curation::{
    auto_iso_choice, config_keys_for, curate, is_concrete_iso, is_manual_focus_choice, is_truthy,
    leaf_params, on_off_choice, type_of, Leaf,
};
use crate::{js_error_code, js_error_message};

/// Same id as the native backend: a body is addressed the same way everywhere
/// (the native ID is the libgphoto2 port, e.g. `usb:001,002`).
pub const BACKEND_ID: &str = "gphoto2";

/// libgphoto2 result codes the backend tells apart.
const GP_ERROR: i32 = -1;
const GP_ERROR_NOT_SUPPORTED: i32 = -6;

#[wasm_bindgen]
extern "C" {
    /// The JS object built by `js/gphoto2-bridge.js`.
    pub type GPhoto2Bridge;

    /// `[{ model, port }]` — only devices the page was granted through WebUSB.
    #[wasm_bindgen(method, catch, js_name = listDevices)]
    async fn list_devices(this: &GPhoto2Bridge) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn open(this: &GPhoto2Bridge, port: &str, model: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch)]
    async fn close(this: &GPhoto2Bridge, port: &str) -> Result<JsValue, JsValue>;

    /// The whole config tree (`Node`).
    #[wasm_bindgen(method, catch)]
    async fn config(this: &GPhoto2Bridge, port: &str) -> Result<JsValue, JsValue>;

    /// One widget by config key, or `null` when the body has no such key.
    #[wasm_bindgen(method, catch)]
    async fn widget(this: &GPhoto2Bridge, port: &str, name: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(method, catch, js_name = setConfig)]
    async fn set_config(
        this: &GPhoto2Bridge,
        port: &str,
        name: &str,
        value: &str,
    ) -> Result<JsValue, JsValue>;

    /// `{ mime, data }` of a live-view frame.
    #[wasm_bindgen(method, catch, js_name = capturePreview)]
    async fn capture_preview(this: &GPhoto2Bridge, port: &str) -> Result<JsValue, JsValue>;

    /// `{ mime, data, name }` of a full still.
    #[wasm_bindgen(method, catch, js_name = captureImage)]
    async fn capture_image(this: &GPhoto2Bridge, port: &str) -> Result<JsValue, JsValue>;
}

/// A widget of the config tree, as serialised by `toucan_gphoto2.cpp`.
#[derive(Deserialize, Debug, Clone)]
pub struct Node {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub readonly: bool,
    #[serde(default)]
    pub value: Option<serde_json::Value>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub min: f32,
    #[serde(default)]
    pub max: f32,
    #[serde(default)]
    pub step: f32,
    #[serde(default)]
    pub children: Vec<Node>,
}

impl Node {
    /// The backend-neutral leaf the shared curation works on, or `None` for
    /// groups, buttons, text and dates — which are never parameters.
    pub fn leaf(&self) -> Option<Leaf> {
        let value = self.value.as_ref();
        match self.kind.as_str() {
            "radio" | "menu" => Some(Leaf::Choice {
                name: self.name.clone(),
                readonly: self.readonly,
                current: value
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                choices: self.choices.clone(),
            }),
            "range" => Some(Leaf::Range {
                name: self.name.clone(),
                readonly: self.readonly,
                value: value.and_then(|v| v.as_f64()).unwrap_or_default() as f32,
                min: self.min,
                max: self.max,
                step: self.step,
            }),
            "toggle" => Some(Leaf::Toggle {
                name: self.name.clone(),
                readonly: self.readonly,
                value: value.and_then(|v| v.as_bool()),
            }),
            _ => None,
        }
    }

    fn is_choice(&self) -> bool {
        matches!(self.kind.as_str(), "radio" | "menu")
    }
}

/// Walks the tree like the native `walk_widget`, tagging each parameter with
/// the config key it came from (the shared `curate` needs it).
pub fn walk(node: &Node, out: &mut Vec<(String, CameraParameter)>) {
    if matches!(node.kind.as_str(), "window" | "section") {
        for child in &node.children {
            walk(child, out);
        }
        return;
    }
    if let Some(leaf) = node.leaf() {
        out.extend(
            leaf_params(&leaf)
                .into_iter()
                .map(|p| (node.name.clone(), p)),
        );
    }
}

/// The parameters of a config tree, exactly as the native backend lists them
/// (minus the focus-drive probe, which needs the camera).
pub fn parameters_from(root: &Node) -> Vec<CameraParameter> {
    let mut tagged = Vec::new();
    walk(root, &mut tagged);
    curate(tagged)
}

#[derive(Deserialize)]
struct BridgeCamera {
    model: String,
    port: String,
}

#[derive(Deserialize)]
struct BridgeFile {
    #[serde(default)]
    mime: String,
}

pub struct GPhoto2Backend {
    bridge: GPhoto2Bridge,
    connected: RefCell<HashSet<String>>,
    /// Whether each device's body really drives its focus motor (see the native
    /// backend's `focus_drive_works`).
    focus_drive: RefCell<HashMap<String, bool>>,
}

impl GPhoto2Backend {
    pub fn new(bridge: GPhoto2Bridge) -> Self {
        Self {
            bridge,
            connected: RefCell::new(HashSet::new()),
            focus_drive: RefCell::new(HashMap::new()),
        }
    }

    fn require_connected(&self, port: &str) -> Result<(), CameraError> {
        if self.is_connected(port) {
            Ok(())
        } else {
            Err(CameraError::NotConnected)
        }
    }

    async fn node(&self, port: &str, name: &str) -> Result<Option<Node>, CameraError> {
        let raw = self.bridge.widget(port, name).await.map_err(gp_error)?;
        if raw.is_null() || raw.is_undefined() {
            return Ok(None);
        }
        serde_wasm_bindgen::from_value(raw)
            .map(Some)
            .map_err(|e| CameraError::Backend(e.to_string()))
    }

    /// The body's widget for `param_type` whose kind satisfies `accept`, trying
    /// the type's config keys in `CONFIG_KEYS` order — the native `widget_for`.
    async fn widget_for(
        &self,
        port: &str,
        param_type: ParameterType,
        accept: fn(&Node) -> bool,
    ) -> Result<Option<Node>, CameraError> {
        for key in config_keys_for(param_type) {
            if let Some(node) = self.node(port, key).await? {
                if accept(&node) {
                    return Ok(Some(node));
                }
            }
        }
        Ok(None)
    }

    async fn set(&self, port: &str, node: &Node, value: &str) -> Result<(), CameraError> {
        self.bridge
            .set_config(port, &node.name, value)
            .await
            .map_err(gp_error)?;
        Ok(())
    }

    async fn set_iso_auto(&self, port: &str, on: bool) -> Result<(), CameraError> {
        let flag = if on { "true" } else { "false" };
        if let Some(toggle) = self
            .widget_for(port, ParameterType::IsoAuto, |n| n.kind == "toggle")
            .await?
        {
            return self.set(port, &toggle, flag).await;
        }
        if let Some(radio) = self
            .widget_for(port, ParameterType::IsoAuto, Node::is_choice)
            .await?
        {
            let target = on_off_choice(&radio.choices, on)
                .ok_or(CameraError::NotSupported)?
                .clone();
            return self.set(port, &radio, &target).await;
        }
        let iso = self
            .widget_for(port, ParameterType::Iso, Node::is_choice)
            .await?
            .ok_or(CameraError::NotSupported)?;
        let target = if on {
            auto_iso_choice(&iso.choices).cloned()
        } else {
            iso.choices.iter().find(|c| is_concrete_iso(c)).cloned()
        }
        .ok_or(CameraError::NotSupported)?;
        self.set(port, &iso, &target).await
    }

    async fn set_focus_auto(&self, port: &str, on: bool) -> Result<(), CameraError> {
        let widget = self
            .widget_for(port, ParameterType::FocusMode, Node::is_choice)
            .await?
            .ok_or(CameraError::NotSupported)?;
        let target = widget
            .choices
            .iter()
            .find(|c| is_manual_focus_choice(c) != on)
            .ok_or(CameraError::NotSupported)?
            .clone();
        self.set(port, &widget, &target).await
    }

    /// Whether the body can drive its focus motor — probed once by moving it one
    /// step, then cached. See the native backend for why the widget alone
    /// cannot tell.
    async fn focus_drive_works(&self, port: &str) -> bool {
        if let Some(known) = self.focus_drive.borrow().get(port) {
            return *known;
        }
        let Ok(Some(widget)) = self
            .widget_for(port, ParameterType::Focus, |n| n.kind == "range")
            .await
        else {
            return true;
        };
        // The motor only obeys in live view; a preview grab gets the body there.
        let _ = self.bridge.capture_preview(port).await;
        match self.bridge.set_config(port, &widget.name, "1").await {
            Ok(_) => {
                self.focus_drive.borrow_mut().insert(port.to_string(), true);
                true
            }
            Err(e) if is_focus_refusal(&e) => {
                crate::api::log(&format!(
                    "[gphoto2] {port}: the body refuses to drive the focus motor ({}) — hiding Focus",
                    js_error_message(&e)
                ));
                self.focus_drive
                    .borrow_mut()
                    .insert(port.to_string(), false);
                false
            }
            Err(_) => true, // busy / I/O: inconclusive, ask again next time
        }
    }

    async fn drive_focus(&self, port: &str, value: &str) -> Result<(), CameraError> {
        let _ = self.bridge.capture_preview(port).await;
        let widget = match self
            .widget_for(port, ParameterType::Focus, |n| n.kind == "range")
            .await?
        {
            Some(range) => {
                value
                    .trim()
                    .parse::<f32>()
                    .map_err(|_| CameraError::NotSupported)?;
                range
            }
            None => self
                .widget_for(port, ParameterType::Focus, Node::is_choice)
                .await?
                .ok_or(CameraError::NotSupported)?,
        };
        match self
            .bridge
            .set_config(port, &widget.name, value.trim())
            .await
        {
            Ok(_) => Ok(()),
            Err(e) if is_focus_refusal(&e) => {
                self.focus_drive
                    .borrow_mut()
                    .insert(port.to_string(), false);
                Err(CameraError::Backend(
                    "gphoto2: this camera does not support driving the focus motor".to_string(),
                ))
            }
            Err(e) => Err(gp_error(e)),
        }
    }
}

/// "This body cannot do it" rather than "not right now" (native `is_focus_refusal`).
fn is_focus_refusal(err: &JsValue) -> bool {
    matches!(
        js_error_code(err),
        Some(GP_ERROR) | Some(GP_ERROR_NOT_SUPPORTED)
    )
}

/// Keeps libgphoto2's own message ("Could not claim the USB device"…).
fn gp_error(err: JsValue) -> CameraError {
    CameraError::Backend(js_error_message(&err))
}

fn file_bytes(raw: &JsValue) -> Result<(String, Vec<u8>), CameraError> {
    let file: BridgeFile = serde_wasm_bindgen::from_value(raw.clone())
        .map_err(|e| CameraError::Backend(e.to_string()))?;
    let data = js_sys::Reflect::get(raw, &JsValue::from_str("data"))
        .map_err(|_| CameraError::Backend("capture returned no data".into()))?;
    Ok((file.mime, js_sys::Uint8Array::new(&data).to_vec()))
}

impl WebBackend for GPhoto2Backend {
    fn backend_id(&self) -> &str {
        BACKEND_ID
    }

    fn list_devices(&self) -> LocalFuture<'_, Result<Vec<DeviceInfo>, CameraError>> {
        Box::pin(async move {
            let raw = self.bridge.list_devices().await.map_err(gp_error)?;
            let cameras: Vec<BridgeCamera> = serde_wasm_bindgen::from_value(raw)
                .map_err(|e| CameraError::Backend(e.to_string()))?;
            let connected = self.connected.borrow();
            Ok(cameras
                .into_iter()
                .map(|c| DeviceInfo {
                    id: DeviceId::new(BACKEND_ID, &c.port).encode(),
                    connected: connected.contains(&c.port),
                    name: c.model,
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
            // The driver is chosen from the model autodetect reports for this
            // port, like the native backend's `CameraDescriptor` lookup.
            let raw = self.bridge.list_devices().await.map_err(gp_error)?;
            let cameras: Vec<BridgeCamera> = serde_wasm_bindgen::from_value(raw)
                .map_err(|e| CameraError::Backend(e.to_string()))?;
            let camera = cameras
                .into_iter()
                .find(|c| c.port == native_id)
                .ok_or_else(|| CameraError::DeviceNotFound(native_id.to_string()))?;
            self.bridge
                .open(&camera.port, &camera.model)
                .await
                .map_err(gp_error)?;
            self.connected.borrow_mut().insert(native_id.to_string());
            Ok(())
        })
    }

    fn disconnect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>> {
        Box::pin(async move {
            self.connected.borrow_mut().remove(native_id);
            self.bridge.close(native_id).await.map_err(gp_error)?;
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
            let raw = self.bridge.config(native_id).await.map_err(gp_error)?;
            let root: Node = serde_wasm_bindgen::from_value(raw)
                .map_err(|e| CameraError::Backend(e.to_string()))?;
            let mut params = parameters_from(&root);
            if params.iter().any(|p| type_of(p) == ParameterType::Focus)
                && !self.focus_drive_works(native_id).await
            {
                params.retain(|p| type_of(p) != ParameterType::Focus);
            }
            Ok(params)
        })
    }

    fn get_live_view_frame<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            let raw = self
                .bridge
                .capture_preview(native_id)
                .await
                .map_err(gp_error)?;
            Ok(file_bytes(&raw)?.1)
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
            match param_type {
                ParameterType::IsoAuto => {
                    return self.set_iso_auto(native_id, is_truthy(value)).await
                }
                ParameterType::FocusAuto => {
                    return self.set_focus_auto(native_id, is_truthy(value)).await
                }
                ParameterType::Focus => return self.drive_focus(native_id, value).await,
                _ => {}
            }
            // Radio on most bodies, Range or Toggle on some: probe each kind
            // over every key the parameter is known by.
            if let Some(radio) = self
                .widget_for(native_id, param_type, Node::is_choice)
                .await?
            {
                let choice = on_off_choice(&radio.choices, is_truthy(value))
                    .cloned()
                    .unwrap_or_else(|| value.to_string());
                return self.set(native_id, &radio, &choice).await;
            }
            if let Some(range) = self
                .widget_for(native_id, param_type, |n| n.kind == "range")
                .await?
            {
                value
                    .trim()
                    .parse::<f32>()
                    .map_err(|_| CameraError::NotSupported)?;
                return self.set(native_id, &range, value.trim()).await;
            }
            if let Some(toggle) = self
                .widget_for(native_id, param_type, |n| n.kind == "toggle")
                .await?
            {
                let flag = if is_truthy(value) { "true" } else { "false" };
                return self.set(native_id, &toggle, flag).await;
            }
            Err(CameraError::NotSupported)
        })
    }

    fn capture_photo<'a>(
        &'a self,
        native_id: &'a str,
    ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
        Box::pin(async move {
            self.require_connected(native_id)?;
            let raw = self
                .bridge
                .capture_image(native_id)
                .await
                .map_err(gp_error)?;
            let (mime, data) = file_bytes(&raw)?;
            // The API only ever returns JPEG; say so instead of handing a raw
            // file over as image/jpeg.
            if !mime.is_empty() && mime != "image/jpeg" {
                return Err(CameraError::Backend(format!(
                    "the camera produced {mime}, not a JPEG — select a JPEG image quality"
                )));
            }
            Ok(data)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Node {
        // A Canon-like tree: ISO with an "Auto" entry, bulb in the shutter list,
        // RAW formats, a read-only widget, a single-choice selector.
        serde_json::from_value(json!({
            "name": "main", "type": "window", "children": [
                { "name": "settings", "type": "section", "children": [
                    { "name": "capturetarget", "type": "radio", "value": "Internal RAM",
                      "choices": ["Internal RAM", "Memory card"] },
                ]},
                { "name": "imgsettings", "type": "section", "children": [
                    { "name": "iso", "type": "radio", "value": "Auto",
                      "choices": ["Auto", "100", "200", "400"] },
                    { "name": "imageformat", "type": "radio", "value": "Large Fine JPEG",
                      "choices": ["Large Fine JPEG", "RAW", "RAW + Large Fine JPEG", "Small JPEG"] },
                ]},
                { "name": "capturesettings", "type": "section", "children": [
                    { "name": "shutterspeed", "type": "radio", "value": "1/60",
                      "choices": ["bulb", "30", "1/60"] },
                    { "name": "aperture", "type": "radio", "value": "5.6", "readonly": true,
                      "choices": ["4", "5.6"] },
                    { "name": "exposurecompensation", "type": "radio", "value": "0", "choices": ["0"] },
                    { "name": "manualfocusdrive", "type": "range", "value": 0,
                      "min": -32767, "max": 32767, "step": 1 },
                ]},
                { "name": "status", "type": "section", "children": [
                    { "name": "serialnumber", "type": "text", "value": "123", "readonly": true },
                    { "name": "datetime", "type": "datetime", "value": 0 },
                ]},
            ]
        }))
        .unwrap()
    }

    #[test]
    fn the_tree_is_curated_like_the_native_backend() {
        let params = parameters_from(&tree());
        let types: Vec<ParameterType> = params.iter().map(type_of).collect();
        assert_eq!(
            types,
            vec![
                ParameterType::IsoAuto,
                ParameterType::Iso,
                ParameterType::ImageQuality,
                ParameterType::ShutterSpeed,
                ParameterType::Focus,
            ],
            "read-only aperture, single-choice exposure compensation, unmapped and text widgets are hidden"
        );
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(
            json[0],
            json!({ "kind": "boolean", "type": "iso_auto", "current": true, "disabled": false })
        );
        assert_eq!(
            json[1]["disabled"], true,
            "ISO is read-only while auto is on"
        );
        assert_eq!(
            json[1]["options"].as_array().unwrap().len(),
            3,
            "Auto is not an ISO value"
        );
        let formats: Vec<&str> = json[2]["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["value"].as_str().unwrap())
            .collect();
        assert_eq!(formats, vec!["Large Fine JPEG", "Small JPEG"]);
        let speeds = json[3]["options"].as_array().unwrap();
        assert!(!speeds.iter().any(|o| o["value"] == "bulb"));
    }

    #[test]
    fn widget_kinds_map_to_leaves() {
        let toggle: Node =
            serde_json::from_value(json!({ "name": "autoiso", "type": "toggle", "value": true }))
                .unwrap();
        assert_eq!(
            toggle.leaf(),
            Some(Leaf::Toggle {
                name: "autoiso".into(),
                readonly: false,
                value: Some(true)
            })
        );
        // A toggle the driver reports as neither on nor off carries no value.
        let action: Node =
            serde_json::from_value(json!({ "name": "autofocusdrive", "type": "toggle" })).unwrap();
        assert!(matches!(
            action.leaf(),
            Some(Leaf::Toggle { value: None, .. })
        ));
        let menu: Node = serde_json::from_value(
            json!({ "name": "f-number", "type": "menu", "value": "f/4", "choices": ["f/4"] }),
        )
        .unwrap();
        assert!(matches!(menu.leaf(), Some(Leaf::Choice { .. })));
        let button: Node =
            serde_json::from_value(json!({ "name": "x", "type": "button" })).unwrap();
        assert_eq!(button.leaf(), None);
    }
}
