use std::{
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use atman_daemon::{DaemonState, dispatch, dispatch_as};
use atman_proto::{JsonRpcRequest, methods};
use atman_runtime::{
    config_hub::ConfigHub,
    flow_store::{RuntimeStore, SessionDescriptor},
    project_catalog::ProjectCatalogStore,
    session_meta::SessionMeta,
    storage::ProjectContext,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn read(state: &Arc<DaemonState>, params: Value) -> Value {
    let reply = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::PROJECT_DIRECTORY, params),
    )
    .await;
    assert!(reply.error.is_none(), "{:?}", reply.error);
    reply.result.unwrap()
}

fn timestamp(path: &Path, seconds: u64) {
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
        .unwrap();
}

#[tokio::test]
async fn paged_project_sessions_preserve_sources_and_recheck_only_on_refresh() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let data = base.join("data");
    let work = base.join("workspace");
    let empty = base.join("registered-empty");
    let unregistered = base.join("unregistered");
    for path in [&work, &empty, &unregistered] {
        fs::create_dir(path).unwrap();
    }
    let now = chrono::Utc::now();
    let catalog = ProjectCatalogStore::new(&data);
    let project = catalog.register_pinned(&work, now).unwrap();
    catalog.register(&empty, now).unwrap();
    let owner = RuntimeStore::open(&data).unwrap();
    let cfg = ConfigHub::from_config_dir(base.join("config"));
    let id = Uuid::now_v7();
    let native = owner
        .create_session(SessionDescriptor {
            id,
            title: "Bravo".into(),
            project: Some(ProjectContext::resolve(&cfg, &work, &data).unwrap()),
            configuration: Default::default(),
        })
        .unwrap();
    let journal = data
        .join("runtime-v1/sessions")
        .join(id.to_string())
        .join("events.jsonl");
    timestamp(&journal, 2000);
    let legacy = data.join("sessions").join(id.to_string());
    fs::create_dir_all(&legacy).unwrap();
    let meta = SessionMeta {
        project_fingerprint: Some(project.fingerprint.clone()),
        title: Some("Alpha".into()),
        ..Default::default()
    };
    meta.save(&legacy).unwrap();
    let events = legacy.join("events.jsonl");
    let historical_body = b"unreadable as an Atman event";
    fs::write(&events, historical_body).unwrap();
    timestamp(&events, 1_000);
    let orphan = data.join("sessions").join(Uuid::now_v7().to_string());
    fs::create_dir(&orphan).unwrap();
    SessionMeta {
        project_root: Some(unregistered.clone()),
        title: Some("Old history".into()),
        ..Default::default()
    }
    .save(&orphan)
    .unwrap();
    let state = Arc::new(DaemonState::new(data.clone()));
    let groups = json!({"target": {"kind":"projects"}, "limit": 1});
    let project_page = read(&state, groups).await["page"].clone();
    assert_eq!(project_page["total"], 3);
    let first = &project_page["rows"]["projects"][0];
    assert_eq!(first["record"]["fingerprint"], project.fingerprint);
    assert_eq!(first["session_count"], 2);
    assert_eq!(first["preview"][0]["kind"], "native");
    assert_eq!(first["preview"][1]["kind"], "legacy");
    assert_eq!(first["preview"][0]["id"], first["preview"][1]["id"]);
    let rest = read(
        &state,
        json!({"target":{"kind":"projects"}, "after": project_page["after"]}),
    )
    .await;
    assert_eq!(rest["page"]["rows"]["projects"][0]["session_count"], 0);
    assert_eq!(
        rest["page"]["rows"]["projects"][1]["project_root"],
        json!(unregistered)
    );
    assert_eq!(
        rest["page"]["rows"]["projects"][1]["preview"][0]["title"],
        "Old history"
    );

    let query =
        |sort| json!({"target": {"kind":"sessions", "project_root":work}, "sort":sort, "limit":1});
    let oldest = read(&state, query("oldest_first")).await;
    assert_eq!(oldest["page"]["rows"]["sessions"][0]["title"], "Alpha");
    let named = read(&state, query("name_ascending")).await;
    assert_eq!(named["page"]["rows"]["sessions"][0]["title"], "Alpha");
    let mut continued = query("name_ascending");
    continued["after"] = named["page"]["after"].clone();
    let second = read(&state, continued.clone()).await;
    assert_eq!(second["page"]["rows"]["sessions"][0]["title"], "Bravo");
    let mut different = continued.clone();
    different["sort"] = json!("oldest_first");
    assert_eq!(read(&state, different).await["state"], "changed");
    let mut other = continued.clone();
    other["target"]["project_root"] = json!(empty);
    assert_eq!(read(&state, other).await["state"], "changed");
    let found = read(
        &state,
        json!({"target":{"kind":"projects"}, "search":" ALPHA "}),
    )
    .await;
    assert_eq!(found["page"]["total"], 1);
    assert_eq!(found["page"]["rows"]["projects"][0]["session_count"], 1);
    let absent = read(
        &state,
        json!({"target":{"kind":"sessions", "project_root": base.join("unused")}}),
    )
    .await;
    assert_eq!(absent["state"], "unknown_project");

    SessionMeta {
        title: Some("Zulu".into()),
        ..meta
    }
    .save(&legacy)
    .unwrap();
    assert_eq!(read(&state, continued.clone()).await, second);
    let mut refresh = query("name_ascending");
    refresh["refresh"] = json!(true);
    let renamed = read(&state, refresh).await;
    assert_eq!(renamed["page"]["rows"]["sessions"][0]["title"], "Bravo");
    assert_eq!(read(&state, continued).await["state"], "changed");
    let denied = dispatch_as(
        state,
        JsonRpcRequest::new(1, methods::PROJECT_DIRECTORY, query("name_ascending")),
        "unknown",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    assert_eq!(fs::read(&events).unwrap(), historical_body);
    assert!(!legacy.join("stats.json").exists());
    assert!(!legacy.join(".stats.lock").exists());
    assert!(native.projection().flows.is_empty());
}
