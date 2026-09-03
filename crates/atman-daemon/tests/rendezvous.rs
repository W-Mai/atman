use std::sync::Arc;
use std::time::Duration;

use atman_daemon::{DaemonState, dispatch, dispatch_as};
use atman_proto::{JsonRpcRequest, PromptId};
use uuid::Uuid;

async fn idle_session(
    state: &Arc<DaemonState>,
) -> (Arc<atman_runtime::Session>, atman_proto::SessionId) {
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = atman_proto::SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "local-daemon")
        .await
        .unwrap();
    (session, session_id)
}

#[tokio::test]
async fn resolve_prompt_wakes_registered_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (_session, session_id) = idle_session(&state).await;

    let pid = PromptId(Uuid::now_v7());
    let rx =
        state.register_pending_prompt(&session_id, pid.clone(), "prompt", serde_json::Value::Null);
    let command = atman_proto::ResolvePromptRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id,
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
    let resolved = resp
        .into_method_output::<atman_proto::rpc::ResolvePrompt>()
        .unwrap();
    assert!(resolved.resolved);
    assert_eq!(
        resolved.status,
        atman_proto::PromptResolutionStatus::Resolved
    );

    let retry = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(2, &command).unwrap(),
    )
    .await;
    assert!(
        retry
            .into_method_output::<atman_proto::rpc::ResolvePrompt>()
            .unwrap()
            .resolved
    );
}

#[tokio::test]
async fn resolve_prompt_returns_false_when_no_waiter() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (_session, session_id) = idle_session(&state).await;

    let req = JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(
        1,
        &atman_proto::ResolvePromptRequest {
            request_id: Some(atman_proto::RequestId::now()),
            session_id,
            prompt_id: PromptId(Uuid::now_v7()),
            answer: serde_json::Value::Null,
        },
    )
    .unwrap();
    let resp = dispatch(state, req).await;
    let result = resp
        .into_method_output::<atman_proto::rpc::ResolvePrompt>()
        .unwrap();
    assert!(!result.resolved);
    assert_eq!(result.status, atman_proto::PromptResolutionStatus::NotFound);
}

#[tokio::test]
async fn drop_pending_prompt_closes_receiver() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (_session, session_id) = idle_session(&state).await;
    let pid = PromptId(Uuid::now_v7());
    let rx =
        state.register_pending_prompt(&session_id, pid.clone(), "prompt", serde_json::Value::Null);
    state.drop_pending_prompt(&session_id, &pid);
    let res = rx.await;
    assert!(res.is_err(), "receiver should error after sender dropped");
}

#[tokio::test]
async fn prompt_resolution_is_session_scoped_and_reports_terminal_competition() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = atman_proto::SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session, "alice")
        .await
        .unwrap();
    let prompt_id = PromptId(Uuid::now_v7());
    let receiver = state.register_pending_prompt(
        &session_id,
        prompt_id.clone(),
        "prompt",
        serde_json::Value::Null,
    );
    let request = |request_id| atman_proto::ResolvePromptRequest {
        request_id: Some(request_id),
        session_id: session_id.clone(),
        prompt_id: prompt_id.clone(),
        answer: serde_json::json!({"choice": "yes"}),
    };

    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(
            1,
            &request(atman_proto::RequestId::now()),
        )
        .unwrap(),
        "bob",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));

    let accepted = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(
            2,
            &request(atman_proto::RequestId::now()),
        )
        .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::ResolvePrompt>()
    .unwrap();
    assert_eq!(
        receiver.await.unwrap(),
        serde_json::json!({"choice": "yes"})
    );
    assert!(accepted.resolved);

    let competing = dispatch_as(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::ResolvePrompt>(
            3,
            &request(atman_proto::RequestId::now()),
        )
        .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::ResolvePrompt>()
    .unwrap();
    assert!(!competing.resolved);
    assert_eq!(
        competing.status,
        atman_proto::PromptResolutionStatus::AlreadyResolved
    );
    assert_eq!(competing.revision, accepted.revision);
    assert_eq!(competing.cursor, accepted.cursor);
}
