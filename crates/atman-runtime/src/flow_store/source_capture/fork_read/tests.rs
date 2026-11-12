use super::*;
use crate::{
    flow_store::source_capture::tests::{checkpoint, fixture, publish},
    flow_store::{
        FlowTransition,
        completion::InvocationResult,
        history_operation::{HistoryOperationTarget, HistoryReviewRequest},
        inbox::SenderIdentity,
    },
    projection::flow::FlowRecord,
};
use atman_rt::{RunId, TurnId, TurnOutcome};

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

fn shared(read: &ForkConversation) -> Vec<String> {
    read.transcript
        .entries()
        .iter()
        .flat_map(|entry| entry.record.shared_messages())
        .map(Message::text_concat)
        .collect()
}

#[test]
fn fork_read_preserves_full_history_and_selects_model_checkpoint_at_the_same_boundary() {
    let (_directory, mut flow, lease) = fixture();
    let baseline = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    assert!(baseline.transcript.entries().is_empty());
    assert!(baseline.model_context.is_empty());
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "original"),
    );
    let original = flow.cursor().clone();
    checkpoint(&mut flow, &lease, "summary");
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "suffix"),
    );
    let completion = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    let observed = request(&flow);
    let state = flow.state().clone();
    let plan = flow
        .capture_fork_conversation(&observed, ForkReadBudget::default(), |_| Ok(()))
        .unwrap();
    assert_eq!(flow.state(), &state);
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("active")
    };
    let next = active.lease.clone();
    checkpoint(&mut flow, &next, "later summary");
    publish(
        &mut flow,
        &next,
        Message::assistant_text(next.turn.clone(), "later output"),
    );
    let running = flow.state().clone();
    let read = plan.prepare().unwrap();
    assert_eq!(flow.state(), &running);
    assert_eq!(shared(&read), ["original", "suffix"]);
    assert_eq!(
        read.model_context
            .iter()
            .map(Message::text_concat)
            .collect::<Vec<_>>(),
        ["summary", "suffix"]
    );
    assert_eq!(read.transcript.cursor(), Some(&completion.receipt));
    assert_eq!(read.transcript.entries()[0].id.commit, original.sequence());
    assert_eq!(read.transcript.flow(), &observed.observed.flow);
    assert!(matches!(
        read.transcript.entries().last().unwrap().record,
        FlowRecord::Completion(_)
    ));
    assert_eq!(read.transcript.turns().count(), 1);
    assert!(
        read.transcript
            .turns()
            .next()
            .unwrap()
            .completion()
            .is_some()
    );
    let later = flow
        .capture_fork_conversation(&observed, ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(shared(&later), shared(&read));
    assert_eq!(later.model_context, read.model_context);

    let mut forged = observed.clone();
    forged.observed.cursor.digest = "0".repeat(64);
    assert!(
        flow.capture_fork_conversation(&forged, ForkReadBudget::default(), |_| Ok(()))
            .is_err()
    );
    for budget in [
        ForkReadBudget {
            max_bytes: 1,
            ..Default::default()
        },
        ForkReadBudget {
            max_batches: 1,
            ..Default::default()
        },
    ] {
        assert!(
            flow.capture_fork_conversation(&observed, budget, |_| Ok(()))
                .is_err()
        );
    }
    let result = flow.capture_fork_conversation(
        &forged,
        ForkReadBudget {
            max_bytes: 0,
            max_batches: 0,
        },
        |_| Err(io::ErrorKind::PermissionDenied.into()),
    );
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
}

#[test]
fn fork_read_uses_selected_ancestry_without_relabeling_original_records() {
    let (_directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "retained"),
    );
    checkpoint(&mut flow, &lease, "retained summary");
    let completion = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    let original = request(&flow);
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("active")
    };
    let abandoned = active.lease.clone();
    publish(
        &mut flow,
        &abandoned,
        Message::assistant_text(abandoned.turn.clone(), "abandoned"),
    );
    checkpoint(&mut flow, &abandoned, "abandoned summary");
    flow.finish_turn(
        abandoned,
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    let operation = flow
        .review_history_operation(
            HistoryReviewRequest {
                id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::now_v7()),
                flow: original.observed.flow.clone(),
                expected_head: flow.history_head(),
                target: HistoryOperationTarget::Revert {
                    observed: request(&flow).observed,
                    cutoff: Box::new(ConversationCutoff::AfterCompletedTurn {
                        lease,
                        receipt: completion.receipt.require_durable().unwrap().clone(),
                    }),
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
    let read = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(shared(&read), ["retained"]);
    assert_eq!(
        read.model_context
            .iter()
            .map(Message::text_concat)
            .collect::<Vec<_>>(),
        ["retained summary"]
    );
    assert_eq!(read.transcript.history_head(), Some(&flow.history_head()));
    assert_eq!(read.transcript.cursor(), Some(&completion.receipt));
    assert!(
        flow.capture_fork_conversation(&original, ForkReadBudget::default(), |_| Ok(()))
            .is_err()
    );
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
        flow.state().state()
    else {
        panic!("active")
    };
    let divergent = active.lease.clone();
    publish(
        &mut flow,
        &divergent,
        Message::assistant_text(divergent.turn.clone(), "divergent"),
    );
    flow.finish_turn(
        divergent,
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    let selected = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(shared(&selected), ["retained", "divergent"]);
    assert_eq!(
        selected
            .model_context
            .iter()
            .map(Message::text_concat)
            .collect::<Vec<_>>(),
        ["retained summary", "divergent"]
    );
    assert_eq!(
        selected.transcript.entries()[0].id,
        read.transcript.entries()[0].id
    );
    assert_eq!(selected.transcript.turns().count(), 2);
}

#[test]
fn fork_read_keeps_authored_boundary_identity_and_does_not_admit_pending_input() {
    use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload};
    let (_directory, mut flow, initial) = fixture();
    flow.finish_turn(
        initial,
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    let authored = flow
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
                    text: "authored".into(),
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
                messages: vec![Message::user_text(lease.turn.clone(), "authored")],
            },
        )
        .unwrap();
    flow.finish_turn(
        lease.clone(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap();
    flow.accept_input(
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
                text: "pending".into(),
            },
        },
        |_| Ok(()),
    )
    .unwrap();
    let cursor = flow.cursor().clone();
    let read = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(shared(&read), ["authored"]);
    let input = read
        .transcript
        .entries()
        .iter()
        .find(|entry| matches!(entry.record, FlowRecord::Input(_)))
        .unwrap();
    assert_eq!(
        read.transcript.user_turn_boundary(input.id),
        Some(ConversationCutoff::BeforeUserTurn {
            lease,
            input: authored.message_id,
            receipt: admission.require_durable().unwrap().clone(),
        })
    );
}

#[test]
fn fork_read_rejects_incomplete_tool_context_instead_of_silently_filtering_it() {
    let (_directory, mut flow, lease) = fixture();
    let mut message = Message::assistant_text(lease.turn.clone(), "");
    message.parts = vec![MessagePart::ToolUse {
        id: "pending".into(),
        name: "read".into(),
        input: serde_json::json!({}),
        intent: None,
    }];
    publish(&mut flow, &lease, message);
    flow.finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    let plan = flow
        .capture_fork_conversation(&request(&flow), ForkReadBudget::default(), |_| Ok(()))
        .unwrap();
    assert!(plan.prepare().is_err());
}
