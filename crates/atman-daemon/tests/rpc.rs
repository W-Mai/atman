use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch};
use atman_proto::{JsonRpcRequest, methods};

#[tokio::test]
async fn capabilities_are_typed_and_match_the_registry() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new_with_generation(
        tmp.path().to_path_buf(),
        "test-generation".into(),
    ));
    let req = JsonRpcRequest::for_method::<atman_proto::rpc::DaemonCapabilities>(
        0,
        &atman_proto::CapabilitiesRequest {
            protocol_version: Some(atman_proto::PROTOCOL_VERSION),
            ..Default::default()
        },
    )
    .unwrap();
    let capabilities = dispatch(state, req)
        .await
        .into_method_output::<atman_proto::rpc::DaemonCapabilities>()
        .unwrap();

    assert_eq!(capabilities.protocol_version, atman_proto::PROTOCOL_VERSION);
    assert_eq!(capabilities.daemon_generation.0, "test-generation");
    assert_eq!(
        capabilities.methods.len(),
        atman_daemon::SUPPORTED_METHODS.len()
    );
    assert!(capabilities.supports::<atman_proto::rpc::GetSessionSnapshot>());
    assert!(capabilities.supports::<atman_proto::rpc::GetSessionUpdates>());
    assert!(capabilities.supports::<atman_proto::rpc::RunFlow>());
}

#[tokio::test]
async fn ping_returns_pong() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(1, methods::PING, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("ping should succeed");
    assert_eq!(result["pong"], serde_json::json!(true));
    assert!(result["version"].is_string());
}

#[tokio::test]
async fn create_session_returns_an_idle_snapshot_and_replays_retries() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("data")));
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(project_root.clone(), Some(config_dir), None).unwrap(),
    ));
    let command = atman_proto::CreateSessionRequest {
        request_id: Some(atman_proto::RequestId::now()),
        project_root: Some(project_root.to_string_lossy().into_owned()),
        title: Some("Remote session".into()),
    };

    let first = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(1, &command).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(2, &command).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();

    assert_eq!(retry.projection.metadata.id, first.projection.metadata.id);
    assert_eq!(
        first.projection.lifecycle,
        atman_proto::SessionLifecycle::Idle
    );
    assert_eq!(first.projection.metadata.title, "Remote session");
    let canonical_project_root = std::fs::canonicalize(&project_root).unwrap();
    assert_eq!(
        first.projection.metadata.project_root.as_deref(),
        Some(canonical_project_root.to_string_lossy().as_ref())
    );
    assert!(first.projection.runs.is_empty());
    assert!(
        state
            .sessions_root()
            .join(first.projection.metadata.id.to_string())
            .join("events.jsonl")
            .is_file()
    );
}

#[tokio::test]
async fn method_not_found_returns_jsonrpc_error() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(2, "no_such_method", serde_json::json!({}));
    let resp = dispatch(state, req).await;
    assert!(resp.result.is_none());
    let err = resp.error.expect("expect error");
    assert_eq!(err.code, atman_proto::JsonRpcError::METHOD_NOT_FOUND);
}

#[tokio::test]
async fn list_sessions_reads_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("events.jsonl"),
        "{\"type\":\"flow_start\",\"ts\":\"2025-01-01T00:00:00Z\"}\n",
    )
    .unwrap();

    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(3, methods::LIST_SESSIONS, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("expect result");
    let arr = result.as_array().expect("expect array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["event_count"], 1);
    assert_eq!(arr[0]["status"], "finished");
}

#[tokio::test]
async fn get_events_reads_finished_sessions_by_persisted_sequence() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::now_v7();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("events.jsonl"),
        "{\"type\":\"one\",\"seq\":10}\n{\"type\":\"two\",\"seq\":20}\n",
    )
    .unwrap();

    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let request = JsonRpcRequest::for_method::<atman_proto::rpc::GetEvents>(
        4,
        &atman_proto::GetEventsRequest {
            session_id: atman_proto::SessionId(sid),
            since_seq: Some(10),
        },
    )
    .unwrap();
    let page = dispatch(state, request)
        .await
        .into_method_output::<atman_proto::rpc::GetEvents>()
        .unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].cursor, atman_proto::EventCursor(20));
    assert_eq!(page.next_cursor, atman_proto::EventCursor(20));
    assert!(!page.has_more);
}

