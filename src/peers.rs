//! Shared in-memory registry of remote peers.
//!
//! A "peer" is another HTTP camera server this instance relays cameras from.
//! Two kinds are supported, distinguished by [`PeerKind`]:
//!
//! - [`PeerKind::Toucan`]   — another toucan-camera-server instance, driven by
//!   the [`remote`] backend over toucan's own REST API.
//! - [`PeerKind::StopMotion`] — a Stop Motion Studio "remote camera" server,
//!   driven by the [`stopmotionstudio`] backend over its protocol
//!   (`docs/remote-camera-protocol.md`).
//!
//! The registry is shared between the backends (which read it to route and fan
//! out requests, each filtering to its own kind) and the `/peers` management
//! routes (which mutate it). It holds no on-disk state.
//!
//! [`remote`]: crate::backends::remote
//! [`stopmotionstudio`]: crate::backends::stopmotionstudio

use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// Which protocol a peer speaks, and therefore which backend relays it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PeerKind {
    /// Another toucan-camera-server instance (default).
    #[default]
    Toucan,
    /// A Stop Motion Studio "remote camera" server.
    Stopmotion,
}

/// A registered peer reachable over HTTP.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Stable identifier assigned on registration (used by `DELETE /peers/{id}`).
    pub id: String,
    /// Normalized base URL with scheme and no trailing slash, e.g.
    /// `http://192.168.1.5:8040`.
    pub url: String,
    /// Optional bearer token sent to the peer on every proxied request.
    pub token: Option<String>,
    /// Protocol the peer speaks.
    pub kind: PeerKind,
}

/// Public view of a peer returned by the API. Since the server is local
/// (loopback / LAN), the token is surfaced so the UI can display it.
#[derive(Debug, Clone, Serialize)]
pub struct PeerView {
    pub id: String,
    pub url: String,
    pub token: Option<String>,
    pub kind: PeerKind,
}

impl From<&Peer> for PeerView {
    fn from(p: &Peer) -> Self {
        Self {
            id: p.id.clone(),
            url: p.url.clone(),
            token: p.token.clone(),
            kind: p.kind,
        }
    }
}

#[derive(Default)]
pub struct PeerRegistry {
    peers: RwLock<Vec<Peer>>,
}

impl PeerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Public, full view of every registered peer.
    pub fn list(&self) -> Vec<PeerView> {
        self.peers
            .read()
            .unwrap()
            .iter()
            .map(PeerView::from)
            .collect()
    }

    /// `(url, token)` pairs of a given kind, used by the matching backend to fan
    /// out and route requests. Peers of other kinds are excluded.
    pub fn routing_snapshot(&self, kind: PeerKind) -> Vec<(String, Option<String>)> {
        self.peers
            .read()
            .unwrap()
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| (p.url.clone(), p.token.clone()))
            .collect()
    }

    /// Token registered for a given (already-normalized) peer URL, if any.
    pub fn token_for(&self, url: &str) -> Option<String> {
        self.peers
            .read()
            .unwrap()
            .iter()
            .find(|p| p.url == url)
            .and_then(|p| p.token.clone())
    }

    /// Registers a peer. Adding a URL that already exists updates its token and
    /// kind in place (idempotent) rather than creating a duplicate.
    pub fn add(&self, url: &str, token: Option<String>, kind: PeerKind) -> PeerView {
        let url = normalize_url(url);
        let mut peers = self.peers.write().unwrap();

        if let Some(existing) = peers.iter_mut().find(|p| p.url == url) {
            existing.token = token;
            existing.kind = kind;
            return PeerView::from(&*existing);
        }

        let peer = Peer {
            id: uuid::Uuid::new_v4().to_string(),
            url,
            token,
            kind,
        };
        let view = PeerView::from(&peer);
        peers.push(peer);
        view
    }

    /// Removes a peer by id. Returns whether a peer was actually removed.
    pub fn remove(&self, id: &str) -> bool {
        let mut peers = self.peers.write().unwrap();
        let before = peers.len();
        peers.retain(|p| p.id != id);
        peers.len() != before
    }
}

