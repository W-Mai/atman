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
        entry: EntrySnapshot {
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
        },
    }
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
    let control = json!({"flow": second.address, "expected_revision": before["execution"]["revision"], "action": "pause"});
    rpc(&state, methods::FLOW_CONTROL, control.clone()).await;
    let stale = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_CONTROL, control),
    )
    .await;
    assert!(stale.error.is_some());
    let paused = rpc(&state, methods::FLOW_GET, request.clone()).await;
    assert_eq!(paused["execution"]["pause_count"], 2);
    rpc(&state, methods::FLOW_CONTROL, json!({"flow": second.address, "expected_revision": paused["execution"]["revision"], "action": "resume"})).await;
    let resumed = rpc(&state, methods::FLOW_GET, request).await;
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
            rpc(&state, methods::FLOW_CONTROL, json!({"flow": second.address, "expected_revision": before["execution"]["revision"], "action": "pause"})).await;
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
        rpc(&state, methods::FLOW_CONTROL, json!({"flow": second.address, "expected_revision": reopened["execution"]["revision"], "action": "resume"})).await;
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
