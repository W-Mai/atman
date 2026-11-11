use super::*;
use crate::{
    flow_store::{
        FlowFact, FlowTransition, RuntimeStore,
        completion::InvocationResult,
        position::FlowBatch,
        source_capture::tests::{checkpoint, fixture, publish},
        view::FlowUpdate,
    },
    message::Message,
    projection::flow::FlowTranscript,
};
use atman_rt::TurnOutcome;
use std::path::Path;

fn inspect(directory: &Path, flow: &FlowStore) -> io::Result<ConversationHistory> {
    RuntimeStore::inspect_session(directory, flow.state().address().session_id.0)?
        .inspect_history_branches(&flow.state().address().instance_id)
}

fn create(history: &ConversationHistory, cursor: CommitReceipt) -> HistoryHeadChange {
    HistoryHeadChange {
        operation: Uuid::new_v4(),
        expected: history.head.clone(),
        selection: HistorySelection::Create {
            branch: ConversationBranchId::random(),
            parent: SourceSelection {
                flow: history.flow.clone(),
                branch: history.head.branch,
                cursor,
            },
        },
    }
}

// Only tests append these facts until the operation owner and all head-aware projections exist.
fn append(flow: &mut FlowStore, change: HistoryHeadChange) -> CommitReceipt {
    let receipt = flow
        .journal
        .commit(&[FlowFact::HistoryHeadChanged(Box::new(change))])
        .unwrap();
    flow.cursor = receipt.clone();
    receipt.require_durable().unwrap().clone()
}

fn marker(flow: &mut FlowStore) -> CommitReceipt {
    let receipt = flow
        .journal
        .commit(&[FlowFact::Transition(FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        })])
        .unwrap();
    flow.cursor = receipt.clone();
    receipt.require_durable().unwrap().clone()
}

fn has(spans: &[ConversationSpan], receipt: &CommitReceipt) -> bool {
    spans
        .iter()
        .any(|span| (span.first_sequence..=span.last_sequence).contains(&receipt.sequence))
}

fn start_turn(flow: &mut FlowStore) -> atman_rt::TurnLease<crate::event::AtmanUuid> {
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: atman_rt::TurnId::now(),
            run: atman_rt::RunId::now(),
        },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("expected an active turn");
    };
    active.lease.clone()
}

#[test]
fn unchanged_native_history_has_one_initial_head_without_migration_or_writes() {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "original"),
    );
    checkpoint(&mut flow, &lease, "summary");
    flow.finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    let path = flow.dir().unwrap().join("events.jsonl");
    let before = std::fs::read(&path).unwrap();
    let history = inspect(directory.path(), &flow).unwrap();
    assert_eq!(history.head, flow.history_head());
    assert_eq!(history.head.version.get(), 0);
    assert_eq!(history.branches.len(), 1);
    assert!(history.changes.is_empty());
    assert_eq!(history.observed, *flow.cursor().require_durable().unwrap());
    assert_eq!(
        history.ancestry(history.head.branch).unwrap(),
        [ConversationSpan {
            branch: history.head.branch,
            first_sequence: 1,
            last_sequence: history.observed.sequence,
        }]
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn restored_branches_retain_disjoint_spans_and_children_keep_their_original_cutoff() {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "original"),
    );
    let cutoff = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap()
        .receipt
        .require_durable()
        .unwrap()
        .clone();
    checkpoint(&mut flow, &lease, "later checkpoint");
    let later_checkpoint = flow.cursor().require_durable().unwrap().clone();
    let initial = inspect(directory.path(), &flow).unwrap();
    let root = initial.head.branch;
    let selected_a = append(&mut flow, create(&initial, cutoff.clone()));
    let own_a = marker(&mut flow);
    let a = inspect(directory.path(), &flow).unwrap();
    let branch_a = a.head.branch;
    let restored = append(
        &mut flow,
        HistoryHeadChange {
            operation: Uuid::new_v4(),
            expected: a.head,
            selection: HistorySelection::Restore { branch: root },
        },
    );
    let root_later = marker(&mut flow);
    let resumed = inspect(directory.path(), &flow).unwrap();
    let selected_b = append(&mut flow, create(&resumed, later_checkpoint.clone()));
    let own_b = marker(&mut flow);
    let history = inspect(directory.path(), &flow).unwrap();
    assert_eq!(history.head.version.get(), 3);
    assert_eq!(history.branches.len(), 3);
    let root_spans = history.ancestry(root).unwrap();
    assert!(
        has(&root_spans, &later_checkpoint)
            && has(&root_spans, &restored)
            && has(&root_spans, &root_later)
    );
    assert!(
        !has(&root_spans, &selected_a) && !has(&root_spans, &own_a) && !has(&root_spans, &own_b)
    );
    let a_spans = history.ancestry(branch_a).unwrap();
    assert!(has(&a_spans, &cutoff) && has(&a_spans, &selected_a) && has(&a_spans, &own_a));
    assert!(
        !has(&a_spans, &later_checkpoint) && !has(&a_spans, &root_later) && !has(&a_spans, &own_b)
    );
    let b_spans = history.ancestry(history.head.branch).unwrap();
    assert!(
        has(&b_spans, &later_checkpoint) && has(&b_spans, &selected_b) && has(&b_spans, &own_b)
    );
    assert!(!has(&b_spans, &own_a) && !has(&b_spans, &root_later));
    let before = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    assert_eq!(
        inspect(directory.path(), &flow).unwrap().changes,
        history.changes
    );
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        before
    );
}

