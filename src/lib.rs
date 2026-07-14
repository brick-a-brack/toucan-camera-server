mod auth;
pub mod backends;
pub mod camera;
pub mod routes;
pub mod shutdown;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use axum::{extract::State, routing::{get, put}, Json, Router};
use axum::response::Html;
use tower_http::cors::CorsLayer;
use serde::Serialize;

use routes::cameras::{self, AppState, BackendState};

#[derive(Serialize)]
struct HealthCheck {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    instance_id: String,
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn health(State(state): State<AppState>) -> Json<HealthCheck> {
    Json(HealthCheck {
        status: "ok",
        service: "toucan-camera-server",
        version: env!("CARGO_PKG_VERSION"),
        instance_id: (*state.instance_id).clone(),
    })
}

/// Result of [`build_backends`]: the backend registry plus any shared state the
/// route layer needs to reference directly (currently the remote peer registry).
pub struct BuiltBackends {
    pub state: BackendState,
    #[cfg(feature = "backend-remote")]
    pub peers: Arc<backends::remote::PeerRegistry>,
}

pub fn build_backends() -> BuiltBackends {
    #[allow(unused_mut)]
    let mut map: HashMap<String, Arc<dyn camera::CameraBackend>> = HashMap::new();
    eprintln!("[main] build_backends() called");

    // The peer registry is shared between the remote backend (which reads it to
    // route and fan out requests) and the /peers routes (which mutate it).
    #[cfg(feature = "backend-remote")]
    let peers = Arc::new(backends::remote::PeerRegistry::new());

    #[cfg(feature = "backend-remote")]
    match backends::remote::RemoteBackend::new(peers.clone()) {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Remote backend failed to initialize: {e}"),
    }

    // Single-vendor SDK backends are wrapped in `LazyBackend`: the heavy SDK/DLL and
    // its OS thread are only created once a USB device of that vendor is detected, so
    // an unused brand costs nothing and can't interfere on the bus. (EDSDK and the
    // Nikon SDK coexist on macOS thanks to build.rs renaming the Nikon driver's
    // clashing ObjC PTP classes.)
    #[cfg(feature = "backend-canon")]
    {
        // Canon USB vendor id.
        let b: Arc<dyn camera::CameraBackend> = Arc::new(backends::lazy::LazyBackend::new(
            "canon",
            &[0x04A9],
            10,
            || Ok(Arc::new(backends::canon::CanonBackend::new()?)),
        ));
        map.insert(b.backend_id().to_string(), b);
    }

    #[cfg(all(feature = "backend-nikon-zs2", any(target_os = "macos", target_os = "windows")))]
    {
        // Nikon USB vendor id.
        let b: Arc<dyn camera::CameraBackend> = Arc::new(backends::lazy::LazyBackend::new(
            "nikon-zs2",
            &[0x04B0],
            10,
            || Ok(Arc::new(backends::nikon_zs2::NikonZs2Backend::new()?)),
        ));
        map.insert(b.backend_id().to_string(), b);
    }

    #[cfg(all(feature = "backend-gphoto2", any(target_os = "linux", target_os = "macos")))]
    match backends::gphoto2::GPhoto2Backend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] gphoto2 backend failed to initialize: {e}"),
    }

    #[cfg(all(feature = "backend-webcam-linux", target_os = "linux"))]
    match backends::webcam_linux::WebcamLinuxBackend::new() {
        Ok(b) => {
            let b: Arc<dyn camera::CameraBackend> = Arc::new(b);
            map.insert(b.backend_id().to_string(), b);
        }
        Err(e) => eprintln!("[error] Linux webcam backend failed to initialize: {e}"),
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

    // macOS: pre-warm each backend in the background so the first /cameras is fast
    // — triggers the Nikon SDK warm-up (when a body is present) up front instead of
    // on the first user request.
    #[cfg(target_os = "macos")]
    {
        let warm: Vec<Arc<dyn camera::CameraBackend>> = map.values().cloned().collect();
        std::thread::spawn(move || {
            for backend in warm {
                let _ = backend.list_devices();
            }
        });
    }

    BuiltBackends {
        state: Arc::new(map),
        #[cfg(feature = "backend-remote")]
        peers,
    }
}

pub struct Args {
    pub token: String,
    pub port:  Option<u16>,
    /// Bind on `0.0.0.0` (LAN) instead of loopback. Always on for Android.
    pub expose: bool,
}

