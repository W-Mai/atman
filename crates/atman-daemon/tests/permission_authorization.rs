use std::collections::BTreeSet;
use std::sync::Arc;

use atman_daemon::{DaemonState, LiveSession, dispatch_as};
use atman_proto::{JsonRpcRequest, SessionId, methods};
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::permission::{
    ApprovalAuthority, PermissionIntent, ResourceProvenance, SubmissionOutcome,
};
use atman_runtime::stream::StreamFrame;
use atman_runtime::tool::Tier;
use chrono::Utc;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn register_session(
    state: &DaemonState,
    owner: &str,
) -> (
    SessionId,
    Arc<atman_runtime::Session>,
    broadcast::Receiver<StreamFrame>,
) {
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let stream = session.stream_tx().subscribe();
    let session_id = SessionId(session.id().0);
    state.register_broker(session_id.clone(), session.clone(), owner);
    state.register_live(
        session_id.clone(),
        LiveSession {
            run_id: atman_proto::FlowRunId(Uuid::now_v7()),
            flow_name: "permission-test".into(),
            cancel: CancellationToken::new(),
            started_at: Utc::now(),
        },
    );
    (session_id, session, stream)
}

fn pending_request(
    session: &atman_runtime::Session,
    tool_use_id: &str,
) -> atman_runtime::permission::PendingPermission {
    let requester = session
        .flow_registry
        .register_root(
            session.id().0.to_string(),
            atman_runtime::FlowRunId::now(),
            EffectiveAuthority::root(&session.trust_config(), true, None),
        )
        .unwrap();
    let intent = PermissionIntent {
        tool_use_id: tool_use_id.into(),
        tool_name: "bash.spawn".into(),
        tier: Tier::Two,
        risks: BTreeSet::new(),
        args_digest: format!("sha256:{tool_use_id}"),
        preview: None,
        provenance: ResourceProvenance::none(),
    };
    let target = ApprovalAuthority::User(
        session
            .permission_broker()
            .user_authority(&requester.session_id, None),
    );
    let SubmissionOutcome::Pending(pending) = session
        .permission_broker()
        .submit_to(
            Some(&requester.session_id),
            Some(&requester.run_id),
            intent,
            false,
            target,
            &session.trust_config(),
        )
        .unwrap()
    else {
        panic!("expected a pending permission request");
    };
    *pending
}

