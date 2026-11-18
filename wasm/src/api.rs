//! The server's HTTP contract, served in-process.
//!
//! Every route of `src/routes/cameras.rs` (minus the MJPEG stream itself, which
//! the JS wrapper assembles from single frames) with the same paths, bodies,
//! status codes and error messages, so a client written against the native
//! server works unchanged against `ToucanServer.request()` / `.fetch()`.

use std::rc::Rc;

use serde::Deserialize;
use serde_json::json;

use crate::averaging::{average_frames, parse_frame_count};
use crate::backend::WebBackend;
use crate::camera::{CameraError, DeviceId, DeviceInfo, ParameterType};

/// The backends registered for this page, in registration order.
#[derive(Clone, Default)]
pub struct Backends(Vec<Rc<dyn WebBackend>>);

impl Backends {
    pub fn push(&mut self, backend: Rc<dyn WebBackend>) {
        self.0.push(backend);
    }

    pub fn ids(&self) -> Vec<String> {
        self.0.iter().map(|b| b.backend_id().to_string()).collect()
    }

    fn get(&self, backend_id: &str) -> Option<&Rc<dyn WebBackend>> {
        self.0.iter().find(|b| b.backend_id() == backend_id)
    }
}

/// An HTTP-shaped answer: what the native server would have sent.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn json(status: u16, value: &impl serde::Serialize) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_default(),
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &json!({ "error": message.into() }))
    }

    fn ok() -> Self {
        Self {
            status: 200,
            content_type: "text/plain",
            body: Vec::new(),
        }
    }

    fn jpeg(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            content_type: "image/jpeg",
            body,
        }
    }

    /// The error message of an error response, if this is one.
    pub fn error_message(&self) -> Option<String> {
        if self.status < 400 {
            return None;
        }
        serde_json::from_slice::<serde_json::Value>(&self.body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .or_else(|| Some(format!("HTTP {}", self.status)))
    }
}

/// What a route resolved to. Live view is the one route whose body is a stream,
/// so it is handed back to the JS wrapper rather than produced here.
#[derive(Debug, Clone, PartialEq)]
pub enum Routed {
    Response(HttpResponse),
    /// `GET /cameras/{id}/liveview` on a connected device: stream its frames.
    LiveView {
        id: String,
    },
}

/// The `GET /health` payload, same fields as the native server plus the
/// backends registered in this page.
pub fn health(instance_id: &str, backends: &Backends) -> serde_json::Value {
    json!({
        "status": "ok",
        "service": "toucan-camera-server",
        "version": env!("CARGO_PKG_VERSION"),
        "instance_id": instance_id,
        "runtime": "wasm",
        "backends": backends.ids(),
    })
}

