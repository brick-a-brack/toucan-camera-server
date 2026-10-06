mod auth;
pub mod backends;
pub mod camera;
pub mod routes;
pub mod shutdown;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
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

    #[cfg(all(feature = "backend-sony", any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        // Sony USB vendor id.
        let b: Arc<dyn camera::CameraBackend> = Arc::new(backends::lazy::LazyBackend::new(
            "sony",
            &[0x054C],
            10,
            || Ok(Arc::new(backends::sony::SonyBackend::new()?)),
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
    // Android defaults to LAN (the phone is driven from another device); --expose
    // opts in on other platforms. Either way the caller can override it.
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
/// Precedence: the `BIND_ADDR` environment variable always wins; otherwise
/// `expose` selects `0.0.0.0` (reachable from the LAN) vs. `127.0.0.1` (loopback
/// only). This holds on every platform, Android included — the caller decides
/// (`--expose` on the CLI, the `expose` argument of the `startServer` JNI call).
/// Only the *default* differs: [`parse_args`] defaults it on for Android.
pub fn resolve_bind_addr(expose: bool) -> IpAddr {
    if let Ok(addr) = std::env::var("BIND_ADDR") {
        if let Ok(ip) = addr.parse::<IpAddr>() {
            return ip;
        }
    }
    bind_addr_for(expose)
}

/// The `expose` → address mapping, without the `BIND_ADDR` override. Split out so
/// it is testable without touching the process environment (which would race the
/// other tests).
fn bind_addr_for(expose: bool) -> IpAddr {
    use std::net::Ipv4Addr;

    if expose {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn expose_binds_every_interface() {
        assert_eq!(bind_addr_for(true), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
    }

    #[test]
    fn without_expose_binds_loopback_only() {
        assert_eq!(bind_addr_for(false), IpAddr::V4(Ipv4Addr::LOCALHOST));
    }

    #[test]
    fn port_defaults_to_8040() {
        assert_eq!(resolve_port(None), 8040);
        assert_eq!(resolve_port(Some(9001)), 9001);
    }
}

// ---------------------------------------------------------------------------
// Server core
// ---------------------------------------------------------------------------

/// Inputs for [`bind_server`] — what the CLI and the Android JNI entry point
/// both have to supply.
pub struct ServerConfig {
    pub port: u16,
    /// Bind `0.0.0.0` (reachable from the LAN) instead of loopback. Honoured on
    /// every platform, Android included — see [`resolve_bind_addr`].
    pub expose: bool,
    /// Shared with the auth middleware, so the token can be rotated while the
    /// server runs (the Android app does this from `setToken`).
    pub token: Arc<RwLock<String>>,
}

/// A server whose socket is bound but which is not serving yet.
///
/// Binding is split from serving so the caller learns the **real** address
/// before the first request is accepted: the requested port may be taken, in
/// which case the OS picks a free one. The Android UI displays that port, so it
/// cannot be assumed.
pub struct BoundServer {
    pub addr: SocketAddr,
    pub instance_id: Arc<String>,
    /// The registry behind the running server, kept for status reporting.
    pub backends: BackendState,
    listener: tokio::net::TcpListener,
    app: Router,
}

/// Builds the backends, the router and the listener.
///
/// Also registers the backends with [`shutdown`], so a later teardown releases
/// every SDK session. Fails only when no port at all could be bound.
pub async fn bind_server(config: ServerConfig) -> std::io::Result<BoundServer> {
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

    let backends = built.state.clone();
    let state = AppState::new(
        built.state,
        config.token,
        instance_id.clone(),
        #[cfg(feature = "backend-remote")]
        built.peers.clone(),
    );
    let app = build_router(state);

    let host = resolve_bind_addr(config.expose);
    let listener = match tokio::net::TcpListener::bind((host, config.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[warn] port {} unavailable ({e}), letting the OS assign a free port",
                config.port,
            );
            tokio::net::TcpListener::bind((host, 0)).await?
        }
    };
    let addr = listener.local_addr()?;

    Ok(BoundServer { addr, instance_id, backends, listener, app })
}

impl BoundServer {
    /// Serves until the returned future is dropped (or the accept loop errors).
    pub async fn serve(self) -> std::io::Result<()> {
        axum::serve(self.listener, self.app).await
    }

    /// Serves until `signal` resolves, then drains the in-flight connections.
    ///
    /// Beware: the live view is an endless MJPEG response, so the drain only
    /// ends once every streaming client is gone — bound the wait (see
    /// [`shutdown_signal`]) or cancel this future instead.
    pub async fn serve_with_shutdown<F>(self, signal: F) -> std::io::Result<()>
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        axum::serve(self.listener, self.app)
            .with_graceful_shutdown(signal)
            .await
    }
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
        let t = android_jni::lock(&ANDROID_TOKEN);
        if t.is_empty() { args.token } else { t.clone() }
    };
    #[cfg(not(target_os = "android"))]
    let initial_token = args.token;

    let token = Arc::new(RwLock::new(initial_token));

    #[cfg(target_os = "android")]
    {
        *android_jni::lock(&ACTIVE_TOKEN) = Some(token.clone());
    }

    eprintln!("[info] binding on port {port}");
    let bound = match bind_server(ServerConfig { port, expose, token: token.clone() }).await {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[error] failed to bind on port {port}: {e}");
            return;
        }
    };

    let addr = bound.addr;
    eprintln!("[config] PORT={}", addr.port());
    eprintln!("[config] EXPOSE={expose}");
    eprintln!("[config] TOKEN={}", read_token(&token));
    eprintln!("[info] Listening on http://{}/?token={}", addr, read_token(&token));

    // Windows drives shutdown through the console control handler (which exits the
    // process directly — see `shutdown::install_console_handler`), so serve plainly.
    #[cfg(windows)]
    if let Err(e) = bound.serve().await {
        eprintln!("[error] server stopped: {e}");
    }

    // Elsewhere, stop serving on Ctrl-C and release the backends before returning.
    #[cfg(not(windows))]
    {
        if let Err(e) = bound.serve_with_shutdown(shutdown_signal()).await {
            eprintln!("[error] server stopped: {e}");
        }
        shutdown::run();
    }
}

