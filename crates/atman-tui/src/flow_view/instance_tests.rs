use std::{collections::BTreeMap, num::NonZeroUsize};

use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnLease};
use atman_runtime::{
    event::{AtmanUuid, Event},
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
        RuntimeStore, SessionDescriptor,
        completion::{InvocationResult, StoredValue},
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, InputContent, SenderIdentity},
        publication::MessagePublication,
        trace::{ExecutionEvent, ExecutionScope},
    },
    message::{Message, MessageOrigin, MessagePart, MessageRole},
    projection::flow::FlowTranscript,
    source_program::RevisionBundle,
};
use uuid::Uuid;

use super::{FlowViewModel, ToolDisclosureAction};
use crate::app::{Disclosure, OutputItem, ToolCallStatus};

fn start(store: &mut FlowStore) -> TurnLease<AtmanUuid> {
    store
        .transition(
            store.state().revision(),
            FlowTransition::TurnStarted {
                turn: TurnId::now(),
                run: RunId::now(),
            },
        )
        .unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = store.state().state() else {
        panic!("turn not active")
    };
    active.lease.clone()
}

fn publish(
    store: &mut FlowStore,
    lease: &TurnLease<AtmanUuid>,
    run: &RunId<AtmanUuid>,
    message: Message,
) {
    store
        .publish_message(MessagePublication {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: run.clone(),
            context_run: None,
            message,
        })
        .unwrap();
}

fn sync(
    store: &mut FlowStore,
    source: &mut FlowTranscript,
    main: &mut FlowViewModel,
    floating: &mut FlowViewModel,
) {
    loop {
        let page = store
            .read_updates(source.cursor(), NonZeroUsize::new(2).unwrap())
            .unwrap();
        for batch in page.batches {
            source.apply(batch).unwrap();
            main.sync(source, None).unwrap();
            floating.sync(source, None).unwrap();
        }
        if !page.has_more {
            break;
        }
    }
}

fn call_status(view: &FlowViewModel, index: usize) -> ToolCallStatus {
    let OutputItem::ToolDispatch { calls } = &view[index] else {
        panic!("tool block missing")
    };
    calls[0].status
}

fn welcome_card() -> crate::app::StartupCard {
    crate::app::StartupCard {
        version: "test".into(),
        recent: vec![],
        directory: crate::app::StartupDirectory::Projects,
    }
}

