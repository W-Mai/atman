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
            sid.clone(),
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

    assert!(state.cancel_run(&sid, &run_id, "mallory").await.is_err());
    assert!(!cancel.is_cancelled());

    let request_id = atman_proto::RequestId::now();
    let command = atman_proto::CancelRunRequest {
        request_id: Some(request_id),
        session_id: sid.clone(),
        run_id: run_id.clone(),
    };
    let req = JsonRpcRequest::for_method::<atman_proto::rpc::CancelRun>(1, &command).unwrap();
    let resp = dispatch(state.clone(), req).await;
    let result: atman_proto::CancelRunResponse =
        serde_json::from_value(resp.result.expect("cancel_run returns result")).unwrap();
    assert!(result.cancelled);
    assert_eq!(result.status, atman_proto::RunCancellationStatus::Accepted);
    assert_eq!(result.session_id, sid);
    assert!(cancel.is_cancelled());
    let snapshot = state.session_snapshot(&sid, "local-daemon").await.unwrap();
    assert_eq!(snapshot.cursor, result.cursor);
    assert_eq!(snapshot.projection.revision, result.revision);
    assert_eq!(
        snapshot.projection.runs[0].state,
        atman_proto::RunLifecycle::Cancelling
    );

    assert!(state.finish_run(&sid, &run_id));
    wait_until_finished(&state, &sid).await;
    let retry = dispatch(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::CancelRun>(2, &command).unwrap(),
    )
    .await;
    let retry: atman_proto::CancelRunResponse =
        serde_json::from_value(retry.result.unwrap()).unwrap();
    assert_eq!(retry.status, atman_proto::RunCancellationStatus::Accepted);
    assert_eq!(retry.cursor, result.cursor);
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
    assert_eq!(
        state
            .cancel_run(&sid, &first, "alice")
            .await
            .unwrap()
            .status,
        atman_proto::RunCancellationStatus::NotFound
    );
    assert_eq!(
        state
            .cancel_run(&sid, &second, "alice")
            .await
            .unwrap()
            .status,
        atman_proto::RunCancellationStatus::Accepted
    );
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
async fn session_actor_projects_pending_forms_until_submission() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    state
        .register_session(sid.clone(), session.clone(), "alice")
        .await
        .unwrap();

    let form_id = "form-1".to_string();
    let response = session.forms().request(atman_runtime::form::PendingForm {
        form_id: form_id.clone(),
        run_id: atman_runtime::event::FlowRunId(Uuid::now_v7()),
        tool_use_id: "tool-1".into(),
        form: atman_runtime::form::CompositeForm {
            questions: vec![atman_runtime::form::FormQuestion {
                id: "target".into(),
                kind: atman_runtime::form::FormKind::Text {
                    prompt: "Target directory".into(),
                    placeholder: Some("/tmp/example".into()),
                    multiline: false,
                },
            }],
        },
        kind: atman_runtime::form::FormKind::Text {
            prompt: "Target directory".into(),
            placeholder: Some("/tmp/example".into()),
            multiline: false,
        },
        emitted_at: chrono::Utc::now(),
    });

    let projected = loop {
        let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
        if let Some(form) = snapshot.projection.interactions.forms.first() {
            break form.clone();
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(projected.id, form_id);
    assert_eq!(projected.questions[0].id, "target");
    assert_eq!(
        projected.questions[0].kind,
        atman_proto::FormQuestionKind::Text
    );
    assert_eq!(
        projected.questions[0].placeholder.as_deref(),
        Some("/tmp/example")
    );

    assert!(session.forms().submit(
        &form_id,
        atman_runtime::form::FormSubmission::Submitted {
            answers: vec![atman_runtime::form::FormAnswer::TextEntered {
                text: "/tmp/target".into(),
            }],
        },
    ));
    assert!(matches!(
        response.await.unwrap(),
        atman_runtime::form::FormSubmission::Submitted { .. }
    ));
    loop {
        let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
        if snapshot.projection.interactions.forms.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn session_actor_projects_compact_review_until_decision() {
    let tmp = tempfile::tempdir().unwrap();
    let state = DaemonState::new(tmp.path().to_path_buf());
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let sid = SessionId(session.id().0);
    state
        .register_session(sid.clone(), session.clone(), "alice")
        .await
        .unwrap();

    let review_id = "review-1".to_string();
    let response =
        session
            .compact_reviews()
            .request(atman_runtime::session::PendingCompactReview {
                review_id: review_id.clone(),
                summary: "summary".into(),
                slice_preview: "preview".into(),
                slice_count: 3,
                range_start: 2,
                range_end: 5,
                tokens_before: 1_024,
                emitted_at: chrono::Utc::now(),
            });

    let projected = loop {
        let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
        if let Some(review) = snapshot.projection.interactions.compact_review {
            break review;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(projected.id, review_id);
    assert_eq!(projected.summary, "summary");
    assert_eq!(projected.slice_count, 3);
    assert_eq!(projected.range_start, 2);
    assert_eq!(projected.range_end, 5);
    assert_eq!(projected.tokens_before, 1_024);

    assert!(session.compact_reviews().decide(
        &review_id,
        atman_runtime::session::CompactReviewDecision::AcceptEdited {
            summary: "edited summary".into(),
        },
    ));
    assert!(matches!(
        response.await.unwrap(),
        atman_runtime::session::CompactReviewDecision::AcceptEdited { summary }
            if summary == "edited summary"
    ));
    loop {
        let snapshot = state.session_snapshot(&sid, "alice").await.unwrap();
        if snapshot.projection.interactions.compact_review.is_none() {
            break;
        }
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
async fn runtime_stream_frames_publish_ordered_ephemeral_signals() {
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
                flow_name: "agent".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    let before = state.session_snapshot(&sid, "alice").await.unwrap();
    let stream = session.stream_tx();
    stream
        .send(atman_runtime::stream::StreamFrame::LlmChunk {
            text: "hello".into(),
            model: "test-model".into(),
            run_id: Some(run_id.to_string()),
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::ThinkingChunk {
            text: "inspect".into(),
            run_id: Some(run_id.to_string()),
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::ToolCallDraft {
            index: 0,
            call_id: "call-1".into(),
            name: "fs.read".into(),
            arguments_delta: "{\"path\":".into(),
            run_id: Some(run_id.clone().to_string()),
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::LlmRetry {
            run_id: Some(run_id.to_string()),
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::Notification(
            atman_runtime::stream::NotificationFrame {
                run_id: Some(run_id.to_string()),
                level: atman_runtime::notify::NotifyLevel::Error,
                location: atman_runtime::notify::NotifyLocation::Inline,
                lifecycle: atman_runtime::notify::NotifyLifecycle::UntilReplaced,
                stack: atman_runtime::notify::NotifyStack::Replace {
                    key: format!("llm-call:{run_id}:node"),
                },
                message: "request failed".into(),
            },
        ))
        .unwrap();

    let updates = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let updates = state
                .session_updates(&sid, "alice", before.cursor, None)
                .await
                .unwrap();
            if updates.events.len() == 5 {
                break updates;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        updates
            .events
            .iter()
            .map(|event| event.cursor.0)
            .collect::<Vec<_>>(),
        vec![
            before.cursor.0 + 1,
            before.cursor.0 + 2,
            before.cursor.0 + 3,
            before.cursor.0 + 4,
            before.cursor.0 + 5
        ]
    );
    assert!(matches!(
        &updates.events[0].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::LlmText { run_id: id, text }
        } if id == &run_id && text == "hello"
    ));
    assert!(matches!(
        &updates.events[1].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::Thinking { run_id: id, text }
        } if id == &run_id && text == "inspect"
    ));
    assert!(matches!(
        &updates.events[2].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::ToolCallDraft { run_id: id, name, .. }
        } if id == &run_id && name == "fs.read"
    ));
    assert!(matches!(
        &updates.events[3].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::LlmRetry { run_id: id }
        } if id == &run_id
    ));
    assert!(matches!(
        &updates.events[4].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::Notification { notification }
        } if notification.run_id.as_ref() == Some(&run_id)
            && notification.level == atman_proto::NoticeLevel::Error
            && notification.message == "request failed"
    ));
    let after = state.session_snapshot(&sid, "alice").await.unwrap();
    assert_eq!(after.projection.revision, before.projection.revision);
    assert_eq!(after.cursor, updates.next_cursor);
}

#[tokio::test]
async fn process_signal_follows_its_durable_resource_projection() {
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
                flow_name: "agent".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    let before = state.session_snapshot(&sid, "alice").await.unwrap();
    let task_id = state.task_registry().register(
        atman_runtime::TaskKind::Bash,
        "build".into(),
        "bg-1".into(),
        atman_runtime::TaskOwner::new(
            sid.to_string(),
            Some(atman_runtime::event::FlowRunId(run_id.0)),
        ),
        CancellationToken::new(),
    );
    session
        .stream_tx()
        .send(atman_runtime::stream::StreamFrame::BashChunk {
            handle: "bg-1".into(),
            tool_use_id: None,
            kind: "stdout".into(),
            line: "building\n".into(),
            call_intent: None,
            run_id: None,
        })
        .unwrap();

    let updates = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let updates = state
                .session_updates(&sid, "alice", before.cursor, None)
                .await
                .unwrap();
            if updates.events.len() >= 2 {
                break updates;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let resource_id = atman_proto::ResourceId(format!("task:{task_id}"));
    let resource_index = updates
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                atman_proto::ServerEvent::ProjectionDelta { delta }
                    if delta.changes.iter().any(|change| matches!(
                        change,
                        atman_proto::ProjectionChange::ResourceUpsert { resource }
                            if resource.id == resource_id
                    ))
            )
        })
        .unwrap();
    let signal_index = updates
        .events
        .iter()
        .position(|event| {
            matches!(
                &event.event,
                atman_proto::ServerEvent::Signal {
                    signal: atman_proto::SessionSignal::ProcessLine {
                        resource_id: id,
                        stream,
                        line,
                    }
                } if id == &resource_id && stream == "stdout" && line == "building\n"
            )
        })
        .unwrap();
    assert!(resource_index < signal_index);
}

#[tokio::test]
async fn compaction_progress_is_session_scoped_and_ephemeral() {
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
                run_id,
                flow_name: "agent".into(),
                cancel: CancellationToken::new(),
                started_at: chrono::Utc::now(),
            },
            "alice",
        )
        .await
        .unwrap();
    let before = state.session_snapshot(&sid, "alice").await.unwrap();
    let stream = session.stream_tx();
    stream
        .send(atman_runtime::stream::StreamFrame::CompactionSummary {
            phase: atman_runtime::stream::CompactionPhase::Running,
            range_start: 2,
            range_end: 8,
            summary: String::new(),
            before_tokens: 10_000,
            after_tokens: 0,
            compacted_count: 7,
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::CompactionDelta {
            range_start: 2,
            range_end: 8,
            text: "summary chunk".into(),
        })
        .unwrap();
    stream
        .send(atman_runtime::stream::StreamFrame::CompactionSummary {
            phase: atman_runtime::stream::CompactionPhase::Failed,
            range_start: 2,
            range_end: 8,
            summary: "review rejected".into(),
            before_tokens: 10_000,
            after_tokens: 10_000,
            compacted_count: 7,
        })
        .unwrap();

    let updates = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            let updates = state
                .session_updates(&sid, "alice", before.cursor, None)
                .await
                .unwrap();
            if updates.events.len() == 3 {
                break updates;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        &updates.events[0].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::CompactionStarted {
                range_start: 2,
                range_end: 8,
                before_tokens: 10_000,
                compacted_count: 7,
            }
        }
    ));
    assert!(matches!(
        &updates.events[1].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::CompactionText { text, .. }
        } if text == "summary chunk"
    ));
    assert!(matches!(
        &updates.events[2].event,
        atman_proto::ServerEvent::Signal {
            signal: atman_proto::SessionSignal::CompactionFailed { reason, .. }
        } if reason == "review rejected"
    ));
    let after = state.session_snapshot(&sid, "alice").await.unwrap();
    assert_eq!(after.projection.revision, before.projection.revision);
    assert_eq!(after.cursor, updates.next_cursor);
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
async fn cancel_run_missing_session_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let req = JsonRpcRequest::new(
        1,
        methods::CANCEL_RUN,
        serde_json::json!({
            "session_id": SessionId(Uuid::now_v7()),
            "run_id": FlowRunId(Uuid::now_v7())
        }),
    );
    let resp = dispatch(state, req).await;
    assert!(resp.error.is_some());
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
    let session = Arc::new(atman_runtime::Session::open(tmp.path()).unwrap());
    let sid = SessionId(session.id().0);
    state
        .register_session(sid.clone(), session, "local-daemon")
        .await
        .unwrap();

    let req = JsonRpcRequest::new(
        1,
        methods::RENAME_SESSION,
        serde_json::json!({"session_id": sid, "title": "Login fix"}),
    );
    let resp = dispatch(state.clone(), req).await;
    let response: atman_proto::RenameSessionResponse =
        serde_json::from_value(resp.result.expect("rename returns response")).unwrap();
    assert_eq!(response.session.title, "Login fix");
    assert_eq!(response.session.name_source, atman_proto::NameSource::User);
    let snapshot = state.session_snapshot(&sid, "local-daemon").await.unwrap();
    assert_eq!(response.revision, snapshot.projection.revision);
    assert_eq!(response.cursor, snapshot.cursor);
}
