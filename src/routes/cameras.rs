use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use serde_json::json;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use serde::Deserialize;

use crate::camera::{CameraBackend, CameraError, DeviceId, DeviceInfo, ParameterType};

// ---------------------------------------------------------------------------
// State types
// ---------------------------------------------------------------------------

pub type BackendState = Arc<HashMap<String, Arc<dyn CameraBackend>>>;

/// One broadcast sender per active live-view device.
/// The key is the opaque (encoded) device ID.
/// Wrapped in Arc so the capture loop can use ptr_eq to avoid removing a
/// sender that was replaced by a newer connection while the loop was exiting.
type LiveViewSenders = Arc<Mutex<HashMap<String, Arc<broadcast::Sender<Arc<Bytes>>>>>>;

#[derive(Clone)]
pub struct AppState {
    pub backends: BackendState,
    pub live_views: LiveViewSenders,
    pub token: Arc<RwLock<String>>,
    /// Unique ID generated once at startup, exposed in `/health` so peers can
    /// detect self-registration attempts.
    pub instance_id: Arc<String>,
    /// Shared peer registry, also held by the remote backend.
    #[cfg(feature = "backend-remote")]
    pub peers: Arc<crate::backends::remote::PeerRegistry>,
}

impl axum::extract::FromRef<AppState> for BackendState {
    fn from_ref(state: &AppState) -> Self {
        state.backends.clone()
    }
}

