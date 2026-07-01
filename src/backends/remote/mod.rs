//! Remote backend — relays cameras exposed by other toucan-camera-server
//! instances over HTTP.
//!
//! ## Identity scheme
//! This backend never sees opaque IDs of *its own* server. Like every other
//! backend it works with native IDs. A remote native ID encodes the peer and
//! the remote (peer-side) opaque device ID as `"<peer_url>|<remote_opaque_id>"`.
//! The route layer wraps that into this server's opaque ID via
//! `DeviceId::new("remote", native).encode()`.
//!
//! `<peer_url>` is a normalized base URL (e.g. `http://192.168.1.5:8040`, no
//! trailing slash) and never contains `|`; `<remote_opaque_id>` is base64url
//! and never contains `|` either, so the first `|` is an unambiguous separator.
//!
//! ## Sync trait over async HTTP
//! The `CameraBackend` trait is synchronous, but HTTP I/O is async. This
//! backend owns a dedicated multi-threaded tokio runtime. Each synchronous
//! method spawns an owned (`'static`) future on that runtime and blocks on a
//! `std::sync::mpsc` channel for the result — the same "block on a channel"
//! pattern the SDK-thread backends use, with no nested `block_on`.
//!
//! ## Live view
//! `get_live_view_frame` is polled by the shared capture loop. The first poll
//! starts a relay task that opens the peer's MJPEG stream and keeps the latest
//! JPEG frame in a shared cell; subsequent polls return that frame. The relay
//! self-terminates ~2 s after polling stops, closing the upstream connection
//! when no one is watching.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

use crate::camera::{
    CameraBackend, CameraError, CameraParameter, DeviceId, DeviceInfo, ParameterType,
};

mod peers;
pub use peers::{normalize_url, Peer, PeerRegistry, PeerView};

/// "Not ready yet" code reused from the Canon EVF path. The shared capture loop
/// treats it as a skippable frame instead of a fatal error.
const NOT_READY: CameraError = CameraError::SdkError(0x0000_A102);

/// Per-request timeouts. The live-view stream is intentionally left untimed.
const TIMEOUT_LIST: Duration = Duration::from_secs(1);
const TIMEOUT_CONTROL: Duration = Duration::from_secs(10);
const TIMEOUT_CAPTURE: Duration = Duration::from_secs(30);

/// Stop a live-view relay this long after the last `get_live_view_frame` poll.
const RELAY_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct RemoteBackend {
    rt: Runtime,
    client: reqwest::Client,
    registry: Arc<PeerRegistry>,
    /// Native IDs we have an open session with (local view of connection state).
    connected: Mutex<HashSet<String>>,
    /// Live-view relays keyed by native ID.
    relays: Mutex<HashMap<String, Relay>>,
}

struct Relay {
    latest: Arc<Mutex<Option<Bytes>>>,
    last_poll: Arc<Mutex<Instant>>,
    handle: JoinHandle<()>,
}

impl RemoteBackend {
    pub fn new(registry: Arc<PeerRegistry>) -> Result<Self, CameraError> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("remote-backend")
            .build()
            .map_err(|e| CameraError::Remote(format!("failed to build runtime: {e}")))?;

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(500))
            .build()
            .map_err(|e| CameraError::Remote(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            rt,
            client,
            registry,
            connected: Mutex::new(HashSet::new()),
            relays: Mutex::new(HashMap::new()),
        })
    }

    /// Spawns an owned future on the dedicated runtime and blocks until it
    /// resolves. Used to drive async HTTP from synchronous trait methods.
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
}

impl Drop for RemoteBackend {
    fn drop(&mut self) {
        for (_, relay) in self.relays.lock().unwrap().drain() {
            relay.handle.abort();
        }
    }
}

impl CameraBackend for RemoteBackend {
    fn backend_id(&self) -> &str {
        "remote"
    }

    fn list_devices(&self) -> Result<Vec<DeviceInfo>, CameraError> {
        let peers = self.registry.routing_snapshot();
        let client = self.client.clone();
        let pairs = self.block_on(async move { Ok(fetch_all_devices(client, peers).await) })?;

        let connected = self.connected.lock().unwrap();
        Ok(pairs
            .into_iter()
            .map(|(native, name)| DeviceInfo {
                id: DeviceId::new("remote", &native).encode(),
                name,
                connected: connected.contains(&native),
                dedup_key: None,
            })
            .collect())
    }

