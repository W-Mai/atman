use super::*;
use crate::flow_store::{
    FlowTransition, RuntimeStore,
    completion::InvocationResult,
    control::{ControlAction, ControlRequest},
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload},
    source_capture::tests::fixture,
};
use atman_rt::{Interruption, RunId, TurnId, TurnOutcome};

pub(super) fn request(flow: &FlowStore, cutoff: ConversationCutoff) -> HistoryOperationRequest {
    HistoryOperationRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        flow: flow.state().address().clone(),
        expected_head: flow.history_head(),
        execution_revision: flow.state().revision(),
        active: match flow.state().state() {
            InstanceState::Open(InstanceActivity::Active(active)) => Some(active.lease.clone()),
            _ => None,
        },
        goal: flow.goal().map(GoalVersion::from),
        queued: flow
            .pending_input_entries()
            .map(|entry| HistoryQueueItem {
                message: entry.envelope.id,
                revision: entry.revision.clone(),
            })
            .collect(),
        target: HistoryOperationTarget::Revert {
            observed: SourceSelection {
                flow: flow.state().address().clone(),
                branch: flow.history_head().branch,
                cursor: flow.cursor().require_durable().unwrap().clone(),
            },
            cutoff: Box::new(cutoff),
            branch: ConversationBranchId::random(),
        },
    }
}

pub(super) fn complete(flow: &mut FlowStore, lease: TurnLease<AtmanUuid>) -> ConversationCutoff {
    let record = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    ConversationCutoff::AfterCompletedTurn {
        lease,
        receipt: record.receipt.require_durable().unwrap().clone(),
    }
}

pub(super) fn queue(flow: &mut FlowStore) {
    flow.accept_input(
        DeliveryRequest {
            expected_head: None,
            request_id: Uuid::new_v4(),
            sender: SenderIdentity::User(Uuid::new_v4()),
            recipient: flow.state().address().clone(),
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Interjection {
                text: "retained input".into(),
            },
        },
        |_| Ok(()),
    )
    .unwrap();
}

#[test]
fn preparation_and_abort_recover_original_receipts_and_admission_ownership() {
    let (directory, mut flow, operation) = reviewed_fixture();
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let state = flow.state().clone();
    let started = flow
        .begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    assert_eq!(flow.state(), &state);
    assert_eq!(flow.history_head(), operation.expected_head);
    assert!(!flow.can_admit_turn());
    assert!(!flow.release_input(&reserved));
    assert!(
        flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| panic!(
            "preparation must not read queued bodies"
        ))
        .unwrap()
        .is_none()
    );
    assert!(
        flow.transition(
            state.revision(),
            FlowTransition::TurnStarted {
                turn: TurnId::now(),
                run: RunId::now()
            }
        )
        .is_err()
    );
    queue(&mut flow);
    assert_eq!(flow.pending_inputs().count(), 2);
    let mut authorized = false;
    assert_eq!(
        flow.begin_history_operation(operation.clone(), |_| {
            authorized = true;
            Ok(())
        })
        .unwrap(),
        started
    );
    assert!(authorized);
    assert!(
        flow.begin_history_operation(operation.clone(), |_| Err(
            io::ErrorKind::PermissionDenied.into()
        ))
        .is_err()
    );
    let before = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    let inspection =
        RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0).unwrap();
    assert_eq!(
        inspection
            .history_operation(&operation.flow.instance_id, &operation.sender, operation.id)
            .unwrap(),
        Some(started.clone())
    );
    assert!(
        inspection
            .history_operation(
                &operation.flow.instance_id,
                &SenderIdentity::User(Uuid::new_v4()),
                operation.id
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        before
    );
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
    assert_eq!(flow.pending_history_operation(), Some(&started));
    assert_eq!(flow.pending_inputs().count(), 2);
    assert!(!flow.can_admit_turn());
    assert_eq!(
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap(),
        started
    );
    let aborted = flow
        .abort_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    assert!(matches!(
        aborted.status,
        HistoryOperationStatus::Aborted { .. }
    ));
    assert_eq!(aborted.accepted, started.accepted);
    let cursor = flow.cursor().clone();
    assert_eq!(
        flow.abort_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        aborted
    );
    assert_eq!(
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap(),
        aborted
    );
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.can_admit_turn());
    drop(flow);
    let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
    assert!(flow.pending_history_operation().is_none());
    assert_eq!(
        flow.history_operation(&operation.sender, operation.id),
        Some(&aborted)
    );
    let HistoryOperationTarget::Revert { cutoff, .. } = &operation.target else {
        unreachable!()
    };
    let next = request(&flow, *cutoff.clone());
    let pending = flow.begin_history_operation(next, |_| Ok(())).unwrap();
    assert_eq!(
        flow.abort_history_operation(operation.sender, operation.id, |_| Ok(()))
            .unwrap(),
        aborted
    );
    assert_eq!(flow.pending_history_operation(), Some(&pending));
}

pub(crate) fn reviewed_fixture() -> (tempfile::TempDir, FlowStore, HistoryOperationRequest) {
    let (directory, mut flow, lease) = fixture();
    let cutoff = complete(&mut flow, lease);
    queue(&mut flow);
    let operation = request(&flow, cutoff);
    (directory, flow, operation)
}

pub(crate) fn active_fixture() -> (tempfile::TempDir, FlowStore, HistoryOperationRequest) {
    let (directory, mut flow, mut operation) = reviewed_fixture();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    operation.execution_revision = flow.state().revision();
    let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
        panic!("fixture must have an active invocation");
    };
    operation.active = Some(active.lease.clone());
    (directory, flow, operation)
}

