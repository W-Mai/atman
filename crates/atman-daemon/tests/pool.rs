use std::sync::Arc;

use atman_daemon::{DaemonState, LiveRun, dispatch};
use atman_proto::{FlowRunId, JsonRpcRequest, SessionId, methods};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn wait_until_finished(state: &DaemonState, session_id: &SessionId) {
    while state.has_live_runs(session_id) {
        tokio::task::yield_now().await;
    }
}

async fn wait_for_runtime_event(state: &DaemonState, session_id: &SessionId, seq: u64) {
    while state.session_runtime_event_seq(session_id) != Some(seq) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn cancel_run_hits_matching_live_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    let run_id = FlowRunId(Uuid::now_v7());
    let cancel = CancellationToken::new();
    state
        .register_session_run(
            sid,
            session,
            LiveRun {
                run_id: run_id.clone(),
                flow_name: "hello".into(),
                cancel: cancel.clone(),
                started_at: chrono::Utc::now(),
            },
            "local-daemon",
        )
        .await
        .unwrap();

    assert!(!state.cancel_run(&run_id, "mallory").await.unwrap());
    assert!(!cancel.is_cancelled());

    let req = JsonRpcRequest::new(
        1,
        methods::CANCEL_RUN,
        serde_json::json!({"run_id": run_id}),
    );
    let resp = dispatch(state.clone(), req).await;
    let result = resp.result.expect("cancel_run returns result");
    assert_eq!(result["cancelled"], serde_json::json!(true));
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn finished_run_keeps_the_owned_session_attachable() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);

    let run_id = FlowRunId(Uuid::now_v7());
    state
        .register_session_run(
            sid.clone(),
            session.clone(),
            LiveRun {
                run_id: run_id.clone(),
                flow_name: "hello".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    assert!(state.has_live_runs(&sid));
    assert_eq!(state.session_revision(&sid), Some(1));
    assert!(!state.owns_live_session(&sid, "mallory"));
    assert!(state.owns_live_session(&sid, "alice"));

    assert!(state.finish_run(&sid, &run_id));
    wait_until_finished(&state, &sid).await;
    assert_eq!(state.session_revision(&sid), Some(2));
    assert!(!state.owns_live_session(&sid, "alice"));
    assert!(state.is_authorized_session(&sid, "alice"));
}

#[tokio::test]
async fn finishing_one_run_preserves_other_runs_in_the_same_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    let first = FlowRunId(Uuid::now_v7());
    let second = FlowRunId(Uuid::now_v7());
    for run_id in [first.clone(), second.clone()] {
        state
            .register_session_run(
                sid.clone(),
                session.clone(),
                LiveRun {
                    run_id,
                    flow_name: "hello".into(),
                    cancel: CancellationToken::new(),
                    started_at: chrono::Utc::now(),
                },
                "alice",
            )
            .await
            .unwrap();
    }

    assert!(state.finish_run(&sid, &first));
    while state.has_live_run(&sid, &first) {
        tokio::task::yield_now().await;
    }
    assert!(state.has_live_runs(&sid));
    assert!(!state.cancel_run(&first, "alice").await.unwrap());
    assert!(state.cancel_run(&second, "alice").await.unwrap());
    assert!(state.finish_run(&sid, &second));
    wait_until_finished(&state, &sid).await;
    assert!(state.is_authorized_session(&sid, "alice"));
}