#[tokio::test]
async fn get_snapshot_replays_idle_sessions_and_marks_interrupted_runs_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::now_v7();
    let run_id = atman_runtime::event::FlowRunId::now();
    let sdir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&sdir).unwrap();
    atman_runtime::session_meta::SessionMeta {
        title: Some("Recovered session".into()),
        name_source: atman_runtime::session_meta::NameSource::User,
        ..Default::default()
    }
    .save(&sdir)
    .unwrap();
    atman_runtime::memory::goal::GoalStore::at(&sdir)
        .set("Recover state")
        .unwrap();
    let event = atman_runtime::event::EventEnvelope {
        seq: 1,
        ts: chrono::Utc::now(),
        event: atman_runtime::event::Event::FlowStart {
            run_id,
            flow_name: "agent".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        },
    };
    std::fs::write(
        sdir.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&event).unwrap()),
    )
    .unwrap();

    let state = Arc::new(DaemonState::new_with_generation(
        tmp.path().to_path_buf(),
        "snapshot-generation".into(),
    ));
    let request = JsonRpcRequest::for_method::<atman_proto::rpc::GetSessionSnapshot>(
        5,
        &atman_proto::GetSessionSnapshotRequest {
            session_id: atman_proto::SessionId(sid),
        },
    )
    .unwrap();
    let snapshot = dispatch(state.clone(), request)
        .await
        .into_method_output::<atman_proto::rpc::GetSessionSnapshot>()
        .unwrap();

    assert_eq!(snapshot.daemon_generation.0, "snapshot-generation");
    assert_eq!(snapshot.projection.metadata.title, "Recovered session");
    assert_eq!(snapshot.projection.goal.as_deref(), Some("Recover state"));
    assert_eq!(
        snapshot.projection.lifecycle,
        atman_proto::SessionLifecycle::Idle
    );
    assert_eq!(
        snapshot.projection.runs[0].state,
        atman_proto::RunLifecycle::Lost
    );
    assert_eq!(snapshot.cursor.0, snapshot.projection.revision.0);

    let updates = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::GetSessionUpdates>(
            6,
            &atman_proto::GetSessionUpdatesRequest {
                session_id: atman_proto::SessionId(sid),
                after_cursor: snapshot.cursor,
                limit: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::GetSessionUpdates>()
    .unwrap();
    assert!(updates.events.is_empty());
    assert!(updates.resync_required.is_none());

    let gap = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::GetSessionUpdates>(
            7,
            &atman_proto::GetSessionUpdatesRequest {
                session_id: atman_proto::SessionId(sid),
                after_cursor: atman_proto::EventCursor::default(),
                limit: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::GetSessionUpdates>()
    .unwrap();
    assert!(gap.resync_required.is_some());
}

#[tokio::test]
async fn rename_session_retries_return_the_original_committed_result() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = uuid::Uuid::now_v7();
    let session_dir = tmp.path().join("sessions").join(sid.to_string());
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("events.jsonl"), "").unwrap();
    atman_runtime::session_meta::SessionMeta::default()
        .save(&session_dir)
        .unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(tmp.path().to_path_buf(), Some(config_dir), None)
            .unwrap(),
    ));
    let request_id = atman_proto::RequestId::now();
    let request = atman_proto::RenameSessionRequest {
        request_id: Some(request_id.clone()),
        session_id: atman_proto::SessionId(sid),
        title: "Committed title".into(),
    };

    let first = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(1, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::RenameSession>()
    .unwrap();
    assert_eq!(first.session.title, "Committed title");
    let snapshot = state
        .session_snapshot(&atman_proto::SessionId(sid), "local-daemon")
        .await
        .unwrap();
    assert_eq!(first.revision, snapshot.projection.revision);
    assert_eq!(first.cursor, snapshot.cursor);

    atman_runtime::session_meta::SessionMeta::set_title(
        &session_dir,
        Some("External change".into()),
    )
    .unwrap();
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(2, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::RenameSession>()
    .unwrap();
    assert_eq!(retry.session.title, "Committed title");
    assert_eq!(retry.revision, first.revision);
    assert_eq!(retry.cursor, first.cursor);
    assert_eq!(
        atman_runtime::session_meta::SessionMeta::load(&session_dir)
            .unwrap()
            .title
            .as_deref(),
        Some("External change")
    );

    let conflict = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(
            3,
            &atman_proto::RenameSessionRequest {
                request_id: Some(request_id),
                session_id: atman_proto::SessionId(sid),
                title: "Different command".into(),
            },
        )
        .unwrap(),
    )
    .await;
    assert_eq!(
        conflict.error.unwrap().code,
        atman_proto::JsonRpcError::INVALID_PARAMS
    );
}
