use std::{collections::BTreeMap, sync::Arc};

use atman_daemon::{DaemonState, dispatch, dispatch_as, run::RunLauncher};
use atman_proto::{JsonRpcError, JsonRpcRequest, methods};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef, FlowTransition, RuntimeStore, SessionDescriptor,
};
use atman_runtime::source_program::RevisionBundle;
use serde_json::{Value, json};
use uuid::Uuid;

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{:?}", response.error);
    response.result.unwrap()
}

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(),
                edges: BTreeMap::new(),
                sources: BTreeMap::from([(
                    "entry:main.at".into(),
                    "flow main() -> string { return \"saved\" }".into(),
                )]),
            },
        }
        .into(),
    }
}

async fn control_http(
    state: &Arc<DaemonState>,
    token: Option<&str>,
    method: &str,
    params: Value,
) -> (axum::http::StatusCode, Value) {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let app = atman_daemon::http::router(Arc::new(atman_daemon::http::HttpState {
        daemon: state.clone(),
        auth_token: "batch-test".into(),
    }));
    let mut request = Request::builder()
        .method("POST")
        .uri("/rpc")
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(
            request
                .body(Body::from(
                    serde_json::to_vec(&JsonRpcRequest::new(1, method, params)).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[tokio::test]
async fn batch_control_rpc_rejects_untrusted_or_invalid_requests_before_opening_storage() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_owned()));
    let command = json!({"request_id": Uuid::new_v4(), "flow": {"session_id": Uuid::new_v4(), "instance_id": Uuid::new_v4()}, "expected_revision": 1, "action": "pause"});
    let valid = json!({"request_id": Uuid::new_v4(), "commands": [command.clone()]});
    for method in [methods::FLOW_CONTROL_BATCH, methods::FLOW_CONTROL_STATUS] {
        for token in [None, Some("wrong")] {
            assert_eq!(
                control_http(&state, token, method, valid.clone()).await.0,
                axum::http::StatusCode::UNAUTHORIZED
            );
        }
        assert!(
            dispatch_as(
                state.clone(),
                JsonRpcRequest::new(1, method, valid.clone()),
                "untrusted"
            )
            .await
            .error
            .is_some()
        );
    }
    let mut forged = valid.clone();
    forged["sender"] = json!({"user": Uuid::new_v4()});
    let mut nested = valid.clone();
    nested["commands"][0]["sender"] = json!({"user": Uuid::new_v4()});
    let mut nil_target = valid.clone();
    nil_target["commands"][0]["flow"]["instance_id"] = json!(Uuid::nil());
    let mut nil_control = valid.clone();
    nil_control["commands"][0]["request_id"] = json!(Uuid::nil());
    for params in [
        forged,
        nested,
        nil_target,
        nil_control,
        json!({"request_id": Uuid::nil(), "commands": [command.clone()]}),
        json!({"request_id": Uuid::new_v4(), "commands": []}),
        json!({"request_id": Uuid::new_v4(), "commands": [command.clone(), command]}),
    ] {
        let (status, response) = control_http(
            &state,
            Some("batch-test"),
            methods::FLOW_CONTROL_BATCH,
            params,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
    for params in [
        json!({"request_id": Uuid::nil()}),
        json!({"request_id": Uuid::new_v4(), "sender": "another user"}),
    ] {
        let (_, response) = control_http(
            &state,
            Some("batch-test"),
            methods::FLOW_CONTROL_STATUS,
            params,
        )
        .await;
        assert_eq!(response["error"]["code"], -32602);
    }
    assert!(!dir.path().join("runtime-v1").exists());
}

#[tokio::test]
async fn batch_control_rpc_preserves_partial_results_and_retry_receipts_across_restart() {
    use atman_runtime::flow_store::profile::{ControlBatchRecord, ControlProgress};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut commands = Vec::new();
    let mut addresses = Vec::new();
    let mut journals = Vec::new();
    for action in ["pause", "close"] {
        let id = Uuid::new_v4();
        let mut catalog = root
            .create_session(SessionDescriptor {
                id,
                project: None,
                configuration: Default::default(),
                title: action.into(),
            })
            .unwrap();
        let descriptor = descriptor(id);
        let mut flow = catalog.create_flow(descriptor.clone()).unwrap();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
        commands.push(json!({"request_id": Uuid::new_v4(), "flow": descriptor.address, "expected_revision": flow.state().revision(), "action": action}));
        addresses.push(descriptor.address);
        journals.push(flow.dir().join("events.jsonl"));
    }
    commands.push(json!({"request_id": Uuid::new_v4(), "flow": descriptor(Uuid::new_v4()).address, "expected_revision": 0, "action": "pause"}));
    let id = Uuid::new_v4();
    let params = json!({"request_id": id, "commands": commands});
    let state = Arc::new(DaemonState::new(dir.path().to_owned()));
    let (first, retry) = tokio::join!(
        control_http(
            &state,
            Some("batch-test"),
            methods::FLOW_CONTROL_BATCH,
            params.clone()
        ),
        control_http(
            &state,
            Some("batch-test"),
            methods::FLOW_CONTROL_BATCH,
            params.clone()
        ),
    );
    for response in [&first, &retry] {
        assert_eq!(response.0, axum::http::StatusCode::OK);
        assert!(response.1["error"].is_null(), "{}", response.1);
    }
    assert_eq!(
        first.1["result"]["submitted"],
        retry.1["result"]["submitted"]
    );
    let finished = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let value = rpc(
                &state,
                methods::FLOW_CONTROL_STATUS,
                json!({"request_id": id}),
            )
            .await;
            let record: ControlBatchRecord = serde_json::from_value(value).unwrap();
            if record.items.iter().all(|item| item.is_terminal()) {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.items.len(), 3);
    for item in &finished.items[..2] {
        assert!(matches!(
            item.history[0].progress,
            ControlProgress::Accepted { .. }
        ));
        assert!(matches!(
            item.history[1].progress,
            ControlProgress::Applied { .. }
        ));
    }
    assert!(
        matches!(&finished.items[2].history[0].progress, ControlProgress::Failed { reason } if !reason.is_empty())
    );
    let paused = rpc(&state, methods::FLOW_GET, json!({"flow": addresses[0]})).await;
    assert_eq!(paused["execution"]["pause_count"], 1);
    // Single-target resume must release the exact key held by the batch pause.
    rpc(&state, methods::FLOW_CONTROL, json!({"flow": addresses[0], "request_id": Uuid::new_v4(), "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
    let resumed = rpc(&state, methods::FLOW_GET, json!({"flow": addresses[0]})).await;
    assert_eq!(resumed["execution"]["pause_count"], 0);
    let reconciled_id = Uuid::new_v4();
    rpc(
        &state,
        methods::FLOW_CONTROL_BATCH,
        json!({"request_id": reconciled_id, "commands": [commands[0].clone()]}),
    )
    .await;
    let reconciled = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let record: ControlBatchRecord = serde_json::from_value(
                rpc(
                    &state,
                    methods::FLOW_CONTROL_STATUS,
                    json!({"request_id": reconciled_id}),
                )
                .await,
            )
            .unwrap();
            if record.items[0].is_terminal() {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        reconciled.items[0].history[0].progress,
        finished.items[0].history[0].progress
    );
    assert!(matches!(
        reconciled.items[0].history[1].progress,
        ControlProgress::Applied { .. }
    ));
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": addresses[0]})).await,
        resumed
    );
    let mut conflict = params.clone();
    conflict["commands"][0]["action"] = json!("close");
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_CONTROL_BATCH, conflict)
        )
        .await
        .error
        .is_some()
    );
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(
                1,
                methods::FLOW_CONTROL_STATUS,
                json!({"request_id": Uuid::new_v4()})
            )
        )
        .await
        .error
        .is_some()
    );
    state.shutdown().await.unwrap();
    journals.push(root.root().join("host/events.jsonl"));
    let original = journals
        .iter()
        .map(std::fs::read)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_owned()));
    let queried = rpc(
        &state,
        methods::FLOW_CONTROL_STATUS,
        json!({"request_id": id}),
    )
    .await;
    assert_eq!(queried, serde_json::to_value(&finished).unwrap());
    assert_eq!(
        rpc(&state, methods::FLOW_CONTROL_BATCH, params).await,
        queried
    );
    // Settled batch queries and retries must not acquire target owners or replay the old pause.
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_GET, json!({"flow": addresses[0]}))
        )
        .await
        .error
        .is_some()
    );
    state.shutdown().await.unwrap();
    assert_eq!(
        journals
            .iter()
            .map(std::fs::read)
            .collect::<Result<Vec<_>, _>>()
            .unwrap(),
        original
    );
}