pub fn parse_args() -> Args {
    let mut args = std::env::args().skip(1);
    let mut token = None::<String>;
    let mut port  = None::<u16>;
    // Android is LAN-exposed by design; --expose opts in on other platforms.
    let mut expose = cfg!(target_os = "android");

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--token"  => token = args.next(),
            "--port"   => port  = args.next().and_then(|v| v.parse().ok()),
            "--expose" => expose = true,
            _          => {}
        }
    }

    Args {
        token: token.unwrap_or_else(|| {
            #[allow(unreachable_code)]
            uuid::Uuid::new_v4().to_string()
        }),
        port,
        expose,
    }
}

pub fn resolve_port(explicit: Option<u16>) -> u16 {
    explicit.unwrap_or(8040)
}

/// Returns the bind address for the HTTP server.
///
/// Precedence: the `BIND_ADDR` environment variable always wins. Otherwise
/// Android binds `0.0.0.0` (LAN accessible) by design, and on every other
/// platform `expose` selects `0.0.0.0` (LAN) vs. `127.0.0.1` (loopback only,
/// the default).
pub fn resolve_bind_addr(expose: bool) -> IpAddr {
    use std::net::Ipv4Addr;

    if let Ok(addr) = std::env::var("BIND_ADDR") {
        if let Ok(ip) = addr.parse::<IpAddr>() {
            return ip;
        }
    }

    #[cfg(target_os = "android")]
    {
        let _ = expose; // Android is always LAN-exposed.
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    }
    #[cfg(not(target_os = "android"))]
    {
        if expose {
            IpAddr::V4(Ipv4Addr::UNSPECIFIED)
        } else {
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        }
    }
}

pub async fn bind_listener(port: u16, expose: bool) -> tokio::net::TcpListener {
    let host = resolve_bind_addr(expose);
    if let Ok(l) = tokio::net::TcpListener::bind((host, port)).await {
        return l;
    }
    eprintln!("[warn] port {port} is in use, letting the OS assign a free port");
    tokio::net::TcpListener::bind((host, 0))
        .await
        .expect("failed to bind to any port")
}

// On Android, the pairing token can be set (and updated) by the Kotlin side via
// the setToken() JNI call before or after startServer(). Other platforms derive
// the token from CLI args as before.
#[cfg(target_os = "android")]
static ANDROID_TOKEN: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

// Holds a shared reference to the live token arc so setToken() can update it
// while the server is running.
#[cfg(target_os = "android")]
static ACTIVE_TOKEN: std::sync::Mutex<Option<Arc<RwLock<String>>>> = std::sync::Mutex::new(None);

/// Builds the full axum router (routes + auth + CORS) from an [`AppState`].
/// Shared by [`run_server`] and the integration tests.
pub fn build_router(state: AppState) -> Router {
    #[allow(unused_mut)]
    let mut app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/cameras", get(cameras::list_cameras))
        .route("/cameras/{id}/connect", put(cameras::connect_camera))
        .route("/cameras/{id}/disconnect", put(cameras::disconnect_camera))
        .route("/cameras/{id}/parameters", get(cameras::get_parameters))
        .route("/cameras/{id}/parameters", put(cameras::set_parameter))
        .route("/cameras/{id}/liveview", get(cameras::live_view))
        .route("/cameras/{id}/capture", axum::routing::post(cameras::capture_photo));

    #[cfg(feature = "backend-remote")]
    {
        use routes::peers;
        app = app
            .route("/peers", get(peers::list_peers).post(peers::add_peer))
            .route("/peers/{id}", axum::routing::delete(peers::delete_peer));
    }

    app.with_state(state.clone())
        .layer(axum::middleware::from_fn_with_state(state, auth::auth_middleware))
        .layer(CorsLayer::permissive())
}

