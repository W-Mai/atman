use super::*;
use crate::flow_store::{
    completion::InvocationResult,
    history_operation::{HistoryOperationTarget, HistoryReviewRequest},
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity},
    source_capture::tests::fixture,
};
use atman_rt::{RunId, TurnId};

fn request(flow: &FlowStore) -> ForkBoundaryRequest {
    ForkBoundaryRequest {
        observed: SourceSelection {
            flow: flow.state().address().clone(),
            branch: flow.history_head().branch,
            cursor: flow.cursor().require_durable().unwrap().clone(),
        },
        expected_head: flow.history_head(),
        cutoff: ForkCutoff::LatestSafe,
    }
}

#[test]
fn fork_tail_excludes_unfinished_turns_and_remains_at_its_original_observation() {
    let (_directory, mut flow, first) = fixture();
    let initial = request(&flow);
    let baseline = flow.resolve_fork_boundary(&initial, |_| Ok(())).unwrap();
    assert!(baseline.boundary.is_none());
    assert_eq!(baseline.source.cursor.sequence, 1);
    let completed = flow
        .finish_turn(first, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    let accepted = flow
        .accept_input(
            DeliveryRequest {
                expected_head: Some(flow.history_head()),
                request_id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::now_v7()),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection {
                    text: "new turn".into(),
                },
            },
            |_| Ok(()),
        )
        .unwrap();
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    let admission = flow
        .commit_input(
            &reserved,
            InputContent::InvocationInput {
                command: None,
                arguments: Default::default(),
                invocation: Default::default(),
                messages: vec![],
            },
        )
        .unwrap();
    let observed = request(&flow);
    let cursor = flow.cursor().clone();
    let state = flow.state().clone();
    let tail = flow.resolve_fork_boundary(&observed, |_| Ok(())).unwrap();
    assert_eq!(
        &tail.source.cursor,
        completed.receipt.require_durable().unwrap()
    );
    assert_eq!(tail.excluded_turns, 1);
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.state(), &state, "review cannot stop its source");
    let explicit = ForkBoundaryRequest {
        cutoff: ForkCutoff::At(Box::new(ConversationCutoff::BeforeUserTurn {
            lease: lease.clone(),
            input: accepted.message_id,
            receipt: admission.require_durable().unwrap().clone(),
        })),
        ..observed.clone()
    };
    let before = flow.resolve_fork_boundary(&explicit, |_| Ok(())).unwrap();
    assert_eq!(
        before.boundary.unwrap().admission.as_ref(),
        Some(admission.require_durable().unwrap())
    );
    assert_eq!(before.excluded_turns, 1);
    let latest = flow
        .finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    assert_eq!(
        flow.resolve_fork_boundary(&observed, |_| Ok(())).unwrap(),
        tail
    );
    assert_eq!(
        &flow
            .resolve_fork_boundary(&request(&flow), |_| Ok(()))
            .unwrap()
            .source
            .cursor,
        latest.receipt.require_durable().unwrap()
    );
}

#[test]
fn fork_boundaries_reject_forged_observations_and_old_head_versions_after_restore() {
    let (_directory, mut flow, lease) = fixture();
    let completion = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    let original = request(&flow);
    flow.transition(
        flow.state().revision(),
        crate::flow_store::FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("active descendant")
    };
    flow.finish_turn(
        active.lease.clone(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    let cutoff = ConversationCutoff::AfterCompletedTurn {
        lease,
        receipt: completion.receipt.require_durable().unwrap().clone(),
    };
    let operation = flow
        .review_history_operation(
            HistoryReviewRequest {
                id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::now_v7()),
                flow: original.observed.flow.clone(),
                expected_head: original.expected_head.clone(),
                target: HistoryOperationTarget::Revert {
                    observed: original.observed.clone(),
                    cutoff: Box::new(cutoff),
                    branch: ConversationBranchId::random(),
                },
            },
            |_| Ok(()),
        )
        .unwrap()
        .request;
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    let selected = flow
        .resolve_fork_boundary(&request(&flow), |_| Ok(()))
        .unwrap();
    assert_eq!(selected.source.branch, flow.history_head().branch);
    assert_eq!(selected.source.cursor, original.observed.cursor);
    assert!(flow.resolve_fork_boundary(&original, |_| Ok(())).is_err());
    let restore = flow
        .review_history_operation(
            HistoryReviewRequest {
                id: Uuid::now_v7(),
                sender: operation.sender.clone(),
                flow: original.observed.flow.clone(),
                expected_head: flow.history_head(),
                target: HistoryOperationTarget::Restore {
                    branch: original.expected_head.branch,
                },
            },
            |_| Ok(()),
        )
        .unwrap()
        .request;
    flow.begin_history_operation(restore.clone(), |_| Ok(()))
        .unwrap();
    flow.stop_history_operation(restore.sender.clone(), restore.id, |_| Ok(()))
        .unwrap();
    flow.apply_history_operation(restore.sender, restore.id, |_| Ok(()))
        .unwrap();
    assert_eq!(flow.history_head().branch, original.expected_head.branch);
    assert!(flow.resolve_fork_boundary(&original, |_| Ok(())).is_err());
    let current = request(&flow);
    let cursor = flow.cursor().clone();
    let mut forged = current.clone();
    forged.observed.cursor.digest = "0".repeat(64);
    assert!(flow.resolve_fork_boundary(&forged, |_| Ok(())).is_err());
    forged.observed.flow.session_id.0 = Uuid::now_v7();
    let error = flow
        .resolve_fork_boundary(&forged, |_| Err(io::ErrorKind::PermissionDenied.into()))
        .unwrap_err();
    assert_eq!(
        error.kind(),
        io::ErrorKind::PermissionDenied,
        "authority precedes source inspection"
    );
    assert_eq!(flow.cursor(), &cursor);
}