impl AppState {
    pub fn new(
        backends: BackendState,
        token: Arc<RwLock<String>>,
        instance_id: Arc<String>,
        #[cfg(feature = "backend-remote")] peers: Arc<crate::backends::remote::PeerRegistry>,
    ) -> Self {
        Self {
            backends,
            live_views: Arc::new(Mutex::new(HashMap::new())),
            token,
            instance_id,
            #[cfg(feature = "backend-remote")]
            peers,
        }
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn list_cameras(State(backends): State<BackendState>) -> Json<Vec<DeviceInfo>> {
    // Query every backend concurrently, each on a blocking thread with a timeout.
    // `list_devices` is a blocking SDK call; running them in parallel means a slow
    // backend can't serialize behind the others, and the timeout means one that is
    // still initializing (e.g. a Nikon SDK warming up) can't stall the listing —
    // it simply appears on a later poll once ready.
    let timeout = std::time::Duration::from_secs(3);

    let tasks: Vec<_> = backends
        .values()
        .cloned()
        .map(|backend| {
            tokio::spawn(async move {
                let priority = backend.dedup_priority();
                let listed = tokio::time::timeout(
                    timeout,
                    tokio::task::spawn_blocking(move || backend.list_devices()),
                )
                .await;
                match listed {
                    Ok(Ok(Ok(found))) => {
                        found.into_iter().map(|d| (priority, d)).collect::<Vec<_>>()
                    }
                    Ok(Ok(Err(e))) => {
                        eprintln!("[error] failed to list devices from backend: {e}");
                        Vec::new()
                    }
                    Ok(Err(_)) => Vec::new(), // spawn_blocking panicked
                    Err(_) => Vec::new(),     // backend too slow this round
                }
            })
        })
        .collect();

    // Await in spawn order so the listing order stays stable across polls.
    let mut devices: Vec<(i32, DeviceInfo)> = Vec::new();
    for task in tasks {
        if let Ok(part) = task.await {
            devices.extend(part);
        }
    }

    Json(dedup_devices(devices))
}

/// Drops cross-backend duplicates: when several backends report the same physical
/// camera (same [`DeviceInfo::dedup_key`]), only the highest-priority backend's
/// entry is kept. Devices without a dedup key are always kept (never deduped).
/// Order is otherwise preserved.
fn dedup_devices(devices: Vec<(i32, DeviceInfo)>) -> Vec<DeviceInfo> {
    use std::collections::HashMap;

    // Diagnostic (TOUCAN_DEDUP_DEBUG=1): one line per /cameras call showing each
    // device's (priority, dedup_key, name) to spot cross-backend key mismatches.
    if crate::camera::dedup_debug_enabled() {
        eprintln!(
            "[dedup] in: {:?}",
            devices
                .iter()
                .map(|(p, d)| (*p, d.dedup_key.clone(), d.name.clone()))
                .collect::<Vec<_>>()
        );
    }

    // Winning priority per dedup key.
    let mut best: HashMap<&str, i32> = HashMap::new();
    for (priority, dev) in &devices {
        if let Some(key) = dev.dedup_key.as_deref() {
            best.entry(key)
                .and_modify(|p| *p = (*p).max(*priority))
                .or_insert(*priority);
        }
    }

    // Keep keyless devices, and for keyed ones only the first at the winning
    // priority (guards against two backends sharing the top priority).
    let mut taken: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(devices.len());
    for (priority, dev) in &devices {
        match dev.dedup_key.as_deref() {
            None => out.push(dev.clone()),
            Some(key) => {
                if best.get(key) == Some(priority) && taken.insert(key) {
                    out.push(dev.clone());
                }
            }
        }
    }
    out
}

pub async fn connect_camera(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    dispatch(&backends, &id, |b, native_id| b.connect(native_id)).await
}

pub async fn get_parameters(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    let native_id = dev_id.native_id.clone();
    let result =
        tokio::task::spawn_blocking(move || backend.get_parameters(&native_id)).await;

    match result {
        Ok(Ok(params)) => Json(params).into_response(),
        Ok(Err(CameraError::NotConnected)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "device is not connected" })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response(),
    }
}

pub async fn disconnect_camera(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // Drop the live-view sender before disconnecting so the capture loop stops
    // and the next connection always starts with a fresh sender.
    state.live_views.lock().await.remove(&id);
    dispatch(&state.backends, &id, |b, native_id| b.disconnect(native_id)).await
}

pub async fn live_view(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match state.backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    // Pre-flight: reject before sending any headers if device is not connected.
    {
        let backend_clone = backend.clone();
        let native_id = dev_id.native_id.clone();
        let connected = tokio::task::spawn_blocking(move || backend_clone.is_connected(&native_id))
            .await
            .unwrap_or(false);

        if !connected {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "device is not connected" })),
            )
                .into_response();
        }
    }

    // Subscribe to the shared broadcast channel, starting a capture loop if needed.
    let rx = {
        let mut senders = state.live_views.lock().await;
        let sender = senders.entry(id.clone()).or_insert_with(|| {
            let (tx, _) = broadcast::channel::<Arc<Bytes>>(4);
            Arc::new(tx)
        }).clone();

        let rx = sender.subscribe();

        // If we are the first subscriber, spawn the capture loop.
        // receiver_count includes the receiver we just created.
        if sender.receiver_count() == 1 {
            let tx = sender.clone();
            let backend_loop = backend.clone();
            let native_id = dev_id.native_id.clone();
            let live_views_loop = state.live_views.clone();
            let opaque_id_loop = id.clone();

            tokio::spawn(async move {
                // Cap at 30 fps (≈32 ms/frame). Backends slower than this run
                // at their natural pace; fast backends (AVFoundation) are
                // prevented from spinning at CPU speed.
                // ~30 fps. This already oversamples real camera live-view rates
                // (~20-30 fps unique), so it captures every fresh frame; polling
                // faster (e.g. 16 ms) only wastes USB bandwidth — which matters when
                // two cameras stream at once: aggressive per-frame USB reads (Canon
                // EVF) starve another camera's passive SDK stream (Nikon).
                let frame_interval = tokio::time::Duration::from_millis(32);
                // Break after ~10 s of consecutive not-ready frames to avoid
                // spinning forever when the camera stalls (e.g. after a
                // parameter change that disrupts the capture pipeline).
                let mut consecutive_misses: u32 = 0;
                const MAX_CONSECUTIVE_MISSES: u32 = 300;
                // Only broadcast frames that actually changed, so polling faster
                // than the camera's frame rate doesn't flood clients with duplicates.
                let mut last_jpeg: Option<Bytes> = None;
                // Cached multipart frame + when it was last sent, for the heartbeat
                // below.
                let mut last_frame: Option<Arc<Bytes>> = None;
                let mut last_send = tokio::time::Instant::now();
                // Re-send the cached frame at least this often (~15x/s) even when
                // the bytes are unchanged. Real cameras have sensor noise so every
                // frame differs and dedup rarely triggers; but virtual cameras
                // (OBS, Logi Capture) re-emit byte-identical JPEGs on a static
                // scene, which the dedup would otherwise drop forever — freezing
                // the preview and starving late `broadcast` subscribers, who only
                // receive frames sent after they subscribe. The heartbeat keeps a
                // static virtual camera alive and feeds late subscribers within
                // one interval.
                const HEARTBEAT: tokio::time::Duration =
                    tokio::time::Duration::from_millis(66);

                loop {
                    let tick = tokio::time::Instant::now();

                    // No subscribers left — stop.
                    if tx.receiver_count() == 0 {
                        break;
                    }

                    let b = backend_loop.clone();
                    let nid = native_id.clone();
                    let result =
                        tokio::task::spawn_blocking(move || b.get_live_view_frame(&nid)).await;

                    match result {
                        Ok(Ok(jpeg)) => {
                            consecutive_misses = 0;
                            let jpeg = Bytes::from(jpeg);
                            // Unchanged frame (we may poll faster than the camera
                            // produces). Re-send the cached frame only on the
                            // heartbeat, otherwise skip to save bandwidth.
                            if last_jpeg.as_ref() == Some(&jpeg) {
                                if last_send.elapsed() >= HEARTBEAT {
                                    if let Some(frame) = &last_frame {
                                        if tx.send(frame.clone()).is_err() {
                                            break;
                                        }
                                        last_send = tokio::time::Instant::now();
                                    }
                                }
                                let elapsed = tick.elapsed();
                                if elapsed < frame_interval {
                                    tokio::time::sleep(frame_interval - elapsed).await;
                                }
                                continue;
                            }
                            last_jpeg = Some(jpeg.clone());

                            let mut buf = format!(
                                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                jpeg.len()
                            )
                            .into_bytes();
                            buf.extend_from_slice(&jpeg);
                            buf.extend_from_slice(b"\r\n");

                            let frame = Arc::new(Bytes::from(buf));
                            last_frame = Some(frame.clone());
                            // send() only errors when there are no receivers.
                            if tx.send(frame).is_err() {
                                break;
                            }
                            last_send = tokio::time::Instant::now();
                        }
                        Ok(Err(crate::camera::CameraError::SdkError(0x0000_A102))) => {
                            // EVF / camera not ready yet — skip frame.
                            consecutive_misses += 1;
                            if consecutive_misses >= MAX_CONSECUTIVE_MISSES {
                                eprintln!("[warn] live view stalled for {native_id} after {consecutive_misses} consecutive misses, stopping loop");
                                break;
                            }
                        }
                        Ok(Err(e)) => {
                            eprintln!("[error] live view frame error for {native_id}: {e}");
                            break;
                        }
                        Err(_) => break, // spawn_blocking panicked
                    }

                    let elapsed = tick.elapsed();
                    if elapsed < frame_interval {
                        tokio::time::sleep(frame_interval - elapsed).await;
                    }
                }

                // Remove the sender only if it is still ours. A reconnect may have
                // already replaced it with a new sender — in that case, leave it alone.
                let mut senders = live_views_loop.lock().await;
                if let Some(current) = senders.get(&opaque_id_loop) {
                    if Arc::ptr_eq(current, &tx) {
                        senders.remove(&opaque_id_loop);
                    }
                }
            });
        }

        rx
    };

    // Convert the broadcast receiver into an HTTP body stream.
    // When the broadcast channel closes (capture loop stopped), chain a deliberate
    // IO error so axum resets the TCP connection instead of ending the response
    // cleanly. A clean end is invisible to the browser (onerror never fires on the
    // <img> element), leaving the live view panel frozen with the last frame.
    let stream = BroadcastStream::new(rx)
        .filter_map(|res| match res {
            Ok(frame) => Some(Ok::<Bytes, std::io::Error>((*frame).clone())),
            Err(_) => None, // lagged frames — just skip
        })
        .chain(tokio_stream::iter(std::iter::once(Err::<Bytes, std::io::Error>(
            std::io::Error::new(std::io::ErrorKind::ConnectionReset, "live view stream closed"),
        ))));

    Response::builder()
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )
        .body(Body::from_stream(stream))
        .unwrap()
}

