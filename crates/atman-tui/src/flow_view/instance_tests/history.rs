use super::*;
use atman_runtime::{
    flow_runtime::FlowWorker,
    flow_store::{
        history_operation::{HistoryOperationRequest, HistoryOperationTarget},
        source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection},
    },
};

fn fixture() -> (tempfile::TempDir, FlowStore, HistoryOperationRequest) {
    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let session_id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id: session_id,
            configuration: Default::default(),
            project: None,
            title: "History identity".into(),
        })
        .unwrap();
    let mut store = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: session_id.into(),
                instance_id: FlowInstanceId::now(),
            },
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
    for (role, part) in [
        (
            MessageRole::Assistant,
            MessagePart::Thinking {
                thinking: "retained reasoning".into(),
                signature: None,
            },
        ),
        (
            MessageRole::Assistant,
            MessagePart::ToolUse {
                id: "read".into(),
                name: "fs.read".into(),
                input: serde_json::json!({"path":"notes.txt"}),
                intent: None,
            },
        ),
        (
            MessageRole::Tool,
            MessagePart::ToolResult {
                tool_use_id: "read".into(),
                content: "retained body".into(),
                is_error: false,
            },
        ),
    ] {
        publish(
            &mut store,
            &lease,
            &lease.run,
            Message {
                role,
                origin: MessageOrigin::default(),
                turn_id: lease.turn.clone(),
                parts: vec![part],
            },
        );
    }
    let mut answer = Message::assistant_text(lease.turn.clone(), "retained answer\n\n".repeat(30));
    answer.origin = MessageOrigin::FinalAnswer;
    publish(&mut store, &lease, &lease.run, answer);
    let completion = store
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
        )
        .unwrap();
    let request = HistoryOperationRequest {
        id: Uuid::now_v7(),
        sender: SenderIdentity::User(Uuid::now_v7()),
        flow: store.state().address().clone(),
        expected_head: store.history_head(),
        execution_revision: store.state().revision(),
        active: None,
        goal: None,
        queued: Vec::new(),
        target: HistoryOperationTarget::Revert {
            observed: SourceSelection {
                flow: store.state().address().clone(),
                branch: store.history_head().branch,
                cursor: store.cursor().require_durable().unwrap().clone(),
            },
            cutoff: Box::new(ConversationCutoff::AfterCompletedTurn {
                lease,
                receipt: completion.receipt.require_durable().unwrap().clone(),
            }),
            branch: ConversationBranchId::random(),
        },
    };
    (directory, store, request)
}

