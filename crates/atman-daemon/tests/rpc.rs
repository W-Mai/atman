use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch};
use atman_proto::{JsonRpcRequest, methods};

#[tokio::test]
async fn ping_returns_pong() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(1, methods::PING, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("ping should succeed");
    assert_eq!(result["pong"], serde_json::json!(true));
    assert!(result["version"].is_string());
}

#[tokio::test]
async fn method_not_found_returns_jsonrpc_error() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(2, "no_such_method", serde_json::json!({}));
    let resp = dispatch(state, req).await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("expect error");
    assert_eq!(err.code, atman_proto::JsonRpcError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn list_sessions_reads_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("events.jsonl"),
        "{\"type\":\"flow_start\",\"ts\":\"2025-01-01T00:00:00Z\"}\n",
    )
    .unwrap();

    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(3, methods::LIST_SESSIONS, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("expect result");
    let arr = result.as_array().expect("expect array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["event_count"], 1);
    assert_eq!(arr[0]["status"], "finished");
}

#[tokio::test]
async fn goal_controls_require_owner_and_current_revision() {
    use atman_daemon::{dispatch_as, state::LiveSession};
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().into()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    session.goal_control.set("finish work", None).unwrap();
    let sid = atman_proto::SessionId(session.id().0);
    state.register_broker(sid.clone(), session.clone(), "owner");
    state.register_live(
        sid.clone(),
        LiveSession {
            run_id: atman_proto::FlowRunId(uuid::Uuid::now_v7()),
            flow_name: "agent".into(),
            cancel: session.flow_cancel_token(),
            started_at: chrono::Utc::now(),
        },
    );
    let params = serde_json::json!({"session_id":sid});
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::GOAL_GET, params.clone()),
        "other",
    )
    .await;
    assert!(denied.error.is_some());
    let read = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(2, methods::GOAL_GET, params.clone()),
        "owner",
    )
    .await
    .result
    .unwrap();
    let write = serde_json::json!({"session_id":sid,"goal_id":read["goal"]["id"],"expected_revision":read["goal"]["revision"]});
    let paused = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(3, methods::GOAL_PAUSE, write.clone()),
        "owner",
    )
    .await
    .result
    .unwrap();
    assert_eq!(paused["goal"]["status"], "paused");
    assert!(!session.flow_cancel_token().is_cancelled());
    let stale = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(4, methods::GOAL_RESUME, write),
        "owner",
    )
    .await;
    assert!(stale.error.is_some());
    let resumed = dispatch_as(state, JsonRpcRequest::new(5, methods::GOAL_RESUME, serde_json::json!({"session_id":sid,"goal_id":paused["goal"]["id"],"expected_revision":paused["goal"]["revision"]})), "owner").await.result.unwrap();
    assert_eq!(resumed["goal"]["status"], "active");
}
