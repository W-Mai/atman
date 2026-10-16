use std::{collections::BTreeMap, io::Write, sync::Arc, time::Duration};

use atman_daemon::{DaemonState, dispatch, dispatch_as, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
    SessionDescriptor,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

fn saved(root: &std::path::Path) -> (Uuid, FlowRef, std::path::PathBuf) {
    let store = RuntimeStore::open(root).unwrap();
    let id = Uuid::now_v7();
    let mut session = store
        .create_session(SessionDescriptor {
            id,
            title: "Retained work".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let address = FlowRef {
        session_id: id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut flow = session
        .create_flow(FlowDescriptor {
            address: address.clone(),
            created_by: None,
            creation_request: Uuid::now_v7(),
            content: EntrySnapshot {
                name: "main".into(),
                path: "/missing/main.at".into(),
                bundle: atman_runtime::source_program::RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: BTreeMap::from([(
                        "entry:main.at".into(),
                        "flow main() -> int { return 7 }".into(),
                    )]),
                    edges: BTreeMap::new(),
                },
            }
            .into(),
        })
        .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: atman_rt::TurnId::now(),
            run: atman_rt::RunId::now(),
        },
    )
    .unwrap();
    (id, address, flow.dir().join("events.jsonl"))
}

#[tokio::test]
async fn discovery_preserves_both_layouts_and_reports_corruption_without_recovery() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().into()));
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::LIST_SESSIONS, json!({})),
        "untrusted",
    )
    .await;
    assert!(denied.error.is_some());
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    assert_eq!(
        rpc(&state, methods::LIST_SESSIONS, json!({})).await,
        json!([])
    );
    assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);

    let (id, _, flow_path) = saved(tmp.path());
    std::fs::OpenOptions::new()
        .append(true)
        .open(&flow_path)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let flow_bytes = std::fs::read(&flow_path).unwrap();
    let inspection = RuntimeStore::inspect_session(tmp.path(), id).unwrap();
    let catalog_path = inspection.dir.join("events.jsonl");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&catalog_path)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let catalog_bytes = std::fs::read(&catalog_path).unwrap();
    let legacy = tmp.path().join("sessions").join(id.to_string());
    std::fs::create_dir_all(&legacy).unwrap();
    atman_runtime::session_meta::SessionMeta::default()
        .save(&legacy)
        .unwrap();
    let (bad, _, _) = saved(tmp.path());
    let bad_path = RuntimeStore::inspect_session(tmp.path(), bad)
        .unwrap()
        .dir
        .join("events.jsonl");
    let bad_text = std::fs::read_to_string(&bad_path)
        .unwrap()
        .replace("Retained work", "Broken record");
    std::fs::write(&bad_path, &bad_text).unwrap();

    let list = rpc(&state, methods::LIST_SESSIONS, json!({})).await;
    let rows = list.as_array().unwrap();
    assert_eq!(rows.len(), 3, "{list}");
    let retained = rows
        .iter()
        .find(|row| row["id"] == json!(id) && row["persistent"].is_object())
        .unwrap();
    assert_eq!(retained["status"], "detached");
    assert_eq!(retained["persistent"]["flow_count"], 1);
    assert_eq!(retained["persistent"]["trailing_bytes"], 11);
    assert!(retained["event_count"].is_null());
    assert!(retained["persistent"]["loaded_flows"].is_null());
    assert!(
        rows.iter()
            .any(|row| row["id"] == json!(id) && row["persistent"].is_null())
    );
    let corrupt = rows.iter().find(|row| row["id"] == json!(bad)).unwrap();
    assert_eq!(corrupt["status"], "unavailable");
    assert!(corrupt["persistent"]["unavailable"].is_string());
    assert_eq!(std::fs::read(&flow_path).unwrap(), flow_bytes);
    assert_eq!(std::fs::read(&catalog_path).unwrap(), catalog_bytes);
    assert_eq!(std::fs::read_to_string(&bad_path).unwrap(), bad_text);

    // Owning the catalog alone must not load or recover an inactive instance for a list view.
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    let list = rpc(
        &state,
        methods::LIST_SESSIONS,
        json!({"search": "retained", "limit": 1}),
    )
    .await;
    assert_eq!(list[0]["status"], "detached");
    assert_eq!(list[0]["persistent"]["loaded_flows"], 0);
    assert_eq!(std::fs::read(&flow_path).unwrap(), flow_bytes);
    state.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_observes_all_peer_execution_and_keeps_missing_projects_selectable() {
    let tmp = tempfile::tempdir().unwrap();
    let config = tmp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let project = {
        #[cfg(unix)]
        {
            let alias = tmp.path().join("alias");
            std::os::unix::fs::symlink(tmp.path(), &alias).unwrap();
            alias.join("project")
        }
        #[cfg(not(unix))]
        {
            tmp.path().join("project")
        }
    };
    std::fs::create_dir_all(&project).unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("data")));
    state.set_launcher(Arc::new(
        RunLauncher::new(project.clone(), Some(config), None).unwrap(),
    ));
    let id = Uuid::now_v7();
    rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "Peer work", "project_root": project}),
    )
    .await;
    let path = project.join("main.at");
    let rename = dispatch(
        state.clone(),
        JsonRpcRequest::new(
            1,
            methods::RENAME_SESSION,
            json!({"session_id": id, "title": "wrong layout"}),
        ),
    )
    .await;
    assert!(rename.error.is_some());
    assert!(
        !state.sessions_root().exists(),
        "legacy rename must not create another layout for a persistent session"
    );
    std::fs::write(
        &path,
        "flow main(wait: bool) -> int { when wait { loop { yield } } return 7 }",
    )
    .unwrap();
    let mut flows = Vec::new();
    for wait in [false, true] {
        let view = rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
        let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
        rpc(&state, methods::FLOW_LAUNCH, json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": view["catalog"]["sequence"], "path": path, "entry": "main", "arguments": {"wait": wait}})).await;
        flows.push(flow);
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let main = rpc(&state, methods::FLOW_GET, json!({"flow": flows[0]})).await;
            let peer = rpc(&state, methods::FLOW_GET, json!({"flow": flows[1]})).await;
            if main["last_completion"].is_object()
                && peer["execution"]["state"]["open"]["active"].is_object()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_dir_all(&project).unwrap();
    let list = rpc(
        &state,
        methods::LIST_SESSIONS,
        json!({"project_root": project, "search": "peer", "limit": 1}),
    )
    .await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["status"], "running");
    assert_eq!(list[0]["persistent"]["flow_count"], 2);
    assert_eq!(list[0]["persistent"]["loaded_flows"], 2);
    assert_eq!(
        list[0]["persistent"]["default_flow"],
        flows[0]["instance_id"]
    );
    let active = rpc(&state, methods::FLOW_GET, json!({"flow": flows[1]})).await;
    rpc(&state, methods::FLOW_CONTROL, json!({"flow": flows[1], "action": "cancel_turn", "expected_revision": active["execution"]["revision"]})).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let list = rpc(&state, methods::LIST_SESSIONS, json!({})).await;
            if list[0]["status"] == "idle" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    state.shutdown().await.unwrap();
    let reopened = Arc::new(DaemonState::new(tmp.path().join("data")));
    let list = rpc(
        &reopened,
        methods::LIST_SESSIONS,
        json!({"project_root": project}),
    )
    .await;
    assert_eq!(list[0]["status"], "detached");
    assert_eq!(list[0]["persistent"]["flow_count"], 2);
    assert!(list[0]["persistent"]["loaded_flows"].is_null());
}