#[test]
fn history_metadata_rejects_stale_heads_and_forged_complete_source_identity() {
    for variant in 0..7 {
        let (directory, mut flow, _) = fixture();
        let history = inspect(directory.path(), &flow).unwrap();
        let mut change = create(&history, history.observed.clone());
        let HistorySelection::Create { branch, parent } = &mut change.selection else {
            unreachable!()
        };
        match variant {
            0 => change.expected.version = HistoryHeadVersion(1),
            1 => change.operation = Uuid::nil(),
            2 => *branch = history.head.branch,
            3 => parent.cursor.digest = "wrong".into(),
            4 => parent.cursor.journal_id = Uuid::new_v4(),
            5 => parent.branch = ConversationBranchId::random(),
            6 => parent.flow.instance_id = super::super::FlowInstanceId::now(),
            _ => unreachable!(),
        }
        append(&mut flow, change);
        assert!(
            inspect(directory.path(), &flow).is_err(),
            "variant {variant}"
        );
    }
}

#[test]
fn an_existing_receipt_from_an_unrelated_branch_is_not_a_valid_parent_cutoff() {
    let (directory, mut flow, _) = fixture();
    let original = inspect(directory.path(), &flow).unwrap();
    append(&mut flow, create(&original, original.observed.clone()));
    let excluded = marker(&mut flow);
    let a = inspect(directory.path(), &flow).unwrap();
    append(
        &mut flow,
        HistoryHeadChange {
            operation: Uuid::new_v4(),
            expected: a.head,
            selection: HistorySelection::Restore {
                branch: original.head.branch,
            },
        },
    );
    let restored = inspect(directory.path(), &flow).unwrap();
    append(&mut flow, create(&restored, excluded));
    assert!(inspect(directory.path(), &flow).is_err());
}

#[test]
fn duplicate_operations_and_unknown_restore_targets_are_rejected() {
    for duplicated in [false, true] {
        let (directory, mut flow, _) = fixture();
        let original = inspect(directory.path(), &flow).unwrap();
        let create = create(&original, original.observed.clone());
        let operation = create.operation;
        append(&mut flow, create);
        let current = inspect(directory.path(), &flow).unwrap();
        append(
            &mut flow,
            HistoryHeadChange {
                operation: if duplicated {
                    operation
                } else {
                    Uuid::new_v4()
                },
                expected: current.head,
                selection: HistorySelection::Restore {
                    branch: if duplicated {
                        original.head.branch
                    } else {
                        ConversationBranchId::random()
                    },
                },
            },
        );
        assert!(inspect(directory.path(), &flow).is_err());
    }
}