#[tokio::test]
async fn authored_turn_actions_keep_admission_identity_through_revert_and_restore() {
    use crate::instance_view::FlowViewSource;
    use atman_runtime::flow_store::{history_operation::HistoryReviewRequest, inbox::UserContent};
    let (_directory, mut store, _) = fixture();
    let flow = store.state().address().clone();
    let user = SenderIdentity::User(Uuid::now_v7());
    let accept = |store: &mut FlowStore, sender: SenderIdentity, delivery, payload| {
        store
            .accept_input(
                DeliveryRequest {
                    expected_head: None,
                    request_id: Uuid::now_v7(),
                    sender,
                    recipient: flow.clone(),
                    delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload,
                },
                |_| Ok(()),
            )
            .unwrap();
    };
    for _ in 0..2 {
        accept(
            &mut store,
            user.clone(),
            DeliveryMode::NextTurn,
            InboxPayload::UserSubmission {
                content: UserContent {
                    text: "same text".into(),
                    images: vec![],
                    presentation: None,
                },
                invocation: Default::default(),
            },
        );
    }
    let batch = store
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = batch.lease().clone();
    let admitted = batch.message_ids().to_vec();
    assert_eq!(admitted.len(), 2);
    let admission = store
        .commit_input(
            &batch,
            InputContent::InvocationInput {
                command: None,
                arguments: Default::default(),
                invocation: Default::default(),
                messages: (0..2)
                    .map(|_| Message::user_text(lease.turn.clone(), "same text"))
                    .collect(),
            },
        )
        .unwrap();
    let mut insertion = None;
    for authored in [true, false] {
        accept(
            &mut store,
            if authored {
                user.clone()
            } else {
                SenderIdentity::Host(Uuid::now_v7())
            },
            DeliveryMode::AtBoundary,
            if authored {
                InboxPayload::Interjection {
                    text: "same text".into(),
                }
            } else {
                InboxPayload::Internal {
                    kind: "notice".into(),
                    content: serde_json::json!("same text"),
                }
            },
        );
        if authored {
            accept(
                &mut store,
                SenderIdentity::Host(Uuid::now_v7()),
                DeliveryMode::AtBoundary,
                InboxPayload::Internal {
                    kind: "notice".into(),
                    content: serde_json::json!("same text"),
                },
            );
        }
        let batch = store
            .reserve_context_input(&lease, |_| true)
            .unwrap()
            .unwrap();
        let id = batch.message_ids()[0];
        // A mixed atomic batch shares its authored admission; a host-only record has none.
        let mut message = Message::user_text(lease.turn.clone(), "same text");
        message.origin = MessageOrigin::Interjection;
        let receipt = store
            .commit_input(
                &batch,
                InputContent::Context {
                    messages: vec![message; batch.message_ids().len()],
                },
            )
            .unwrap();
        if authored {
            insertion = Some(ConversationCutoff::BeforeUserTurn {
                lease: lease.clone(),
                input: id,
                receipt: receipt.require_durable().unwrap().clone(),
            });
        }
    }
    store
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
        )
        .unwrap();
    let worker = FlowWorker::start(store).await.unwrap();
    let mut reader = worker
        .subscribe_view(NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    let captured = reader.refresh().await.unwrap();
    let input_record = captured.transcript().entries().iter().find(|entry| {
        matches!(&entry.record, FlowRecord::Input(input) if input.message_ids == admitted)
    }).unwrap().id;
    assert!(
        captured
            .transcript()
            .user_turn_boundary(FlowRecordId {
                item: input_record.item + 1,
                ..input_record
            })
            .is_none()
    );
    let mut source = FlowViewSource::live(reader);
    let mut view = FlowViewModel::default();
    view.sync_content(source.refresh().await.unwrap(), None)
        .unwrap();
    let actions = |view: &FlowViewModel| {
        view.iter()
            .enumerate()
            .filter(|(_, item)| {
                matches!(
                    item,
                    OutputItem::UserTurn { .. } | OutputItem::Interjection { .. }
                )
            })
            .map(|(index, _)| view.user_turn_boundary(index).cloned())
            .collect::<Vec<_>>()
    };
    let original = actions(&view);
    assert_eq!(original.len(), 5);
    let boundary = ConversationCutoff::BeforeUserTurn {
        lease,
        input: admitted[0],
        receipt: admission.require_durable().unwrap().clone(),
    };
    assert_eq!(
        original,
        vec![
            Some(boundary.clone()),
            Some(boundary),
            insertion.clone(),
            insertion.clone(),
            None
        ]
    );
    let initial = view.instance.head.clone().unwrap();
    let mut review = worker
        .review_history_operation(
            HistoryReviewRequest {
                id: Uuid::now_v7(),
                sender: user.clone(),
                flow: flow.clone(),
                expected_head: initial.clone(),
                target: HistoryOperationTarget::Revert {
                    observed: SourceSelection {
                        flow: flow.clone(),
                        branch: initial.branch,
                        cursor: view.cursor().unwrap().require_durable().unwrap().clone(),
                    },
                    cutoff: Box::new(insertion.unwrap()),
                    branch: ConversationBranchId::random(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(review.admitted_inputs, admitted);
    assert_eq!(review.affected_turns, Some(1));
    for restore in [false, true] {
        worker
            .begin_history_operation(review.request.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker
            .stop_history_operation(user.clone(), review.request.id, |_| Ok(()))
            .await
            .unwrap();
        worker
            .apply_history_operation(user.clone(), review.request.id, |_| Ok(()))
            .await
            .unwrap();
        view.sync_content(source.next().await.unwrap(), None)
            .unwrap();
        if restore {
            assert_eq!(actions(&view), original);
        } else {
            assert!(actions(&view).is_empty());
            review = worker
                .review_history_operation(
                    HistoryReviewRequest {
                        id: Uuid::now_v7(),
                        sender: user.clone(),
                        flow: flow.clone(),
                        expected_head: view.instance.head.clone().unwrap(),
                        target: HistoryOperationTarget::Restore {
                            branch: initial.branch,
                        },
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
        }
    }
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn branch_disclosures_restore_by_original_records_without_sharing_between_views() {
    use super::super::OutputMutation;
    use crate::instance_view::FlowViewSource;
    let (_directory, store, mut operation) = fixture();
    let initial = operation.expected_head.branch;
    let HistoryOperationTarget::Revert {
        branch: divergent, ..
    } = operation.target.clone()
    else {
        unreachable!()
    };
    let worker = FlowWorker::start(store).await.unwrap();
    let mut source = FlowViewSource::live(
        worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap(),
    );
    let mut view = FlowViewModel::default();
    view.sync_content(source.next().await.unwrap(), None)
        .unwrap();
    let thinking = view
        .iter()
        .position(|item| matches!(item, OutputItem::Thinking { .. }))
        .unwrap();
    let tools = view
        .iter()
        .position(|item| matches!(item, OutputItem::ToolDispatch { .. }))
        .unwrap();
    let workflow = view
        .iter()
        .position(|item| matches!(item, OutputItem::WorkflowPanel { .. }))
        .unwrap();
    view.mutate(thinking, OutputMutation::Interaction, |item| {
        let OutputItem::Thinking { disclosure, .. } = item else {
            unreachable!()
        };
        *disclosure = Disclosure::Full;
        true
    });
    view.set_tool_disclosure(tools, "read", Disclosure::Full);
    view.mutate(workflow, OutputMutation::Interaction, |item| {
        let OutputItem::WorkflowPanel { presentation, .. } = item else {
            unreachable!()
        };
        presentation.metrics = true;
        true
    });
    assert!(view.toggle_latest_work_fold());
    let mut sibling_source = FlowViewSource::live(
        worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap(),
    );
    let mut sibling = FlowViewModel::default();
    sibling
        .sync_content(sibling_source.next().await.unwrap(), None)
        .unwrap();
    for branch in [divergent, initial, divergent, initial] {
        worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        worker
            .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        view.sync_content(source.next().await.unwrap(), None)
            .unwrap();
        sibling
            .sync_content(sibling_source.next().await.unwrap(), None)
            .unwrap();
        assert_eq!(view.instance.head.as_ref().unwrap().branch, branch);
        assert!(
            matches!(&view[thinking], OutputItem::Thinking { disclosure, .. } if *disclosure == if branch == initial { Disclosure::Full } else { Disclosure::Summary })
        );
        assert!(
            matches!(&view[tools], OutputItem::ToolDispatch { calls } if calls[0].disclosure == if branch == initial { Disclosure::Full } else { Disclosure::Summary })
        );
        assert!(
            matches!(&view[workflow], OutputItem::WorkflowPanel { presentation, .. } if presentation.metrics == (branch == initial))
        );
        assert_eq!(
            view.work_folds.last().unwrap().is_expanded(),
            branch == initial
        );
        assert!(view.work_folds.last().unwrap().duration.is_zero());
        assert!(
            matches!(&sibling[tools], OutputItem::ToolDispatch { calls } if calls[0].disclosure == Disclosure::Summary)
        );
        assert!(!sibling.work_folds.last().unwrap().is_expanded());
        operation = HistoryOperationRequest {
            id: Uuid::now_v7(),
            expected_head: view.instance.head.clone().unwrap(),
            execution_revision: view.snapshot().unwrap().status.execution.revision(),
            target: HistoryOperationTarget::Restore {
                branch: if branch == initial {
                    divergent
                } else {
                    initial
                },
            },
            ..operation
        };
    }
    drop(source);
    drop(sibling_source);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn artifacts_and_documents_require_the_captured_head_even_for_identical_history() {
    let (_directory, store, request) = fixture();
    let worker = FlowWorker::start(store).await.unwrap();
    let mut reader = worker
        .subscribe_view(NonZeroUsize::new(2).unwrap())
        .await
        .unwrap();
    let view = reader.next().await.unwrap();
    let mut materializer = crate::history::ToolMaterializer::default();
    let old_tools = materializer.capture(view, None, false).unwrap();
    let records = view.transcript().entries().len();
    let mut original = FlowViewModel::default();
    original.sync_materialized(view, &old_tools).unwrap();
    let identity = original.identity();
    let cursor = original.cursor().cloned();
    let retained = original.capture();

    for restoring in [false, true] {
        let mut operation = request.clone();
        if restoring {
            operation.id = Uuid::now_v7();
            operation.expected_head = reader
                .view()
                .unwrap()
                .transcript()
                .history_head()
                .unwrap()
                .clone();
            operation.execution_revision = reader
                .view()
                .unwrap()
                .snapshot()
                .status
                .execution
                .revision();
            operation.target = HistoryOperationTarget::Restore {
                branch: request.expected_head.branch,
            };
        }
        worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        worker
            .apply_history_operation(operation.sender, operation.id, |_| Ok(()))
            .await
            .unwrap();
        let view = reader.refresh().await.unwrap();
        assert_eq!(view.transcript().entries().len(), records);
        assert!(!old_tools.covers(view, records));
        let tools = materializer.capture(view, None, false).unwrap();
        assert!(
            tools.covers(view, 0),
            "new heads require complete artifacts even at equal length"
        );
        assert!(original.sync_materialized(view, &tools).is_err());
        assert_eq!(original.identity(), identity);
        assert_eq!(original.cursor(), cursor.as_ref());
        assert_eq!(original.len(), retained.document().len());
        let mut selected = FlowViewModel::default();
        selected.sync_materialized(view, &tools).unwrap();
        assert_ne!(selected.identity(), identity);
        assert!(selected.iter().any(|item| matches!(item,
            OutputItem::ToolDispatch { calls }
            if calls.iter().any(|call| call.id == "read" && call.detail.is_some()))));
        let incremental = materializer.capture(view, None, false).unwrap();
        assert!(incremental.covers(view, records));
        assert!(!incremental.covers(view, 0));
    }
    drop(reader);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_documents_install_together_after_cancelled_observation_and_cached_refresh() {
    use crate::instance_view::{FlowViewContent, FlowViewSource};
    let (_directory, store, request) = fixture();
    let worker = FlowWorker::start(store).await.unwrap();
    let mut source = FlowViewSource::live(
        worker
            .subscribe_view(NonZeroUsize::new(1).unwrap())
            .await
            .unwrap(),
    );
    let mut app = crate::app::AppState::new(request.flow.session_id.0.to_string(), None);
    app.sync_flow_view(source.next().await.unwrap()).unwrap();
    let mut editor = crate::input::InputEditor::default();
    app.sync_history_draft(&mut editor);
    editor.insert_str("original branch draft");
    app.pending_quote = Some("original branch quote".into());
    let original = app.items.identity();
    let cursor = app.items.cursor().cloned();
    let rows = app.items.len();
    let HistoryOperationTarget::Revert { cutoff, .. } = &request.target else {
        unreachable!()
    };
    let ConversationCutoff::AfterCompletedTurn { lease, .. } = cutoff.as_ref() else {
        unreachable!()
    };
    let late_preview = atman_runtime::flow_runtime::live::LiveOutputSnapshot {
        flow: request.flow.clone(),
        history_head: Some(request.expected_head.clone()),
        after: cursor.clone().unwrap(),
        revision: u64::MAX,
        preview_lease: Some(lease.clone()),
        calls: vec![atman_runtime::flow_runtime::live::CallDraft {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run: lease.run.clone(),
            model: "example".into(),
            publication_pending: true,
            attempt: 0,
            revision: 1,
            parts: vec![atman_runtime::flow_runtime::live::DraftPart::Thinking(
                "old head".into(),
            )],
        }],
    };
    app.hovered_thinking_idx = Some(0);
    app.hovered_output_node = Some((0, "old-node".into()));
    app.last_item_ranges.push(crate::output::ItemRange {
        item_index: 0,
        start_row: 0,
        end_row: 4,
    });
    let version = app.items_version;
    worker
        .begin_history_operation(request.clone(), |_| Ok(()))
        .await
        .unwrap();
    worker
        .stop_history_operation(request.sender.clone(), request.id, |_| Ok(()))
        .await
        .unwrap();
    worker
        .apply_history_operation(request.sender.clone(), request.id, |_| Ok(()))
        .await
        .unwrap();

    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let blocking = worker.clone();
    let repeated = request.clone();
    let blocker = tokio::spawn(async move {
        blocking
            .begin_history_operation(repeated, move |_| {
                entered.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(std::io::Error::other)
            })
            .await
    });
    ready.await.unwrap();
    {
        let next = source.next();
        tokio::pin!(next);
        assert!(futures::poll!(next).is_pending());
    }
    assert_eq!(app.items.identity(), original);
    assert_eq!(app.items.cursor(), cursor.as_ref());
    assert_eq!(app.items.len(), rows);
    app.sync_history_draft(&mut editor);
    assert_eq!(editor.buf(), "original branch draft");
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    let prepared = source.next().await.unwrap();
    assert!(matches!(prepared, FlowViewContent::History { .. }));
    app.sync_history_draft(&mut editor);
    assert_eq!(editor.buf(), "original branch draft");
    app.sync_flow_view(prepared).unwrap();
    app.sync_history_draft(&mut editor);
    assert!(editor.buf().is_empty());
    assert!(app.pending_quote.is_none());
    editor.insert_str("divergent draft");
    assert_ne!(app.items.identity(), original);
    assert_eq!(app.items.len(), rows);
    assert!(!app.items.sync_live(late_preview.clone()).unwrap());
    assert_eq!(app.items.len(), rows);
    assert!(
        app.items_version > version,
        "equal document counters still invalidate view caches"
    );
    assert!(app.hovered_thinking_idx.is_none());
    assert!(app.hovered_output_node.is_none());
    assert!(app.last_item_ranges.is_empty());
    let selected = app.items.identity();
    let restore = HistoryOperationRequest {
        id: Uuid::now_v7(),
        expected_head: app
            .items
            .snapshot()
            .unwrap()
            .history
            .as_ref()
            .unwrap()
            .head()
            .clone(),
        execution_revision: app.items.snapshot().unwrap().status.execution.revision(),
        target: HistoryOperationTarget::Restore {
            branch: request.expected_head.branch,
        },
        ..request
    };
    worker
        .begin_history_operation(restore.clone(), |_| Ok(()))
        .await
        .unwrap();
    worker
        .stop_history_operation(restore.sender.clone(), restore.id, |_| Ok(()))
        .await
        .unwrap();
    worker
        .apply_history_operation(restore.sender.clone(), restore.id, |_| Ok(()))
        .await
        .unwrap();
    // Cached view refresh must retain the prepared replacement instead of consuming it silently.
    source = source.prepare_initial(None, None, false);
    let prepared = source.next().await.unwrap();
    assert!(matches!(prepared, FlowViewContent::History { .. }));
    app.sync_flow_view(prepared).unwrap();
    app.sync_history_draft(&mut editor);
    assert_eq!(editor.buf(), "original branch draft");
    assert_eq!(app.pending_quote.as_deref(), Some("original branch quote"));
    assert_ne!(app.items.identity(), selected);
    assert_eq!(app.items.len(), rows);
    assert!(!app.items.sync_live(late_preview).unwrap());
    assert_eq!(app.items.len(), rows);
    let restored = app.items.identity();
    app.sync_flow_view(source.refresh().await.unwrap()).unwrap();
    app.sync_history_draft(&mut editor);
    assert_eq!(editor.buf(), "original branch draft");
    assert_eq!(app.items.identity(), restored);
    drop(source);
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn branch_reading_tracks_original_records_and_yields_to_explicit_scrolling() {
    use crate::{
        app::AppState,
        instance_view::FlowViewSource,
        output::{LayoutKey, LayoutRequest, RenderCtx},
    };
    fn layout(app: &mut AppState, width: u16) {
        app.layout_cache
            .set_work_folds(app.items.work_fold_projections(None));
        app.layout_cache.update_dirty(
            LayoutKey {
                width,
                theme: crate::theme::current_mode(),
            },
            &app.items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: app.scroll_offset,
                viewport_rows: 200,
                follow_tail_rows: None,
            },
        );
    }
    let (_directory, store, mut operation) = fixture();
    let initial = operation.expected_head.branch;
    let HistoryOperationTarget::Revert {
        branch: divergent, ..
    } = operation.target.clone()
    else {
        unreachable!()
    };
    let worker = FlowWorker::start(store).await.unwrap();
    let mut source = FlowViewSource::live(
        worker
            .subscribe_view(NonZeroUsize::new(2).unwrap())
            .await
            .unwrap(),
    );
    let mut app = AppState::new(operation.flow.session_id.0.to_string(), None);
    app.sync_flow_view(source.next().await.unwrap()).unwrap();
    let answer = app
        .items
        .iter()
        .position(|item| matches!(item, OutputItem::AssistantMd { .. }))
        .unwrap();
    layout(&mut app, 80);
    app.pause_tail_following();
    app.scroll_offset = app.layout_cache.item_row_start(answer).unwrap() + 8;
    for (step, branch) in [divergent, initial, divergent, initial]
        .into_iter()
        .enumerate()
    {
        worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        worker
            .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        app.sync_flow_view(source.next().await.unwrap()).unwrap();
        assert!(!app.follow_tail);
        assert!(
            app.history_reading
                .resolve(&app.items, &app.layout_cache)
                .is_none(),
            "old layout cannot resolve new document positions"
        );
        if step == 3 {
            app.scroll_up(1);
            let chosen = app.scroll_offset;
            layout(&mut app, 45);
            assert!(
                app.history_reading
                    .resolve(&app.items, &app.layout_cache)
                    .is_none()
            );
            assert_eq!(app.scroll_offset, chosen);
        } else {
            layout(&mut app, 45);
            let offset = app
                .history_reading
                .resolve(&app.items, &app.layout_cache)
                .unwrap();
            assert_eq!(
                offset,
                app.layout_cache.item_row_start(answer).unwrap() + if step == 2 { 2 } else { 8 }
            );
            app.scroll_offset = offset;
            if step == 0 {
                assert!(app.items.toggle_latest_work_fold());
                app.items.work_folds.last_mut().unwrap().duration = std::time::Duration::ZERO;
                layout(&mut app, 45);
                app.scroll_offset = app.layout_cache.item_row_start(answer).unwrap() + 2;
            }
        }
        operation = HistoryOperationRequest {
            id: Uuid::now_v7(),
            expected_head: app
                .items
                .snapshot()
                .unwrap()
                .history
                .as_ref()
                .unwrap()
                .head()
                .clone(),
            execution_revision: app.items.snapshot().unwrap().status.execution.revision(),
            target: HistoryOperationTarget::Restore {
                branch: if branch == initial {
                    divergent
                } else {
                    initial
                },
            },
            ..operation
        };
    }
    // A branch that follows output restores that mode independently of a reading branch.
    app.scroll_to_tail();
    for follow in [false, true] {
        worker
            .begin_history_operation(operation.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        worker
            .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .await
            .unwrap();
        app.sync_flow_view(source.next().await.unwrap()).unwrap();
        assert_eq!(app.follow_tail, follow);
        operation = HistoryOperationRequest {
            id: Uuid::now_v7(),
            expected_head: app
                .items
                .snapshot()
                .unwrap()
                .history
                .as_ref()
                .unwrap()
                .head()
                .clone(),
            execution_revision: app.items.snapshot().unwrap().status.execution.revision(),
            target: HistoryOperationTarget::Restore { branch: initial },
            ..operation
        };
    }
    drop(source);
    worker.shutdown().await.unwrap();
}
