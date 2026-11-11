use super::*;
use crate::flow_store::{
    history_operation::tests::{active_fixture, complete, queue, request},
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload, InputContent},
    source_capture::tests::fixture,
};
use atman_rt::{RunId, TurnId};

fn intent(request: &HistoryOperationRequest) -> HistoryReviewRequest {
    HistoryReviewRequest {
        id: request.id,
        sender: request.sender.clone(),
        flow: request.flow.clone(),
        expected_head: request.expected_head.clone(),
        target: request.target.clone(),
    }
}

#[test]
fn review_is_read_only_and_confirmation_revalidates_captured_queue() {
    let (_directory, mut flow, operation) = active_fixture();
    let before = flow.cursor().clone();
    let state = flow.state().clone();
    let review = flow
        .review_history_operation(intent(&operation), |_| Ok(()))
        .unwrap();
    assert_eq!(review.request, operation);
    assert_eq!(review.affected_turns, Some(0));
    assert_eq!(flow.cursor(), &before);
    assert_eq!(flow.state(), &state);
    assert!(flow.pending_history_operation().is_none());
    queue(&mut flow);
    let after_queue = flow.cursor().clone();
    assert!(
        flow.begin_history_operation(review.request, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &after_queue);
    let refreshed = flow
        .review_history_operation(intent(&operation), |_| Ok(()))
        .unwrap();
    assert_eq!(refreshed.request.queued.len(), 2);
    let accepted = flow
        .begin_history_operation(refreshed.request.clone(), |_| Ok(()))
        .unwrap();
    assert_eq!(accepted.selection, refreshed.selection);
    assert_eq!(accepted.request, refreshed.request);
}

#[test]
fn review_requires_original_head_and_authorization_without_side_effects() {
    let (_directory, flow, operation) = active_fixture();
    let before = flow.cursor().clone();
    let denied = flow
        .review_history_operation(intent(&operation), |_| {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
        })
        .unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    let mut changed = intent(&operation);
    changed.expected_head.branch = ConversationBranchId::random();
    assert!(flow.review_history_operation(changed, |_| Ok(())).is_err());
    let mut foreign = intent(&operation);
    foreign.flow.session_id.0 = Uuid::new_v4();
    assert!(flow.review_history_operation(foreign, |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &before);
    assert!(flow.pending_history_operation().is_none());
}

#[test]
fn review_normalizes_inserted_input_and_counts_turns_in_selected_ancestry() {
    let (_directory, mut flow, prior) = fixture();
    let baseline = complete(&mut flow, prior);
    queue(&mut flow);
    queue(&mut flow);
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
    let admitted = flow.committed_inputs().last().unwrap().message_ids.clone();
    assert_eq!(admitted.len(), 2);
    let inserted = flow
        .accept_input(
            DeliveryRequest {
                expected_head: None,
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::AtBoundary,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection {
                    text: "inserted".into(),
                },
            },
            |_| Ok(()),
        )
        .unwrap()
        .message_id;
    let reserved = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let insertion = flow
        .commit_input(
            &reserved,
            InputContent::Context {
                messages: vec![crate::message::Message::user_text(
                    lease.turn.clone(),
                    "inserted",
                )],
            },
        )
        .unwrap();
    complete(&mut flow, lease.clone());
    let operation = request(
        &flow,
        ConversationCutoff::BeforeUserTurn {
            lease,
            input: inserted,
            receipt: insertion.require_durable().unwrap().clone(),
        },
    );
    let review = flow
        .review_history_operation(intent(&operation), |_| Ok(()))
        .unwrap();
    assert_eq!(review.affected_turns, Some(1));
    assert_eq!(review.admitted_inputs, admitted);
    assert_eq!(
        review.recoverable_inputs,
        [admitted.clone(), vec![inserted]].concat()
    );
    assert!(
        matches!(&review.selection, HistorySelection::Create { parent, .. } if parent.cursor.sequence < admission.sequence())
    );
    flow.begin_history_operation(review.request.clone(), |_| Ok(()))
        .unwrap();
    flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    let recovered = flow
        .history_recovered_input(operation.sender.clone(), operation.id, inserted, |_| Ok(()))
        .unwrap();
    assert_eq!(recovered.content.text, "inserted");
    assert_eq!(recovered.delivery, DeliveryMode::AtBoundary);
    for input in &admitted {
        let recovered = flow
            .history_recovered_input(operation.sender.clone(), operation.id, *input, |_| Ok(()))
            .unwrap();
        assert_eq!(recovered.input, *input);
        assert_eq!(recovered.content.text, "retained input");
        assert_eq!(recovered.delivery, DeliveryMode::NextTurn);
    }
    let remaining = request(&flow, baseline);
    assert_eq!(
        flow.review_history_operation(intent(&remaining), |_| Ok(()))
            .unwrap()
            .affected_turns,
        Some(0)
    );
    let restore = HistoryReviewRequest {
        id: Uuid::new_v4(),
        sender: operation.sender,
        flow: operation.flow,
        expected_head: flow.history_head(),
        target: HistoryOperationTarget::Restore {
            branch: operation.expected_head.branch,
        },
    };
    let restored = flow.review_history_operation(restore, |_| Ok(())).unwrap();
    assert_eq!(restored.affected_turns, None);
    assert!(restored.admitted_inputs.is_empty());
    // The removed turn remains in the journal but cannot be selected on the new branch.
    let mut stale = intent(&review.request);
    stale.id = Uuid::new_v4();
    stale.expected_head = flow.history_head();
    if let HistoryOperationTarget::Revert {
        observed, branch, ..
    } = &mut stale.target
    {
        observed.branch = flow.history_head().branch;
        observed.cursor = flow.cursor().require_durable().unwrap().clone();
        *branch = ConversationBranchId::random();
    }
    assert!(flow.review_history_operation(stale, |_| Ok(())).is_err());
}
