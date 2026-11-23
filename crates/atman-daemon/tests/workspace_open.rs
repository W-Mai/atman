#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{os::unix::fs::PermissionsExt, path::Path, sync::Arc};

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, methods};
use atman_runtime::{
    flow_store::{FlowRef, RuntimeStore},
    project_catalog::ProjectCatalogStore,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn host(data: &Path, config: &Path, work: &Path) -> Arc<DaemonState> {
    let host = Arc::new(DaemonState::new(data.to_owned()));
    host.set_launcher(Arc::new(
        RunLauncher::new(work.into(), Some(config.into()), None).unwrap(),
    ));
    host
}

async fn call(host: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let reply = dispatch(host.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(reply.error.is_none(), "{:?}", reply.error);
    reply.result.unwrap()
}

async fn selection(host: &Arc<DaemonState>) -> Value {
    let page = call(
        host,
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"work","components":["nested"]}),
    )
    .await;
    assert_eq!(page["state"], "ready", "{page}");
    page["page"]["selection"].clone()
}

#[tokio::test]
async fn workspace_open_reconciles_project_and_session_after_failure_and_restarts() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let data = base.join("data");
    let config = base.join("config");
    let root = base.join("work");
    let workspace = root.join("nested");
    let local = workspace.join(".atman");
    std::fs::create_dir_all(local.join("commands")).unwrap();
    std::fs::create_dir_all(&config).unwrap();
    let policy = config.join("workspace-roots.toml");
    let configure = |allow| {
        std::fs::write(
            &policy,
            format!(
                "[roots.work]\nlabel=\"Work\"\npath={:?}\nallow_open={allow}\n",
                root.to_str().unwrap()
            ),
        )
        .unwrap()
    };
    configure(false);
    std::fs::write(local.join("config.toml"), "[storage]\nscope='local'\n").unwrap();
    let state = host(&data, &config, &workspace);
    let operation = Uuid::new_v4();
    let query = json!({"operation_id":operation});
    let status = call(
        &state,
        methods::PROJECT_WORKSPACE_OPEN_STATUS,
        query.clone(),
    )
    .await;
    assert_eq!(status["progress"]["state"], "unseen");
    let read_only = json!({"operation_id":operation, "selection":selection(&state).await});
    let refused = call(&state, methods::PROJECT_OPEN_WORKSPACE, read_only).await;
    assert_eq!(refused["failure"], "read_only_root");
    assert!(
        !data.exists(),
        "read-only and unobserved requests must not start a writer"
    );

    configure(true);
    let stale_selection = selection(&state).await;
    let retained = root.join("retained");
    std::fs::rename(&workspace, &retained).unwrap();
    std::os::unix::fs::symlink(&retained, &workspace).unwrap();
    let replaced = call(
        &state,
        methods::PROJECT_OPEN_WORKSPACE,
        json!({"operation_id":Uuid::new_v4(),"selection":stale_selection}),
    )
    .await;
    assert_eq!(replaced["progress"]["state"], "unseen");
    assert_eq!(replaced["failure"], "directory_unavailable");
    assert!(!data.exists());
    std::fs::remove_file(&workspace).unwrap();
    std::fs::rename(&retained, &workspace).unwrap();

    let agent = local.join("commands/agent.at");
    std::fs::write(&agent, "not a valid entry").unwrap();
    state.runtime_host().open_profile(&data).await.unwrap();
    let request = json!({"selection":selection(&state).await, "operation_id":operation});
    let sessions = data.join("runtime-v1/sessions");
    let held = sessions.with_file_name("testing-sessions");
    std::fs::rename(&sessions, &held).unwrap();
    std::fs::write(&sessions, b"temporarily unavailable").unwrap();
    let pinned = call(&state, methods::PROJECT_OPEN_WORKSPACE, request.clone()).await;
    assert_eq!(pinned["progress"]["state"], "project_pinned", "{pinned}");
    assert_eq!(pinned["failure"], "session_unavailable");
    let published = ProjectCatalogStore::new(&data).load().unwrap().projects;
    assert_eq!(published.len(), 1);
    assert!(published[0].pinned);
    assert_eq!(published[0].root, workspace);
    assert_eq!(
        published[0].fingerprint,
        pinned["progress"]["fingerprint"].as_str().unwrap()
    );
    std::fs::remove_file(&sessions).unwrap();
    std::fs::rename(&held, &sessions).unwrap();
    state.shutdown().await.unwrap();

    let state = host(&data, &config, &workspace);
    assert_eq!(
        call(
            &state,
            methods::PROJECT_WORKSPACE_OPEN_STATUS,
            query.clone()
        )
        .await["progress"],
        pinned["progress"]
    );
    let permissions = std::fs::metadata(&workspace).unwrap().permissions();
    std::fs::set_permissions(&workspace, std::fs::Permissions::from_mode(0o777)).unwrap();
    let unsafe_retry = call(&state, methods::PROJECT_OPEN_WORKSPACE, request.clone()).await;
    assert_eq!(unsafe_retry["progress"], pinned["progress"]);
    assert_eq!(unsafe_retry["failure"], "directory_unavailable");
    assert!(RuntimeStore::session_ids(&data).unwrap().is_empty());
    std::fs::set_permissions(&workspace, permissions).unwrap();
    let created = call(&state, methods::PROJECT_OPEN_WORKSPACE, request.clone()).await;
    assert_eq!(created["progress"]["state"], "session_created", "{created}");
    assert_eq!(created["failure"], "conversation_unavailable");
    let session_id = Uuid::parse_str(created["progress"]["session_id"].as_str().unwrap()).unwrap();
    let session = RuntimeStore::inspect_session(&data, session_id).unwrap();
    let project = session.projection.project.unwrap();
    assert_eq!(project.root(), workspace);
    assert_eq!(project.scope(), atman_runtime::storage::StorageScope::Local);
    assert!(session.projection.flows.is_empty());
    state.shutdown().await.unwrap();

    std::fs::write(&agent, "flow agent(input: string) -> string { contract { invocation { user_message: input } } return input }\n").unwrap();
    let state = host(&data, &config, &workspace);
    let ready = call(&state, methods::PROJECT_OPEN_WORKSPACE, request.clone()).await;
    assert_eq!(ready["progress"]["state"], "conversation_ready", "{ready}");
    assert_eq!(
        ready["progress"]["flow"]["session_id"],
        created["progress"]["session_id"]
    );
    assert_eq!(ready["progress"]["catalog"], created["progress"]["catalog"]);
    assert_eq!(
        call(
            &state,
            methods::PROJECT_WORKSPACE_OPEN_STATUS,
            query.clone()
        )
        .await,
        ready
    );
    let address = FlowRef {
        session_id: session_id.into(),
        instance_id: atman_rt::FlowInstanceId(session_id.into()),
    };
    let worker = state.runtime_host().flow(&address).await.unwrap();
    let launch = worker.launch_record().await.unwrap().unwrap();
    assert_eq!(
        launch
            .input
            .execution
            .unwrap()
            .authority
            .workspace_root
            .as_deref(),
        Some(workspace.as_path())
    );
    assert!(launch.acceptance.is_none());
    assert_eq!(RuntimeStore::session_ids(&data).unwrap(), vec![session_id]);
    state.shutdown().await.unwrap();

    let state = host(&data, &config, &workspace);
    configure(false);
    assert_eq!(
        call(&state, methods::PROJECT_OPEN_WORKSPACE, request.clone()).await,
        ready
    );
    let changed = json!({"operation_id":operation,"selection":Uuid::new_v4()});
    let conflict = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_OPEN_WORKSPACE, changed),
    )
    .await;
    assert!(conflict.error.unwrap().message.contains("conflicts"));
    assert_eq!(RuntimeStore::session_ids(&data).unwrap(), vec![session_id]);
    assert_eq!(
        ProjectCatalogStore::new(&data).load().unwrap().projects,
        published
    );
    state.shutdown().await.unwrap();
}
