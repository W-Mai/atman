use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch_as};
use atman_proto::{
    FlowRunId, FormAnswer, FormResolutionStatus, FormSubmission, JsonRpcRequest, RequestId,
    SessionId, SubmitFormRequest, SubmitFormResponse, rpc,
};
use uuid::Uuid;

fn pending_form(form_id: &str, run_id: &FlowRunId) -> atman_runtime::form::PendingForm {
    atman_runtime::form::PendingForm {
        form_id: form_id.into(),
        run_id: atman_runtime::event::FlowRunId(run_id.0),
        tool_use_id: "tool-1".into(),
        form: atman_runtime::form::CompositeForm {
            questions: vec![atman_runtime::form::FormQuestion {
                id: "target".into(),
                kind: atman_runtime::form::FormKind::SingleSelect {
                    prompt: "Target".into(),
                    options: vec!["safe".into()],
                },
            }],
        },
        kind: atman_runtime::form::FormKind::SingleSelect {
            prompt: "Target".into(),
            options: vec!["safe".into()],
        },
        emitted_at: chrono::Utc::now(),
    }
}

async fn wait_for_form(state: &DaemonState, session_id: &SessionId) {
    loop {
        let snapshot = state.session_snapshot(session_id, "alice").await.unwrap();
        if !snapshot.projection.interactions.forms.is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
}

async fn submit(
    state: Arc<DaemonState>,
    principal: &str,
    rpc_id: u64,
    request: &SubmitFormRequest,
) -> atman_proto::JsonRpcResponse {
    dispatch_as(
        state,
        JsonRpcRequest::for_method::<rpc::SubmitForm>(rpc_id, request).unwrap(),
        principal,
    )
    .await
}

#[tokio::test]
async fn form_submission_is_validated_scoped_idempotent_and_convergent() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();
    let run_id = FlowRunId(Uuid::now_v7());
    let form_id = "form-1";
    let response = session.forms().request(pending_form(form_id, &run_id));
    wait_for_form(&state, &session_id).await;

    let invalid = SubmitFormRequest {
        request_id: Some(RequestId::now()),
        session_id: session_id.clone(),
        form_id: form_id.into(),
        submission: FormSubmission::Submitted {
            answers: vec![FormAnswer::Selected {
                index: 0,
                label: "spoofed".into(),
            }],
        },
    };
    assert!(
        submit(state.clone(), "alice", 1, &invalid)
            .await
            .error
            .is_some()
    );
    assert_eq!(session.forms().list_pending().len(), 1);

    let request = SubmitFormRequest {
        request_id: Some(RequestId::now()),
        session_id: session_id.clone(),
        form_id: form_id.into(),
        submission: FormSubmission::Submitted {
            answers: vec![FormAnswer::Selected {
                index: 0,
                label: "safe".into(),
            }],
        },
    };
    let denied = submit(state.clone(), "mallory", 2, &request).await;
    assert!(denied.error.is_some());
    assert_eq!(session.forms().list_pending().len(), 1);

    let resolved = submit(state.clone(), "alice", 3, &request).await;
    let resolved: SubmitFormResponse = serde_json::from_value(resolved.result.unwrap()).unwrap();
    assert!(resolved.resolved);
    assert_eq!(resolved.status, FormResolutionStatus::Resolved);
    assert_eq!(resolved.session_id, session_id);
    assert_eq!(resolved.form_id, form_id);
    assert!(matches!(
        response.await.unwrap(),
        atman_runtime::form::FormSubmission::Submitted { .. }
    ));
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert!(snapshot.projection.interactions.forms.is_empty());
    assert_eq!(resolved.revision, snapshot.projection.revision);
    assert_eq!(resolved.cursor, snapshot.cursor);

    let retry = submit(state.clone(), "alice", 4, &request).await;
    let retry: SubmitFormResponse = serde_json::from_value(retry.result.unwrap()).unwrap();
    assert_eq!(retry.status, FormResolutionStatus::Resolved);
    assert_eq!(retry.revision, resolved.revision);
    assert_eq!(retry.cursor, resolved.cursor);

    let competing = SubmitFormRequest {
        request_id: Some(RequestId::now()),
        ..request
    };
    let competing = submit(state, "alice", 5, &competing).await;
    let competing: SubmitFormResponse = serde_json::from_value(competing.result.unwrap()).unwrap();
    assert!(!competing.resolved);
    assert_eq!(competing.status, FormResolutionStatus::AlreadyResolved);
}
