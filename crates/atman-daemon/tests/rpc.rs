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