/// Reads the shared token, ignoring a poisoned lock (a panic in another holder
/// must not stop us from reporting the token).
fn read_token(token: &Arc<RwLock<String>>) -> String {
    match token.read() {
        Ok(t) => t.clone(),
        Err(e) => e.into_inner().clone(),
    }
}

/// Resolves on Ctrl-C, and guarantees the process actually leaves.
///
/// A graceful shutdown waits for the in-flight connections to finish, and the live
/// view is an endless MJPEG response — a single open stream (a browser tab left on
/// the UI) never completes, so the wait never ends. Worse, tokio keeps its SIGINT
/// handler installed, so the follow-up Ctrl-C the user reflexively hits is
/// swallowed too and the server looks unkillable.
///
/// So bound the wait: release the backends and exit on whichever comes first, a
/// second Ctrl-C or a short grace period. `shutdown::run` is idempotent, so the
/// normal path calling it again after `serve` returns is harmless.
#[cfg(not(windows))]
async fn shutdown_signal() {
    const GRACE: std::time::Duration = std::time::Duration::from_secs(3);

    let _ = tokio::signal::ctrl_c().await;
    eprintln!("[shutdown] Ctrl-C received — closing connections");

    tokio::spawn(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => eprintln!("[shutdown] second Ctrl-C — exiting now"),
            _ = tokio::time::sleep(GRACE) => {
                eprintln!("[shutdown] connections still open after {}s — exiting", GRACE.as_secs());
            }
        }
        shutdown::run();
        // 130 is the conventional exit code for a Ctrl-C death.
        std::process::exit(130);
    });
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
    //! JNI surface consumed by `CameraServerService.kt`.
    //!
    //! Contract, and the reason this is no longer a fire-and-forget `startServer()`:
    //!
    //! * **`startServer` is synchronous up to the bind** and returns the port the
    //!   server actually listens on, or a negative `ERR_*` code. The requested
    //!   port may be taken (`bind_server` then lets the OS pick one), so the
    //!   service has to be told rather than assume 8040 — and a failure must be
    //!   visible instead of showing a "running" server that never bound.
    //! * **`stopServer` really stops**: it cancels the accept loop, releases every
    //!   backend (camera sessions included) and waits for the server thread, so a
    //!   stop then start cycle works inside one process.
    //! * **`isServerRunning` / `serverStatusJson`** report the live state, so the
    //!   UI never keeps its own optimistic copy.

    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, MutexGuard, Once, RwLock};
    use std::time::{Duration, Instant};

    use jni::objects::{JClass, JString};
    use jni::sys::{jboolean, jint, jstring, JNI_FALSE, JNI_TRUE};
    use jni::JNIEnv;
    use serde::Serialize;
    use tokio::sync::watch;

    use crate::routes::cameras::BackendState;

    /// Negative results of `startServer`; any value >= 0 is the bound port.
    pub const ERR_RUNTIME: jint = -1;
    pub const ERR_BIND:    jint = -2;
    pub const ERR_PANIC:   jint = -3;

    /// How long `startServer` waits for the bind to be confirmed. Generous: it
    /// covers `build_backends()` (which creates the Camera2 backend) as well.
    const START_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long `stopServer` waits for the server thread to confirm it is done.
    const STOP_TIMEOUT: Duration = Duration::from_secs(5);
    /// Grace period for tokio tasks still running after the accept loop is cut.
    const RUNTIME_GRACE: Duration = Duration::from_secs(2);

    /// The live server, present exactly while one is running.
    struct Running {
        /// Cuts the accept loop.
        shutdown: watch::Sender<bool>,
        /// Whether the socket is bound to every interface (see `expose` in Status).
        expose: bool,
        /// Signalled by the server thread just before it exits.
        finished: mpsc::Receiver<()>,
        /// The port the socket is actually bound to.
        port: u16,
        /// The port that was *asked for*. Compared against a later start to tell
        /// a configuration change from a repeat call: when the requested port was
        /// taken, `port` is an OS-assigned one and would never match it again —
        /// comparing that instead would rebind on every single call.
        requested_port: u16,
        bind_address: String,
        token: Arc<RwLock<String>>,
        instance_id: Arc<String>,
        backends: BackendState,
        started: Instant,
    }

    static RUNNING:    Mutex<Option<Running>> = Mutex::new(None);
    static LAST_ERROR: Mutex<Option<String>>  = Mutex::new(None);
    static PANIC_HOOK: Once = Once::new();

    /// Locks a global, recovering from poisoning: a panic in one JNI call must not
    /// wedge every later one — there is no process restart to fall back on, the
    /// user would have to force-stop the app.
    pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn read_token(token: &Arc<RwLock<String>>) -> String {
        match token.read() {
            Ok(t) => t.clone(),
            Err(e) => e.into_inner().clone(),
        }
    }

    fn write_token(token: &Arc<RwLock<String>>, value: String) {
        match token.write() {
            Ok(mut t) => *t = value,
            Err(e) => *e.into_inner() = value,
        }
    }

    // -----------------------------------------------------------------------
    // logcat
    // -----------------------------------------------------------------------

    extern "C" {
        fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
    }

    fn write_log(prio: i32, msg: &str) {
        let tag = b"ToucanServer\0";
        let mut buf = msg.to_string();
        buf.push('\0');
        unsafe { __android_log_write(prio, tag.as_ptr(), buf.as_ptr()); }
    }

    fn alog(msg: &str) { write_log(5 /* INFO */, msg); }
    fn alog_err(msg: &str) { write_log(6 /* ERROR */, msg); }

    fn install_panic_hook() {
        PANIC_HOOK.call_once(|| {
            std::panic::set_hook(Box::new(|info| alog_err(&format!("PANIC: {info}"))));
        });
    }

    fn set_last_error(msg: impl Into<String>) {
        let msg = msg.into();
        alog_err(&msg);
        *lock(&LAST_ERROR) = Some(msg);
    }

    // -----------------------------------------------------------------------
    // Status
    // -----------------------------------------------------------------------

    /// Serialized to JSON by `serverStatusJson()` and parsed on the Kotlin side.
    /// Deliberately cheap — everything here is already in memory, so the UI may
    /// poll it. Device information stays on the HTTP API (`GET /cameras`), which
    /// actually enumerates the hardware.
    #[derive(Serialize)]
    struct Status {
        running: bool,
        /// The port in use, or 0 when stopped.
        port: u16,
        /// The address the socket is bound to: `0.0.0.0` when exposed to the
        /// network, `127.0.0.1` when this device only.
        bind_address: String,
        /// Whether other devices on the network can reach the server. Mirrors
        /// [`bind_address`], but explicit so the UI does not have to parse it.
        expose: bool,
        /// The token the auth middleware currently accepts — or, when stopped,
        /// the one a later `startServer()` would use.
        token: String,
        version: &'static str,
        /// Identifies this server run; also returned by `GET /health`.
        instance_id: String,
        uptime_seconds: u64,
        /// Backends registered for this run, e.g. `["camera2-android", "remote"]`.
        backends: Vec<String>,
        /// Why the last `startServer()` failed, if it did. Cleared on success.
        last_error: Option<String>,
    }

    fn status() -> Status {
        let running = lock(&RUNNING);
        let last_error = lock(&LAST_ERROR).clone();
        let version = env!("CARGO_PKG_VERSION");

        match running.as_ref() {
            Some(r) => {
                let mut backends: Vec<String> = r.backends.keys().cloned().collect();
                backends.sort();
                Status {
                    running: true,
                    port: r.port,
                    bind_address: r.bind_address.clone(),
                    expose: r.expose,
                    token: read_token(&r.token),
                    version,
                    instance_id: (*r.instance_id).clone(),
                    uptime_seconds: r.started.elapsed().as_secs(),
                    backends,
                    last_error,
                }
            }
            None => Status {
                running: false,
                port: 0,
                bind_address: String::new(),
                expose: false,
                token: lock(&super::ANDROID_TOKEN).clone(),
                version,
                instance_id: String::new(),
                uptime_seconds: 0,
                backends: Vec::new(),
                last_error,
            },
        }
    }

    // -----------------------------------------------------------------------
    // Start
    // -----------------------------------------------------------------------

    /// What the server thread reports back once it has tried to bind.
    struct Bound {
        port: u16,
        bind_address: String,
        instance_id: Arc<String>,
        backends: BackendState,
    }

    struct StartError {
        code: jint,
        message: String,
    }

    /// `CameraServerService.startServer(port, token, expose): Int`
    ///
    /// Returns the port the server listens on — which may differ from `port` if
    /// that one was taken — or a negative `ERR_*` code. Pass `port <= 0` for the
    /// default (8040), and an empty `token` to keep the one set by `setToken()`.
    ///
    /// `expose` picks the bind address: `true` → `0.0.0.0`, reachable from other
    /// devices on the network; `false` → `127.0.0.1`, this device only (apps on
    /// the phone itself). `BIND_ADDR` still overrides both.
    ///
    /// Calling it again on a running server:
    /// * same `port` and `expose` → nothing is touched, the live port is returned
    ///   (so a service redelivery or a notification refresh is harmless);
    /// * a different `token` → applied in place, no interruption;
    /// * a different `port` or `expose` → the server is **stopped and rebound**,
    ///   because those are fixed when the socket is opened. Every connection is
    ///   dropped and the camera sessions are released.
    ///
    /// Blocks until the socket is bound, so call it off the main thread.
    #[no_mangle]
    pub extern "system" fn Java_com_brickfilms_toucancameraserver_CameraServerService_startServer<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        port: jint,
        token: JString<'local>,
        expose: jboolean,
    ) -> jint {
        install_panic_hook();

        let token: Option<String> = if token.is_null() {
            None
        } else {
            env.get_string(&token).ok().map(Into::into)
        };
        let expose = expose != JNI_FALSE;

        // A panic must not unwind across the FFI boundary: that aborts the
        // process, killing the whole app instead of just failing the toggle.
        std::panic::catch_unwind(|| start(port, token, expose)).unwrap_or_else(|_| {
            set_last_error("startServer panicked");
            ERR_PANIC
        })
    }

    fn start(port: jint, token: Option<String>, expose: bool) -> jint {
        let mut slot = lock(&RUNNING);

        let requested_port = if port <= 0 || port > u16::MAX as jint {
            super::resolve_port(None)
        } else {
            port as u16
        };

        if let Some(running) = slot.as_ref() {
            // The token needs no rebind — the auth middleware reads it through a
            // lock — so apply it in place whether or not we restart below.
            if let Some(new_token) = token.as_ref().filter(|t| !t.is_empty()) {
                if **new_token != read_token(&running.token) {
                    write_token(&running.token, new_token.clone());
                    *lock(&super::ANDROID_TOKEN) = new_token.clone();
                    alog("startServer(): applied a new pairing token to the running server");
                }
            }

            // The bind address and the port, on the other hand, are fixed when the
            // socket is opened: honouring a change means closing it and opening a
            // new one. Do it rather than returning success while still listening on
            // the old address — but only on an actual change, so a repeat call (a
            // service redelivery, a notification refresh) stays idempotent.
            let rebind = expose != running.expose || requested_port != running.requested_port;
            if !rebind {
                alog(&format!("startServer(): already running on port {}", running.port));
                return running.port as jint;
            }

            alog(&format!(
                "startServer(): configuration changed (expose {} -> {}, port {} -> {}) — rebinding",
                running.expose, expose, running.requested_port, requested_port,
            ));
            // Drops every connection and releases the camera sessions: a client
            // streaming live view is cut off, which is inherent to rebinding.
            if let Some(running) = slot.take() {
                stop_running(running);
            }
        }

        // Token precedence: what Kotlin just passed, else what setToken() left
        // pending, else a random one — never an empty (open) token.
        let initial = match token.filter(|t| !t.is_empty()) {
            Some(t) => t,
            None => {
                let pending = lock(&super::ANDROID_TOKEN).clone();
                if pending.is_empty() { uuid::Uuid::new_v4().to_string() } else { pending }
            }
        };
        *lock(&super::ANDROID_TOKEN) = initial.clone();
        let token = Arc::new(RwLock::new(initial));
        *lock(&super::ACTIVE_TOKEN) = Some(token.clone());

        let port = requested_port;

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let (bound_tx, bound_rx) = mpsc::channel::<Result<Bound, StartError>>();
        let (finished_tx, finished_rx) = mpsc::channel::<()>();

        let thread_token = token.clone();
        let spawned = std::thread::Builder::new()
            .name("toucan-server".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = bound_tx.send(Err(StartError {
                            code: ERR_RUNTIME,
                            message: format!("tokio runtime: {e}"),
                        }));
                        return;
                    }
                };

                let bound = rt.block_on(super::bind_server(super::ServerConfig {
                    port,
                    expose,
                    token: thread_token,
                }));
                let bound = match bound {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = bound_tx.send(Err(StartError {
                            code: ERR_BIND,
                            message: format!("bind: {e}"),
                        }));
                        return;
                    }
                };

                let addr = bound.addr;
                let report = Bound {
                    port: addr.port(),
                    bind_address: addr.ip().to_string(),
                    instance_id: bound.instance_id.clone(),
                    backends: bound.backends.clone(),
                };
                if bound_tx.send(Ok(report)).is_err() {
                    // The caller timed out and gave up; do not serve orphaned.
                    crate::shutdown::run();
                    return;
                }
                alog(&format!("serving on http://{addr}"));

                // Cancel the accept loop rather than draining it: the live view is
                // an endless MJPEG response, so a graceful drain would not finish
                // while a client streams and the toggle would look stuck.
                rt.block_on(async {
                    tokio::select! {
                        r = bound.serve() => alog(&format!("accept loop ended: {r:?}")),
                        _ = shutdown_rx.changed() => alog("stop requested"),
                    }
                });

                // Release the camera sessions before the runtime goes away, or the
                // device stays claimed and the next start finds no camera.
                crate::shutdown::run();
                rt.shutdown_timeout(RUNTIME_GRACE);
                let _ = finished_tx.send(());
                alog("server thread exiting");
            });

        if let Err(e) = spawned {
            set_last_error(format!("failed to spawn the server thread: {e}"));
            *lock(&super::ACTIVE_TOKEN) = None;
            return ERR_RUNTIME;
        }

        match bound_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(bound)) => {
                let port = bound.port;
                *slot = Some(Running {
                    shutdown: shutdown_tx,
                    finished: finished_rx,
                    expose,
                    port,
                    requested_port,
                    bind_address: bound.bind_address,
                    token,
                    instance_id: bound.instance_id,
                    backends: bound.backends,
                    started: Instant::now(),
                });
                *lock(&LAST_ERROR) = None;
                alog(&format!("startServer() -> listening on port {port}"));
                port as jint
            }
            Ok(Err(e)) => {
                set_last_error(e.message);
                *lock(&super::ACTIVE_TOKEN) = None;
                e.code
            }
            // Distinguish the two ways `recv_timeout` can fail: they have
            // completely different causes, and reporting a dead thread as a
            // timeout sent us looking in the wrong place once already.
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                set_last_error(
                    "the server thread died before it could bind - look for a PANIC line above"
                        .to_string(),
                );
                *lock(&super::ACTIVE_TOKEN) = None;
                ERR_RUNTIME
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Still alive and may yet bind; cut it loose.
                let _ = shutdown_tx.send(true);
                set_last_error(format!(
                    "the server did not start within {}s",
                    START_TIMEOUT.as_secs(),
                ));
                *lock(&super::ACTIVE_TOKEN) = None;
                ERR_RUNTIME
            }
        }
    }

    // -----------------------------------------------------------------------
    // Stop
    // -----------------------------------------------------------------------

    /// `CameraServerService.stopServer()`
    ///
    /// Cuts the accept loop, releases every backend and waits (bounded) for the
    /// server thread to exit, so the next `startServer()` finds the camera free.
    #[no_mangle]
    pub extern "system" fn Java_com_brickfilms_toucancameraserver_CameraServerService_stopServer<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) {
        if std::panic::catch_unwind(stop).is_err() {
            set_last_error("stopServer panicked");
        }
    }

    fn stop() {
        // Taken out first, so `isServerRunning()` is false for the whole teardown.
        let running = lock(&RUNNING).take();
        let Some(running) = running else {
            alog("stopServer(): no server running");
            return;
        };
        stop_running(running);
    }

    /// Tears down a server already removed from [`RUNNING`]. Shared with the
    /// rebinding path in [`start`], which holds the lock and so cannot call
    /// [`stop`]. Blocks until the server thread confirms, bounded.
    fn stop_running(running: Running) {
        let port = running.port;

        let _ = running.shutdown.send(true);
        match running.finished.recv_timeout(STOP_TIMEOUT) {
            Ok(()) => alog(&format!("server on port {port} released")),
            Err(e) => alog_err(&format!(
                "server on port {port} not confirmed within {}s ({e}) - the next start may have to fall back to another port",
                STOP_TIMEOUT.as_secs(),
            )),
        }
        *lock(&super::ACTIVE_TOKEN) = None;
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// `CameraServerService.isServerRunning(): Boolean` — the real state, not a
    /// flag the caller has to maintain.
    #[no_mangle]
    pub extern "system" fn Java_com_brickfilms_toucancameraserver_CameraServerService_isServerRunning<'local>(
        _env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) -> jboolean {
        if lock(&RUNNING).is_some() { JNI_TRUE } else { JNI_FALSE }
    }

    /// `CameraServerService.serverStatusJson(): String` — see [`Status`].
    /// Returns `"{}"` rather than throwing if the status cannot be built.
    #[no_mangle]
    pub extern "system" fn Java_com_brickfilms_toucancameraserver_CameraServerService_serverStatusJson<'local>(
        env: JNIEnv<'local>,
        _class: JClass<'local>,
    ) -> jstring {
        let json = std::panic::catch_unwind(|| {
            serde_json::to_string(&status()).unwrap_or_else(|_| "{}".to_string())
        })
        .unwrap_or_else(|_| "{}".to_string());

        match env.new_string(json) {
            Ok(s) => s.into_raw(),
            Err(e) => {
                alog_err(&format!("serverStatusJson: {e}"));
                std::ptr::null_mut()
            }
        }
    }

    /// `CameraServerService.setToken(token)`
    ///
    /// Updates the pairing token used by the HTTP auth middleware. Safe before or
    /// after `startServer()`; a running server picks it up immediately.
    #[no_mangle]
    pub extern "system" fn Java_com_brickfilms_toucancameraserver_CameraServerService_setToken<'local>(
        mut env: JNIEnv<'local>,
        _class: JClass<'local>,
        token: JString<'local>,
    ) {
        if token.is_null() {
            return;
        }
        let Ok(token) = env.get_string(&token) else {
            alog_err("setToken: could not read the token string");
            return;
        };
        let token: String = token.into();

        // Never accept an empty token: `auth_middleware` compares the presented
        // value to this one, so an empty one lets `?token=` through — the server
        // would be effectively open on the LAN. `start()` guards the same way.
        if token.is_empty() {
            alog_err("setToken: refusing an empty token (the current one is kept)");
            return;
        }

        // Pending value, read by a later startServer().
        *lock(&super::ANDROID_TOKEN) = token.clone();
        // Live value, effective at once on a running server.
        if let Some(arc) = lock(&super::ACTIVE_TOKEN).as_ref() {
            write_token(arc, token);
        }
        // The token is a credential: log that it changed, never its value.
        alog("pairing token updated");
    }
}
