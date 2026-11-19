use std::{collections::BTreeSet, sync::Arc, time::Duration};

use atman_daemon::{DaemonState, SharedHostServices, dispatch, dispatch_as};
use atman_proto::{JsonRpcRequest, approvals::FlowApprovalQueueResponse, methods};
use atman_runtime::{
    event::AtmanUuid,
    flow_authority::EffectiveAuthority,
    flow_runtime::{
        FlowWorker,
        host::{HostServices, RuntimeHost, SessionHandle},
    },
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, SessionConfiguration,
        SessionDescriptor,
    },
    message::ToolCallIntent,
    permission::{
        ApprovalAuthority, ExecutionBoundary, PendingPermission, PermissionClients,
        PermissionIntent, PermissionResolution, ResourceProvenance, SubmissionOutcome,
    },
    redact::{RedactMode, Redactor},
    source_program::RevisionBundle,
    tool::Tier,
    trust::{RiskKind, TrustConfig, TrustMode},
};
use serde_json::{Value, json};
use uuid::Uuid;

const SECRET: &str = "fixture-private-secret-sequence";

async fn create_flow(session: &SessionHandle, session_id: Uuid) -> FlowWorker {
    session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: session_id.into(),
                instance_id: FlowInstanceId::now(),
            },
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() { }".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .await
        .unwrap()
}

fn pending(worker: &FlowWorker, call: &str, preview: String) -> PendingPermission {
    let context = worker.context();
    let policy = TrustConfig {
        mode: TrustMode::Steady,
        ..TrustConfig::default()
    };
    let requester = context
        .flow_registry
        .register_instance(
            worker.address().clone(),
            atman_rt::RunId::<AtmanUuid>::now(),
            EffectiveAuthority::root(&policy, true, None),
        )
        .unwrap();
    let broker = context.permission_broker();
    let SubmissionOutcome::Pending(request) = broker
        .submit_to(
            Some(&requester.session_id),
            Some(&requester.run_id),
            PermissionIntent {
                tool_use_id: call.into(),
                tool_name: "bash.spawn".into(),
                call_intent: ToolCallIntent::new("Inspect the fixture"),
                tier: Tier::Two,
                risks: BTreeSet::from([RiskKind::ProcessSpawn]),
                args_digest: "internal-argument-digest".into(),
                preview: Some(preview),
                provenance: ResourceProvenance::none().with_risk(RiskKind::ProcessSpawn),
            },
            true,
            ApprovalAuthority::User(broker.user_authority(&requester.session_id, None)),
            &policy,
        )
        .unwrap()
    else {
        panic!("fixture must use the actual pending User path");
    };
    *request
}

fn address(worker: &FlowWorker) -> Value {
    json!({
        "session_id": worker.address().session_id.0,
        "instance_id": worker.address().instance_id.0.0,
    })
}

fn query(flow: Value) -> JsonRpcRequest {
    JsonRpcRequest::new(1, methods::FLOW_APPROVAL_QUEUE, json!({"flow": flow}))
}

async fn ready(state: &Arc<DaemonState>, worker: &FlowWorker) -> Value {
    let result = dispatch(state.clone(), query(address(worker))).await;
    assert!(result.error.is_none(), "{:?}", result.error);
    let value = result.result.unwrap();
    let typed: FlowApprovalQueueResponse = serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(typed, FlowApprovalQueueResponse::Ready { .. }));
    value
}

#[tokio::test]
async fn cold_approval_read_never_opens_a_flow_or_creates_data() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("unopened");
    let state = Arc::new(DaemonState::new(data.clone()));
    let flow = json!({"session_id": Uuid::now_v7(), "instance_id": Uuid::now_v7()});
    let reply = dispatch(state, query(flow.clone())).await;
    assert!(reply.error.is_none());
    assert_eq!(
        reply.result.unwrap(),
        json!({"state": "unavailable", "flow": flow})
    );
    assert!(!data.exists());
}