#[test]
fn archived_views_reuse_history_rendering_without_writers_or_execution() {
    use atman_runtime::{event_log::legacy::LegacyFlowKey, flow_store::migration};
    use serde_json::json;

    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.jsonl");
    let old = Uuid::now_v7();
    let root_run = RunId::<AtmanUuid>::now();
    let child_run = RunId::<AtmanUuid>::now();
    let turn = TurnId::now();
    let mut final_message = Message::assistant_text(turn.clone(), "Child final reply");
    final_message.origin = MessageOrigin::FinalAnswer;
    let mut raw = Vec::new();
    for (index, event) in [
        json!({"type":"flow_start", "run_id":root_run, "flow_name":"agent", "spawned":false}),
        json!({"type":"user_msg", "turn_id":turn, "message": Message::user_text(turn.clone(), "Parent request")}),
        json!({"type":"flow_start", "run_id":child_run, "flow_name":"child", "parent_run_id":root_run, "spawned":true}),
        json!({"type":"assistant_msg", "turn_id":turn, "flow_run_id":child_run, "message":Message::assistant_text(turn.clone(), "Child working reply")}),
        json!({"type":"assistant_msg", "turn_id":turn, "flow_run_id":child_run, "message":final_message}),
        serde_json::to_value(Event::FlowEnd {
            run_id: child_run,
            flow_name: "child".into(),
            status: atman_runtime::event::FlowStatus::Ok,
        }).unwrap(),
    ].into_iter().enumerate() {
        let mut event = event;
        event["seq"] = json!(index + 1);
        // Missing timestamps are intentional and must not become the current time.
        serde_json::to_writer(&mut raw, &event).unwrap();
        raw.push(b'\n');
    }
    raw.extend_from_slice(b"invalid retained line\n\xfftorn");
    std::fs::write(&source, &raw).unwrap();
    let history = atman_runtime::projection::flow::LegacyHistory::inspect(&source, old).unwrap();
    let originals = history
        .flows()
        .iter()
        .map(|key| history.read_flow(key).unwrap())
        .collect::<Vec<_>>();
    let runtime = RuntimeStore::open(temp.path()).unwrap();
    let id = migration::destination_id(&source, old).unwrap();
    let migrated = runtime
        .archive_legacy(
            &source,
            old,
            SessionDescriptor {
                id,
                title: "Archive".into(),
                project: None,
                configuration: Default::default(),
            },
            true,
        )
        .unwrap();
    std::fs::remove_file(&source).unwrap();
    let session = runtime.open_session(id).unwrap();
    let inspection = RuntimeStore::inspect_session(temp.path(), id).unwrap();
    for mapping in &migrated.flows {
        // A reader must work even while another holder owns both journals.
        let writer = session.open_flow(&mapping.instance).unwrap();
        let path = writer.dir().join("events.jsonl");
        let before = std::fs::read(&path).unwrap();
        let view = inspection
            .archived_view(&mapping.instance)
            .unwrap()
            .unwrap();
        assert_eq!(
            view.snapshot().status.execution.state(),
            &InstanceState::Closed
        );
        assert!(view.snapshot().pending.is_empty());
        assert!(view.snapshot().status.goal.is_none());
        let archive = view.transcript().archived_transcript().unwrap();
        for entry in &archive.entries {
            if let atman_runtime::TranscriptEntry::FlowStart { spawned, ts, .. } = entry {
                assert!(!spawned);
                assert_eq!(*ts, None);
            }
        }
        let mut main = FlowViewModel::default();
        let mut floating = FlowViewModel::default();
        main.sync_view(&view, None).unwrap();
        floating.sync_view(&view, None).unwrap();
        let original = originals
            .iter()
            .find(|original| original.owner() == &mapping.owner)
            .unwrap();
        let mut original_main = FlowViewModel::default();
        let mut original_floating = FlowViewModel::default();
        assert!(original_main.sync_legacy(original, None).unwrap());
        assert!(original_floating.sync_legacy(original, None).unwrap());
        assert!(original_main.is_archive());
        assert!(original_main.flow().is_none());
        assert!(original_main.cursor().is_none());
        assert!(original_main.snapshot().is_none());
        assert!(original_main.sync_view(&view, None).is_err());
        assert!(main.sync_legacy(original, None).is_err());
        assert_eq!(original_main.len(), main.len());
        assert_eq!(original_main.work_folds.len(), main.work_folds.len());
        assert!(
            !main
                .iter()
                .any(|item| matches!(item, OutputItem::SubAgentActivity { .. }))
        );
        if matches!(mapping.owner, LegacyFlowKey::SpawnedRun { .. }) {
            assert!(main.iter().any(|item| matches!(item, OutputItem::AssistantMd { md, .. } if md == "Child final reply")));
            assert!(main.toggle_latest_work_fold());
            assert!(main.work_folds.last().unwrap().is_expanded());
            assert!(!floating.work_folds.last().unwrap().is_expanded());
            assert!(original_main.toggle_latest_work_fold());
            assert!(original_main.work_folds.last().unwrap().is_expanded());
            assert!(!original_floating.work_folds.last().unwrap().is_expanded());
        }
        if mapping.owner == LegacyFlowKey::Unassigned {
            assert_eq!(archive.unparsed.len(), 2);
            assert!(main.iter().any(|item| matches!(item, OutputItem::SystemNote { text, .. } if text.contains("invalid retained line"))));
            assert!(original_main.iter().any(|item| matches!(item, OutputItem::SystemNote { text, .. } if text.contains("invalid retained line"))));
        }
        let revisions = main.revisions().to_vec();
        assert!(!main.sync_view(&view, None).unwrap());
        assert_eq!(main.revisions(), revisions);
        let revisions = original_main.revisions().to_vec();
        assert!(!original_main.sync_legacy(original, None).unwrap());
        assert_eq!(original_main.revisions(), revisions);
        original_main.document.replace(vec![]);
        assert!(original_main.sync_legacy(original, None).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[tokio::test]
async fn live_output_preserves_committed_state_and_each_views_disclosure() {
    use atman_runtime::flow_runtime::{
        FlowWorker,
        live::{CallDraft, DraftPart, LiveOutputSnapshot},
    };
    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let session_id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id: session_id,
            configuration: Default::default(),
            project: None,
            title: "Example".into(),
        })
        .unwrap();
    let flow = FlowRef {
        session_id: session_id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut store = session
        .create_flow(FlowDescriptor {
            address: flow.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() {}".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let lease = start(&mut store);
    publish(
        &mut store,
        &lease,
        &lease.run,
        Message::assistant_text(lease.turn.clone(), "Committed text"),
    );
    let worker = FlowWorker::start(store).await.unwrap();
    let mut reader = worker
        .subscribe_view(NonZeroUsize::new(8).unwrap())
        .await
        .unwrap();
    let view = reader.next().await.unwrap();
    let mut main = FlowViewModel::default();
    let mut floating = FlowViewModel::default();
    let mut app = crate::app::AppState::new(session_id.to_string(), None);
    app.startup_card = Some(welcome_card());
    app.sync_flow_view(view).unwrap();
    assert!(
        app.startup_card.is_some(),
        "a startup greeting alone retains the welcome page"
    );
    main.sync_view(view, None).unwrap();
    floating.sync_view(view, None).unwrap();
    let before = main.revisions().to_vec();
    let cursor = main.cursor().cloned();
    let long_answer = "x".repeat(20_000);
    let mut snapshot = LiveOutputSnapshot {
        flow: flow.clone(),
        revision: 1,
        calls: vec![CallDraft {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: lease.run.clone(),
            model: "example".into(),
            attempt: 1,
            revision: 1,
            parts: vec![
                DraftPart::Thinking("Thought".into()),
                DraftPart::Tool {
                    index: 0,
                    call_id: "call".into(),
                    name: "final.answer".into(),
                    arguments: format!("{{\"message\":\"{long_answer}"),
                },
            ],
        }],
    };
    main.sync_live(snapshot.clone()).unwrap();
    floating.sync_live(snapshot.clone()).unwrap();
    let thinking = before.len() + 1;
    main.mutate(thinking, super::OutputMutation::Interaction, |item| {
        let OutputItem::Thinking { disclosure, .. } = item else {
            panic!("thinking missing")
        };
        *disclosure = Disclosure::Full;
        true
    });
    let draft_id = main.revisions().last().unwrap().id;
    let structure = main.structure_revision();
    snapshot.revision += 1;
    snapshot.calls[0].revision += 1;
    let DraftPart::Tool { arguments, .. } = &mut snapshot.calls[0].parts[1] else {
        unreachable!()
    };
    arguments.push_str("end\"}");
    main.sync_live(snapshot.clone()).unwrap();
    floating.sync_live(snapshot.clone()).unwrap();
    assert_eq!(main.revisions().last().unwrap().id, draft_id);
    assert_eq!(
        main.structure_revision(),
        structure,
        "token deltas must not reorder the document"
    );
    assert_eq!(main.cursor(), cursor.as_ref());
    assert_eq!(
        main.snapshot().unwrap().status.cursor,
        cursor.clone().unwrap()
    );
    assert_eq!(&main.revisions()[..before.len()], before.as_slice());
    assert!(matches!(
        &main[thinking],
        OutputItem::Thinking {
            disclosure: Disclosure::Full,
            ..
        }
    ));
    assert!(matches!(
        &floating[thinking],
        OutputItem::Thinking {
            disclosure: Disclosure::Summary,
            ..
        }
    ));
    assert!(
        matches!(main.last(), Some(OutputItem::AssistantMd { md, streaming: true, .. }) if md == &format!("{long_answer}end"))
    );
    assert!(main.source_record(main.len() - 1).is_none());
    assert!(
        main.work_folds.is_empty(),
        "a final-answer preview is not a final publication"
    );
    let revision = main.revision_clock();
    let mut foreign = snapshot.clone();
    foreign.flow.instance_id = FlowInstanceId::now();
    assert!(main.sync_live(foreign).is_err());
    assert_eq!(main.revision_clock(), revision);

    // A new committed boundary preserves preview identity and interaction state.
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused {
                key: Uuid::now_v7().into(),
            },
        )
        .await
        .unwrap();
    main.sync_view(reader.next().await.unwrap(), None).unwrap();
    assert_eq!(main.revisions().last().unwrap().id, draft_id);
    assert!(matches!(
        &main[thinking],
        OutputItem::Thinking {
            disclosure: Disclosure::Full,
            ..
        }
    ));
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::TurnFinished {
                lease,
                outcome: atman_rt::TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    main.sync_view(reader.next().await.unwrap(), None).unwrap();
    assert!(!main.has_live_output());
    let count = main.len();
    snapshot.revision += 1;
    main.sync_live(snapshot.clone()).unwrap();
    assert_eq!(
        main.len(),
        count,
        "a late preview cannot reopen a completed turn"
    );
    snapshot.revision += 1;
    snapshot.calls.clear();
    main.sync_live(snapshot.clone()).unwrap();
    snapshot.revision -= 1;
    assert!(!main.sync_live(snapshot).unwrap());
    assert!(main.snapshot().is_some());
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn native_views_share_committed_history_but_keep_disclosure_and_scoped_results() {
    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let session_id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id: session_id,
            configuration: Default::default(),
            project: None,
            title: "Example".into(),
        })
        .unwrap();
    let address = FlowRef {
        session_id: session_id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut store = session
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "/example/main.at".into(),
                bundle: RevisionBundle {
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
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let mut source = FlowTranscript::new(address.clone());
    let mut main = FlowViewModel::default();
    let mut floating = FlowViewModel::default();
    let mut first_end = 0;
    let mut sequence = 0;
    let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: "hi".into(),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "selected text".into(),
        }),
    };
    for turn in 0..2 {
        let lease = start(&mut store);
        store
            .accept_input(
                DeliveryRequest {
                    request_id: Uuid::now_v7(),
                    sender: SenderIdentity::User(Uuid::now_v7()),
                    recipient: address.clone(),
                    delivery: DeliveryMode::AtBoundary,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: if turn == 0 {
                        InboxPayload::UserSubmission {
                            content: atman_runtime::flow_store::inbox::UserContent {
                                text: presentation.model_text(),
                                images: vec![],
                                presentation: Some(presentation.clone()),
                            },
                            invocation: Default::default(),
                        }
                    } else {
                        InboxPayload::Interjection {
                            text: "continue existing goal".into(),
                        }
                    },
                },
                |_| Ok(()),
            )
            .unwrap();
        let reservation = store
            .reserve_context_input(&lease, |_| true)
            .unwrap()
            .unwrap();
        let mut input = atman_runtime::flow_runtime::input::context_message(
            store.pending_inputs().next().unwrap(),
            &lease.turn,
        )
        .unwrap();
        if turn == 1 {
            input.origin = MessageOrigin::Internal;
        }
        store
            .commit_input(
                &reservation,
                InputContent::Context {
                    messages: vec![input],
                },
            )
            .unwrap();
        let runs = [lease.run.clone(), RunId::now()];
        for run in &runs {
            sequence += 1;
            store
                .append_execution_events(vec![ExecutionEvent {
                    scope: ExecutionScope::Invocation(lease.clone()),
                    sequence,
                    timestamp: at,
                    event: Event::FlowStart {
                        run_id: run.clone(),
                        flow_name: "worker".into(),
                        parent_run_id: Some(RunId::now()),
                        parent_node_id: None,
                        spawned: true,
                    },
                }])
                .unwrap();
            publish(
                &mut store,
                &lease,
                run,
                Message {
                    role: MessageRole::Assistant,
                    origin: MessageOrigin::Internal,
                    turn_id: lease.turn.clone(),
                    parts: vec![MessagePart::ToolUse {
                        id: "same-id".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({"path":"notes.txt"}),
                        intent: None,
                    }],
                },
            );
        }
        sync(&mut store, &mut source, &mut main, &mut floating);
        let calls = main
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                matches!(item, OutputItem::ToolDispatch { .. }).then_some(index)
            })
            .collect::<Vec<_>>();
        let first = calls[calls.len() - 2];
        let second = calls[calls.len() - 1];
        let first_id = main.revisions()[first].id;
        main.toggle_tool_disclosure(first, "same-id", ToolDisclosureAction::Content);
        let main_disclosure = match &main[first] {
            OutputItem::ToolDispatch { calls } => calls[0].disclosure,
            _ => unreachable!(),
        };
        assert_ne!(main_disclosure, Disclosure::Summary);
        assert!(
            matches!(&floating[first], OutputItem::ToolDispatch { calls } if calls[0].disclosure == Disclosure::Summary)
        );
        publish(
            &mut store,
            &lease,
            &runs[0],
            Message {
                role: MessageRole::Tool,
                origin: MessageOrigin::Internal,
                turn_id: lease.turn.clone(),
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "same-id".into(),
                    content: "file contents".into(),
                    is_error: false,
                }],
            },
        );
        sync(&mut store, &mut source, &mut main, &mut floating);
        assert_eq!(call_status(&main, first), ToolCallStatus::Ok);
        assert_eq!(call_status(&main, second), ToolCallStatus::Running);
        assert_eq!(main.revisions()[first].id, first_id);
        assert!(
            matches!(&main[first], OutputItem::ToolDispatch { calls } if calls[0].disclosure == main_disclosure)
        );
        publish(
            &mut store,
            &lease,
            &runs[0],
            Message {
                role: MessageRole::Assistant,
                origin: MessageOrigin::FinalAnswer,
                turn_id: lease.turn.clone(),
                parts: vec![MessagePart::Text {
                    text: format!("answer {turn}"),
                }],
            },
        );
        store
            .finish_turn(lease, InvocationResult::Returned(StoredValue::Unit))
            .unwrap();
        sync(&mut store, &mut source, &mut main, &mut floating);
        assert_eq!(call_status(&main, second), ToolCallStatus::Error);
        assert!(!main.has_active_animation());
        let start = main
            .iter()
            .rev()
            .find_map(|item| match item {
                OutputItem::WorkFoldMarker { start_index, .. } => *start_index,
                _ => None,
            })
            .unwrap();
        if turn == 0 {
            assert_eq!(start, 1);
            first_end = main.len();
        } else {
            assert!(start >= first_end);
        }
    }
    assert_eq!(
        main.iter()
            .filter(|item| matches!(item, OutputItem::UserTurn { .. }))
            .count(),
        1
    );
    assert_eq!(
        main.iter()
            .filter(|item| matches!(item, OutputItem::WorkflowPanel { .. }))
            .count(),
        2
    );
    let revision = main.revision_clock();
    assert!(
        matches!(&main[0], OutputItem::UserTurn { presentation: Some(value), .. } if value == &presentation)
    );
    assert!(!main.sync(&source, None).unwrap());
    assert_eq!(main.revision_clock(), revision);
    assert_eq!(main.len(), floating.len());
    assert_eq!(
        main.work_fold_projections(None),
        floating.work_fold_projections(None)
    );
    assert_eq!(main.work_fold_projections(None).len(), 2);
    assert!(
        main.work_fold_projections(None)
            .iter()
            .all(|fold| fold.total_steps == 2)
    );
    assert!(main.toggle_latest_work_fold());
    assert!(main.work_fold_projections(None).last().unwrap().expanded);
    assert!(
        !floating
            .work_fold_projections(None)
            .last()
            .unwrap()
            .expanded
    );
    assert!(!main.sync(&source, None).unwrap());
    assert!(main.work_fold_projections(None).last().unwrap().expanded);
    for index in 0..main.len() {
        assert_eq!(main.source_record(index), floating.source_record(index));
    }
    let cursor = main.cursor().cloned();
    let foreign = FlowTranscript::new(FlowRef {
        instance_id: FlowInstanceId::now(),
        ..address
    });
    assert!(main.sync(&foreign, None).is_err());
    assert_eq!(main.cursor(), cursor.as_ref());
    assert_eq!(main.revision_clock(), revision);
    let mut replay = FlowViewModel::default();
    replay.sync(&source, None).unwrap();
    assert_eq!(replay.len(), main.len());
    for (a, b) in main.iter().zip(replay.iter()) {
        match (a, b) {
            (
                OutputItem::WorkflowPanel { graph: a, .. },
                OutputItem::WorkflowPanel { graph: b, .. },
            ) => assert_eq!(a, b),
            (OutputItem::AssistantMd { md: a, .. }, OutputItem::AssistantMd { md: b, .. }) => {
                assert_eq!(a, b)
            }
            (
                OutputItem::WorkFoldMarker { start_index: a, .. },
                OutputItem::WorkFoldMarker { start_index: b, .. },
            ) => assert_eq!(a, b),
            _ => assert_eq!(std::mem::discriminant(a), std::mem::discriminant(b)),
        }
    }
    // A legacy replacement cannot silently invalidate native record indices.
    replay.replace(vec![OutputItem::Divider]);
    assert!(replay.sync(&source, None).is_err());
    assert_eq!(replay.len(), 1);

    use atman_runtime::{
        flow_runtime::FlowWorker,
        flow_store::{
            goal::GoalRequest,
            planning::{PlanningEdit, PlanningRequest},
        },
        memory::goal::GoalEdit,
    };
    let worker = FlowWorker::start(store).await.unwrap();
    let mut reader = worker
        .subscribe_view(NonZeroUsize::new(2).unwrap())
        .await
        .unwrap();
    let view = reader.next().await.unwrap();
    assert!(main.sync_view(view, None).unwrap());
    assert!(floating.sync_view(view, None).unwrap());
    assert!(std::sync::Arc::ptr_eq(
        main.snapshot().unwrap(),
        floating.snapshot().unwrap()
    ));
    assert_eq!(main.revision_clock(), revision);
    assert!(main.work_fold_projections(None).last().unwrap().expanded);
    assert!(
        !floating
            .work_fold_projections(None)
            .last()
            .unwrap()
            .expanded
    );
    assert!(!main.sync_view(view, None).unwrap());

    worker
        .edit_goal(
            GoalRequest {
                id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected: None,
                edit: GoalEdit::Set {
                    text: "Review current output".into(),
                    replace_completed: false,
                },
                start_turn: false,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    worker
        .edit_planning(
            PlanningRequest {
                id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected_revision: None,
                edit: PlanningEdit::WritePlan {
                    id: "review".into(),
                    title: "Review".into(),
                    steps: vec!["Inspect".into()],
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    worker
        .accept_input(
            DeliveryRequest {
                request_id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: worker.address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                scope: None,
                correlation: None,
                payload: InboxPayload::Interjection {
                    text: "Pending input".into(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let view = reader.next().await.unwrap();
    assert!(main.sync_view(view, None).unwrap());
    assert!(floating.sync_view(view, None).unwrap());
    assert_eq!(
        main.revision_clock(),
        revision,
        "state-only updates must retain document layout"
    );
    assert_eq!(main.cursor(), Some(&main.snapshot().unwrap().status.cursor));
    assert!(std::sync::Arc::ptr_eq(
        main.snapshot().unwrap(),
        floating.snapshot().unwrap()
    ));
    assert_eq!(
        main.snapshot()
            .unwrap()
            .status
            .goal
            .as_ref()
            .unwrap()
            .objective,
        "Review current output"
    );
    assert_eq!(main.snapshot().unwrap().planning.plans.len(), 1);
    assert_eq!(main.snapshot().unwrap().pending.len(), 1);
    let cursor = main.cursor().cloned();
    assert!(main.sync(&source, None).is_err());
    assert_eq!(main.cursor(), cursor.as_ref());
    assert_eq!(main.snapshot().unwrap().status.cursor, cursor.unwrap());

    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused {
                key: Uuid::now_v7().into(),
            },
        )
        .await
        .unwrap();
    let view = reader.next().await.unwrap();
    main.sync(view.transcript(), None).unwrap();
    assert!(
        main.snapshot().is_none(),
        "document-only advance must not retain stale control state"
    );
    assert!(main.sync_view(view, None).unwrap());
    assert_eq!(main.snapshot().unwrap().status.execution.pauses().len(), 1);
    let mut app = crate::app::AppState::new(session_id.to_string(), None)
        .with_session(Some(worker.context()));
    app.startup_card = Some(welcome_card());
    app.sync_flow_view(view).unwrap();
    assert!(
        app.startup_card.is_none(),
        "restored user turns must remain visible"
    );
    let version = app.items_version;
    let original_count = app.items.len();
    app.push_note("local control acknowledgement", crate::app::NoteLevel::Info);
    app.cancel_running_activities();
    assert_eq!(app.items.len(), original_count);
    assert!(app.items.snapshot().is_some());
    assert_eq!(app.plans.len(), 1);
    let goal = app.goal_record().unwrap();
    assert!(
        matches!(app.goal_toggle_control(), Some(crate::TuiControl::GoalControl { action: atman_runtime::goal::GoalAction::Pause, goal_id: Some(id), revision: Some(revision), .. }) if id == goal.id && revision == goal.revision)
    );
    app.sync_flow_view(view).unwrap();
    assert_eq!(
        app.items_version, version,
        "same snapshot must retain document layout"
    );
    floating.replace(vec![OutputItem::Divider]);
    assert!(floating.snapshot().is_none());
    assert!(floating.sync_view(view, None).is_err());
    assert!(main.snapshot().is_some());
    drop(reader);
    worker.shutdown().await.unwrap();
}

#[test]
fn empty_views_still_bind_to_their_source_identity() {
    let source = FlowTranscript::new(FlowRef {
        session_id: Uuid::now_v7().into(),
        instance_id: FlowInstanceId::now(),
    });
    let mut view = FlowViewModel::default();
    assert!(!view.sync(&source, None).unwrap());
    assert_eq!(view.flow(), Some(source.flow()));
    let foreign = FlowTranscript::new(FlowRef {
        instance_id: FlowInstanceId::now(),
        ..source.flow().clone()
    });
    assert!(view.sync(&foreign, None).is_err());
    assert_eq!(view.flow(), Some(source.flow()));

    use atman_runtime::{event_log::legacy::LegacyFlowKey, projection::flow::LegacyHistory};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let session = Uuid::now_v7();
    std::fs::write(&path, b"").unwrap();
    let original = LegacyHistory::inspect(&path, session)
        .unwrap()
        .read_flow(&LegacyFlowKey::Default)
        .unwrap();
    assert!(view.sync_legacy(&original, None).is_err());
    let mut legacy = FlowViewModel::default();
    assert!(legacy.sync_legacy(&original, None).unwrap());
    assert!(legacy.is_empty());
    assert!(legacy.flow().is_none());
    assert!(legacy.cursor().is_none());
    assert!(legacy.snapshot().is_none());
    assert!(legacy.sync(&source, None).is_err());
    std::fs::write(&path, b"\n").unwrap();
    let changed = LegacyHistory::inspect(&path, session)
        .unwrap()
        .read_flow(&LegacyFlowKey::Default)
        .unwrap();
    assert!(legacy.sync_legacy(&changed, None).is_err());
    assert!(!legacy.sync_legacy(&original, None).unwrap());
}
