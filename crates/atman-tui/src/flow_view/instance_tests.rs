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

#[test]
fn native_views_share_committed_history_but_keep_disclosure_and_scoped_results() {
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
                    payload: InboxPayload::Interjection {
                        text: "input".into(),
                    },
                },
                |_| Ok(()),
            )
            .unwrap();
        let reservation = store
            .reserve_context_input(&lease, |_| true)
            .unwrap()
            .unwrap();
        let mut input = Message::user_text(
            lease.turn.clone(),
            if turn == 0 {
                "hi"
            } else {
                "continue existing goal"
            },
        );
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
}

#[test]
fn empty_native_view_still_binds_to_its_source_identity() {
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
}