/// Normalizes a peer URL: trims whitespace, defaults the scheme to `http://`,
/// and strips a trailing slash so URLs concatenate cleanly.
pub fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    };
    with_scheme.trim_end_matches('/').to_string()
}

/// Per-request timeout used while validating a peer at registration time.
const VALIDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

fn auth(builder: reqwest::RequestBuilder, token: &Option<String>) -> reqwest::RequestBuilder {
    match token {
        Some(t) => builder.bearer_auth(t),
        None => builder,
    }
}

/// Verifies a peer is reachable and actually speaks the expected protocol, using
/// the credentials it will later be called with. Returns a human-readable
/// message on failure so a peer is never registered when it can't be reached,
/// can't be authenticated, or is the wrong kind of server.
pub async fn validate_peer(
    client: &reqwest::Client,
    url: &str,
    token: &Option<String>,
    kind: PeerKind,
) -> Result<(), String> {
    match kind {
        PeerKind::Toucan => validate_toucan(client, url, token).await,
        PeerKind::Stopmotion => validate_stopmotion(client, url, token).await,
    }
}

/// Hits `/health` (behind the same auth as every route) and checks the service
/// banner identifies a toucan-camera-server.
async fn validate_toucan(
    client: &reqwest::Client,
    url: &str,
    token: &Option<String>,
) -> Result<(), String> {
    let resp = auth(client.get(format!("{url}/health")), token)
        .timeout(VALIDATE_TIMEOUT)
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
    Ok(())
}

/// Hits `POST /status` and checks the JSON carries `REMOTE_CAMERA_PROTOCOL_VERSION`,
/// the signature of a Stop Motion Studio remote camera server.
async fn validate_stopmotion(
    client: &reqwest::Client,
    url: &str,
    token: &Option<String>,
) -> Result<(), String> {
    let resp = auth(client.post(format!("{url}/status")), token)
        .timeout(VALIDATE_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("cannot reach camera: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("camera returned HTTP {}", resp.status().as_u16()));
    }

    let status: serde_json::Value = resp
        .json()
        .await
        .map_err(|_| "camera did not return a valid status response".to_string())?;
    if status.get("REMOTE_CAMERA_PROTOCOL_VERSION").is_none() {
        return Err("this URL is not a Stop Motion Studio remote camera".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_adds_scheme_and_trims_slash() {
        assert_eq!(normalize_url("192.168.1.5:8040"), "http://192.168.1.5:8040");
        assert_eq!(normalize_url("http://host:8040/"), "http://host:8040");
        assert_eq!(normalize_url("  https://host  "), "https://host");
    }

    #[test]
    fn add_is_idempotent_per_url_and_updates_kind() {
        let reg = PeerRegistry::new();
        let a = reg.add("host:8040", None, PeerKind::Toucan);
        let b = reg.add("http://host:8040/", Some("tok".into()), PeerKind::Stopmotion);
        assert_eq!(a.id, b.id, "same URL should reuse the same peer id");
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.token_for("http://host:8040").as_deref(), Some("tok"));
        assert_eq!(b.kind, PeerKind::Stopmotion, "kind updated in place");
    }

    #[test]
    fn routing_snapshot_filters_by_kind() {
        let reg = PeerRegistry::new();
        reg.add("toucan-a", None, PeerKind::Toucan);
        reg.add("sms-a", None, PeerKind::Stopmotion);
        reg.add("toucan-b", None, PeerKind::Toucan);

        let toucan = reg.routing_snapshot(PeerKind::Toucan);
        assert_eq!(toucan.len(), 2);
        let sms = reg.routing_snapshot(PeerKind::Stopmotion);
        assert_eq!(sms.len(), 1);
        assert_eq!(sms[0].0, "http://sms-a");
    }

    #[test]
    fn remove_reports_whether_anything_changed() {
        let reg = PeerRegistry::new();
        let p = reg.add("host", None, PeerKind::Toucan);
        assert!(reg.remove(&p.id));
        assert!(!reg.remove(&p.id));
        assert!(reg.list().is_empty());
    }
}