#[tokio::test]
async fn session_open_reuses_the_owner_loaded_by_control_recovery() {
    use atman_runtime::flow_store::{
        control::{ControlAction, ControlRequest},
        inbox::SenderIdentity,
        profile::{ControlBatchRequest, ProfileStore},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut catalog = root
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Recovered control".into(),
        })
        .unwrap();
    let descriptor = descriptor(id);
    let mut flow = catalog.create_flow(descriptor.clone()).unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    let sender = SenderIdentity::User(Uuid::new_v4());
    ProfileStore::open(&root)
        .unwrap()
        .submit_controls(ControlBatchRequest {
            id: Uuid::new_v4(),
            sender: sender.clone(),
            commands: vec![ControlRequest {
                id: Uuid::new_v4(),
                sender,
                target: descriptor.address.clone(),
                expected_revision: flow.state().revision(),
                pause_key: Uuid::new_v4().into(),
                action: ControlAction::Pause,
            }],
        })
        .unwrap();
    drop((flow, catalog));
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    rpc(&state, methods::MANAGEMENT_GET, json!({})).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let response = dispatch(
                state.clone(),
                JsonRpcRequest::new(1, methods::FLOW_GET, json!({"flow": descriptor.address})),
            )
            .await;
            if response
                .result
                .is_some_and(|value| value["execution"]["pause_count"] == 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let opened = rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    assert_eq!(opened["title"], "Recovered control");
    assert_eq!(opened["flow_count"], 1);
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_open_is_idempotent_and_restores_without_starting_flows() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let view = rpc(&state, methods::MANAGEMENT_GET, json!({})).await;
    assert!(view["profile"]["management"].is_null());
    assert!(RuntimeStore::session_ids(dir.path()).unwrap().is_empty());
    let (first, second) = tokio::join!(
        rpc(&state, methods::MANAGEMENT_OPEN, json!({})),
        rpc(&state, methods::MANAGEMENT_OPEN, json!({}))
    );
    assert_eq!(first, second);
    assert_eq!(
        first["profile"]["profile_id"],
        view["profile"]["profile_id"]
    );
    assert_eq!(first["session"]["flow_count"], 0);
    assert!(first["session"]["default_flow"].is_null());
    assert_eq!(first["profile"]["management"]["grants"], json!([]));
    state.shutdown().await.unwrap();
    let restarted = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    assert_eq!(
        rpc(&restarted, methods::MANAGEMENT_OPEN, json!({})).await,
        first
    );
    assert_eq!(RuntimeStore::session_ids(dir.path()).unwrap().len(), 1);
    restarted.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_selection_preserves_receipts_across_replacement_restart_and_missing_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let target = Uuid::new_v4();
    let mut catalog = root
        .create_session(SessionDescriptor {
            id: target,
            project: None,
            configuration: Default::default(),
            title: "Controller".into(),
        })
        .unwrap();
    let flow = descriptor(target);
    catalog.create_flow(flow.clone()).unwrap();
    drop(catalog);
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let initial = rpc(&state, methods::MANAGEMENT_OPEN, json!({})).await;
    let request = json!({
        "request_id": Uuid::new_v4(), "expected": initial["profile"]["cursor"], "session_id": target,
        "grants": [{"id": Uuid::new_v4(), "instance_id": flow.address.instance_id, "target": {"kind": "all_sessions"}, "actions": ["discover", "watch"]}],
    });
    let selected = rpc(&state, methods::MANAGEMENT_SELECT, request.clone()).await;
    assert_eq!(
        selected["current"]["profile"]["management"]["session"]["id"],
        json!(target)
    );
    assert_eq!(
        selected["current"]["profile"]["management"]["grants"][0]["grantee"],
        json!(flow.address)
    );
    let restored = rpc(
        &state,
        methods::MANAGEMENT_SELECT,
        json!({
            "request_id": Uuid::new_v4(), "expected": selected["receipt"],
            "session_id": initial["profile"]["management"]["session"]["id"], "grants": [],
        }),
    )
    .await;
    let retry = rpc(&state, methods::MANAGEMENT_SELECT, request.clone()).await;
    assert_eq!(retry["receipt"], selected["receipt"]);
    assert_eq!(retry["current"], restored["current"]);
    let mut stale = request.clone();
    stale["request_id"] = json!(Uuid::new_v4());
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::MANAGEMENT_SELECT, stale)
        )
        .await
        .error
        .is_some()
    );
    state.shutdown().await.unwrap();
    // Retrying an acknowledged request must not require reopening its old target.
    std::fs::remove_dir_all(root.root().join("sessions").join(target.to_string())).unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    assert_eq!(
        rpc(&state, methods::MANAGEMENT_SELECT, request.clone()).await,
        retry
    );
    let mut conflict = request;
    conflict["grants"] = json!([]);
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::MANAGEMENT_SELECT, conflict)
        )
        .await
        .error
        .is_some()
    );
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_management_binding_is_reported_and_can_be_replaced_explicitly() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let initial = rpc(&state, methods::MANAGEMENT_OPEN, json!({})).await;
    let old = initial["profile"]["management"]["session"]["id"]
        .as_str()
        .unwrap();
    state.shutdown().await.unwrap();
    std::fs::remove_dir_all(dir.path().join("runtime-v1/sessions").join(old)).unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let view = rpc(&state, methods::MANAGEMENT_GET, json!({})).await;
    assert!(view["unavailable"].is_string());
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::MANAGEMENT_OPEN, json!({}))
        )
        .await
        .error
        .is_some()
    );
    assert!(RuntimeStore::session_ids(dir.path()).unwrap().is_empty());
    let target = Uuid::new_v4();
    rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id":target,"title":"Repair"}),
    )
    .await;
    rpc(&state, methods::MANAGEMENT_SELECT, json!({"request_id":Uuid::new_v4(),"expected":view["profile"]["cursor"],"session_id":target,"grants":[]})).await;
    let fixed = rpc(&state, methods::MANAGEMENT_OPEN, json!({})).await;
    assert!(fixed["unavailable"].is_null());
    assert_eq!(
        fixed["profile"]["management"]["session"]["id"],
        json!(target)
    );
    assert_eq!(fixed["session"]["flow_count"], 0);
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_transport_authentication_precedes_profile_creation() {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    for method in [
        methods::MANAGEMENT_GET,
        methods::MANAGEMENT_OPEN,
        methods::MANAGEMENT_SELECT,
    ] {
        assert!(
            dispatch_as(
                state.clone(),
                JsonRpcRequest::new(1, method, json!({})),
                "untrusted-flow"
            )
            .await
            .error
            .is_some()
        );
    }
    assert!(!dir.path().join("runtime-v1").exists());
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(
                1,
                methods::MANAGEMENT_OPEN,
                json!({"session_id":Uuid::new_v4()})
            )
        )
        .await
        .error
        .is_some()
    );
    assert!(!dir.path().join("runtime-v1").exists());
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn broken_profile_does_not_fail_unrelated_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    drop(atman_runtime::flow_store::profile::ProfileStore::open(&root).unwrap());
    std::fs::write(root.root().join("host/identity.json"), "broken").unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    state.set_launcher(Arc::new(
        RunLauncher::new(dir.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::MANAGEMENT_GET, json!({}))
        )
        .await
        .error
        .is_some()
    );
    let id = Uuid::new_v4();
    let session = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id":id,"title":"Work"}),
    )
    .await;
    let source = dir.path().join("work.at");
    std::fs::write(
        &source,
        "flow main() -> string { return final.answer(message: \"independent\") }",
    )
    .unwrap();
    let address = json!({"session_id":id,"instance_id":Uuid::new_v4()});
    rpc(&state, methods::FLOW_LAUNCH, json!({
        "flow":address,"request_id":Uuid::new_v4(),"expected_catalog_sequence":session["catalog"]["sequence"],
        "path":source,"entry":"main"
    })).await;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let view = rpc(&state, methods::FLOW_GET, json!({"flow":address})).await;
            if !view["last_completion"].is_null() {
                assert_eq!(
                    view["last_completion"]["result"]["kind"], "returned",
                    "{view}"
                );
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn persistent_management_shares_catalog_pages_and_independent_pause_controls() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::now_v7();
    let mut catalog = root
        .create_session(SessionDescriptor {
            id,
            title: "Saved work".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let first = descriptor(id);
    let second = descriptor(id);
    for descriptor in [&first, &second] {
        let mut flow = catalog.create_flow(descriptor.clone()).unwrap();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
        if descriptor == &second {
            flow.transition(
                flow.state().revision(),
                FlowTransition::ExecutionPaused {
                    key: Uuid::now_v7().into(),
                },
            )
            .unwrap();
        }
    }
    drop(catalog);
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    let opened = rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    assert_eq!(opened["flow_count"], 2);
    assert_eq!(opened["default_flow"], json!(first.address.instance_id));
    let page = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": id, "limit": 1}),
    )
    .await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["instances"][0]["flow"], json!(second.address));
    assert!(page["instances"][0]["unavailable"].is_null());
    let earlier = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": id, "limit": 1, "before": page["next"]}),
    )
    .await;
    assert_eq!(earlier["instances"][0]["flow"], json!(first.address));
    assert!(earlier["next"].is_null());
    let invalid_cursor = dispatch(
        state.clone(),
        JsonRpcRequest::new(
            1,
            methods::SESSION_FLOWS,
            json!({"session_id": id, "before": Uuid::now_v7()}),
        ),
    )
    .await;
    assert!(invalid_cursor.error.is_some());

    let request = json!({"flow": second.address});
    let before = rpc(&state, methods::FLOW_GET, request.clone()).await;
    let selected = rpc(&state, methods::SESSION_SELECT_FLOW, request.clone()).await;
    assert_eq!(selected["default_flow"], json!(second.address.instance_id));
    assert_eq!(
        rpc(&state, methods::FLOW_GET, request.clone()).await,
        before,
        "view selection must not change execution"
    );
    let control = json!({"request_id": Uuid::now_v7(), "flow": second.address, "expected_revision": before["execution"]["revision"], "action": "pause"});
    let receipt = rpc(&state, methods::FLOW_CONTROL, control.clone()).await;
    assert_eq!(
        rpc(&state, methods::FLOW_CONTROL, control.clone()).await,
        receipt
    );
    let mut stale_request = control.clone();
    stale_request["request_id"] = json!(Uuid::now_v7());
    let stale = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_CONTROL, stale_request),
    )
    .await;
    assert!(stale.error.is_some());
    let paused = rpc(&state, methods::FLOW_GET, request.clone()).await;
    assert_eq!(paused["execution"]["pause_count"], 2);
    rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": second.address, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
    let resumed = rpc(&state, methods::FLOW_GET, request.clone()).await;
    assert_eq!(rpc(&state, methods::FLOW_CONTROL, control).await, receipt);
    assert_eq!(rpc(&state, methods::FLOW_GET, request).await, resumed);
    assert_eq!(resumed["execution"]["pause_count"], 1);
    assert_eq!(resumed["execution"]["can_start_turn"], false);
    assert_eq!(
        rpc(&state, methods::FLOW_GET, json!({"flow": first.address})).await["execution"]["can_start_turn"],
        true
    );
}

