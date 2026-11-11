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
    let mut answer = Message::assistant_text(lease.turn.clone(), "retained answer");
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
    release.send(()).unwrap();
    blocker.await.unwrap().unwrap();
    let prepared = source.next().await.unwrap();
    assert!(matches!(prepared, FlowViewContent::History { .. }));
    app.sync_flow_view(prepared).unwrap();
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
    assert_ne!(app.items.identity(), selected);
    assert_eq!(app.items.len(), rows);
    assert!(!app.items.sync_live(late_preview).unwrap());
    assert_eq!(app.items.len(), rows);
    let restored = app.items.identity();
    app.sync_flow_view(source.refresh().await.unwrap()).unwrap();
    assert_eq!(app.items.identity(), restored);
    drop(source);
    worker.shutdown().await.unwrap();
}