    fn connect(&self, native_id: &str) -> Result<(), CameraError> {
        let (base, token) = self.route(native_id)?;
        let client = self.client.clone();
        let result = self.block_on(proxy_action(client, base, "connect", token));
        if result.is_ok() {
            self.connected.lock().unwrap().insert(native_id.to_string());
        }
        result
    }

    fn disconnect(&self, native_id: &str) -> Result<(), CameraError> {
        // Tear down local state first so the upstream live-view stream is
        // released even if the remote disconnect call fails.
        if let Some(relay) = self.relays.lock().unwrap().remove(native_id) {
            relay.handle.abort();
        }
        self.connected.lock().unwrap().remove(native_id);

        let (base, token) = self.route(native_id)?;
        let client = self.client.clone();
        self.block_on(proxy_action(client, base, "disconnect", token))
    }

    fn is_connected(&self, native_id: &str) -> bool {
        self.connected.lock().unwrap().contains(native_id)
    }

    fn get_parameters(&self, native_id: &str) -> Result<Vec<CameraParameter>, CameraError> {
        let (base, token) = self.route(native_id)?;
        let client = self.client.clone();
        self.block_on(proxy_get_parameters(client, base, token))
    }

    fn set_parameter(
        &self,
        native_id: &str,
        param_type: ParameterType,
        value: &str,
    ) -> Result<(), CameraError> {
        let (base, token) = self.route(native_id)?;
        let client = self.client.clone();
        let value = value.to_string();
        self.block_on(proxy_set_parameter(client, base, token, param_type, value))
    }

    fn capture_photo(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (base, token) = self.route(native_id)?;
        let client = self.client.clone();
        self.block_on(proxy_capture(client, base, token))
    }

    fn get_live_view_frame(&self, native_id: &str) -> Result<Vec<u8>, CameraError> {
        let (base, token) = self.route(native_id)?;

        let mut relays = self.relays.lock().unwrap();
        let needs_start = match relays.get(native_id) {
            Some(relay) => relay.handle.is_finished(),
            None => true,
        };

        if needs_start {
            let latest = Arc::new(Mutex::new(None));
            let last_poll = Arc::new(Mutex::new(Instant::now()));
            let handle = self.rt.spawn(relay_loop(
                self.client.clone(),
                base,
                token,
                latest.clone(),
                last_poll.clone(),
            ));
            relays.insert(
                native_id.to_string(),
                Relay {
                    latest,
                    last_poll,
                    handle,
                },
            );
        }

        let relay = relays
            .get(native_id)
            .ok_or_else(|| CameraError::Remote("relay missing".into()))?;
        *relay.last_poll.lock().unwrap() = Instant::now();
        let frame = relay.latest.lock().unwrap().clone();
        drop(relays);

        match frame {
            Some(bytes) => Ok(bytes.to_vec()),
            None => Err(NOT_READY),
        }
    }
}

