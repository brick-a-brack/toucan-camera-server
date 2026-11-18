//! toucan-camera-server for the browser.
//!
//! The same camera API as the native server — opaque device IDs, the
//! `CameraParameter` model, the routes and their status codes, frame averaging —
//! running in a web page, with the browser as the "operating system":
//!
//! - webcams through `getUserMedia` (`webcam` backend, `js/webcam-bridge.js`);
//! - PTP/USB still cameras through libgphoto2 compiled with Emscripten, talking
//!   to the body over WebUSB (`gphoto2` backend, `js/gphoto2-bridge.js`).
//!
//! The parts that define the API are compiled from the server's own sources
//! (`#[path]` includes below), so the two cannot drift apart.
//!
//! JS sees one class, [`ToucanServer`]: typed methods (`listCameras()`,
//! `connect(id)`, …) and `request(method, path, body)`, which answers exactly
//! what the HTTP server would. `js/toucan.js` wraps it into a `fetch()` that
//! also serves the MJPEG live-view stream.

use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

#[path = "../../src/camera/types.rs"]
pub mod camera;

#[path = "../../src/camera/averaging.rs"]
pub mod averaging;

#[path = "../../src/backends/gphoto2/curation.rs"]
pub mod curation;

pub mod api;
pub mod backend;
pub mod gphoto2;
pub mod webcam;

use api::{Backends, HttpResponse, Routed};
use camera::ParameterType;

#[wasm_bindgen(typescript_custom_section)]
const TS_OPTIONS: &str = r#"
/** Bridges to the browser's camera stacks; omit one to disable that backend. */
export interface ToucanServerOptions {
  webcam?: object;
  gphoto2?: object;
}
"#;

/// The camera API, served in-page.
#[wasm_bindgen]
pub struct ToucanServer {
    inner: Rc<Inner>,
}

struct Inner {
    backends: Backends,
    instance_id: String,
}

#[wasm_bindgen]
impl ToucanServer {
    /// Registers a backend for each bridge given in `options`
    /// (`{ webcam, gphoto2 }`, both optional).
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> ToucanServer {
        let mut backends = Backends::default();
        let get = |key: &str| {
            js_sys::Reflect::get(&options, &JsValue::from_str(key))
                .ok()
                .filter(|v| !v.is_undefined() && !v.is_null())
        };
        if let Some(bridge) = get("webcam") {
            backends.push(Rc::new(webcam::WebcamBackend::new(bridge.unchecked_into())));
        }
        if let Some(bridge) = get("gphoto2") {
            backends.push(Rc::new(gphoto2::GPhoto2Backend::new(
                bridge.unchecked_into(),
            )));
        }
        let instance_id = format!(
            "wasm-{:08x}",
            (js_sys::Math::random() * u32::MAX as f64) as u32
        );
        ToucanServer {
            inner: Rc::new(Inner {
                backends,
                instance_id,
            }),
        }
    }

    /// Ids of the registered backends.
    #[wasm_bindgen(getter)]
    pub fn backends(&self) -> Vec<String> {
        self.inner.backends.ids()
    }

    /// The `GET /health` payload.
    pub fn health(&self) -> JsValue {
        to_js(&api::health(&self.inner.instance_id, &self.inner.backends))
    }