#[test]
fn readers_without_head_aware_execution_and_transcripts_refuse_branched_history() {
    let (directory, mut flow, _) = fixture();
    let initial = inspect(directory.path(), &flow).unwrap();
    let address = flow.state().address().clone();
    let change = create(&initial, initial.observed.clone());
    assert!(
        FlowFact::HistoryHeadChanged(Box::new(change.clone()))
            .validate_transient()
            .is_err()
    );
    let receipt = append(&mut flow, change.clone());
    let session = RuntimeStore::inspect_session(directory.path(), address.session_id.0).unwrap();
    assert_eq!(
        session
            .inspect_flow(&address.instance_id)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    let mut transcript = FlowTranscript::new(address.clone());
    assert_eq!(
        transcript
            .apply(FlowBatch {
                receipt: receipt.into(),
                facts: vec![FlowUpdate::HistoryHeadChanged(Box::new(change))],
            })
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(transcript.cursor().is_none() && transcript.entries().is_empty());
    drop(flow);
    let root = RuntimeStore::open(directory.path()).unwrap();
    let owner = root.open_session(address.session_id.0).unwrap();
    let error = owner.open_flow(&address.instance_id).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
}

#[test]
fn branch_reads_select_ancestor_checkpoints_and_preserve_disjoint_original_history() {
    use crate::flow_store::{
        context::{ContextCheckpoint, context_digest},
        conversation::ConversationReadKind::{History, ModelContext},
        publication::MessagePublication,
    };
    let (directory, mut flow, lease) = fixture();
    let message = |text: &str| Message::assistant_text(lease.turn.clone(), text);
    let prefix = message("prefix");
    let root_tail_message = message("root tail");
    let root_summary = Message::user_text(lease.turn.clone(), "root summary");
    publish(&mut flow, &lease, prefix.clone());
    let cutoff = flow.cursor().require_durable().unwrap().clone();
    checkpoint(&mut flow, &lease, "root summary");
    publish(&mut flow, &lease, message("root tail"));
    let root_tail = flow.cursor().require_durable().unwrap().clone();
    flow.finish_turn(
        lease.clone(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    let original = inspect(directory.path(), &flow).unwrap();
    append(&mut flow, create(&original, cutoff.clone()));
    let lease = start_turn(&mut flow);
    let message = |text: &str| Message::assistant_text(lease.turn.clone(), text);
    publish(&mut flow, &lease, message("child"));
    let child = inspect(directory.path(), &flow).unwrap();
    let read = |source: &SourceSelection, kind| {
        RuntimeStore::inspect_session(directory.path(), source.flow.session_id.0)
            .unwrap()
            .read_branch_conversation(source, kind)
    };
    let source = |history: &ConversationHistory| SourceSelection {
        flow: history.flow.clone(),
        branch: history.head.branch,
        cursor: history.observed.clone(),
    };
    let child_source = source(&child);
    let expected = vec![prefix.clone(), message("child")];
    assert_eq!(
        read(&child_source, ModelContext).unwrap().messages,
        expected
    );
    assert_eq!(read(&child_source, History).unwrap().messages, expected);

    // The writer is deliberately not branch-enabled. Record a checkpoint against the selected
    // branch window directly to exercise inspection independently of execution mutation.
    let receipt = flow
        .journal
        .commit(&[FlowFact::ContextCheckpoint(Box::new(ContextCheckpoint {
            id: Uuid::new_v4(),
            purpose: Default::default(),
            expected: context_digest(&expected).unwrap(),
            messages: vec![message("child summary")],
            summary: "child summary".into(),
            range: 0..expected.len(),
            count: expected.len(),
            before_tokens: 100,
            after_tokens: 10,
        }))])
        .unwrap();
    flow.cursor = receipt;
    publish(&mut flow, &lease, message("child tail"));
    let child_tail = flow.cursor().require_durable().unwrap().clone();
    flow.publish_message(MessagePublication {
        id: Uuid::new_v4(),
        lease: lease.clone(),
        run: lease.run.clone(),
        context_run: Some(lease.run.clone()),
        message: message("isolated"),
    })
    .unwrap();
    let child = inspect(directory.path(), &flow).unwrap();
    assert_eq!(
        read(&source(&child), ModelContext).unwrap().messages,
        vec![message("child summary"), message("child tail")]
    );
    assert_eq!(
        read(&source(&child), History).unwrap().messages,
        vec![prefix.clone(), message("child"), message("child tail")]
    );
    flow.finish_turn(
        lease.clone(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    append(
        &mut flow,
        HistoryHeadChange {
            operation: Uuid::new_v4(),
            expected: child.head.clone(),
            selection: HistorySelection::Restore {
                branch: original.head.branch,
            },
        },
    );
    let resumed_lease = start_turn(&mut flow);
    let resumed_message = Message::assistant_text(resumed_lease.turn.clone(), "root resumed");
    publish(&mut flow, &resumed_lease, resumed_message.clone());
    let restored = inspect(directory.path(), &flow).unwrap();
    assert_eq!(
        read(&source(&restored), ModelContext).unwrap().messages,
        vec![
            root_summary.clone(),
            root_tail_message.clone(),
            resumed_message.clone()
        ]
    );
    assert_eq!(
        read(&source(&restored), History).unwrap().messages,
        vec![prefix, root_tail_message.clone(), resumed_message]
    );
    // A captured observation remains fixed even after the source switches and appends.
    assert_eq!(
        read(&child_source, ModelContext).unwrap().messages,
        expected
    );
    let mut invalid_source = source(&restored);
    invalid_source.cursor = child_tail;
    assert!(read(&invalid_source, History).is_err());
    invalid_source.cursor = root_tail.clone();
    invalid_source.cursor.digest = "forged".into();
    assert!(read(&invalid_source, History).is_err());
    invalid_source = source(&restored);
    invalid_source.flow.session_id = Uuid::new_v4().into();
    let session =
        RuntimeStore::inspect_session(directory.path(), original.flow.session_id.0).unwrap();
    assert!(
        session
            .read_branch_conversation(&invalid_source, History)
            .is_err()
    );

    flow.finish_turn(
        resumed_lease,
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    append(&mut flow, create(&restored, root_tail));
    let sibling = inspect(directory.path(), &flow).unwrap();
    assert_eq!(
        read(&source(&sibling), ModelContext).unwrap().messages,
        vec![root_summary, root_tail_message]
    );
    let path = flow.dir().unwrap().join("events.jsonl");
    let before = std::fs::read(&path).unwrap();
    read(&source(&sibling), History).unwrap();
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn shared_conversation_plan_matches_owner_history_and_context_without_branching() {
    use crate::flow_store::conversation::ConversationReadKind::{History, ModelContext};
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "before"),
    );
    checkpoint(&mut flow, &lease, "summary");
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "after"),
    );
    let history = inspect(directory.path(), &flow).unwrap();
    let source = SourceSelection {
        flow: history.flow,
        branch: history.head.branch,
        cursor: history.observed,
    };
    let session =
        RuntimeStore::inspect_session(directory.path(), source.flow.session_id.0).unwrap();
    assert_eq!(
        session
            .read_branch_conversation(&source, History)
            .unwrap()
            .messages,
        flow.history_messages().unwrap()
    );
    assert_eq!(
        session
            .read_branch_conversation(&source, ModelContext)
            .unwrap()
            .messages,
        flow.context_messages().unwrap()
    );
}

#[test]
fn selected_replay_rejects_active_switches_stale_checkpoints_and_old_turn_publications() {
    use crate::flow_store::{conversation::ConversationReadKind, publication::MessagePublication};
    for failure in 0..3 {
        let (directory, mut flow, lease) = fixture();
        publish(
            &mut flow,
            &lease,
            Message::assistant_text(lease.turn.clone(), "selected prefix"),
        );
        let cutoff = flow.cursor().require_durable().unwrap().clone();
        checkpoint(&mut flow, &lease, "abandoned summary");
        if failure != 0 {
            flow.finish_turn(
                lease.clone(),
                InvocationResult::Unobserved(TurnOutcome::Completed),
            )
            .unwrap();
        }
        let initial = inspect(directory.path(), &flow).unwrap();
        append(&mut flow, create(&initial, cutoff));
        match failure {
            1 => {
                let child_lease = start_turn(&mut flow);
                // The unmodified writer still sees the abandoned window. Its digest must not
                // authorize a checkpoint against the selected branch during read-only replay.
                checkpoint(&mut flow, &child_lease, "wrong branch checkpoint");
            }
            2 => {
                let receipt = flow
                    .journal
                    .commit(&[FlowFact::MessagePublished(Box::new(MessagePublication {
                        id: Uuid::new_v4(),
                        lease: lease.clone(),
                        run: lease.run.clone(),
                        context_run: None,
                        message: Message::assistant_text(lease.turn.clone(), "late abandoned turn"),
                    }))])
                    .unwrap();
                flow.cursor = receipt;
            }
            _ => {}
        }
        let history = inspect(directory.path(), &flow).unwrap();
        let session =
            RuntimeStore::inspect_session(directory.path(), history.flow.session_id.0).unwrap();
        let path = flow.dir().unwrap().join("events.jsonl");
        let before = std::fs::read(&path).unwrap();
        assert!(
            session
                .read_branch_conversation(
                    &SourceSelection {
                        flow: history.flow,
                        branch: history.head.branch,
                        cursor: history.observed,
                    },
                    ConversationReadKind::ModelContext
                )
                .is_err(),
            "failure case {failure}"
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