#[derive(Deserialize)]
pub struct SetParameterBody {
    #[serde(rename = "type")]
    param_type: ParameterType,
    /// Always a string. Range params: stringified integer. Select / RangeSelect: opaque key.
    value: String,
}

pub async fn set_parameter(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
    Json(body): Json<SetParameterBody>,
) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    let native_id = dev_id.native_id.clone();
    let result = tokio::task::spawn_blocking(move || {
        backend.set_parameter(&native_id, body.param_type, &body.value)
    })
    .await;

    match result {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(CameraError::NotConnected)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "device is not connected" })),
        )
            .into_response(),
        Ok(Err(CameraError::NotSupported)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unknown parameter type" })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response(),
    }
}

pub async fn capture_photo(
    State(backends): State<BackendState>,
    Path(id): Path<String>,
) -> Response {
    let dev_id = match DeviceId::decode(&id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    let native_id = dev_id.native_id.clone();
    let result = tokio::task::spawn_blocking(move || backend.capture_photo(&native_id)).await;

    match result {
        Ok(Ok(bytes)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap(),
        Ok(Err(CameraError::NotConnected)) => (
            StatusCode::CONFLICT,
            Json(json!({ "error": "device is not connected" })),
        )
            .into_response(),
        Ok(Err(CameraError::NotSupported)) => (
            StatusCode::METHOD_NOT_ALLOWED,
            Json(json!({ "error": "photo capture not supported by this backend" })),
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Decodes an opaque device ID, routes to the correct backend, and runs `op` on a
/// blocking thread (the SDK calls are blocking — keep them off the async executor
/// so one slow/stuck backend can't freeze the HTTP server).
async fn dispatch(
    backends: &HashMap<String, Arc<dyn CameraBackend>>,
    opaque_id: &str,
    op: fn(&dyn CameraBackend, &str) -> Result<(), CameraError>,
) -> Response {
    let dev_id = match DeviceId::decode(opaque_id) {
        Ok(d) => d,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid device id" })),
            )
                .into_response()
        }
    };

    let backend = match backends.get(&dev_id.backend) {
        Some(b) => b.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": format!("unknown backend: {}", dev_id.backend) })),
            )
                .into_response()
        }
    };

    let native_id = dev_id.native_id;
    let result = tokio::task::spawn_blocking(move || op(backend.as_ref(), &native_id))
        .await
        .unwrap_or(Err(CameraError::SdkError(0xFFFF_FFFF)));

    match result {
        Ok(()) => StatusCode::OK.into_response(),
        Err(CameraError::DeviceNotFound(id)) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": format!("device not found: {id}") })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::*;

    fn dev(id: &str, key: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: id.to_string(),
            connected: false,
            dedup_key: key.map(str::to_string),
        }
    }

    #[test]
    fn higher_priority_backend_wins_duplicate() {
        // gphoto2 (prio 0) and the Nikon SDK (prio 10) both report the same body.
        let out = dedup_devices(vec![
            (0, dev("gphoto", Some("04b0:z5ii"))),
            (10, dev("nikon", Some("04b0:z5ii"))),
        ]);
        let ids: Vec<&str> = out.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["nikon"]);
    }

    #[test]
    fn keyless_and_distinct_devices_are_all_kept() {
        let out = dedup_devices(vec![
            (0, dev("webcam", None)),                    // no key → always kept
            (0, dev("gphoto-d850", Some("04b0:d850"))),  // no SDK entry → unique
            (10, dev("nikon-z5ii", Some("04b0:z5ii"))),
            (0, dev("gphoto-z5ii", Some("04b0:z5ii"))),  // dup of the Nikon one
        ]);
        let ids: Vec<&str> = out.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["webcam", "gphoto-d850", "nikon-z5ii"]);
    }

    #[test]
    fn two_keyless_duplicates_of_same_key_collapse_to_one() {
        // Same priority + same key (e.g. two backends both prio 0) → keep first.
        let out = dedup_devices(vec![
            (0, dev("a", Some("04a9:eosr5"))),
            (0, dev("b", Some("04a9:eosr5"))),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
    }
}