#[tokio::test]
async fn session_creation_is_idempotent_and_unknown_principals_never_acquire_storage() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    state.set_launcher(Arc::new(
        RunLauncher::new(dir.path().to_path_buf(), Some(config), None).unwrap(),
    ));
    let id = Uuid::now_v7();
    let params = json!({"session_id": id, "title": "Projectless work", "project_root": null});
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::SESSION_CREATE, params.clone()),
        "another-principal",
    )
    .await;
    assert!(denied.error.is_some());
    assert!(!data.join("runtime-v1").exists());
    for (method, invalid) in [
        (
            methods::SESSION_CREATE,
            json!({"session_id": id, "title": "  "}),
        ),
        (methods::SESSION_OPEN, json!({"session_id": Uuid::nil()})),
        (
            methods::FLOW_CONTROL,
            json!({"flow": {"session_id": id, "instance_id": Uuid::now_v7()}, "action": "pause"}),
        ),
        (
            methods::FLOW_CONTROL,
            json!({"flow": {"session_id": id, "instance_id": Uuid::now_v7()}, "action": "pause", "expected_revision": 0}),
        ),
        (
            methods::SESSION_FLOWS,
            json!({"session_id": id, "limit": 0}),
        ),
        (
            methods::SESSION_FLOWS,
            json!({"session_id": id, "limit": 51}),
        ),
    ] {
        let reply = dispatch(state.clone(), JsonRpcRequest::new(1, method, invalid)).await;
        assert_eq!(reply.error.unwrap().code, JsonRpcError::INVALID_PARAMS);
    }
    assert!(!data.join("runtime-v1").exists());
    let (first, repeated) = tokio::join!(
        rpc(&state, methods::SESSION_CREATE, params.clone()),
        rpc(&state, methods::SESSION_CREATE, params)
    );
    assert_eq!(first, repeated);
    assert!(first["project_root"].is_null());
    assert!(first["default_flow"].is_null());
    let conflicting = dispatch(
        state.clone(),
        JsonRpcRequest::new(
            1,
            methods::SESSION_CREATE,
            json!({"session_id": id, "title": "Conflicting title"}),
        ),
    )
    .await;
    assert!(conflicting.error.is_some());
    assert_eq!(
        rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await["title"],
        "Projectless work"
    );
}