#[tokio::test]
async fn permission_rpc_real_pending_requests_support_groups_revisions_and_once() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (session_id, session, mut events) = register_session(&state, "alice");
    let group_pending = pending_request(&session, "group-call");
    let request_pending = pending_request(&session, "request-call");

    let list = JsonRpcRequest::new(
        1,
        methods::LIST_PERMISSION_REQUESTS,
        serde_json::json!({"session_id": session_id}),
    );
    let listed = dispatch_as(state.clone(), list, "alice").await;
    assert!(listed.error.is_none(), "owner list failed: {listed:?}");
    let listed: atman_proto::ListPermissionRequestsResponse =
        serde_json::from_value(listed.result.unwrap()).unwrap();
    assert_eq!(listed.requests.len(), 2);
    assert!(
        listed
            .requests
            .iter()
            .any(|request| request.request_id == group_pending.request.request_id.0)
    );
    assert!(
        listed
            .requests
            .iter()
            .any(|request| request.request_id == request_pending.request.request_id.0)
    );

    let group = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(
            2,
            methods::CREATE_PERMISSION_GROUP,
            serde_json::json!({
                "session_id": session_id,
                "request_ids": [group_pending.request.request_id.0],
                "expected_request_revisions": {
                    group_pending.request.request_id.0.to_string(): group_pending.request.revision
                },
                "label": "grouped shell"
            }),
        ),
        "alice",
    )
    .await;
    assert!(group.error.is_none(), "group creation failed: {group:?}");
    let group = group.result.unwrap();
    let group_id: Uuid = serde_json::from_value(group["group_id"].clone()).unwrap();
    let group_revision: u64 = serde_json::from_value(group["revision"].clone()).unwrap();

    let stale_group = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(
            3,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "session_id": session_id,
                "selector": {"group_id": group_id, "expected_group_revision": group_revision + 1},
                "action": "approve"
            }),
        ),
        "alice",
    )
    .await;
    assert!(
        stale_group.error.is_some(),
        "stale group revision must fail"
    );

    let resolved_group = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(
            4,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "session_id": session_id,
                "selector": {"group_id": group_id, "expected_group_revision": group_revision},
                "action": "approve"
            }),
        ),
        "alice",
    )
    .await;
    assert!(
        resolved_group.error.is_none(),
        "group resolve failed: {resolved_group:?}"
    );

    let stale_request = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(
            5,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "session_id": session_id,
                "selector": {"request_ids": [request_pending.request.request_id.0], "expected_request_revisions": {request_pending.request.request_id.0.to_string(): request_pending.request.revision + 1}},
                "action": "deny"
            }),
        ),
        "alice",
    )
    .await;
    assert!(
        stale_request.error.is_some(),
        "stale request revision must fail"
    );

    let request_json = |id| {
        JsonRpcRequest::new(
            id,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "session_id": session_id,
                "selector": {"request_ids": [request_pending.request.request_id.0], "expected_request_revisions": {request_pending.request.request_id.0.to_string(): request_pending.request.revision}},
                "action": "deny"
            }),
        )
    };
    let (first, second) = tokio::join!(
        dispatch_as(state.clone(), request_json(6), "alice"),
        dispatch_as(state.clone(), request_json(7), "alice")
    );
    assert_eq!(
        [first.error.is_none(), second.error.is_none()]
            .into_iter()
            .filter(|succeeded| *succeeded)
            .count(),
        1,
        "a request must resolve exactly once"
    );

    let mut saw_created = false;
    let mut saw_resolved = false;
    while let Ok(frame) = events.try_recv() {
        saw_created |= matches!(frame, StreamFrame::PermissionGroupCreated { .. });
        saw_resolved |= matches!(frame, StreamFrame::PermissionGroupResolved { .. });
    }
    assert!(
        saw_created,
        "group creation must be projected to the stream"
    );
    assert!(
        saw_resolved,
        "group resolution must be projected to the stream"
    );
}

#[tokio::test]
async fn permission_rpcs_fail_closed_for_wrong_principal_and_cross_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (owned, _, _) = register_session(&state, "alice");
    let (other, _, _) = register_session(&state, "bob");

    let list = |session_id: &SessionId| {
        JsonRpcRequest::new(
            1,
            methods::LIST_PERMISSION_REQUESTS,
            serde_json::json!({"session_id": session_id}),
        )
    };
    let create = |session_id: &SessionId| {
        JsonRpcRequest::new(
            2,
            methods::CREATE_PERMISSION_GROUP,
            serde_json::json!({
                "session_id": session_id,
                "request_ids": [],
                "expected_request_revisions": {},
                "label": "cross-session"
            }),
        )
    };
    let resolve = |session_id: &SessionId| {
        JsonRpcRequest::new(
            3,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "session_id": session_id,
                "request_ids": [],
                "expected_request_revisions": {},
                "action": "deny"
            }),
        )
    };

    let allowed = dispatch_as(state.clone(), list(&owned), "alice").await;
    assert!(
        allowed.error.is_none(),
        "owner should be allowed: {allowed:?}"
    );

    for request in [list(&owned), create(&owned), resolve(&owned)] {
        let response = dispatch_as(state.clone(), request, "mallory").await;
        assert!(response.error.is_some(), "wrong principal must fail closed");
    }
    for request in [list(&other), create(&other), resolve(&other)] {
        let response = dispatch_as(state.clone(), request, "alice").await;
        assert!(
            response.error.is_some(),
            "cross-session access must fail closed"
        );
    }

    let orphan = Arc::new(atman_runtime::Session::open_ephemeral());
    let orphan_id = SessionId(orphan.id().0);
    state.register_broker(orphan_id.clone(), orphan, "alice");
    state.deregister_live(&orphan_id);
    let response = dispatch_as(state, list(&orphan_id), "alice").await;
    assert!(
        response.error.is_some(),
        "a broker without a live-session entry must fail closed"
    );
}