/// Routes one request. `path` may carry a query string (`?frames=N`).
pub async fn route(
    backends: &Backends,
    instance_id: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Routed {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let method = method.to_ascii_uppercase();

    let response = match (method.as_str(), segments.as_slice()) {
        ("GET", ["health"]) => HttpResponse::json(200, &health(instance_id, backends)),
        ("GET", ["cameras"]) => HttpResponse::json(200, &list_cameras(backends).await),
        ("PUT", ["cameras", id, "connect"]) => connect(backends, id).await,
        ("PUT", ["cameras", id, "disconnect"]) => disconnect(backends, id).await,
        ("GET", ["cameras", id, "parameters"]) => get_parameters(backends, id).await,
        ("PUT", ["cameras", id, "parameters"]) => set_parameter(backends, id, body).await,
        ("GET", ["cameras", id, "liveview"]) => match resolve(backends, id) {
            Err(response) => response,
            Ok((backend, native_id)) if !backend.is_connected(&native_id) => {
                HttpResponse::error(409, "device is not connected")
            }
            Ok(_) => return Routed::LiveView { id: id.to_string() },
        },
        ("POST", ["cameras", id, "capture"]) => {
            capture(backends, id, query_param(query, "frames").as_deref()).await
        }
        (_, ["health"] | ["cameras"] | ["cameras", _, _]) => {
            HttpResponse::error(405, "method not allowed")
        }
        _ => HttpResponse::error(404, "not found"),
    };
    Routed::Response(response)
}

/// Every backend's devices, one after the other. A failing backend is logged
/// and skipped, like on the server. (No cross-backend dedup: in a browser the
/// webcam and WebUSB backends never see the same interface of a device.)
pub async fn list_cameras(backends: &Backends) -> Vec<DeviceInfo> {
    let mut devices = Vec::new();
    for backend in &backends.0 {
        match backend.list_devices().await {
            Ok(found) => devices.extend(found),
            Err(e) => log(&format!(
                "[error] failed to list devices from {}: {e}",
                backend.backend_id()
            )),
        }
    }
    devices
}

pub async fn connect(backends: &Backends, id: &str) -> HttpResponse {
    match resolve(backends, id) {
        Err(response) => response,
        Ok((backend, native_id)) => unit_response(backend.connect(&native_id).await),
    }
}

pub async fn disconnect(backends: &Backends, id: &str) -> HttpResponse {
    match resolve(backends, id) {
        Err(response) => response,
        Ok((backend, native_id)) => unit_response(backend.disconnect(&native_id).await),
    }
}

pub async fn get_parameters(backends: &Backends, id: &str) -> HttpResponse {
    let (backend, native_id) = match resolve(backends, id) {
        Ok(found) => found,
        Err(response) => return response,
    };
    match backend.get_parameters(&native_id).await {
        Ok(params) => HttpResponse::json(200, &params),
        Err(CameraError::NotConnected) => HttpResponse::error(409, "device is not connected"),
        Err(e) => HttpResponse::error(500, e.to_string()),
    }
}

#[derive(Deserialize)]
struct SetParameterBody {
    #[serde(rename = "type")]
    param_type: ParameterType,
    value: String,
}

pub async fn set_parameter(backends: &Backends, id: &str, body: Option<&str>) -> HttpResponse {
    let (backend, native_id) = match resolve(backends, id) {
        Ok(found) => found,
        Err(response) => return response,
    };
    // axum's Json extractor answers 422 to a body it cannot deserialize.
    let body: SetParameterBody = match serde_json::from_str(body.unwrap_or("")) {
        Ok(body) => body,
        Err(e) => return HttpResponse::error(422, format!("invalid body: {e}")),
    };
    match backend
        .set_parameter(&native_id, body.param_type, &body.value)
        .await
    {
        Ok(()) => HttpResponse::ok(),
        Err(CameraError::NotConnected) => HttpResponse::error(409, "device is not connected"),
        Err(CameraError::NotSupported) => HttpResponse::error(400, "unknown parameter type"),
        Err(e) => HttpResponse::error(500, e.to_string()),
    }
}

/// `POST /cameras/{id}/capture[?frames=N]`: one still, or N consecutive stills
/// averaged pixel by pixel (the server's `average_frames`, compiled from the
/// same source file).
pub async fn capture(backends: &Backends, id: &str, frames: Option<&str>) -> HttpResponse {
    let (backend, native_id) = match resolve(backends, id) {
        Ok(found) => found,
        Err(response) => return response,
    };
    let count = parse_frame_count(frames);

    let mut shots = Vec::with_capacity(count as usize);
    for _ in 0..count {
        match backend.capture_photo(&native_id).await {
            Ok(jpeg) => shots.push(jpeg),
            Err(CameraError::NotConnected) => {
                return HttpResponse::error(409, "device is not connected")
            }
            Err(CameraError::NotSupported) => {
                return HttpResponse::error(405, "photo capture not supported by this backend")
            }
            Err(e) => return HttpResponse::error(500, e.to_string()),
        }
    }

    // A single shot is passed through untouched (no re-encode), like the server.
    if shots.len() == 1 {
        return HttpResponse::jpeg(shots.remove(0));
    }
    match average_frames(&shots) {
        Ok(merged) => HttpResponse::jpeg(merged),
        Err(e) => HttpResponse::error(500, e),
    }
}

/// One live-view frame, for the JS stream assembler.
pub async fn live_view_frame(backends: &Backends, id: &str) -> Result<Vec<u8>, HttpResponse> {
    let (backend, native_id) = resolve(backends, id)?;
    match backend.get_live_view_frame(&native_id).await {
        Ok(jpeg) => Ok(jpeg),
        Err(CameraError::NotConnected) => Err(HttpResponse::error(409, "device is not connected")),
        Err(e) => Err(HttpResponse::error(500, e.to_string())),
    }
}

/// Decodes an opaque ID and finds its backend — the server's `dispatch` prologue.
fn resolve(backends: &Backends, id: &str) -> Result<(Rc<dyn WebBackend>, String), HttpResponse> {
    let dev_id = DeviceId::decode(id).map_err(|_| HttpResponse::error(400, "invalid device id"))?;
    let backend = backends
        .get(&dev_id.backend)
        .ok_or_else(|| HttpResponse::error(404, format!("unknown backend: {}", dev_id.backend)))?;
    Ok((backend.clone(), dev_id.native_id))
}

fn unit_response(result: Result<(), CameraError>) -> HttpResponse {
    match result {
        Ok(()) => HttpResponse::ok(),
        Err(CameraError::DeviceNotFound(id)) => {
            HttpResponse::error(404, format!("device not found: {id}"))
        }
        Err(e) => HttpResponse::error(500, e.to_string()),
    }
}

/// The raw value of `name` in a query string (no percent-decoding needed for
/// the only parameter the API has, `frames`).
fn query_param(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('=').or(Some((pair, ""))))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

pub fn log(message: &str) {
    #[cfg(target_arch = "wasm32")]
    web_log(message);
    #[cfg(not(target_arch = "wasm32"))]
    eprintln!("{message}");
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = warn)]
    fn web_log(message: &str);
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::backend::LocalFuture;
    use crate::camera::CameraParameter;

    /// Drives a future that never actually waits (the fake backend answers at
    /// once) — enough to test the router without an async runtime.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    fn solid_jpeg(value: u8) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([value, value, value]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
            .encode_image(&img)
            .expect("encode");
        out
    }

    #[derive(Default)]
    struct Fake {
        connected: RefCell<HashSet<String>>,
        shots: RefCell<u8>,
    }

    impl WebBackend for Fake {
        fn backend_id(&self) -> &str {
            "fake"
        }
        fn list_devices(&self) -> LocalFuture<'_, Result<Vec<DeviceInfo>, CameraError>> {
            Box::pin(async move {
                Ok(vec![DeviceInfo {
                    id: DeviceId::new("fake", "cam0").encode(),
                    name: "Fake camera".into(),
                    connected: self.connected.borrow().contains("cam0"),
                    dedup_key: None,
                }])
            })
        }
        fn connect<'a>(&'a self, native_id: &'a str) -> LocalFuture<'a, Result<(), CameraError>> {
            Box::pin(async move {
                if native_id != "cam0" {
                    return Err(CameraError::DeviceNotFound(native_id.into()));
                }
                self.connected.borrow_mut().insert(native_id.into());
                Ok(())
            })
        }
        fn disconnect<'a>(
            &'a self,
            native_id: &'a str,
        ) -> LocalFuture<'a, Result<(), CameraError>> {
            Box::pin(async move {
                self.connected.borrow_mut().remove(native_id);
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
                if !self.is_connected(native_id) {
                    return Err(CameraError::NotConnected);
                }
                Ok(vec![CameraParameter::Boolean {
                    param_type: ParameterType::FocusAuto,
                    current: true,
                    disabled: false,
                }])
            })
        }
        fn get_live_view_frame<'a>(
            &'a self,
            _: &'a str,
        ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
            Box::pin(async move { Ok(solid_jpeg(0)) })
        }
        fn set_parameter<'a>(
            &'a self,
            _: &'a str,
            param_type: ParameterType,
            _: &'a str,
        ) -> LocalFuture<'a, Result<(), CameraError>> {
            Box::pin(async move {
                match param_type {
                    ParameterType::FocusAuto => Ok(()),
                    _ => Err(CameraError::NotSupported),
                }
            })
        }
        fn capture_photo<'a>(
            &'a self,
            native_id: &'a str,
        ) -> LocalFuture<'a, Result<Vec<u8>, CameraError>> {
            Box::pin(async move {
                if !self.is_connected(native_id) {
                    return Err(CameraError::NotConnected);
                }
                // Alternate dark and bright shots so averaging is observable.
                let mut shots = self.shots.borrow_mut();
                *shots += 1;
                Ok(solid_jpeg(if *shots % 2 == 1 { 100 } else { 200 }))
            })
        }
    }

    fn backends() -> Backends {
        let mut b = Backends::default();
        b.push(Rc::new(Fake::default()));
        b
    }

    fn call(b: &Backends, method: &str, path: &str, body: Option<&str>) -> HttpResponse {
        match block_on(route(b, "test", method, path, body)) {
            Routed::Response(r) => r,
            Routed::LiveView { .. } => HttpResponse::ok(),
        }
    }

    fn cam() -> String {
        DeviceId::new("fake", "cam0").encode()
    }

    #[test]
    fn health_lists_the_backends() {
        let r = call(&backends(), "GET", "/health", None);
        assert_eq!(r.status, 200);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["backends"], json!(["fake"]));
    }

    #[test]
    fn camera_lifecycle_matches_the_server_contract() {
        let b = backends();
        let id = cam();
        let list: serde_json::Value =
            serde_json::from_slice(&call(&b, "GET", "/cameras", None).body).unwrap();
        assert_eq!(list[0]["id"], id.as_str());
        assert_eq!(list[0]["connected"], false);
        assert!(
            list[0].get("dedup_key").is_none(),
            "dedup_key is never serialized"
        );

        assert_eq!(
            call(&b, "GET", &format!("/cameras/{id}/parameters"), None).status,
            409
        );
        assert_eq!(
            call(&b, "PUT", &format!("/cameras/{id}/connect"), None).status,
            200
        );
        let params = call(&b, "GET", &format!("/cameras/{id}/parameters"), None);
        assert_eq!(params.status, 200);
        let params: serde_json::Value = serde_json::from_slice(&params.body).unwrap();
        assert_eq!(params[0]["kind"], "boolean");
        assert_eq!(params[0]["type"], "focus_auto");

        let set =
            |body: &str| call(&b, "PUT", &format!("/cameras/{id}/parameters"), Some(body)).status;
        assert_eq!(set(r#"{"type":"focus_auto","value":"false"}"#), 200);
        assert_eq!(set(r#"{"type":"zoom","value":"3"}"#), 400);
        assert_eq!(set(r#"{"type":"not_a_param","value":"3"}"#), 422);

        assert_eq!(
            call(&b, "PUT", &format!("/cameras/{id}/disconnect"), None).status,
            200
        );
        assert_eq!(
            call(&b, "POST", &format!("/cameras/{id}/capture"), None).status,
            409
        );
    }

    #[test]
    fn bad_ids_and_routes() {
        let b = backends();
        assert_eq!(call(&b, "PUT", "/cameras/!!!/connect", None).status, 400);
        let unknown = DeviceId::new("nope", "x").encode();
        assert_eq!(
            call(&b, "PUT", &format!("/cameras/{unknown}/connect"), None).status,
            404
        );
        let missing = DeviceId::new("fake", "cam9").encode();
        assert_eq!(
            call(&b, "PUT", &format!("/cameras/{missing}/connect"), None).status,
            404
        );
        assert_eq!(call(&b, "GET", "/nothing", None).status, 404);
        assert_eq!(call(&b, "DELETE", "/cameras", None).status, 405);
    }

    #[test]
    fn live_view_needs_a_connection() {
        let b = backends();
        let id = cam();
        let path = format!("/cameras/{id}/liveview");
        assert_eq!(call(&b, "GET", &path, None).status, 409);
        call(&b, "PUT", &format!("/cameras/{id}/connect"), None);
        assert_eq!(
            block_on(route(&b, "t", "GET", &path, None)),
            Routed::LiveView { id: id.clone() }
        );
    }

    #[test]
    fn capture_averages_frames() {
        let b = backends();
        let id = cam();
        call(&b, "PUT", &format!("/cameras/{id}/connect"), None);

        let single = call(&b, "POST", &format!("/cameras/{id}/capture"), None);
        assert_eq!(single.status, 200);
        assert_eq!(single.content_type, "image/jpeg");
        assert_eq!(
            single.body,
            solid_jpeg(100),
            "one shot is passed through untouched"
        );

        let merged = call(&b, "POST", &format!("/cameras/{id}/capture?frames=2"), None);
        assert_eq!(merged.status, 200);
        let img = image::load_from_memory(&merged.body).unwrap().to_rgb8();
        let px = img.get_pixel(4, 4).0[0] as i32;
        assert!((px - 150).abs() <= 3, "mean of 200 and 100 was {px}");
    }

    #[test]
    fn query_parsing() {
        assert_eq!(query_param("frames=3", "frames").as_deref(), Some("3"));
        assert_eq!(query_param("a=1&frames=", "frames").as_deref(), Some(""));
        assert_eq!(query_param("frames", "frames").as_deref(), Some(""));
        assert_eq!(query_param("", "frames"), None);
    }
}