#[test]
fn daemon_reopening_preserves_selection_and_pause_without_starting_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::now_v7();
    let mut catalog = root
        .create_session(SessionDescriptor {
            id,
            title: "Reopen work".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let first = descriptor(id);
    let second = descriptor(id);
    for descriptor in [&first, &second] {
        let mut flow = catalog.create_flow(descriptor.clone()).unwrap();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
    }
    drop(catalog);
    let before = {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
            rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
            rpc(&state, methods::SESSION_SELECT_FLOW, json!({"flow": second.address})).await;
            let before = rpc(&state, methods::FLOW_GET, json!({"flow": second.address})).await;
            rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": second.address, "expected_revision": before["execution"]["revision"], "action": "pause"})).await;
            rpc(&state, methods::FLOW_GET, json!({"flow": second.address})).await
        })
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
        let summary = rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
        assert_eq!(summary["default_flow"], json!(second.address.instance_id));
        let reopened = rpc(&state, methods::FLOW_GET, json!({"flow": second.address})).await;
        assert_eq!(reopened, before);
        rpc(&state, methods::FLOW_CONTROL, json!({"request_id": Uuid::now_v7(), "flow": second.address, "expected_revision": reopened["execution"]["revision"], "action": "resume"})).await;
        let resumed = rpc(&state, methods::FLOW_GET, json!({"flow": second.address})).await;
        assert_eq!(resumed["execution"]["pause_count"], 0);
        assert_eq!(resumed["execution"]["state"], reopened["execution"]["state"]);
        assert_eq!(resumed["execution"]["can_start_turn"], true);
    });
}

