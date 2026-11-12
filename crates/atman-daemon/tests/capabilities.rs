use std::{collections::BTreeSet, sync::Arc};

use atman_daemon::{
    DaemonState, dispatch, dispatch_as,
    http::{HttpState, router},
    unix::UnixServer,
};
use atman_proto::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    capabilities::{
        CAPABILITY_MISMATCH, CapabilitiesResponse, CapabilityMismatch, CapabilityMismatchKind,
    },
    methods::{self, Method},
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use utoipa::OpenApi;

async fn capabilities(state: &Arc<DaemonState>, params: Value) -> JsonRpcResponse {
    dispatch(
        state.clone(),
        JsonRpcRequest::new(7, methods::CAPABILITIES, params),
    )
    .await
}

#[tokio::test]
async fn advertised_methods_are_dispatched_without_advertising_planned_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("unopened");
    let state = Arc::new(DaemonState::new(data.clone()));
    let response = capabilities(&state, json!({})).await;
    let catalog: CapabilitiesResponse = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(!data.exists(), "capability discovery must not open storage");
    assert_eq!(catalog.host_generation, state.daemon_generation());
    assert_eq!(catalog.protocol_revision, 1);
    assert_eq!(catalog.schema_revision, 1);
    assert_eq!(
        catalog.methods[methods::FLOW_OUTPUT]
            .response_schema
            .as_deref(),
        Some("FlowOutputPage")
    );
    assert_eq!(
        catalog.methods[methods::FLOW_EVENTS]
            .response_schema
            .as_deref(),
        Some("FlowEventPage")
    );
    assert_eq!(
        catalog.methods[methods::SESSION_FLOWS]
            .response_schema
            .as_deref(),
        Some("FlowListPage")
    );
    for method in [
        Method::FlowGet,
        Method::FlowInvoke,
        Method::GoalGet,
        Method::GoalSet,
        Method::GoalPause,
        Method::GoalResume,
        Method::GoalClear,
    ] {
        assert_eq!(catalog.methods[method.name()].revision, 2);
    }
    let names: BTreeSet<_> = Method::ALL.iter().map(|method| method.name()).collect();
    assert_eq!(names.len(), Method::ALL.len());
    assert_eq!(
        catalog
            .methods
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        names
    );
    for method in Method::ALL {
        assert_eq!(Method::from_name(method.name()), Some(*method));
        assert_eq!(serde_json::to_value(method).unwrap(), json!(method.name()));
        // Null cannot create a target, but still reaches each registered dispatch arm.
        let reply = dispatch(
            state.clone(),
            JsonRpcRequest::new(8, method.name(), Value::Null),
        )
        .await;
        assert_ne!(
            reply.error.as_ref().map(|error| error.code),
            Some(JsonRpcError::METHOD_NOT_FOUND),
            "{}",
            method.name()
        );
    }
    for name in [
        methods::GET_EVENTS,
        "quickask.open",
        "flow.transient",
        "flow.position",
        "flow.live",
        "session.fork",
        "flow.revert",
        "web",
    ] {
        assert!(!catalog.methods.contains_key(name));
        let reply = dispatch(state.clone(), JsonRpcRequest::new(9, name, json!({}))).await;
        assert_eq!(reply.error.unwrap().code, JsonRpcError::METHOD_NOT_FOUND);
    }
    assert!(!data.exists());
    let repeated: CapabilitiesResponse =
        serde_json::from_value(capabilities(&state, json!({})).await.result.unwrap()).unwrap();
    assert_eq!(catalog, repeated);
    let restarted = Arc::new(DaemonState::new(data));
    let after: CapabilitiesResponse =
        serde_json::from_value(capabilities(&restarted, json!({})).await.result.unwrap()).unwrap();
    assert_ne!(catalog.host_generation, after.host_generation);
    assert_eq!(catalog.methods, after.methods);
}