pub async fn run_server() {
    let args   = parse_args();
    let port   = resolve_port(args.port);
    let expose = args.expose;

    #[cfg(target_os = "android")]
    let initial_token = {
        let t = ANDROID_TOKEN.lock().unwrap();
        if t.is_empty() { args.token } else { t.clone() }
    };
    #[cfg(not(target_os = "android"))]
    let initial_token = args.token;

    let token = Arc::new(RwLock::new(initial_token));

    #[cfg(target_os = "android")]
    {
        *ACTIVE_TOKEN.lock().unwrap() = Some(token.clone());
    }

    let instance_id = Arc::new(uuid::Uuid::new_v4().to_string());

    let built = build_backends();

    // Register the backends for the process-wide shutdown path so their SDK
    // sessions are released on Ctrl-C / graceful stop instead of being left
    // claimed (which would keep the camera from re-enumerating on the next run).
    shutdown::set_backends(built.state.clone());
    // Baseline Ctrl-C handler for the non-Nikon case; the Nikon backend re-installs
    // it after its SDK init so ours stays on top of the SDK's swallowing handler.
    #[cfg(windows)]
    shutdown::install_console_handler();

    let state = AppState::new(
        built.state,
        token.clone(),
        instance_id.clone(),
        #[cfg(feature = "backend-remote")]
        built.peers.clone(),
    );

    let app = build_router(state);

    eprintln!("[info] binding on port {port}");
    let listener = bind_listener(port, expose).await;
    let addr = listener.local_addr().unwrap();
    eprintln!("[config] PORT={}", addr.port());
    eprintln!("[config] EXPOSE={expose}");
    eprintln!("[config] TOKEN={}", token.read().unwrap());
    eprintln!("[info] Listening on http://{}/?token={}", addr, token.read().unwrap());

    // Windows drives shutdown through the console control handler (which exits the
    // process directly — see `shutdown::install_console_handler`), so serve plainly.
    #[cfg(windows)]
    axum::serve(listener, app).await.unwrap();

    // Elsewhere, stop serving on Ctrl-C and release the backends before returning.
    #[cfg(not(windows))]
    {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .unwrap();
        shutdown::run();
    }
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
    use std::sync::Mutex;
    use tokio::sync::watch;

    static SHUTDOWN: Mutex<Option<watch::Sender<bool>>> = Mutex::new(None);

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
    pub extern "C" fn Java_com_brickfilms_toucancameraserver_CameraServerService_startServer(
        _env: *mut std::ffi::c_void,
        _this: *mut std::ffi::c_void,
    ) {
        alog("startServer() called");

        std::panic::set_hook(Box::new(|info| {
            let msg = format!("PANIC: {info}");
            alog_err(&msg);
        }));

        let (tx, mut rx) = watch::channel(false);
        *SHUTDOWN.lock().unwrap() = Some(tx);

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
    pub extern "C" fn Java_com_brickfilms_toucancameraserver_CameraServerService_stopServer(
        _env: *mut std::ffi::c_void,
        _this: *mut std::ffi::c_void,
    ) {
        if let Some(tx) = SHUTDOWN.lock().unwrap().as_ref() {
            let _ = tx.send(true);
        }
    }

    /// Called from Kotlin: CameraServerService.setToken(token)
    ///
    /// Updates the pairing token used by the HTTP auth middleware. Safe to call
    /// before or after startServer() — changes take effect immediately if the
    /// server is already running.
    #[no_mangle]
    pub unsafe extern "C" fn Java_com_brickfilms_toucancameraserver_CameraServerService_setToken(
        env: *mut *const *mut std::ffi::c_void,
        _this: *mut std::ffi::c_void,
        j_token: *mut std::ffi::c_void,
    ) {
        // Extract UTF-8 string from the Java jstring via the JNI vtable.
        // GetStringUTFChars is at vtable index 169, ReleaseStringUTFChars at 170.
        // These offsets are mandated by the JNI spec and are stable across all JVMs.
        let token_str = {
            if j_token.is_null() { return; }
            let vtable: *const *mut std::ffi::c_void = *env;
            type GetChars = unsafe extern "C" fn(
                *mut *const *mut std::ffi::c_void,
                *mut std::ffi::c_void,
                *mut u8,
            ) -> *const std::os::raw::c_char;
            type ReleaseChars = unsafe extern "C" fn(
                *mut *const *mut std::ffi::c_void,
                *mut std::ffi::c_void,
                *const std::os::raw::c_char,
            );
            let get_chars: GetChars = std::mem::transmute(*vtable.add(169));
            let release_chars: ReleaseChars = std::mem::transmute(*vtable.add(170));

            let chars = get_chars(env, j_token, std::ptr::null_mut());
            if chars.is_null() { return; }
            let s = std::ffi::CStr::from_ptr(chars).to_string_lossy().into_owned();
            release_chars(env, j_token, chars);
            s
        };

        // Update the pending token (read by startServer if called after this).
        *super::ANDROID_TOKEN.lock().unwrap() = token_str.clone();

        // Update the live token arc (effective immediately on the running server).
        if let Some(arc) = super::ACTIVE_TOKEN.lock().unwrap().as_ref() {
            *arc.write().unwrap() = token_str.clone();
        }

        alog(&format!("token set to: {token_str}"));
    }
}