#[test]
fn admission_hold_does_not_pause_cancel_cleanup_or_release_another_pause() {
    let (_directory, mut flow, lease) = fixture();
    let cutoff = complete(&mut flow, lease);
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let operation = request(&flow, cutoff);
    let active = operation.active.clone().unwrap();
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    assert!(flow.state().can_receive_at_boundary(&active).unwrap());
    let pause = Uuid::new_v4().into();
    flow.transition(
        flow.state().revision(),
        FlowTransition::ExecutionPaused { key: pause },
    )
    .unwrap();
    flow.control(ControlRequest {
        id: Uuid::new_v4(),
        sender: operation.sender.clone(),
        target: operation.flow.clone(),
        expected_revision: flow.state().revision(),
        pause_key: Uuid::new_v4().into(),
        action: ControlAction::CancelTurn,
    })
    .unwrap();
    queue(&mut flow);
    let completion = flow
        .finish_turn(active, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    assert_eq!(
        completion.completion.outcome,
        TurnOutcome::Interrupted(Interruption::Cancelled)
    );
    assert!(flow.pending_history_operation().is_some());
    flow.abort_history_operation(operation.sender, operation.id, |_| Ok(()))
        .unwrap();
    assert!(flow.state().pauses().contains(&pause));
    assert!(!flow.can_admit_turn());
    flow.transition(
        flow.state().revision(),
        FlowTransition::ExecutionResumed { key: pause },
    )
    .unwrap();
    assert!(flow.can_admit_turn());
    assert_eq!(flow.pending_inputs().count(), 1);
}

#[test]
fn rejected_reviews_and_conflicting_retries_leave_no_new_hold_or_effects() {
    let (_directory, mut flow, lease) = fixture();
    let cutoff = complete(&mut flow, lease);
    let operation = request(&flow, cutoff);
    let before = std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap();
    let mut rejected = Vec::new();
    let mut changed = operation.clone();
    if let HistoryOperationTarget::Revert { cutoff, .. } = &operation.target
        && let ConversationCutoff::AfterCompletedTurn { lease, .. } = cutoff.as_ref()
    {
        changed.active = Some(lease.clone());
    }
    rejected.push(changed);
    let mut changed = operation.clone();
    changed.execution_revision += 1;
    rejected.push(changed);
    let mut changed = operation.clone();
    changed.expected_head.branch = ConversationBranchId::random();
    rejected.push(changed);
    let mut changed = operation.clone();
    if let HistoryOperationTarget::Revert { observed, .. } = &mut changed.target {
        observed.cursor.digest.push('0');
    }
    rejected.push(changed);
    let mut changed = operation.clone();
    changed.sender = SenderIdentity::Host(Uuid::new_v4());
    rejected.push(changed);
    let mut changed = operation.clone();
    changed.goal = Some(GoalVersion {
        id: "missing".into(),
        revision: 1,
    });
    rejected.push(changed);
    let mut changed = operation.clone();
    changed.queued.push(HistoryQueueItem {
        message: Uuid::new_v4(),
        revision: flow.cursor().clone(),
    });
    rejected.push(changed);
    for request in rejected {
        assert!(flow.begin_history_operation(request, |_| Ok(())).is_err());
        assert!(flow.pending_history_operation().is_none());
    }
    assert_eq!(
        std::fs::read(flow.dir().unwrap().join("events.jsonl")).unwrap(),
        before
    );
    let started = flow
        .begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    let before = flow.cursor().clone();
    let mut changed = operation.clone();
    changed.target = HistoryOperationTarget::Restore {
        branch: ConversationBranchId::random(),
    };
    assert!(flow.begin_history_operation(changed, |_| Ok(())).is_err());
    let mut competing = operation.clone();
    competing.id = Uuid::new_v4();
    assert!(flow.begin_history_operation(competing, |_| Ok(())).is_err());
    assert!(
        flow.abort_history_operation(SenderIdentity::User(Uuid::new_v4()), operation.id, |_| Ok(
            ()
        ))
        .is_err()
    );
    assert_eq!(flow.cursor(), &before);
    assert_eq!(flow.pending_history_operation(), Some(&started));
}

#[test]
fn replay_rejects_invalid_preparation_and_new_turns_before_repairing_tail() {
    for invalid_selection in [false, true] {
        let (directory, mut flow, lease) = fixture();
        let cutoff = complete(&mut flow, lease);
        let operation = request(&flow, cutoff);
        let expected = if invalid_selection {
            flow.journal
                .commit(&[FlowFact::HistoryOperation(Box::new(
                    HistoryOperationEvent::Started {
                        request: Box::new(operation.clone()),
                        selection: HistorySelection::Create {
                            branch: ConversationBranchId::random(),
                            parent: SourceSelection {
                                flow: operation.flow.clone(),
                                branch: operation.expected_head.branch,
                                cursor: flow.cursor().require_durable().unwrap().clone(),
                            },
                        },
                        at: Utc::now(),
                    },
                ))])
                .unwrap();
            "cutoff does not match"
        } else {
            flow.begin_history_operation(operation.clone(), |_| Ok(()))
                .unwrap();
            flow.journal
                .commit(&[FlowFact::Transition(FlowTransition::TurnStarted {
                    turn: TurnId::now(),
                    run: RunId::now(),
                })])
                .unwrap();
            "holding new turn admission"
        };
        let path = flow.dir().unwrap().join("events.jsonl");
        drop(flow);
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"partial\":")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        let inspection =
            RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0).unwrap();
        assert!(
            inspection
                .history_operation(&operation.flow.instance_id, &operation.sender, operation.id)
                .unwrap_err()
                .to_string()
                .contains(expected)
        );
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        assert!(
            session
                .open_flow(&operation.flow.instance_id)
                .err()
                .unwrap()
                .to_string()
                .contains(expected)
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}
