use std::{collections::BTreeSet, sync::Arc};

use atman_daemon::{DaemonState, dispatch, dispatch_as};
use atman_proto::{
    JsonRpcRequest, methods,
    projects::{ProjectListPage, SetProjectPinnedResponse},
};
use atman_runtime::project_catalog::ProjectCatalogStore;
use serde_json::{Value, json};

async fn page(state: &Arc<DaemonState>, params: Value) -> ProjectListPage {
    let response = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_LIST, params),
    )
    .await;
    assert!(response.error.is_none(), "{:?}", response.error);
    serde_json::from_value(response.result.unwrap()).unwrap()
}

fn files(path: &std::path::Path) -> BTreeSet<std::ffi::OsString> {
    path.read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect()
}

#[tokio::test]
async fn project_directory_observes_without_writes_and_pins_with_revision_checks() {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    let state = Arc::new(DaemonState::new(data.clone()));
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_LIST, json!({})),
        "untrusted",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    assert!(page(&state, json!({})).await.projects.is_empty());
    assert!(!data.exists());

    let first = fixture.path().join("first");
    let second = fixture.path().join("second");
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    let store = ProjectCatalogStore::new(&data);
    let now = chrono::Utc::now();
    let a = store.register_pinned(&first, now).unwrap();
    let b = store.register(&second, now).unwrap();
    let catalog = data.join("project-catalog.json");
    let original = std::fs::read(&catalog).unwrap();
    std::fs::remove_file(data.join(".project-catalog.lock")).unwrap();
    let initial_files = files(&data);
    let first = page(&state, json!({"limit":1})).await;
    assert_eq!(first.projects[0].fingerprint, a.fingerprint);
    let cursor = first.after.unwrap();
    assert_eq!(cursor.revision, first.revision);
    let second = page(&state, json!({"after":cursor})).await;
    assert_eq!(second.projects[0].fingerprint, b.fingerprint);
    assert!(second.after.is_none());
    assert_eq!(std::fs::read(&catalog).unwrap(), original);
    assert_eq!(files(&data), initial_files);

    store.set_pinned(&b.fingerprint, true).unwrap();
    let stale = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_LIST, json!({"after":cursor})),
    )
    .await;
    assert!(stale.error.unwrap().message.contains("restart paging"));
    for params in [
        json!({"limit":51}),
        json!({"project_root":"/not-authority"}),
    ] {
        let bad = dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::PROJECT_LIST, params),
        )
        .await;
        assert_eq!(bad.error.unwrap().code, -32602);
    }
    verify_pin_rechecks_observation_without_opening_sessions().await;
}

