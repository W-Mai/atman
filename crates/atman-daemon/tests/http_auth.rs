use std::sync::Arc;

use atman_daemon::{
    DaemonState,
    http::{HttpState, router},
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn build_state() -> Arc<HttpState> {
    let tmp = tempfile::tempdir().unwrap();
    let daemon = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    // leak tempdir so it survives the test lifetime; safe: process exits after test
    std::mem::forget(tmp);
    Arc::new(HttpState {
        daemon,
        auth_token: "secret-token".to_string(),
    })
}

#[tokio::test]
async fn http_missing_authorization_returns_401() {
    let state = build_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .body(Body::from(
                    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_wrong_token_returns_401() {
    let state = build_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .header("Authorization", "Bearer wrong")
                .body(Body::from(
                    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn http_correct_token_dispatches_ping() {
    let state = build_state();
    let app = router(state);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .header("Authorization", "Bearer secret-token")
                .body(Body::from(
                    "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}",
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["result"]["pong"], serde_json::json!(true));
}