#[tokio::test]
async fn missing_flow_remains_in_catalog_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::now_v7();
    let mut catalog = root
        .create_session(SessionDescriptor {
            id,
            title: "Incomplete storage".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let first = descriptor(id);
    let missing = descriptor(id);
    drop(catalog.create_flow(first.clone()).unwrap());
    let flow = catalog.create_flow(missing.clone()).unwrap();
    let path = flow.dir().to_path_buf();
    drop(flow);
    drop(catalog);
    std::fs::rename(path, dir.path().join("unavailable-flow")).unwrap();
    let state = Arc::new(DaemonState::new(dir.path().to_path_buf()));
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    let page = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": id, "limit": 1}),
    )
    .await;
    assert_eq!(page["total"], 2);
    assert_eq!(page["instances"][0]["flow"], json!(missing.address));
    assert!(page["instances"][0]["execution"].is_null());
    assert!(page["instances"][0]["unavailable"].is_string());
    let earlier = rpc(
        &state,
        methods::SESSION_FLOWS,
        json!({"session_id": id, "limit": 1, "before": page["next"]}),
    )
    .await;
    assert_eq!(earlier["instances"][0]["flow"], json!(first.address));
    assert_eq!(
        earlier["instances"][0]["execution"]["state"],
        json!(atman_rt::InstanceState::<atman_runtime::event::AtmanUuid>::Prepared)
    );
}
