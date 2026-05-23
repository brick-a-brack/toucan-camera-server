//! Integration tests for the `/peers` management routes.
//!
//! Run with: `cargo test --features backend-remote` (or `backend-stopmotionstudio`).
#![cfg(feature = "peers")]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`

use toucan_camera::build_router;
use toucan_camera::peers::PeerRegistry;
use toucan_camera::routes::cameras::{AppState, BackendState};

const TOKEN: &str = "test-token";

/// Builds a router with no camera backends and an empty peer registry.
/// Clone it per request so the shared (Arc-backed) state persists across calls.
fn app() -> axum::Router {
    let backends: BackendState = Arc::new(HashMap::new());
    let token = Arc::new(RwLock::new(TOKEN.to_string()));
    let peers = Arc::new(PeerRegistry::new());
    build_router(AppState::new(backends, token, peers))
}

/// Spawns a minimal server whose `/health` returns the given JSON, used as a
/// peer to add. Returns its base URL (`http://127.0.0.1:<port>`).
async fn spawn_peer(health: Value) -> String {
    let app = axum::Router::new().route(
        "/health",
        axum::routing::get(move || {
            let body = health.clone();
            async move { axum::Json(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Spawns a minimal Stop Motion Studio remote camera whose `POST /status`
/// returns the given JSON. Returns its base URL.
async fn spawn_sms_peer(status: Value) -> String {
    let app = axum::Router::new().route(
        "/status",
        axum::routing::post(move || {
            let body = status.clone();
            async move { axum::Json(body) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

/// Returns a port that is (momentarily) free — bound then released — so a
/// connection to it is refused.
async fn closed_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn authed(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    let body = match body {
        Some(json) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    builder.body(body).unwrap()
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn toucan_health() -> Value {
    json!({ "status": "ok", "service": "toucan-camera-server", "version": "test" })
}

#[tokio::test]
async fn peer_lifecycle_add_list_delete() {
    let app = app();
    let peer_url = spawn_peer(toucan_health()).await;

    // Add a reachable, valid peer.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/peers",
            Some(&json!({ "url": peer_url, "token": "sekret" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let created = json_body(resp).await;
    let id = created["id"].as_str().expect("id present").to_string();
    assert_eq!(created["url"], peer_url);
    assert_eq!(created["token"], "sekret", "token is surfaced for the local UI");

    // List shows the peer.
    let resp = app.clone().oneshot(authed("GET", "/peers", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = json_body(resp).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["id"], id);

    // Delete it, then deleting again reports not found.
    let resp = app
        .clone()
        .oneshot(authed("DELETE", &format!("/peers/{id}"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    let resp = app
        .clone()
        .oneshot(authed("DELETE", &format!("/peers/{id}"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unreachable_peer_is_rejected_and_not_stored() {
    let app = app();
    let url = format!("127.0.0.1:{}", closed_port().await);

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/peers",
            Some(&json!({ "url": url }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    // The registry must stay empty.
    let resp = app.oneshot(authed("GET", "/peers", None)).await.unwrap();
    let list = json_body(resp).await;
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn non_toucan_peer_is_rejected() {
    let app = app();
    let peer_url = spawn_peer(json!({ "service": "something-else" })).await;

    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/peers",
            Some(&json!({ "url": peer_url }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let resp = app.oneshot(authed("GET", "/peers", None)).await.unwrap();
    assert!(json_body(resp).await.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stopmotion_peer_validated_via_status_and_stored_with_kind() {
    let app = app();
    let peer_url = spawn_sms_peer(json!({ "REMOTE_CAMERA_PROTOCOL_VERSION": 4 })).await;

    // A reachable remote camera (the right protocol) is accepted, tagged stopmotion.
    let resp = app
        .clone()
        .oneshot(authed(
            "POST",
            "/peers",
            Some(&json!({ "url": peer_url, "kind": "stopmotion" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(json_body(resp).await["kind"], "stopmotion");

    // A toucan server (no /status) is rejected when added as a stopmotion peer.
    let bogus = spawn_peer(toucan_health()).await;
    let resp = app
        .oneshot(authed(
            "POST",
            "/peers",
            Some(&json!({ "url": bogus, "kind": "stopmotion" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn peers_require_authentication() {
    let resp = app()
        .oneshot(Request::builder().uri("/peers").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
