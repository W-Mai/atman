use std::sync::Arc;

use atman_daemon::{
    DaemonState,
    http::{HttpState, router},
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

fn build_state(tmp: &tempfile::TempDir) -> Arc<HttpState> {
    let daemon = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    Arc::new(HttpState {
        daemon,
        auth_token: "secret".to_string(),
    })
}

async fn get_openapi(app: axum::Router, auth: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method("GET").uri("/openapi.json");
    if let Some(bearer) = auth {
        req = req.header("Authorization", format!("Bearer {bearer}"));
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, body.to_vec())
}

#[tokio::test]
async fn openapi_json_returns_301_document_with_expected_shape() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(build_state(&tmp));
    let (status, bytes) = get_openapi(app, Some("secret")).await;
    assert_eq!(status, StatusCode::OK);

    let doc: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
    assert_eq!(doc["openapi"].as_str().unwrap(), "3.1.0");
    assert_eq!(doc["info"]["title"].as_str().unwrap(), "atman daemon");

    let paths = doc["paths"].as_object().expect("paths object");
    assert!(paths.contains_key("/rpc"), "paths keys: {paths:?}");
    assert!(paths.contains_key("/events"));
    assert!(paths.contains_key("/openapi.json"));

    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("components.schemas object");
    for expected in [
        "JsonRpcRequest",
        "JsonRpcResponse",
        "JsonRpcError",
        "RunFlowRequest",
        "RunFlowResponse",
        "CancelRunRequest",
        "ResolvePromptRequest",
        "GetEventsRequest",
        "SessionSummary",
        "SessionStatus",
    ] {
        assert!(
            schemas.contains_key(expected),
            "missing schema {expected}; got {:?}",
            schemas.keys().collect::<Vec<_>>()
        );
    }

    let bearer = &doc["components"]["securitySchemes"]["bearer_token"];
    assert_eq!(bearer["type"].as_str(), Some("http"));
    assert_eq!(bearer["scheme"].as_str(), Some("bearer"));
}

#[tokio::test]
async fn openapi_json_requires_bearer_token() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(build_state(&tmp));
    let (status, _) = get_openapi(app, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn openapi_json_rejects_wrong_bearer_token() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(build_state(&tmp));
    let (status, _) = get_openapi(app, Some("wrong")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