impl RemoteBackend {
    /// Splits a native ID into the request base URL `"<peer_url>/cameras/<id>"`
    /// and the peer's auth token (if one is registered for that URL).
    fn route(&self, native_id: &str) -> Result<(String, Option<String>), CameraError> {
        let (peer_url, remote_id) = native_id
            .split_once('|')
            .ok_or(CameraError::InvalidDeviceId)?;
        let base = format!("{peer_url}/cameras/{remote_id}");
        let token = self.registry.token_for(peer_url);
        Ok((base, token))
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

/// Maps a non-success status to a `CameraError`; returns `None` on success.
fn status_error(status: reqwest::StatusCode) -> Option<CameraError> {
    if status.is_success() {
        return None;
    }
    Some(match status.as_u16() {
        409 => CameraError::NotConnected,
        404 => CameraError::DeviceNotFound("remote device".into()),
        405 => CameraError::NotSupported,
        s => CameraError::Remote(format!("peer returned HTTP {s}")),
    })
}

/// Verifies a peer is reachable and is actually a toucan-camera-server, using
/// the credentials it will later be called with. Returns the peer's instance_id
/// on success, or a human-readable message on failure.
pub async fn validate_peer(
    client: &reqwest::Client,
    url: &str,
    token: &Option<String>,
) -> Result<String, String> {
    let resp = auth(client.get(format!("{url}/health")), token)
        .timeout(TIMEOUT_LIST)
        .send()
        .await
        .map_err(|e| format!("cannot reach peer: {e}"))?;

    if resp.status() == reqwest::StatusCode::FORBIDDEN {
        return Err("authentication failed — check the peer token".into());
    }
    if !resp.status().is_success() {
        return Err(format!("peer returned HTTP {}", resp.status().as_u16()));
    }

    let health: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| "peer did not return a valid health response".to_string())?;
    if health.get("service").and_then(|v| v.as_str()) != Some("toucan-camera-server") {
        return Err("this URL is not a toucan-camera-server instance".into());
    }

    let instance_id = health
        .get("instance_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    Ok(instance_id)
}

async fn proxy_action(
    client: reqwest::Client,
    base: String,
    action: &'static str,
    token: Option<String>,
) -> Result<(), CameraError> {
    let resp = auth(client.put(format!("{base}/{action}")), &token)
        .timeout(TIMEOUT_CONTROL)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    match status_error(resp.status()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn proxy_get_parameters(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
) -> Result<Vec<CameraParameter>, CameraError> {
    let resp = auth(client.get(format!("{base}/parameters")), &token)
        .timeout(TIMEOUT_CONTROL)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    if let Some(e) = status_error(resp.status()) {
        return Err(e);
    }
    resp.json::<Vec<CameraParameter>>()
        .await
        .map_err(|e| CameraError::Remote(format!("decoding parameters failed: {e}")))
}

async fn proxy_set_parameter(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    param_type: ParameterType,
    value: String,
) -> Result<(), CameraError> {
    let body = serde_json::json!({ "type": param_type, "value": value });
    let resp = auth(client.put(format!("{base}/parameters")), &token)
        .timeout(TIMEOUT_CONTROL)
        .json(&body)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    match status_error(resp.status()) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

async fn proxy_capture(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
) -> Result<Vec<u8>, CameraError> {
    let resp = auth(client.post(format!("{base}/capture")), &token)
        .timeout(TIMEOUT_CAPTURE)
        .send()
        .await
        .map_err(|e| CameraError::Remote(e.to_string()))?;
    if let Some(e) = status_error(resp.status()) {
        return Err(e);
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| CameraError::Remote(e.to_string()))
}

/// Fans out to every peer concurrently and returns `(native_id, name)` pairs.
/// Unreachable or erroring peers contribute nothing rather than failing the list.
async fn fetch_all_devices(
    client: reqwest::Client,
    peers: Vec<(String, Option<String>)>,
) -> Vec<(String, String)> {
    let mut handles = Vec::with_capacity(peers.len());
    for (url, token) in peers {
        let client = client.clone();
        handles.push(tokio::spawn(fetch_peer_devices(client, url, token)));
    }

    let mut out = Vec::new();
    for handle in handles {
        if let Ok(mut devices) = handle.await {
            out.append(&mut devices);
        }
    }
    out
}

async fn fetch_peer_devices(
    client: reqwest::Client,
    peer_url: String,
    token: Option<String>,
) -> Vec<(String, String)> {
    let resp = match auth(client.get(format!("{peer_url}/cameras")), &token)
        .timeout(TIMEOUT_LIST)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            eprintln!("[warn] peer {peer_url} returned HTTP {}", r.status());
            return Vec::new();
        }
        Err(e) => {
            eprintln!("[warn] peer {peer_url} unreachable: {e}");
            return Vec::new();
        }
    };

    let host_port = host_port(&peer_url);
    match resp.json::<Vec<DeviceInfo>>().await {
        Ok(devices) => devices
            .into_iter()
            .map(|d| {
                let native = format!("{peer_url}|{}", d.id);
                // Tag relayed cameras with the peer they come from so identical
                // device names across peers stay distinguishable.
                let name = format!("{} ({host_port})", d.name);
                (native, name)
            })
            .collect(),
        Err(e) => {
            eprintln!("[warn] peer {peer_url} sent malformed device list: {e}");
            Vec::new()
        }
    }
}

/// Strips the scheme from a normalized peer URL, leaving `host:port`.
fn host_port(url: &str) -> &str {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url)
}

// ---------------------------------------------------------------------------
// Live-view relay
// ---------------------------------------------------------------------------

async fn relay_loop(
    client: reqwest::Client,
    base: String,
    token: Option<String>,
    latest: Arc<Mutex<Option<Bytes>>>,
    last_poll: Arc<Mutex<Instant>>,
) {
    let url = format!("{base}/liveview");

    while last_poll.lock().unwrap().elapsed() <= RELAY_IDLE_TIMEOUT {
        let resp = match auth(client.get(&url), &token).send().await {
            Ok(r) if r.status().is_success() => r,
            _ => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };

        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        while let Some(chunk) = stream.next().await {
            if last_poll.lock().unwrap().elapsed() > RELAY_IDLE_TIMEOUT {
                return;
            }
            let chunk = match chunk {
                Ok(c) => c,
                Err(_) => break, // upstream stream broke — reconnect
            };
            buf.extend_from_slice(&chunk);
            while let Some(frame) = extract_frame(&mut buf) {
                *latest.lock().unwrap() = Some(frame);
            }
            // Safety valve against a malformed stream growing without bound.
            if buf.len() > 16 * 1024 * 1024 {
                buf.clear();
            }
        }
    }
}

/// Extracts one complete MJPEG part from the front of `buf`, draining the bytes
/// it consumes. Returns `None` while the buffer holds only a partial part.
///
/// Each part looks like:
/// `--frame\r\nContent-Type: image/jpeg\r\nContent-Length: N\r\n\r\n<N bytes>\r\n`
fn extract_frame(buf: &mut Vec<u8>) -> Option<Bytes> {
    let header_end = find(buf, b"\r\n\r\n")?;
    let header = String::from_utf8_lossy(&buf[..header_end]);

    let len: usize = header.lines().find_map(|line| {
        let line = line.trim();
        let rest = line
            .strip_prefix("Content-Length:")
            .or_else(|| line.strip_prefix("content-length:"))?;
        rest.trim().parse().ok()
    })?;

    let body_start = header_end + 4;
    let body_end = body_start.checked_add(len)?;
    if buf.len() < body_end {
        return None; // body not fully buffered yet
    }

    let frame = Bytes::copy_from_slice(&buf[body_start..body_end]);

    // Consume the part, plus the trailing CRLF when it has arrived.
    let mut consume = body_end;
    if buf.len() >= consume + 2 && &buf[consume..consume + 2] == b"\r\n" {
        consume += 2;
    }
    buf.drain(..consume);
    Some(frame)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_frame_parses_one_part() {
        let mut buf =
            b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 3\r\n\r\n\xff\xd8\xd9\r\n"
                .to_vec();
        let frame = extract_frame(&mut buf).expect("a complete frame");
        assert_eq!(&frame[..], &[0xff, 0xd8, 0xd9]);
        assert!(buf.is_empty(), "the whole part should be consumed");
    }

    #[test]
    fn extract_frame_waits_for_full_body() {
        // Header announces 5 bytes but only 2 are buffered.
        let mut buf =
            b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 5\r\n\r\n\xff\xd8".to_vec();
        assert!(extract_frame(&mut buf).is_none());
    }

    #[test]
    fn host_port_strips_scheme() {
        assert_eq!(host_port("http://192.168.1.5:8040"), "192.168.1.5:8040");
        assert_eq!(host_port("https://host:8040"), "host:8040");
        assert_eq!(host_port("192.168.1.5:8040"), "192.168.1.5:8040");
    }

    #[test]
    fn extract_frame_handles_back_to_back_parts() {
        let mut buf = Vec::new();
        for _ in 0..2 {
            buf.extend_from_slice(
                b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 2\r\n\r\n\xff\xd8\r\n",
            );
        }
        assert_eq!(&extract_frame(&mut buf).unwrap()[..], &[0xff, 0xd8]);
        assert_eq!(&extract_frame(&mut buf).unwrap()[..], &[0xff, 0xd8]);
        assert!(extract_frame(&mut buf).is_none());
    }
}