async fn verify_pin_rechecks_observation_without_opening_sessions() {
    let fixture = tempfile::tempdir().unwrap();
    let data = fixture.path().join("data");
    let state = Arc::new(DaemonState::new(data.clone()));
    let store = ProjectCatalogStore::new(&data);
    let now = chrono::Utc::now();
    let target = store.register(&fixture.path().join("target"), now).unwrap();
    let other = store.register(&fixture.path().join("other"), now).unwrap();
    let observed = page(&state, json!({})).await;
    let pin = || {
        JsonRpcRequest::new(
            2,
            methods::PROJECT_SET_PINNED,
            json!({
                "fingerprint": target.fingerprint, "expected_revision": observed.revision, "pinned": true,
            }),
        )
    };
    assert!(
        dispatch_as(state.clone(), pin(), "untrusted")
            .await
            .error
            .is_some()
    );
    assert!(
        !store
            .load()
            .unwrap()
            .projects
            .iter()
            .any(|project| project.pinned)
    );

    let receipt = dispatch(state.clone(), pin()).await;
    assert!(receipt.error.is_none(), "{:?}", receipt.error);
    let receipt: SetProjectPinnedResponse =
        serde_json::from_value(receipt.result.unwrap()).unwrap();
    assert_eq!(receipt.project.last_opened, target.last_opened);
    let updated = page(&state, json!({})).await;
    assert_eq!(updated.revision, receipt.revision);
    assert_eq!(updated.projects[0], receipt.project);
    assert!(updated.projects[0].pinned);
    let retry = dispatch(state.clone(), pin()).await;
    assert_eq!(
        retry.result.unwrap(),
        serde_json::to_value(receipt).unwrap()
    );

    let stale = dispatch(state.clone(), JsonRpcRequest::new(3, methods::PROJECT_SET_PINNED, json!({
        "fingerprint": other.fingerprint, "expected_revision": observed.revision, "pinned": true,
    }))).await;
    assert!(stale.error.unwrap().message.contains("refresh"));
    assert_eq!(page(&state, json!({})).await, updated);
    assert!(state.runtime_host().sessions().is_empty());
    state.shutdown().await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn workspace_pages_require_current_policy_and_the_original_anchored_scope() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let config = base.join("config");
    let data = base.join("data");
    let workspace = base.join("workspace");
    let second = base.join("second");
    let private = base.join("private");
    for path in [
        config.clone(),
        workspace.join("alpha"),
        workspace.join("zulu"),
        workspace.join("子目录"),
        workspace.join(".hidden"),
        second.join("different"),
        private.join("private-name"),
    ] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(workspace.join("a-file"), b"excluded").unwrap();
    std::os::unix::fs::symlink(&private, workspace.join("link")).unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(workspace.clone(), Some(config.clone()), None).unwrap(),
    ));
    let rpc = |method, params| dispatch(state.clone(), JsonRpcRequest::new(1, method, params));
    let roots = rpc(methods::PROJECT_WORKSPACE_ROOTS, json!({}))
        .await
        .result
        .unwrap();
    assert_eq!(roots, json!({"state":"ready","directory":{"roots":[]}}));
    let path = config.join("workspace-roots.toml");
    let policy = |root: &std::path::Path| {
        format!(
            "[roots.work]\nlabel=\"Work\"\npath={:?}\n[roots.second]\nlabel=\"Second\"\npath={:?}\n",
            root.to_str().unwrap(),
            second.to_str().unwrap(),
        )
    };
    std::fs::write(&path, policy(&workspace)).unwrap();
    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_WORKSPACE_ROOTS, json!({})),
        "untrusted",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    let foreign_request = json!({"root_id":"ungranted","components":[]});
    let foreign = rpc(methods::PROJECT_WORKSPACE_FOLDERS, foreign_request)
        .await
        .result
        .unwrap();
    assert_eq!(
        foreign,
        json!({"state":"unavailable","reason":"unknown_root"})
    );
    for rejected in [
        "..",
        ".",
        "/tmp",
        "~",
        "C:",
        "a/b",
        "a\\b",
        "archive:ads",
        ".hidden",
        "NUL",
    ] {
        let invalid = rpc(
            methods::PROJECT_WORKSPACE_FOLDERS,
            json!({"root_id":"work","components":[rejected]}),
        )
        .await;
        assert_eq!(invalid.error.unwrap().code, -32602, "{rejected}");
    }
    let link = rpc(
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"work","components":["link"]}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(
        link,
        json!({"state":"unavailable","reason":"directory_unavailable"})
    );
    let request = json!({"root_id":"work","components":[],"limit":1});
    let first = rpc(methods::PROJECT_WORKSPACE_FOLDERS, request.clone())
        .await
        .result
        .unwrap();
    assert_eq!(first["page"]["folders"], json!(["alpha"]));
    assert!(!first.to_string().contains(base.to_str().unwrap()));
    let cursor = first["page"]["after"].clone();
    let mut after = request;
    after["after"] = cursor;
    let wrong = rpc(
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"second","components":[],"after":after["after"]}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(wrong["reason"], "directory_changed");
    let second_page = rpc(methods::PROJECT_WORKSPACE_FOLDERS, after.clone())
        .await
        .result
        .unwrap();
    assert_eq!(second_page["page"]["folders"], json!(["zulu"]));
    let mut end = after.clone();
    end["after"] = second_page["page"]["after"].clone();
    let final_page = rpc(methods::PROJECT_WORKSPACE_FOLDERS, end)
        .await
        .result
        .unwrap();
    assert_eq!(final_page["page"]["folders"], json!(["子目录"]));
    assert!(final_page["page"]["after"].is_null());
    std::fs::create_dir(workspace.join("later")).unwrap();
    let stale = rpc(methods::PROJECT_WORKSPACE_FOLDERS, after.clone())
        .await
        .result
        .unwrap();
    assert_eq!(stale["reason"], "directory_changed");
    std::fs::write(&path, "").unwrap();
    let revoked = rpc(methods::PROJECT_WORKSPACE_FOLDERS, after)
        .await
        .result
        .unwrap();
    assert_eq!(revoked["reason"], "directory_changed");
    std::fs::write(&path, policy(&base)).unwrap();
    let private_parent = rpc(methods::PROJECT_WORKSPACE_ROOTS, json!({}))
        .await
        .result
        .unwrap();
    assert_eq!(private_parent["reason"], "invalid_policy");
    std::fs::write(&path, policy(&workspace)).unwrap();
    let large = workspace.join("large");
    std::fs::create_dir(&large).unwrap();
    for index in 0..4097 {
        std::fs::write(large.join(index.to_string()), b"").unwrap();
    }
    let capped = rpc(
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"work","components":["large"],"limit":1}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(capped["reason"], "directory_too_large");
    assert!(
        !data.exists(),
        "directory reads must not initialize Profile storage"
    );
    let first = rpc(
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"work","components":[],"limit":1}),
    )
    .await
    .result
    .unwrap();
    state.runtime_host().open_profile(&data).await.unwrap();
    let changed_profile = rpc(
        methods::PROJECT_WORKSPACE_FOLDERS,
        json!({"root_id":"work","components":[],"limit":1,"after":first["page"]["after"]}),
    )
    .await
    .result
    .unwrap();
    assert_eq!(changed_profile["reason"], "directory_changed");
    state.shutdown().await.unwrap();
}
