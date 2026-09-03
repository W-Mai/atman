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
        capabilities.snapshot_schema_version,
        atman_proto::SNAPSHOT_SCHEMA_VERSION
    );
    assert_eq!(
        capabilities.event_schema_version,
        atman_proto::PROJECTION_EVENT_SCHEMA_VERSION
    );
    assert_eq!(
        capabilities.methods.len(),
        atman_daemon::SUPPORTED_METHODS.len()
    );
    assert!(capabilities.supports::<atman_proto::rpc::GetSessionSnapshot>());
    assert!(capabilities.supports::<atman_proto::rpc::GetSessionUpdates>());
    assert!(capabilities.supports::<atman_proto::rpc::RunFlow>());
    assert!(capabilities.supports::<atman_proto::rpc::ListProjects>());
    assert!(capabilities.supports::<atman_proto::rpc::CloseSession>());
    assert!(capabilities.supports::<atman_proto::rpc::DeleteSession>());
    assert!(capabilities.supports::<atman_proto::rpc::UpdateSessionTrust>());
}

#[tokio::test]
async fn project_list_rebuilds_persisted_projects_and_applies_queries() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    let first_root = tmp.path().join("alpha-project");
    let second_root = tmp.path().join("beta-project");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&first_root).unwrap();
    std::fs::create_dir_all(&second_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[storage]\nscope = \"global\"\n",
    )
    .unwrap();
    let state = Arc::new(DaemonState::new(data_dir));
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(first_root.clone(), Some(config_dir.clone()), None)
            .unwrap(),
    ));

    for root in [&first_root, &second_root] {
        let request = atman_proto::CreateSessionRequest {
            request_id: Some(atman_proto::RequestId::now()),
            project_root: Some(root.display().to_string()),
            title: None,
        };
        dispatch(
            state.clone(),
            JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(1, &request).unwrap(),
        )
        .await
        .into_method_output::<atman_proto::rpc::CreateSession>()
        .unwrap();
    }

    let listed = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ListProjects>(
            2,
            &atman_proto::ListProjectsRequest::default(),
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::ListProjects>()
    .unwrap();
    assert_eq!(listed.total, 2);
    assert_eq!(listed.projects.len(), 2);
    assert!(
        listed
            .projects
            .iter()
            .all(|project| project.session_count == 1 && project.active_session_count == 0)
    );

    let filtered = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::ListProjects>(
            3,
            &atman_proto::ListProjectsRequest {
                search: Some("BETA".into()),
                limit: Some(1),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::ListProjects>()
    .unwrap();
    assert_eq!(filtered.total, 1);
    assert_eq!(filtered.projects[0].name, "beta-project");
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
async fn update_session_trust_persists_and_projects_the_complete_policy() {
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
        atman_daemon::run::RunLauncher::new(project_root.clone(), Some(config_dir.clone()), None)
            .unwrap(),
    ));
    let created = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(
            1,
            &atman_proto::CreateSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                project_root: Some(project_root.display().to_string()),
                title: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();
    let session_id = created.projection.metadata.id;
    let trust = atman_proto::TrustProjection {
        mode: atman_proto::TrustMode::Eager,
        theme: atman_proto::TrustTheme::Weather,
        escalation: atman_proto::TrustEscalation::Allow,
        eager_tiers: atman_proto::TrustTierOverrides {
            tier3: Some(atman_proto::TrustPolicyAction::Deny),
            ..Default::default()
        },
        eager_risks: atman_proto::TrustRiskOverrides {
            network: Some(atman_proto::TrustPolicyAction::Auto),
            ..Default::default()
        },
    };

    let response = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::UpdateSessionTrust>(
            2,
            &atman_proto::UpdateSessionTrustRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
                trust: trust.clone(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::UpdateSessionTrust>()
    .unwrap();

    assert_eq!(response.session_id, session_id);
    assert_eq!(response.trust, trust);
    assert!(response.cursor > created.cursor);
    let snapshot = state
        .session_snapshot(&session_id, "local-daemon")
        .await
        .unwrap();
    assert_eq!(snapshot.projection.trust, trust);
    assert_eq!(snapshot.cursor, response.cursor);

    let global = atman_runtime::config_hub::ConfigHub::from_config_dir(&config_dir)
        .trust_config()
        .unwrap();
    assert_eq!(global.mode, atman_runtime::trust::TrustMode::Eager);
    assert_eq!(global.theme, atman_runtime::trust::Theme::Weather);
    assert_eq!(
        global.escalation,
        atman_runtime::trust::EscalationPolicy::Allow
    );
    assert_eq!(
        global.tiers.eager.tier3,
        Some(atman_runtime::trust::PolicyAction::Deny)
    );
    assert_eq!(
        global.risks.eager.network,
        Some(atman_runtime::trust::PolicyAction::Auto)
    );
}

#[tokio::test]
async fn close_session_unloads_runtime_and_keeps_history_recoverable() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("data")));
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(project_root, Some(config_dir), None).unwrap(),
    ));
    let created = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(
            1,
            &atman_proto::CreateSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                project_root: None,
                title: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();
    let session_id = created.projection.metadata.id;
    let close = atman_proto::CloseSessionRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
    };

    let closed = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CloseSession>(2, &close).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CloseSession>()
    .unwrap();
    assert_eq!(closed.status, atman_proto::SessionCloseStatus::Closed);
    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CloseSession>(3, &close).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CloseSession>()
    .unwrap();
    assert_eq!(retry, closed);

    let already_closed = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CloseSession>(
            4,
            &atman_proto::CloseSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CloseSession>()
    .unwrap();
    assert_eq!(
        already_closed.status,
        atman_proto::SessionCloseStatus::AlreadyClosed
    );

    let snapshot = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::GetSessionSnapshot>(
            5,
            &atman_proto::GetSessionSnapshotRequest { session_id },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::GetSessionSnapshot>()
    .unwrap();
    assert_eq!(
        snapshot.projection.lifecycle,
        atman_proto::SessionLifecycle::Idle
    );
}

#[tokio::test]
async fn delete_session_waits_for_resources_and_removes_history_atomically() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    let config_dir = tmp.path().join("config");
    std::fs::create_dir_all(&project_root).unwrap();
    std::fs::create_dir_all(&config_dir).unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().join("data")));
    state.set_launcher(Arc::new(
        atman_daemon::run::RunLauncher::new(project_root, Some(config_dir), None).unwrap(),
    ));
    let created = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::CreateSession>(
            1,
            &atman_proto::CreateSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                project_root: None,
                title: None,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::CreateSession>()
    .unwrap();
    let session_id = created.projection.metadata.id;
    let session_dir = state.sessions_root().join(session_id.to_string());
    let task_id = state.task_registry().register(
        atman_runtime::TaskKind::Bash,
        "pending cleanup".into(),
        "bg-delete-test".into(),
        atman_runtime::TaskOwner::new(session_id.to_string(), None),
        tokio_util::sync::CancellationToken::new(),
    );

    let busy = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::DeleteSession>(
            2,
            &atman_proto::DeleteSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::DeleteSession>()
    .unwrap();
    assert_eq!(busy.status, atman_proto::SessionDeleteStatus::Busy);
    assert!(session_dir.is_dir());

    state
        .task_registry()
        .finish(&task_id, atman_runtime::TaskStatus::Ok);
    let delete = atman_proto::DeleteSessionRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
    };
    let deleted = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::DeleteSession>(3, &delete).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::DeleteSession>()
    .unwrap();
    assert_eq!(deleted.status, atman_proto::SessionDeleteStatus::Deleted);
    assert!(!session_dir.exists());

    let retry = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::DeleteSession>(4, &delete).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::DeleteSession>()
    .unwrap();
    assert_eq!(retry, deleted);
    let missing = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::DeleteSession>(
            5,
            &atman_proto::DeleteSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id,
            },
        )
        .unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::DeleteSession>()
    .unwrap();
    assert_eq!(missing.status, atman_proto::SessionDeleteStatus::NotFound);
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

    state.begin_shutdown();
    let retry_during_shutdown = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(3, &request).unwrap(),
    )
    .await
    .into_method_output::<atman_proto::rpc::RenameSession>()
    .unwrap();
    assert_eq!(retry_during_shutdown.session.title, "Committed title");

    let rejected = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(
            4,
            &atman_proto::RenameSessionRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: atman_proto::SessionId(sid),
                title: "Rejected title".into(),
            },
        )
        .unwrap(),
    )
    .await;
    assert_eq!(rejected.error.unwrap().message, "daemon is shutting down");

    let ping = dispatch(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::Ping>(
            5,
            &atman_proto::EmptyParams::default(),
        )
        .unwrap(),
    )
    .await;
    assert!(ping.error.is_none());

    let conflict = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::RenameSession>(
            6,
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