#[tokio::test]
async fn user_queue_observes_real_flow_brokers_and_native_decision_boundary() {
    let directory = tempfile::tempdir().unwrap();
    let permissions = Arc::new(PermissionClients::default());
    let host = RuntimeHost::with_services(HostServices {
        redactor: Some(Arc::new(Redactor::from_pairs(
            &[("fixture", SECRET)],
            RedactMode::Full,
        ))),
        permission_clients: Arc::downgrade(&permissions),
        ..HostServices::default()
    });
    let state = Arc::new(DaemonState::with_shared_host(
        directory.path().to_path_buf(),
        host.generation().to_string(),
        SharedHostServices {
            host: host.clone(),
            permissions,
            operator: Uuid::now_v7(),
            prompts: None,
        },
    ));
    let session_id = Uuid::now_v7();
    let session = host
        .create_session(
            directory.path(),
            SessionDescriptor {
                id: session_id,
                title: "Approval fixture".into(),
                project: None,
                configuration: SessionConfiguration::HostProvided,
            },
        )
        .await
        .unwrap();
    let first = create_flow(&session, session_id).await;
    let second = create_flow(&session, session_id).await;
    let mut first_request = pending(&first, "first-call", format!("Inspect {SECRET}"));
    let second_request = pending(&second, "second-call", SECRET.repeat(400));
    let broker = first.context().permission_broker();
    let id = first_request.request.request_id.clone();
    let group = broker
        .user_create_group(
            &session_id.to_string(),
            BTreeSet::from([id.clone()]),
            format!("Grouped {SECRET}"),
            &[(id.clone(), first_request.request.revision)].into(),
        )
        .unwrap();

    let first_snapshot = ready(&state, &first).await;
    let second_snapshot = ready(&state, &second).await;
    assert_eq!(first_snapshot["flow"], address(&first));
    assert_eq!(first_snapshot["requests"].as_array().unwrap().len(), 1);
    let item = &first_snapshot["requests"][0];
    assert_eq!(item["request_id"], json!(id.0));
    assert_eq!(item["revision"], first_request.request.revision);
    assert_eq!(item["tool_use_id"], "first-call");
    assert_eq!(item["detail"]["preview"], "Inspect <REDACTED:fixture>");
    assert_eq!(item["detail"]["execution_boundary"], "direct");
    assert_eq!(item["detail"]["risks"], json!(["process_spawn"]));
    assert_eq!(
        first_snapshot["groups"][0]["group_id"],
        json!(group.group_id.0)
    );
    assert_eq!(first_snapshot["groups"][0]["request_ids"], json!([id.0]));
    assert!(!first_snapshot.to_string().contains(SECRET));
    assert!(
        !first_snapshot
            .to_string()
            .contains("internal-argument-digest")
    );
    assert_eq!(
        second_snapshot["requests"][0]["request_id"],
        json!(second_request.request.request_id.0)
    );
    assert_eq!(second_snapshot["requests"][0]["detail"]["incomplete"], true);
    assert!(second_snapshot["requests"][0]["detail"]["preview"].is_null());
    assert_eq!(second_snapshot["groups"], json!([]));
    assert!(matches!(
        first_request.resolution.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    let read_unauthorized =
        dispatch_as(state.clone(), query(address(&first)), "another-principal").await;
    assert!(read_unauthorized.error.is_some());
    let mut crossed = address(&first);
    crossed["session_id"] = json!(Uuid::now_v7());
    let missing = dispatch(state.clone(), query(crossed.clone())).await;
    assert_eq!(
        missing.result.unwrap(),
        json!({"state": "unavailable", "flow": crossed})
    );

    let decide = |revision| {
        JsonRpcRequest::new(
            2,
            methods::RESOLVE_PERMISSION_REQUESTS,
            json!({
                "session_id": session_id,
                "instance_id": first.address().instance_id.0.0,
                "selector": {"group_id": group.group_id.0, "expected_group_revision": revision},
                "action": "approve",
                "scope": "current_call",
            }),
        )
    };
    assert!(
        dispatch(state.clone(), decide(group.revision + 1))
            .await
            .error
            .is_some()
    );
    let (left, right) = tokio::join!(
        dispatch(state.clone(), decide(group.revision)),
        dispatch(state.clone(), decide(group.revision)),
    );
    assert_ne!(left.error.is_none(), right.error.is_none());
    let acknowledgement = left.result.or(right.result).unwrap();
    let resolved = tokio::time::timeout(Duration::from_secs(3), first_request.resolution)
        .await
        .expect("the original requester should receive the decision")
        .unwrap();
    let PermissionResolution::Decision(decision) = resolved else {
        panic!("broker did not authorize the original call");
    };
    assert_eq!(decision.execution_boundary, ExecutionBoundary::Direct);
    assert_eq!(acknowledgement["resolutions"][0]["request_id"], json!(id.0));
    assert_eq!(
        acknowledgement["resolutions"][0]["decision"],
        json!({"decision_id": decision.decision_id.0, "action": "approved"})
    );
    assert_eq!(ready(&state, &first).await["requests"], json!([]));
    assert_eq!(ready(&state, &first).await["groups"], json!([]));
    assert_eq!(
        ready(&state, &second).await["requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let second_id = second_request.request.request_id.0;
    let denial = JsonRpcRequest::new(
        3,
        methods::RESOLVE_PERMISSION_REQUESTS,
        json!({
            "session_id": session_id,
            "instance_id": second.address().instance_id.0.0,
            "selector": {
                "request_ids": [second_id],
                "expected_request_revisions": { (second_id.to_string()): second_request.request.revision },
            },
            "action": "deny",
        }),
    );
    let denied = dispatch(state.clone(), denial).await.result.unwrap();
    let PermissionResolution::Decision(decision) = second_request.resolution.await.unwrap() else {
        panic!("broker did not deny the original call");
    };
    assert_eq!(denied["resolutions"][0]["request_id"], json!(second_id));
    assert_eq!(
        denied["resolutions"][0]["decision"],
        json!({"decision_id": decision.decision_id.0, "action": "denied"})
    );
    let excess = (0..65)
        .map(|index| pending(&second, &format!("excess-{index}"), "Inspect".into()))
        .collect::<Vec<_>>();
    let full = dispatch(state.clone(), query(address(&second))).await;
    assert_eq!(
        full.result.unwrap(),
        json!({"state": "capacity", "flow": address(&second)})
    );
    assert_eq!(excess.len(), 65);
    host.shutdown().await.unwrap();
}