#[tokio::test]
async fn session_actor_projects_durable_events_and_watch_state() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    let run_id = FlowRunId(Uuid::now_v7());
    state
        .register_session_run(
            sid.clone(),
            session.clone(),
            LiveRun {
                run_id: run_id.clone(),
                flow_name: "hello".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();

    let turn_id = atman_runtime::event::TurnId::now();
    session
        .sink()
        .emit(atman_runtime::event::Event::TurnStart { turn_id });
    session.sink().emit(atman_runtime::event::Event::FlowStart {
        run_id: atman_runtime::event::FlowRunId(run_id.0),
        flow_name: "hello".into(),
        parent_run_id: None,
        parent_node_id: None,
        spawned: false,
    });
    wait_for_runtime_event(&state, &sid, 2).await;
    let event_revision = state.session_projection_revision(&sid).unwrap();
    assert!(event_revision.0 > 0);

    session.set_goal(Some("Keep clients convergent".into()));
    while state.session_projection_revision(&sid) == Some(event_revision) {
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn live_snapshot_is_actor_consistent_and_redacted() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new_with_generation(tmp.path().to_path_buf(), "generation-a".into());
    let session = Arc::new(
        atman_runtime::Session::open_with_redactor(
            tmp.path(),
            Some(Arc::new(atman_runtime::redact::Redactor::builtin())),
        )
        .unwrap(),
    );
    let sid = SessionId(session.id().0);
    let run_id = FlowRunId(Uuid::now_v7());
    state
        .register_session_run(
            sid.clone(),
            session.clone(),
            LiveRun {
                run_id: run_id.clone(),
                flow_name: "hello".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    let turn_id = atman_runtime::event::TurnId::now();
    session.sink().emit(atman_runtime::event::Event::UserMsg {
        turn_id: turn_id.clone(),
        flow_run_id: Some(atman_runtime::event::FlowRunId(run_id.0)),
        message: atman_runtime::message::Message::user_text(
            turn_id,
            "token=sk-abcdefghijklmnop1234567890",
        ),
    });
    wait_for_runtime_event(&state, &sid, 1).await;

    let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
    let json = serde_json::to_string(&snapshot).unwrap();
    assert_eq!(snapshot.daemon_generation.0, "generation-a");
    assert_eq!(snapshot.cursor.0, snapshot.projection.revision.0);
    assert!(json.contains("<REDACTED:openai_api_key>"));
    assert!(!json.contains("sk-abcdefghijklmnop"));
    assert!(state.session_snapshot(&sid, "mallory").await.is_err());

    session.set_goal(Some("keep sk-abcdefghijklmnopqrstuvwxyz secret".into()));
    while state.session_projection_revision(&sid) == Some(snapshot.projection.revision) {
        tokio::task::yield_now().await;
    }
    let updates = state
        .session_updates(&sid, "alice", snapshot.cursor, Some(1))
        .await
        .unwrap();
    assert_eq!(updates.events.len(), 1);
    assert_eq!(updates.events[0].daemon_generation.0, "generation-a");
    assert_eq!(updates.events[0].session_id, sid);
    match &updates.events[0].event {
        atman_proto::ServerEvent::ProjectionDelta { delta } => {
            assert_eq!(delta.base_revision, snapshot.projection.revision);
            assert_eq!(delta.revision.0, delta.base_revision.0 + 1);
        }
        event => panic!("expected projection delta, got {event:?}"),
    }
    let updates_json = serde_json::to_string(&updates).unwrap();
    assert!(updates_json.contains("<REDACTED:openai_api_key>"));
    assert!(!updates_json.contains("sk-abcdefghijklmnop"));

    let gap = state
        .session_updates(
            &sid,
            "alice",
            atman_proto::EventCursor(updates.next_cursor.0 + 1),
            None,
        )
        .await
        .unwrap();
    assert!(gap.resync_required.is_some());
}

#[tokio::test]
async fn session_updates_page_in_order_and_report_retention_gaps() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    state
        .register_session_run(
            sid.clone(),
            session.clone(),
            LiveRun {
                run_id: FlowRunId(Uuid::now_v7()),
                flow_name: "hello".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
    assert_eq!(snapshot.projection.runs.len(), 1);
    assert_eq!(
        snapshot.projection.runs[0].state,
        atman_proto::RunLifecycle::Starting
    );
    assert_eq!(
        snapshot.projection.lifecycle,
        atman_proto::SessionLifecycle::Active
    );

    for goal in ["one", "two", "three"] {
        let revision = state.session_projection_revision(&sid).unwrap();
        session.set_goal(Some(goal.into()));
        while state.session_projection_revision(&sid) == Some(revision) {
            tokio::task::yield_now().await;
        }
    }
    let first_page = state
        .session_updates(&sid, "alice", snapshot.cursor, Some(2))
        .await
        .unwrap();
    assert_eq!(first_page.events.len(), 2);
    assert!(first_page.has_more);
    let second_page = state
        .session_updates(&sid, "alice", first_page.next_cursor, Some(2))
        .await
        .unwrap();
    assert_eq!(second_page.events.len(), 1);
    assert!(!second_page.has_more);
    assert!(second_page.resync_required.is_none());

    for index in 0..2_050 {
        let revision = state.session_projection_revision(&sid).unwrap();
        session.set_goal(Some(format!("retention-{index}")));
        while state.session_projection_revision(&sid) == Some(revision) {
            tokio::task::yield_now().await;
        }
    }
    let gap = state
        .session_updates(&sid, "alice", snapshot.cursor, None)
        .await
        .unwrap();
    assert!(gap.events.is_empty());
    assert!(gap.resync_required.is_some());
}

#[tokio::test]
async fn list_sessions_accepts_search_and_limit_query() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(
        1,
        methods::LIST_SESSIONS,
        serde_json::json!({"search": "missing", "limit": 1}),
    );
    let resp = dispatch(state, req).await;
    assert!(resp.error.is_none());
    assert_eq!(resp.result.unwrap(), serde_json::json!([]));
}

#[tokio::test]
async fn cancel_run_missing_returns_false() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(
        1,
        methods::CANCEL_RUN,
        serde_json::json!({"run_id": FlowRunId(Uuid::now_v7())}),
    );
    let resp = dispatch(state, req).await;
    let result = resp.result.expect("returns ok");
    assert_eq!(result["cancelled"], serde_json::json!(false));
}

#[tokio::test]
async fn list_sessions_includes_live_only_entry_as_running() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));

    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    state
        .register_session_run(
            sid.clone(),
            session,
            LiveRun {
                run_id: FlowRunId(Uuid::now_v7()),
                flow_name: "hello".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "local-daemon",
        )
        .await
        .unwrap();

    let req = JsonRpcRequest::new(1, methods::LIST_SESSIONS, serde_json::json!({}));
    let resp = dispatch(state, req).await;
    let arr = resp.result.expect("ok").as_array().unwrap().clone();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "running");
    assert_eq!(arr[0]["id"], serde_json::to_value(sid).unwrap());
    assert_eq!(arr[0]["title"], "Untitled session");
}

#[tokio::test]
async fn rename_session_updates_metadata_and_list_summary() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let sid = SessionId(Uuid::now_v7());
    let dir = tmp.path().join("sessions").join(sid.0.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    atman_runtime::session_meta::SessionMeta::default()
        .save(&dir)
        .unwrap();

    let req = JsonRpcRequest::new(
        1,
        methods::RENAME_SESSION,
        serde_json::json!({"session_id": sid, "title": "Login fix"}),
    );
    let resp = dispatch(state, req).await;
    let summary = resp.result.expect("rename returns summary");
    assert_eq!(summary["title"], "Login fix");
    assert_eq!(summary["name_source"], "user");
}
