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
    assert!(paths.contains_key("/event-ticket"));
    assert!(paths.contains_key("/events"));
    assert!(paths.contains_key("/session-events"));
    assert!(paths.contains_key("/openapi.json"));
    let description = doc["info"]["description"].as_str().unwrap();
    for method in [
        "daemon.capabilities",
        "session.send_message",
        "session.interject",
        "session.update_trust",
        "session.compact",
        "list_permission_requests",
        "create_permission_group",
        "resolve_permission_requests",
    ] {
        assert!(description.contains(method), "missing RPC method {method}");
    }
    assert!(paths["/events"]["get"]["responses"].get("403").is_some());
    let event_parameters = paths["/events"]["get"]["parameters"]
        .as_array()
        .expect("event query parameters");
    assert!(
        event_parameters
            .iter()
            .any(|parameter| parameter["name"] == "ticket")
    );
    assert!(
        event_parameters
            .iter()
            .all(|parameter| parameter["name"] != "token")
    );
    assert_eq!(
        paths["/rpc"]["post"]["security"].as_array().unwrap().len(),
        1
    );
    assert_eq!(
        paths["/event-ticket"]["post"]["security"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        paths["/events"]["get"]["security"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let schemas = doc["components"]["schemas"]
        .as_object()
        .expect("components.schemas object");
    for expected in [
        "JsonRpcRequest",
        "JsonRpcResponse",
        "JsonRpcError",
        "CapabilitiesRequest",
        "CapabilitiesResponse",
        "MethodCapability",
        "InlineImage",
        "EventTicketRequest",
        "EventTicketResponse",
        "CreateSessionRequest",
        "SendMessageRequest",
        "SendMessageResponse",
        "InterjectSessionRequest",
        "InterjectSessionResponse",
        "InterjectionLevel",
        "InterjectionState",
        "StartRunRequest",
        "StartRunResponse",
        "RunFlowRequest",
        "RunFlowResponse",
        "CancelRunRequest",
        "ResolvePromptRequest",
        "PromptResolutionStatus",
        "ResolveCompactReviewRequest",
        "CompactReviewResolutionStatus",
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
    let event_ticket = &doc["components"]["securitySchemes"]["event_ticket"];
    assert_eq!(event_ticket["type"].as_str(), Some("apiKey"));
    assert_eq!(event_ticket["in"].as_str(), Some("query"));
    assert_eq!(event_ticket["name"].as_str(), Some("ticket"));
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
