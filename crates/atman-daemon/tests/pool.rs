use std::sync::Arc;

use atman_daemon::{DaemonState, LiveSession, dispatch};
use atman_proto::{FlowRunId, JsonRpcRequest, SessionId, methods};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
async fn cancel_run_hits_matching_live_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let sid = SessionId(Uuid::now_v7());
    let run_id = FlowRunId(Uuid::now_v7());
    let cancel = CancellationToken::new();
    state.register_live(
        sid,
        LiveSession {
            run_id: run_id.clone(),
            flow_name: "hello".into(),
            cancel: cancel.clone(),
            started_at: chrono::Utc::now(),
        },
    );

    let req = JsonRpcRequest::new(
        1,
        methods::CANCEL_RUN,
        serde_json::json!({"run_id": run_id}),
    );
    let resp = dispatch(state.clone(), req).await;
    let result = resp.result.expect("cancel_run returns result");
    assert_eq!(result["cancelled"], serde_json::json!(true));
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn cancel_run_missing_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(
        1,
        methods::CANCEL_RUN,
        serde_json::json!({"run_id": FlowRunId(Uuid::now_v7())}),
    );
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("returns ok");
    assert_eq!(result["cancelled"], serde_json::json!(false));
}

#[tokio::test]
async fn list_sessions_includes_live_only_entry_as_running() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let sid = SessionId(Uuid::now_v7());
    state.register_live(
        sid.clone(),
        LiveSession {
            run_id: FlowRunId(Uuid::now_v7()),
            flow_name: "hello".into(),
            cancel: CancellationToken::new(),
            started_at: chrono::Utc::now(),
        },
    );

    let req = JsonRpcRequest::new(1, methods::LIST_SESSIONS, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let arr = resp.result.expect("ok").as_array().unwrap().clone();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "running");
    assert_eq!(arr[0]["id"], serde_json::to_value(sid).unwrap());
}
