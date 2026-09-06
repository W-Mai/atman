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
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = state.session_snapshot(session_id, "alice").await.unwrap();
            if !snapshot.projection.interactions.forms.is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("form was not projected");
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

#[tokio::test]
async fn dropped_form_wait_is_abandoned_without_removing_another_request() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();
    let before = state.session_snapshot(&session_id, "alice").await.unwrap();
    let run_id = FlowRunId(Uuid::now_v7());
    let first = session.forms().request(pending_form("first", &run_id));
    let _second = session.forms().request(pending_form("second", &run_id));
    drop(first);
    let response = submit(
        state.clone(),
        "alice",
        1,
        &SubmitFormRequest {
            request_id: Some(RequestId::now()),
            session_id: session_id.clone(),
            form_id: "first".into(),
            submission: FormSubmission::Rejected,
        },
    )
    .await;
    let response: SubmitFormResponse = serde_json::from_value(response.result.unwrap()).unwrap();
    assert!(!response.resolved);
    assert_eq!(response.status, FormResolutionStatus::Abandoned);
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert_eq!(snapshot.projection.interactions.forms.len(), 1);
    assert_eq!(snapshot.projection.interactions.forms[0].id, "second");
    let updates = state
        .session_updates(&session_id, "alice", before.cursor, None)
        .await
        .unwrap();
    let form_changes = updates
        .events
        .iter()
        .flat_map(|event| match &event.event {
            atman_proto::ServerEvent::ProjectionDelta { delta } => delta.changes.as_slice(),
            _ => &[],
        })
        .filter_map(|change| match change {
            atman_proto::ProjectionChange::InteractionUpsert {
                interaction: atman_proto::InteractionItem::Form { form },
            } => Some(format!("upsert:{}", form.id)),
            atman_proto::ProjectionChange::InteractionRemove {
                target: atman_proto::InteractionTarget::Form { form_id },
            } => Some(format!("remove:{form_id}")),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        form_changes,
        ["upsert:first", "upsert:second", "remove:first"]
    );
}

#[test]
fn runtime_and_rpc_form_submissions_share_one_wire_contract() {
    use atman_runtime::form::{FormAnswer as RuntimeAnswer, FormSubmission as RuntimeSubmission};
    for submission in [
        RuntimeSubmission::Rejected,
        RuntimeSubmission::Submitted {
            answers: vec![
                RuntimeAnswer::Confirmed { value: true },
                RuntimeAnswer::Selected {
                    index: 0,
                    label: "first".into(),
                },
                RuntimeAnswer::MultiSelected {
                    indices: vec![0],
                    labels: vec!["first".into()],
                },
                RuntimeAnswer::TextEntered {
                    text: "answer".into(),
                },
                RuntimeAnswer::Cancelled,
            ],
        },
    ] {
        let runtime_json = serde_json::to_value(&submission).unwrap();
        let rpc: FormSubmission = serde_json::from_value(runtime_json.clone()).unwrap();
        let rpc_json = serde_json::to_value(rpc).unwrap();
        assert_eq!(runtime_json, rpc_json);
        assert_eq!(
            serde_json::from_value::<RuntimeSubmission>(rpc_json).unwrap(),
            submission,
        );
    }
}
