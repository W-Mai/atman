use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, dispatch};
use atman_proto::{JsonRpcRequest, PromptId, methods};
use uuid::Uuid;

#[tokio::test]
async fn resolve_prompt_wakes_registered_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let pid = PromptId(Uuid::now_v7());
    let rx = state.register_pending_prompt(pid.clone());
    let command = atman_proto::ResolvePromptRequest {
        request_id: Some(atman_proto::RequestId::now()),
        prompt_id: pid,
        answer: serde_json::json!({"choice": "yes"}),
    };

    let state_rpc = state.clone();
    let command_rpc = command.clone();
    let rpc_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        let req =
            JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(1, &command_rpc).unwrap();
        dispatch(state_rpc, req).await
    });

    let answer = tokio::time::timeout(Duration::from_secs(1), rx)
        .await
        .expect("timed out")
        .expect("channel closed");
    assert_eq!(answer, serde_json::json!({"choice": "yes"}));

    let resp = rpc_task.await.unwrap();
    assert_eq!(resp.result.unwrap()["resolved"], serde_json::json!(true));

    let retry = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(2, &command).unwrap(),
    )
    .await;
    assert_eq!(retry.result.unwrap()["resolved"], serde_json::json!(true));
}

#[tokio::test]
async fn resolve_prompt_returns_false_when_no_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let req = JsonRpcRequest::new(
        1,
        methods::RESOLVE_PROMPT,
        serde_json::json!({"prompt_id": PromptId(Uuid::now_v7()), "answer": null}),
    );
    let resp = dispatch(state, req).await;
    assert_eq!(resp.result.unwrap()["resolved"], serde_json::json!(false));
}

#[tokio::test]
async fn drop_pending_prompt_closes_receiver() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let pid = PromptId(Uuid::now_v7());
    let rx = state.register_pending_prompt(pid.clone());
    state.drop_pending_prompt(&pid);
    let res = rx.await;
    assert!(res.is_err(), "receiver should error after sender dropped");
}
