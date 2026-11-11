use super::*;
use crate::flow_store::{
    FlowTransition, RuntimeStore,
    completion::InvocationResult,
    history_operation::tests::{active_fixture, reviewed_fixture},
};
use atman_rt::{RunId, TurnId, TurnOutcome, TurnPhase};

#[test]
fn cancellation_is_atomic_and_reconciles_only_its_original_completion_after_restart() {
    let (directory, mut flow, operation) = active_fixture();
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    // Ordinary execution may advance phases after review, without changing its lease.
    flow.transition(
        flow.state().revision(),
        FlowTransition::PhaseChanged {
            lease: operation.active.clone().unwrap(),
            phase: TurnPhase::Finishing,
        },
    )
    .unwrap();
    let stopping = flow
        .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    assert_eq!(stopping.status, HistoryOperationStatus::Stopping);
    assert!(
        matches!(flow.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == TurnPhase::Cancelling)
    );
    let cancel = stopping.cancellation.as_ref().unwrap();
    assert_eq!(
        flow.recorded_control(&cancel.request)
            .unwrap()
            .unwrap()
            .receipt,
        cancel.receipt
    );
    assert_eq!(flow.cursor(), &cancel.receipt);
    assert!(
        flow.completions
            .get(operation.active.as_ref().unwrap())
            .is_none()
    );
    flow.reconcile_history_stop().unwrap();
    assert_eq!(flow.pending_history_operation(), Some(&stopping));
    assert!(!flow.can_admit_turn());
    let inspection =
        RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0).unwrap();
    assert_eq!(
        inspection
            .history_operation(&operation.flow.instance_id, &operation.sender, operation.id)
            .unwrap(),
        Some(stopping.clone())
    );
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let mut flow = session.restore_flow(&operation.flow.instance_id).unwrap();
    assert_eq!(
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        stopping
    );
    let done = flow
        .finish_turn(
            operation.active.clone().unwrap(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    // A restart between the completion and operation receipt keeps the same cancellation identity.
    drop(flow);
    let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
    assert_eq!(flow.pending_history_operation(), Some(&stopping));
    flow.reconcile_history_stop().unwrap();
    let ready = flow.pending_history_operation().unwrap().clone();
    assert_eq!(ready.cancellation, stopping.cancellation);
    assert!(
        matches!(&ready.status, HistoryOperationStatus::Ready {completion: Some(receipt), ..} if receipt == &done.receipt)
    );
    assert_eq!(flow.history_head(), operation.expected_head);
    assert!(!flow.can_admit_turn());
    assert_eq!(flow.pending_inputs().count(), 1);
    assert!(
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Err(
            io::ErrorKind::PermissionDenied.into()
        ))
        .is_err()
    );
    assert_eq!(
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        ready
    );
    drop(flow);
    assert_eq!(
        session
            .open_flow(&operation.flow.instance_id)
            .unwrap()
            .pending_history_operation(),
        Some(&ready)
    );
}

#[test]
fn abort_and_retry_do_not_cancel_a_later_lease_or_release_independent_pauses() {
    let (_directory, mut flow, operation) = active_fixture();
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    let pause = Uuid::new_v4().into();
    flow.transition(
        flow.state().revision(),
        FlowTransition::ExecutionPaused { key: pause },
    )
    .unwrap();
    let stopping = flow
        .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    let aborted = flow
        .abort_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    assert_eq!(aborted.cancellation, stopping.cancellation);
    assert!(flow.state().pauses().contains(&pause));
    flow.finish_turn(
        operation.active.clone().unwrap(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    flow.reconcile_history_stop().unwrap();
    assert_eq!(
        flow.history_operation(&operation.sender, operation.id),
        Some(&aborted)
    );
    flow.transition(
        flow.state().revision(),
        FlowTransition::ExecutionResumed { key: pause },
    )
    .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::InterruptionRecovered,
    )
    .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let current = flow.state().clone();
    let cursor = flow.cursor().clone();
    assert_eq!(
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        aborted
    );
    assert_eq!(flow.state(), &current);
    assert_eq!(flow.cursor(), &cursor);
}

#[test]
fn idle_or_already_completed_review_needs_no_cancellation() {
    for active in [false, true] {
        let (_directory, mut flow, operation) = if active {
            active_fixture()
        } else {
            reviewed_fixture()
        };
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        if let Some(lease) = operation.active.clone() {
            flow.finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
                .unwrap();
        }
        let ready = flow
            .stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        assert!(matches!(ready.status, HistoryOperationStatus::Ready { .. }));
        assert!(ready.cancellation.is_none());
        assert!(!flow.can_admit_turn());
    }
}

#[test]
fn premature_or_forged_completion_is_rejected_before_tail_repair() {
    for premature in [false, true] {
        let (directory, mut flow, operation) = active_fixture();
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        if !premature {
            flow.finish_turn(
                operation.active.clone().unwrap(),
                InvocationResult::Unobserved(TurnOutcome::Completed),
            )
            .unwrap();
        }
        flow.journal
            .commit(&[FlowFact::HistoryOperation(Box::new(
                HistoryOperationEvent::ExecutionStopped {
                    sender: operation.sender.clone(),
                    id: operation.id,
                    completion: Some(
                        flow.history_operations
                            .find(&operation.sender, operation.id)
                            .unwrap()
                            .accepted
                            .clone(),
                    ),
                    at: Utc::now(),
                },
            ))])
            .unwrap();
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
                .is_err()
        );
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        assert!(session.open_flow(&operation.flow.instance_id).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

#[test]
fn cancellation_batches_cannot_split_or_substitute_the_reviewed_control() {
    for invalid_revision in [false, true] {
        let (directory, mut flow, operation) = active_fixture();
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        let control = ControlRequest {
            id: Uuid::new_v4(),
            sender: operation.sender.clone(),
            target: operation.flow.clone(),
            expected_revision: flow.state().revision() + u64::from(invalid_revision),
            pause_key: Uuid::new_v4().into(),
            action: if invalid_revision {
                ControlAction::CancelTurn
            } else {
                ControlAction::Pause
            },
        };
        let at = Utc::now();
        let event = FlowFact::HistoryOperation(Box::new(HistoryOperationEvent::Stopping {
            sender: operation.sender.clone(),
            id: operation.id,
            control: Box::new(control.clone()),
            at,
        }));
        assert!(FlowFact::validate_batch(std::slice::from_ref(&event)).is_err());
        let mut changed = control.clone();
        changed.id = Uuid::new_v4();
        assert!(
            FlowFact::validate_batch(&[
                event.clone(),
                FlowFact::Controlled(Box::new(ControlUpdate {
                    request: changed,
                    at
                }))
            ])
            .is_err()
        );
        flow.journal
            .commit(&[
                event,
                FlowFact::Controlled(Box::new(ControlUpdate {
                    request: control,
                    at,
                })),
            ])
            .unwrap();
        drop(flow);
        let inspection =
            RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0).unwrap();
        assert!(
            inspection
                .history_operation(&operation.flow.instance_id, &operation.sender, operation.id)
                .is_err()
        );
    }
}
