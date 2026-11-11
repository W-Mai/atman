use super::*;
use atman_runtime::{
    flow_runtime::FlowWorker,
    flow_store::{
        history_operation::{HistoryOperationRequest, HistoryOperationTarget},
        source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection},
    },
};

#[tokio::test]
async fn artifacts_and_documents_require_the_captured_head_even_for_identical_history() {
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
                origin: MessageOrigin::Internal,
                turn_id: lease.turn.clone(),
                parts: vec![part],
            },
        );
    }
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
