//! Peer management routes shared by the HTTP-proxying backends.
//!
//! - `GET    /peers`       — list registered peers (token surfaced for the local UI)
//! - `POST   /peers`       — register a peer `{ url, token?, kind? }`
//! - `DELETE /peers/{id}`  — remove a peer
//!
//! `kind` selects the protocol (`toucan` — default — or `stopmotion`) and
//! therefore which backend relays the peer.

use std::time::Duration;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

use crate::peers::{normalize_url, validate_peer, PeerKind, PeerView};
use crate::routes::cameras::AppState;

pub async fn list_peers(State(state): State<AppState>) -> Json<Vec<PeerView>> {
    Json(state.peers.list())
}

#[derive(Deserialize)]
pub struct AddPeerBody {
    /// Base URL of the peer. Scheme defaults to `http://` if omitted.
    pub url: String,
    pub token: Option<String>,
    /// Protocol the peer speaks; defaults to `toucan` when omitted.
    #[serde(default)]
    pub kind: PeerKind,
}

pub async fn add_peer(State(state): State<AppState>, Json(body): Json<AddPeerBody>) -> Response {
    if body.url.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "url must not be empty" })),
        )
            .into_response();
    }

    let url = normalize_url(&body.url);

    // Reject peers we can't reach or authenticate against, so the registry never
    // holds a dead or misconfigured entry.
    let client = match reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to build HTTP client: {e}") })),
            )
                .into_response()
        }
    };

    if let Err(e) = validate_peer(&client, &url, &body.token, body.kind).await {
        return (StatusCode::BAD_GATEWAY, Json(json!({ "error": e }))).into_response();
    }

    let peer = state.peers.add(&url, body.token, body.kind);
    (StatusCode::CREATED, Json(peer)).into_response()
}

pub async fn delete_peer(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.peers.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "unknown peer id" })),
        )
            .into_response()
    }
}
