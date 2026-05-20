mod auth;
pub mod backends;
pub mod camera;
pub mod routes;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use axum::{routing::{get, put}, Json, Router};
use axum::response::Html;
use tower_http::cors::CorsLayer;
use serde::Serialize;

use routes::cameras::{self, AppState, BackendState};

#[derive(Serialize)]
struct HealthCheck {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn health() -> Json<HealthCheck> {
    Json(HealthCheck {
        status: "ok",
        service: "toucan-camera-server",
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub fn build_backends() -> BackendState {
    #[allow(unused_mut)]
    let mut map: HashMap<String, Arc<dyn camera::CameraBackend>> = HashMap::new();
    eprintln!("[main] build_backends() called");

    #[cfg(feature = "backend-canon")]
    match backends::canon::CanonBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Canon backend failed to initialize: {e}"),
    }

    #[cfg(all(feature = "backend-webcam-macos", target_os = "macos"))]
    match backends::webcam_macos::WebcamMacosBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] macOS webcam backend failed to initialize: {e}"),
    }

    eprintln!("[main] webcam-windows feature={} target_windows={}", cfg!(feature = "backend-webcam-windows"), cfg!(target_os = "windows"));
    #[cfg(all(feature = "backend-webcam-windows", target_os = "windows"))]
    match backends::webcam_windows::WebcamWindowsBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Windows webcam backend failed to initialize: {e}"),
    }

    #[cfg(all(feature = "backend-camera2-android", target_os = "android"))]
    match backends::camera2_android::Camera2AndroidBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Android Camera2 backend failed to initialize: {e}"),
    }

    eprintln!("[main] registered backends: {:?}", map.keys().collect::<Vec<_>>());
    Arc::new(map)
}

pub struct Args {
    pub token: String,
    pub port:  Option<u16>,
}

pub fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut token = None::<String>;
    let mut port  = None::<u16>;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--token" => token = args.next(),
            "--port"  => port  = args.next().and_then(|v| v.parse().ok()),
            _         => {}
        }
    }

    Args {
        token: token.unwrap_or_else(|| {
            #[cfg(target_os = "android")]
            { return "token".to_string(); }
            #[allow(unreachable_code)]
            uuid::Uuid::new_v4().to_string()
        }),
        port,
    }
}

pub fn resolve_port(explicit: Option<u16>) -> u16 {
    explicit.unwrap_or(8040)
}

/// Returns the bind address for the HTTP server.
/// On Android defaults to 0.0.0.0 (LAN accessible) so other devices on the
/// same network can reach the API. On all other platforms defaults to 127.0.0.1
/// (loopback only). The BIND_ADDR environment variable always takes precedence.
pub fn resolve_bind_addr() -> IpAddr {
    if let Ok(addr) = std::env::var("BIND_ADDR") {
        if let Ok(ip) = addr.parse::<IpAddr>() {
            return ip;
        }
    }
    #[cfg(target_os = "android")]
    { return IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED); }
    #[allow(unreachable_code)]
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
}

pub async fn bind_listener(port: u16) -> tokio::net::TcpListener {
    let host = resolve_bind_addr();
    if let Ok(l) = tokio::net::TcpListener::bind((host, port)).await {
        return l;
    }
    eprintln!("[warn] port {port} is in use, letting the OS assign a free port");
    tokio::net::TcpListener::bind((host, 0))
        .await
        .expect("failed to bind to any port")
}

pub async fn run_server() {
    let args  = parse_args();
    let port  = resolve_port(args.port);
    let token = args.token;

    let backends = build_backends();
    let state = AppState::new(backends, token.clone());

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/cameras", get(cameras::list_cameras))
        .route("/cameras/{id}/connect", put(cameras::connect_camera))
        .route("/cameras/{id}/disconnect", put(cameras::disconnect_camera))
        .route("/cameras/{id}/parameters", get(cameras::get_parameters))
        .route("/cameras/{id}/parameters", put(cameras::set_parameter))
        .route("/cameras/{id}/liveview", get(cameras::live_view))
        .route("/cameras/{id}/capture", axum::routing::post(cameras::capture_photo))
        .with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::auth_middleware))
        .layer(CorsLayer::permissive());

    eprintln!("[info] binding on port {port}");
    let listener = bind_listener(port).await;
    let addr = listener.local_addr().unwrap();
    eprintln!("[config] PORT={}", addr.port());
    eprintln!("[config] TOKEN={}", token);
    eprintln!("[info] Listening on http://{}/?token={}", addr, token);
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// Android JNI entry points
// ---------------------------------------------------------------------------
//
// The Rust code is compiled as a cdylib loaded by CameraServerService.kt via
// System.loadLibrary("toucan_camera_server"). The service calls startServer()
// once on creation and stopServer() on destruction.

#[cfg(target_os = "android")]
pub mod android_jni {
    use std::sync::OnceLock;
    use tokio::sync::watch;

    static SHUTDOWN: OnceLock<watch::Sender<bool>> = OnceLock::new();

    extern "C" {
        fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
    }

    fn alog(msg: &str) {
        let tag = b"ToucanServer\0";
        let mut buf = msg.to_string();
        buf.push('\0');
        unsafe { __android_log_write(5 /* INFO */, tag.as_ptr(), buf.as_ptr() as *const u8); }
    }

    fn alog_err(msg: &str) {
        let tag = b"ToucanServer\0";
        let mut buf = msg.to_string();
        buf.push('\0');
        unsafe { __android_log_write(6 /* ERROR */, tag.as_ptr(), buf.as_ptr() as *const u8); }
    }

    /// Called from Kotlin: CameraServerService.startServer()
    #[no_mangle]
    pub extern "C" fn Java_com_example_birdcamera_CameraServerService_startServer(
        _env: *mut std::ffi::c_void,
        _this: *mut std::ffi::c_void,
    ) {
        alog("startServer() called");

        std::panic::set_hook(Box::new(|info| {
            let msg = format!("PANIC: {info}");
            alog_err(&msg);
        }));

        let (tx, mut rx) = watch::channel(false);
        SHUTDOWN.get_or_init(|| tx);

        alog("spawning server thread");
        std::thread::Builder::new()
            .name("toucan-server".to_string())
            .spawn(move || {
                alog("server thread started, building tokio runtime");
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(r) => r,
                    Err(e) => {
                        alog_err(&format!("failed to build tokio runtime: {e}"));
                        return;
                    }
                };
                alog("tokio runtime built, starting run_server()");
                rt.block_on(async {
                    tokio::select! {
                        _ = super::run_server() => {
                            alog("run_server() returned");
                        },
                        _ = rx.changed() => {
                            alog("server shutdown requested");
                        }
                    }
                });
                alog("server thread exiting");
            })
            .expect("failed to spawn server thread");
        alog("startServer() done");
    }

    /// Called from Kotlin: CameraServerService.stopServer()
    #[no_mangle]
    pub extern "C" fn Java_com_example_birdcamera_CameraServerService_stopServer(
        _env: *mut std::ffi::c_void,
        _this: *mut std::ffi::c_void,
    ) {
        if let Some(tx) = SHUTDOWN.get() {
            let _ = tx.send(true);
        }
    }
}