#[tokio::test]
async fn requirements_report_typed_mismatch_and_reject_unnegotiated_positions() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("unopened");
    let state = Arc::new(DaemonState::new(data.clone()));
    for (params, kind, supported, method) in [
        (
            json!({"protocol_revision": 2}),
            CapabilityMismatchKind::ProtocolRevision,
            Some(1),
            None,
        ),
        (
            json!({"schema_revision": 2}),
            CapabilityMismatchKind::SchemaRevision,
            Some(1),
            None,
        ),
        (
            json!({"methods": {"flow.events": 2}}),
            CapabilityMismatchKind::MethodRevision,
            Some(1),
            Some("flow.events"),
        ),
        (
            json!({"methods": {"flow.transient": 2}}),
            CapabilityMismatchKind::MethodUnsupported,
            None,
            Some("flow.transient"),
        ),
    ] {
        let error = capabilities(&state, params).await.error.unwrap();
        assert_eq!(error.code, CAPABILITY_MISMATCH);
        let detail: CapabilityMismatch = serde_json::from_value(error.data.unwrap()).unwrap();
        assert_eq!(
            detail,
            CapabilityMismatch {
                kind,
                requested: 2,
                supported,
                method: method.map(str::to_owned)
            }
        );
    }
    assert!(
        capabilities(
            &state,
            json!({"protocol_revision": 1, "schema_revision": 1, "methods": {"flow.events": 1, "flow.invoke": 2}})
        )
        .await
        .error
        .is_none()
    );
    let many: serde_json::Map<String, Value> = (0..129)
        .map(|index| (index.to_string(), json!(1)))
        .collect();
    for params in [
        json!({"unknown": true}),
        json!({"protocol_revision": -1}),
        json!({"methods": many}),
        json!({"methods": {"x".repeat(129): 1}}),
    ] {
        assert_eq!(
            capabilities(&state, params).await.error.unwrap().code,
            JsonRpcError::INVALID_PARAMS
        );
    }
    let transient = json!({"transient": {"host_generation": uuid::Uuid::now_v7(), "flow_id": uuid::Uuid::now_v7(), "sequence": 1, "digest": "a".repeat(64)}});
    let reply = dispatch(state.clone(), JsonRpcRequest::new(10, methods::FLOW_EVENTS, json!({"flow": {"session_id": uuid::Uuid::now_v7(), "instance_id": uuid::Uuid::now_v7()}, "after": transient}))).await;
    assert_eq!(reply.error.unwrap().code, JsonRpcError::INVALID_PARAMS);
    let denied = dispatch_as(
        state,
        JsonRpcRequest::new(11, methods::CAPABILITIES, json!({})),
        "another-operator",
    )
    .await;
    assert_eq!(denied.error.unwrap().code, JsonRpcError::APPLICATION_ERROR);
    assert!(!data.exists());
}

#[tokio::test]
async fn capability_schemas_and_authenticated_transports_share_the_same_contract() {
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(directory.path().join("unopened")));
    let request = JsonRpcRequest::new(12, methods::CAPABILITIES, json!({}));
    let app = router(Arc::new(HttpState {
        daemon: state.clone(),
        auth_token: "test-token".into(),
    }));
    let denied = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let reply = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/rpc")
                .header("Authorization", "Bearer test-token")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reply.status(), StatusCode::OK);
    let body = axum::body::to_bytes(reply.into_body(), 1_000_000)
        .await
        .unwrap();
    let http: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
    let catalog: CapabilitiesResponse =
        serde_json::from_value(http.result.clone().unwrap()).unwrap();
    let schema = serde_json::to_value(atman_daemon::openapi::AtmanOpenApi::openapi()).unwrap();
    for capability in catalog.methods.values() {
        for name in capability
            .request_schema
            .iter()
            .chain(capability.response_schema.iter())
        {
            assert!(
                schema["components"]["schemas"].get(name).is_some(),
                "missing schema {name}"
            );
        }
    }
    let path = directory.path().join("daemon.sock");
    let server = UnixServer::bind(&path).await.unwrap();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(server.serve(state.clone(), shutdown.clone()));
    let stream = UnixStream::connect(path).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut encoded = serde_json::to_vec(&request).unwrap();
    encoded.push(b'\n');
    write.write_all(&encoded).await.unwrap();
    let mut lines = BufReader::new(read).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let unix: JsonRpcResponse = serde_json::from_str(&line).unwrap();
    assert_eq!(unix.result, http.result);
    assert!(unix.error.is_none());
    shutdown.cancel();
    run.await.unwrap().unwrap();
    assert!(!state.data_dir().exists());
}
