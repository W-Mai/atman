use std::collections::BTreeSet;
use std::sync::Arc;

use atman_daemon::{DaemonState, LiveRun, dispatch_as};
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

async fn register_session(
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
    state
        .register_session_run(
            session_id.clone(),
            session.clone(),
            LiveRun {
                turn_id: atman_runtime::event::TurnId::now(),
                run_id: atman_proto::FlowRunId(Uuid::now_v7()),
                flow_name: "permission-test".into(),
                cancel: CancellationToken::new(),
                started_at: Utc::now(),
            },
            owner,
        )
        .await
        .unwrap();
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
        call_intent: None,
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
async fn transport_pairs_share_approval_decisions_and_converge() {
    use atman_client::{
        Client, ClientError, ClientIdentity, HttpTransport, SessionClientError, UnixTransport,
    };
    use atman_daemon::{
        http::{HttpState, router},
        unix::UnixServer,
    };
    use atman_proto::{
        ApprovalState, PermissionRpcAction, PermissionRpcScope, PermissionRpcSelector,
    };
    use std::time::Duration;

    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (session_id, session, _events) = register_session(&state, "local-daemon").await;
    let socket = tmp.path().join("atman.sock");
    let unix = UnixServer::bind(&socket).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let http = router(Arc::new(HttpState {
        daemon: state.clone(),
        auth_token: "test-token".into(),
    }));
    let shutdown = CancellationToken::new();
    let unix_task = tokio::spawn(unix.serve(state.clone(), shutdown.clone()));
    let http_task = tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            axum::serve(listener, http)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        }
    });
    let mut clients = Vec::new();
    for name in ["unix-a", "unix-b"] {
        clients.push(
            Client::connect(UnixTransport::new(&socket), ClientIdentity::new(name, "1"))
                .await
                .unwrap(),
        );
    }
    for name in ["http-a", "http-b"] {
        clients.push(
            Client::connect(
                HttpTransport::new(&base, "test-token").unwrap(),
                ClientIdentity::new(name, "1"),
            )
            .await
            .unwrap(),
        );
    }

    for (left, right) in [(0, 1), (0, 2), (2, 3)] {
        let pending = pending_request(&session, &format!("pair-{left}-{right}"));
        let request_id = pending.request.request_id.clone();
        let revision = pending.request.revision;
        let a = clients[left]
            .attach_session(session_id.clone())
            .await
            .unwrap();
        let b = clients[right]
            .attach_session(session_id.clone())
            .await
            .unwrap();
        assert_eq!(a.current().projection(), b.current().projection());
        assert!(
            a.current()
                .projection()
                .interactions
                .approvals
                .iter()
                .any(|item| item.id == request_id.0 && item.state == ApprovalState::Pending)
        );

        let mut watchers = [a.subscribe(), b.subscribe()];
        let mut workers = tokio::task::JoinSet::new();
        for client in [a.clone(), b.clone()] {
            workers.spawn(async move { client.synchronize().await });
        }
        let selector = PermissionRpcSelector::Requests {
            request_ids: vec![request_id.0],
            expected_request_revisions: [(request_id.0, revision)].into(),
        };
        let (first, second) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(
                a.resolve_permissions(
                    selector.clone(),
                    PermissionRpcAction::Approve,
                    Some(PermissionRpcScope::CurrentCall),
                    None
                ),
                b.resolve_permissions(selector, PermissionRpcAction::Deny, None, None)
            )
        })
        .await
        .expect("competing permission commands timed out");
        assert_eq!(
            usize::from(first.is_ok()) + usize::from(second.is_ok()),
            1,
            "pair {left}/{right}: {first:?}, {second:?}"
        );
        let expected_state = if first.is_ok() {
            ApprovalState::Approved
        } else {
            ApprovalState::Denied
        };
        let error = first.err().or_else(|| second.err()).unwrap();
        assert!(
            matches!(error, SessionClientError::Client(ClientError::Rpc(ref rpc)) if rpc.message.contains("stale")),
            "{error:?}"
        );
        assert_eq!(
            session
                .permission_broker()
                .get(&request_id)
                .unwrap()
                .revision,
            revision + 1
        );
        assert_eq!(
            session
                .sink()
                .snapshot()
                .iter()
                .filter(|event| matches!(event,
                    atman_runtime::event::Event::PermissionRequestApproved { payload }
                    | atman_runtime::event::Event::PermissionRequestDenied { payload }
                    if payload.request_id.as_ref() == Some(&request_id)
                ))
                .count(),
            1
        );

        let expected = state
            .session_snapshot(&session_id, "local-daemon")
            .await
            .unwrap();
        for watcher in &mut watchers {
            let current = tokio::time::timeout(
                Duration::from_secs(10),
                watcher.wait_for(|state| state.cursor() >= expected.cursor),
            )
            .await
            .expect("approval did not synchronize")
            .unwrap();
            assert_eq!(current.projection(), &expected.projection);
            assert!(
                current
                    .projection()
                    .interactions
                    .approvals
                    .iter()
                    .any(|item| item.id == request_id.0 && item.state == expected_state)
            );
        }
        workers.abort_all();
        while let Some(result) = workers.join_next().await {
            assert!(result.unwrap_err().is_cancelled());
        }
        let reconnected = clients[right]
            .attach_session(session_id.clone())
            .await
            .unwrap();
        assert_eq!(reconnected.current().projection(), &expected.projection);
        assert_eq!(reconnected.current().cursor(), expected.cursor);
    }

    let invalid = Client::connect(
        HttpTransport::new(&base, "wrong-token").unwrap(),
        ClientIdentity::new("unix-a", "1"),
    )
    .await;
    assert!(matches!(
        invalid,
        Err(ClientError::Transport(
            atman_client::TransportError::HttpStatus { status: 401, .. }
        ))
    ));
    let (foreign_id, _, _) = register_session(&state, "another-operator").await;
    for client in [&clients[0], &clients[2]] {
        assert!(client.attach_session(foreign_id.clone()).await.is_err());
    }

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(10), unix_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), http_task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn permission_rpc_real_pending_requests_support_groups_revisions_and_once() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let (session_id, session, mut events) = register_session(&state, "alice").await;
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
    assert_eq!(listed.session_id, session_id);
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

    let create_request_id = atman_proto::RequestId::now();
    let create_group = |id| {
        JsonRpcRequest::new(
            id,
            methods::CREATE_PERMISSION_GROUP,
            serde_json::json!({
                "request_id": create_request_id,
                "session_id": session_id,
                "request_ids": [group_pending.request.request_id.0],
                "expected_request_revisions": {
                    group_pending.request.request_id.0.to_string(): group_pending.request.revision
                },
                "label": "grouped shell"
            }),
        )
    };
    let group = dispatch_as(state.clone(), create_group(2), "alice").await;
    assert!(group.error.is_none(), "group creation failed: {group:?}");
    let group = group.result.unwrap();
    assert_eq!(group["session_id"], serde_json::json!(session_id));
    let group_id: Uuid = serde_json::from_value(group["group_id"].clone()).unwrap();
    let group_revision: u64 = serde_json::from_value(group["revision"].clone()).unwrap();
    let group_cursor: atman_proto::EventCursor =
        serde_json::from_value(group["cursor"].clone()).unwrap();
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert_eq!(snapshot.cursor, group_cursor);
    assert_eq!(
        snapshot.projection.revision,
        serde_json::from_value(group["session_revision"].clone()).unwrap()
    );
    let group_retry = dispatch_as(state.clone(), create_group(20), "alice").await;
    assert_eq!(group_retry.result.unwrap(), group);

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
    let resolved_group: atman_proto::ResolvePermissionRequestsResponse =
        serde_json::from_value(resolved_group.result.unwrap()).unwrap();
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert_eq!(resolved_group.session_id, session_id);
    assert_eq!(resolved_group.cursor, snapshot.cursor);
    assert_eq!(resolved_group.revision, snapshot.projection.revision);

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

    let resolve_request_id = atman_proto::RequestId::now();
    let request_json = |id| {
        JsonRpcRequest::new(
            id,
            methods::RESOLVE_PERMISSION_REQUESTS,
            serde_json::json!({
                "request_id": resolve_request_id,
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
    assert!(first.error.is_none(), "first retry failed: {first:?}");
    assert!(second.error.is_none(), "second retry failed: {second:?}");
    assert_eq!(first.result, second.result);
    let resolved: atman_proto::ResolvePermissionRequestsResponse =
        serde_json::from_value(first.result.unwrap()).unwrap();
    assert_eq!(resolved.session_id, session_id);
    let snapshot = state.session_snapshot(&session_id, "alice").await.unwrap();
    assert_eq!(resolved.cursor, snapshot.cursor);
    assert_eq!(resolved.revision, snapshot.projection.revision);

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
    let (owned, _, _) = register_session(&state, "alice").await;
    let (other, _, _) = register_session(&state, "bob").await;

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
    let orphan_run = atman_proto::FlowRunId(Uuid::now_v7());
    state
        .register_session_run(
            orphan_id.clone(),
            orphan,
            LiveRun {
                turn_id: atman_runtime::event::TurnId::now(),
                run_id: orphan_run.clone(),
                flow_name: "finished".into(),
                cancel: CancellationToken::new(),
                started_at: Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    state.finish_run(&orphan_id, &orphan_run);
    while state.has_live_runs(&orphan_id) {
        tokio::task::yield_now().await;
    }
    let response = dispatch_as(state, list(&orphan_id), "alice").await;
    assert!(
        response.error.is_some(),
        "a broker without a live-session entry must fail closed"
    );
}