    /// `GET /cameras` → `DeviceInfo[]`.
    #[wasm_bindgen(js_name = listCameras)]
    pub fn list_cameras(&self) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move { Ok(to_js(&api::list_cameras(&inner.backends).await)) })
    }

    /// `PUT /cameras/{id}/connect`.
    pub fn connect(&self, id: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move { unit(api::connect(&inner.backends, &id).await) })
    }

    /// `PUT /cameras/{id}/disconnect`.
    pub fn disconnect(&self, id: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move { unit(api::disconnect(&inner.backends, &id).await) })
    }

    /// `GET /cameras/{id}/parameters` → `CameraParameter[]`.
    #[wasm_bindgen(js_name = getParameters)]
    pub fn get_parameters(&self, id: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move { json_body(api::get_parameters(&inner.backends, &id).await) })
    }

    /// `PUT /cameras/{id}/parameters` with `{ type, value }`.
    #[wasm_bindgen(js_name = setParameter)]
    pub fn set_parameter(&self, id: String, param_type: String, value: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            // Validate the type here so a typo is a clear error, not a 422.
            let parsed: Result<ParameterType, _> =
                serde_json::from_value(serde_json::Value::String(param_type.clone()));
            if parsed.is_err() {
                return Err(api_error(&HttpResponse::error(
                    400,
                    format!("unknown parameter type: {param_type}"),
                )));
            }
            let body = serde_json::json!({ "type": param_type, "value": value }).to_string();
            unit(api::set_parameter(&inner.backends, &id, Some(&body)).await)
        })
    }

    /// One live-view frame (JPEG `Uint8Array`).
    #[wasm_bindgen(js_name = liveViewFrame)]
    pub fn live_view_frame(&self, id: String) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            match api::live_view_frame(&inner.backends, &id).await {
                Ok(jpeg) => Ok(js_sys::Uint8Array::from(jpeg.as_slice()).into()),
                Err(response) => Err(api_error(&response)),
            }
        })
    }

    /// `POST /cameras/{id}/capture[?frames=N]` → JPEG `Uint8Array`.
    pub fn capture(&self, id: String, frames: Option<u32>) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let frames = frames.map(|n| n.to_string());
            let response = api::capture(&inner.backends, &id, frames.as_deref()).await;
            if response.status >= 400 {
                return Err(api_error(&response));
            }
            Ok(js_sys::Uint8Array::from(response.body.as_slice()).into())
        })
    }

    /// Routes an HTTP-style request, answering what the native server would:
    /// `{ status, contentType, body: Uint8Array }`, or `{ status: 200,
    /// liveView: id }` for a live-view request the caller has to stream.
    pub fn request(&self, method: String, path: String, body: Option<String>) -> js_sys::Promise {
        let inner = self.inner.clone();
        future_to_promise(async move {
            let routed = api::route(
                &inner.backends,
                &inner.instance_id,
                &method,
                &path,
                body.as_deref(),
            )
            .await;
            let out = js_sys::Object::new();
            let set = |k: &str, v: JsValue| {
                let _ = js_sys::Reflect::set(&out, &JsValue::from_str(k), &v);
            };
            match routed {
                Routed::Response(r) => {
                    set("status", JsValue::from(r.status));
                    set("contentType", JsValue::from_str(r.content_type));
                    set("body", js_sys::Uint8Array::from(r.body.as_slice()).into());
                }
                Routed::LiveView { id } => {
                    set("status", JsValue::from(200));
                    set("liveView", JsValue::from_str(&id));
                }
            }
            Ok(out.into())
        })
    }
}

/// The version of this build (the server's crate version).
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[wasm_bindgen(start)]
fn start() {
    // Rust panics land in the console with a message instead of "unreachable".
    std::panic::set_hook(Box::new(|info| {
        api::log(&format!("[toucan-wasm] panic: {info}"));
    }));
}

fn to_js<T: serde::Serialize>(value: &T) -> JsValue {
    value
        .serialize(&serde_wasm_bindgen::Serializer::json_compatible())
        .unwrap_or(JsValue::NULL)
}

/// A JS `Error` carrying the API's message and HTTP status (`err.status`).
fn api_error(response: &HttpResponse) -> JsValue {
    let message = response.error_message().unwrap_or_else(|| "error".into());
    let err = js_sys::Error::new(&message);
    let _ = js_sys::Reflect::set(
        &err,
        &JsValue::from_str("status"),
        &JsValue::from(response.status),
    );
    err.into()
}

fn unit(response: HttpResponse) -> Result<JsValue, JsValue> {
    if response.status >= 400 {
        Err(api_error(&response))
    } else {
        Ok(JsValue::UNDEFINED)
    }
}

fn json_body(response: HttpResponse) -> Result<JsValue, JsValue> {
    if response.status >= 400 {
        return Err(api_error(&response));
    }
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap_or_default();
    Ok(to_js(&value))
}

/// The message of a JS exception (an `Error`, a string, or anything else).
pub(crate) fn js_error_message(err: &JsValue) -> String {
    if let Some(e) = err.dyn_ref::<js_sys::Error>() {
        let name = String::from(e.name());
        let message = String::from(e.message());
        return if name.is_empty() || name == "Error" {
            message
        } else {
            format!("{name}: {message}")
        };
    }
    err.as_string().unwrap_or_else(|| format!("{err:?}"))
}

/// The numeric `code` an error carries (libgphoto2's result code), if any.
pub(crate) fn js_error_code(err: &JsValue) -> Option<i32> {
    js_sys::Reflect::get(err, &JsValue::from_str("code"))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|v| v as i32)
}
