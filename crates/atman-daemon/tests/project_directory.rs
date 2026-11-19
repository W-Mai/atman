use std::{collections::BTreeSet, sync::Arc};

use atman_daemon::{DaemonState, dispatch, dispatch_as};
use atman_proto::{JsonRpcRequest, methods, projects::ProjectListPage};
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
async fn project_list_reads_empty_projects_without_writing_or_reconciling() {
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
}
