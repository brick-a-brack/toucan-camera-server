//! Integration tests for the live-view loop lifecycle across connect /
//! disconnect / reconnect, driven against an in-memory mock backend.
//!
//! The mock records what the route layer does to the camera: how many frame
//! polls run at once (two loops on one device would show as 2) and whether a
//! poll was still running when the session was closed.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bytes::Bytes;
use tokio_stream::StreamExt;
use tower::ServiceExt; // for `oneshot`

use toucan_camera::build_router;
use toucan_camera::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterType,
};
use toucan_camera::routes::cameras::{AppState, BackendState};

const TOKEN: &str = "test-token";
const NATIVE_ID: &str = "cam0";

/// A minimal valid baseline JPEG is not needed: the route only treats the bytes
/// as opaque frames (dimensions are read for the end-of-stream art only, with a
/// fallback). Each frame differs so the loop never dedups them.
#[derive(Default)]
struct MockBackend {
    connected: Mutex<HashSet<String>>,
    /// Frame polls currently running.
    in_flight: AtomicUsize,
    /// Highest number of frame polls ever running at once.
    max_in_flight: AtomicUsize,
    /// Set when `disconnect` ran while a frame poll was in flight.
    overlapped_disconnect: AtomicBool,
    frame_no: AtomicUsize,
}

impl CameraBackend for MockBackend {
    fn backend_id(&self) -> &str {
        "mock"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        Ok(vec![DeviceInfo {
            id: DeviceId::new("mock", NATIVE_ID).encode(),
            name: "Mock camera".into(),
            connected: self.is_connected(NATIVE_ID),
            dedup_key: None,
        }])
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        self.connected.lock().unwrap().insert(native_id.to_string());
        Ok(())
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            self.overlapped_disconnect.store(true, Ordering::SeqCst);
        }
        self.connected.lock().unwrap().remove(native_id);
        Ok(())
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.connected.lock().unwrap().contains(native_id)
    }

    fn get_parameters(&self, _native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        Ok(Vec::new())
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        // A real camera's frame read takes a while; this is the window in which
        // a disconnect used to race the poll.
        std::thread::sleep(Duration::from_millis(40));
        let connected = self.is_connected(native_id);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if !connected {
            return Err(CameraError::NotConnected);
        }
        let n = self.frame_no.fetch_add(1, Ordering::SeqCst);
        Ok(format!("frame-{n}").into_bytes())
    }

    fn set_parameter(
        &self,
        _native_id: &str,
        _param_type: ParameterType,
        _value: &str,
    ) -> Result<(), CameraError> {
        Ok(())
    }
}

fn app(mock: Arc<MockBackend>) -> axum::Router {
    let mut map: HashMap<String, Arc<dyn CameraBackend>> = HashMap::new();
    map.insert("mock".into(), mock);
    let backends: BackendState = Arc::new(map);
    let token = Arc::new(RwLock::new(TOKEN.to_string()));
    let instance_id = Arc::new("test-instance".to_string());
    #[cfg(feature = "backend-remote")]
    let state = AppState::new(
        backends,
        token,
        instance_id,
        Arc::new(toucan_camera::backends::remote::PeerRegistry::new()),
    );
    #[cfg(not(feature = "backend-remote"))]
    let state = AppState::new(backends, token, instance_id);
    build_router(state)
}

fn opaque_id() -> String {
    DeviceId::new("mock", NATIVE_ID).encode()
}

async fn put(app: &axum::Router, action: &str) -> StatusCode {
    let req = Request::put(format!("/cameras/{}/{action}?token={TOKEN}", opaque_id()))
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Opens the MJPEG stream and returns its body as a stream of chunks.
async fn open_stream(
    app: &axum::Router,
) -> impl tokio_stream::Stream<Item = Result<Bytes, axum::Error>> + Unpin {
    let req = Request::get(format!("/cameras/{}/liveview?token={TOKEN}", opaque_id()))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.into_body().into_data_stream()
}

/// Reads chunks until one contains a real mock frame (`frame-N`).
async fn expect_live_frame(
    stream: &mut (impl tokio_stream::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
) {
    let found = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.expect("stream errored");
            if chunk.windows(6).any(|w| w == b"frame-") {
                return true;
            }
            assert!(
                !chunk.ends_with(b"--frame--\r\n"),
                "stream ended instead of delivering frames"
            );
        }
        false
    })
    .await
    .expect("no frame within 3 s");
    assert!(found, "stream closed without a frame");
}

/// Reads until the stream closes, asserting it ends with the closing boundary.
async fn expect_stream_end(
    stream: &mut (impl tokio_stream::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
) {
    let mut last = Bytes::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(chunk) = stream.next().await {
            last = chunk.expect("stream errored");
        }
    })
    .await
    .expect("stream did not end within 3 s");
    assert!(
        last.ends_with(b"--frame--\r\n"),
        "stream did not end cleanly"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_ends_stream_and_never_overlaps_a_poll() {
    let mock = Arc::new(MockBackend::default());
    let app = app(mock.clone());

    assert_eq!(put(&app, "connect").await, StatusCode::OK);
    let mut stream = open_stream(&app).await;
    expect_live_frame(&mut stream).await;

    // Disconnect while the client is still watching.
    assert_eq!(put(&app, "disconnect").await, StatusCode::OK);
    assert!(
        !mock.overlapped_disconnect.load(Ordering::SeqCst),
        "the session was closed while a frame poll was still running"
    );
    expect_stream_end(&mut stream).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fast_reconnect_while_watching_runs_a_single_loop() {
    let mock = Arc::new(MockBackend::default());
    let app = app(mock.clone());

    assert_eq!(put(&app, "connect").await, StatusCode::OK);
    // The old client keeps its stream open across the whole cycle.
    let mut old = open_stream(&app).await;
    expect_live_frame(&mut old).await;

    for _ in 0..5 {
        assert_eq!(put(&app, "disconnect").await, StatusCode::OK);
        assert_eq!(put(&app, "connect").await, StatusCode::OK);
    }
    let mut new = open_stream(&app).await;
    expect_live_frame(&mut new).await;
    // Let a few polls happen: an orphaned loop would poll alongside the new one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    expect_live_frame(&mut new).await;

    assert_eq!(
        mock.max_in_flight.load(Ordering::SeqCst),
        1,
        "several capture loops polled the same camera at once"
    );
    assert!(!mock.overlapped_disconnect.load(Ordering::SeqCst));
    // The stream that was open across the disconnect was ended, not kept alive.
    expect_stream_end(&mut old).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restarting_a_stream_gets_frames_not_end_of_stream() {
    let mock = Arc::new(MockBackend::default());
    let app = app(mock.clone());
    assert_eq!(put(&app, "connect").await, StatusCode::OK);

    for _ in 0..10 {
        let mut stream = open_stream(&app).await;
        expect_live_frame(&mut stream).await;
        drop(stream); // client stops the live view and immediately restarts it
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(mock.max_in_flight.load(Ordering::SeqCst), 1);
}
