//! In-memory registry of remote peers shared between the [`RemoteBackend`] and
//! the `/peers` management routes.
//!
//! [`RemoteBackend`]: super::RemoteBackend

use std::sync::RwLock;

use serde::Serialize;

/// A registered peer: another toucan-camera-server instance reachable over HTTP.
#[derive(Debug, Clone)]
pub struct Peer {
    /// Stable identifier assigned on registration (used by `DELETE /peers/{id}`).
    pub id: String,
    /// Normalized base URL with scheme and no trailing slash, e.g.
    /// `http://192.168.1.5:8040`.
    pub url: String,
    /// Optional bearer token sent to the peer on every proxied request.
    pub token: Option<String>,
}

/// Public view of a peer returned by the API. Since the server is local
/// (loopback / LAN), the token is surfaced so the UI can display it.
#[derive(Debug, Clone, Serialize)]
pub struct PeerView {
    pub id: String,
    pub url: String,
    pub token: Option<String>,
}

impl From<&Peer> for PeerView {
    fn from(p: &Peer) -> Self {
        Self {
            id: p.id.clone(),
            url: p.url.clone(),
            token: p.token.clone(),
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

    /// Public, token-free view of every registered peer.
    pub fn list(&self) -> Vec<PeerView> {
        self.peers
            .read()
            .unwrap()
            .iter()
            .map(PeerView::from)
            .collect()
    }

    /// `(url, token)` pairs the backend uses to fan out and route requests.
    pub fn routing_snapshot(&self) -> Vec<(String, Option<String>)> {
        self.peers
            .read()
            .unwrap()
            .iter()
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

    /// Registers a peer. Adding a URL that already exists updates its token in
    /// place (idempotent) rather than creating a duplicate.
    pub fn add(&self, url: &str, token: Option<String>) -> PeerView {
        let url = normalize_url(url);
        let mut peers = self.peers.write().unwrap();

        if let Some(existing) = peers.iter_mut().find(|p| p.url == url) {
            existing.token = token;
            return PeerView::from(&*existing);
        }

        let peer = Peer {
            id: uuid::Uuid::new_v4().to_string(),
            url,
            token,
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
    fn add_is_idempotent_per_url() {
        let reg = PeerRegistry::new();
        let a = reg.add("host:8040", None);
        let b = reg.add("http://host:8040/", Some("tok".into()));
        assert_eq!(a.id, b.id, "same URL should reuse the same peer id");
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.token_for("http://host:8040").as_deref(), Some("tok"));
    }

    #[test]
    fn remove_reports_whether_anything_changed() {
        let reg = PeerRegistry::new();
        let p = reg.add("host", None);
        assert!(reg.remove(&p.id));
        assert!(!reg.remove(&p.id));
        assert!(reg.list().is_empty());
    }
}
