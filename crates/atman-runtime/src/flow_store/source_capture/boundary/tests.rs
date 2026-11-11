use super::*;
use crate::flow_store::{
    completion::InvocationResult,
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
    source_capture::tests::{checkpoint, fixture, publish, texts},
};
use atman_rt::{Interruption, RunId, TurnId};

fn observed(flow: &FlowStore) -> SourceSelection {
    let cursor = flow.cursor().require_durable().unwrap().clone();
    SourceSelection {
        flow: flow.state().address().clone(),
        branch: ConversationBranchId::initial(flow.state().address(), cursor.journal_id),
        cursor,
    }
}

fn accept(flow: &mut FlowStore, text: &str, delivery: DeliveryMode) -> Uuid {
    flow.accept_input(
        DeliveryRequest {
            expected_head: None,
            request_id: Uuid::new_v4(),
            sender: SenderIdentity::User(Uuid::new_v4()),
            recipient: flow.state().address().clone(),
            delivery,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::UserSubmission {
                content: UserContent {
                    text: text.into(),
                    images: vec![],
                    presentation: None,
                },
                invocation: BTreeMap::new(),
            },
        },
        |_| Ok(()),
    )
    .unwrap()
    .message_id
}

fn admit(flow: &mut FlowStore, text: &str) -> (TurnLease<AtmanUuid>, CommitReceipt) {
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    let receipt = flow
        .commit_input(
            &reserved,
            InputContent::InvocationInput {
                command: None,
                arguments: BTreeMap::new(),
                invocation: BTreeMap::new(),
                messages: vec![Message::user_text(lease.turn.clone(), text)],
            },
        )
        .unwrap();
    (lease, receipt.require_durable().unwrap().clone())
}

fn finish(flow: &mut FlowStore, lease: &TurnLease<AtmanUuid>) -> CommitReceipt {
    flow.finish_turn(
        lease.clone(),
        InvocationResult::Unobserved(TurnOutcome::Completed),
    )
    .unwrap()
    .receipt
    .require_durable()
    .unwrap()
    .clone()
}

#[test]
fn batched_and_inserted_inputs_resolve_before_the_whole_original_turn() {
    let (_directory, mut flow, prior) = fixture();
    publish(
        &mut flow,
        &prior,
        Message::assistant_text(prior.turn.clone(), "baseline"),
    );
    finish(&mut flow, &prior);
    let first = accept(&mut flow, "one", DeliveryMode::NextTurn);
    let second = accept(&mut flow, "two", DeliveryMode::NextTurn);
    let before = flow.cursor().require_durable().unwrap().clone();
    let (lease, admission) = admit(&mut flow, "one + two");
    checkpoint(&mut flow, &lease, "summary after selected turn began");
    let inserted = accept(&mut flow, "three", DeliveryMode::AtBoundary);
    let reserved = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let insertion = flow
        .commit_input(
            &reserved,
            InputContent::Context {
                messages: vec![Message::user_text(lease.turn.clone(), "three")],
            },
        )
        .unwrap()
        .require_durable()
        .unwrap()
        .clone();
    for (input, receipt) in [
        (first, &admission),
        (second, &admission),
        (inserted, &insertion),
    ] {
        let request = BoundaryReferenceRequest {
            observed: observed(&flow),
            cutoff: ConversationCutoff::BeforeUserTurn {
                lease: lease.clone(),
                input,
                receipt: receipt.clone(),
            },
            budget: ReferenceBudget::default(),
        };
        let (plan, boundary) = flow.capture_reference_boundary_plan(&request).unwrap();
        let (source, reference) = plan.prepare().unwrap();
        assert_eq!(source.cursor, before);
        assert_eq!(boundary.admission.as_ref(), Some(&admission));
        assert_eq!(boundary.selected, request.cutoff);
        assert_eq!(texts(&reference), ["baseline"]);
    }
    let batch = flow.journal.read_owned(&admission.into()).unwrap();
    assert!(matches!(batch.facts.as_slice(), [
        FlowFact::Transition(super::super::super::FlowTransition::TurnStarted { .. }),
        FlowFact::InputCommitted(input),
    ] if input.message_ids == [first, second]));
}

#[test]
fn completed_cutoff_excludes_later_turns_and_their_checkpoints() {
    let (_directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "answer"),
    );
    let completion = finish(&mut flow, &lease);
    let request = BoundaryReferenceRequest {
        observed: observed(&flow),
        cutoff: ConversationCutoff::AfterCompletedTurn {
            lease,
            receipt: completion.clone(),
        },
        budget: ReferenceBudget::default(),
    };
    accept(&mut flow, "later", DeliveryMode::NextTurn);
    let (later, _) = admit(&mut flow, "later");
    checkpoint(&mut flow, &later, "later summary");
    let (plan, boundary) = flow.capture_reference_boundary_plan(&request).unwrap();
    let (source, reference) = plan.prepare().unwrap();
    assert_eq!(source.cursor, completion);
    assert!(boundary.admission.is_none());
    assert_eq!(texts(&reference), ["answer"]);
}

#[test]
fn boundary_rejects_foreign_or_unacknowledged_identity_without_reading_payloads() {
    let (_directory, mut flow, prior) = fixture();
    finish(&mut flow, &prior);
    let input = accept(&mut flow, "selected", DeliveryMode::NextTurn);
    let before = observed(&flow);
    let (lease, receipt) = admit(&mut flow, "selected");
    let request = BoundaryReferenceRequest {
        observed: observed(&flow),
        cutoff: ConversationCutoff::BeforeUserTurn {
            lease,
            input,
            receipt,
        },
        budget: ReferenceBudget::default(),
    };
    let mut forged = request.clone();
    forged.observed.cursor.digest = "wrong".into();
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request.clone();
    forged.observed = before;
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request.clone();
    forged.observed.branch = ConversationBranchId::initial(flow.state().address(), Uuid::new_v4());
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request.clone();
    forged.observed.flow.instance_id = super::super::super::FlowInstanceId::now();
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request.clone();
    let ConversationCutoff::BeforeUserTurn { lease, .. } = &mut forged.cutoff else {
        unreachable!()
    };
    lease.run = RunId::now();
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request.clone();
    let ConversationCutoff::BeforeUserTurn { input, .. } = &mut forged.cutoff else {
        unreachable!()
    };
    *input = accept(&mut flow, "still pending", DeliveryMode::NextTurn);
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
    forged = request;
    let ConversationCutoff::BeforeUserTurn { receipt, .. } = &mut forged.cutoff else {
        unreachable!()
    };
    receipt.digest = "wrong".into();
    assert!(flow.capture_reference_boundary_plan(&forged).is_err());
}

#[test]
fn final_publication_and_interrupted_turn_are_not_successful_completion_boundaries() {
    let (_directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "answer"),
    );
    let mut request = BoundaryReferenceRequest {
        observed: observed(&flow),
        cutoff: ConversationCutoff::AfterCompletedTurn {
            lease: lease.clone(),
            receipt: flow.cursor().require_durable().unwrap().clone(),
        },
        budget: ReferenceBudget::default(),
    };
    assert!(flow.capture_reference_boundary_plan(&request).is_err());
    let completion = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Interrupted(Interruption::HostLost)),
        )
        .unwrap();
    request.observed = observed(&flow);
    request.cutoff = ConversationCutoff::AfterCompletedTurn {
        lease,
        receipt: completion.receipt.require_durable().unwrap().clone(),
    };
    assert!(flow.capture_reference_boundary_plan(&request).is_err());
}

#[test]
fn message_role_cannot_turn_flow_authored_input_into_a_user_boundary() {
    let (_directory, mut flow, prior) = fixture();
    finish(&mut flow, &prior);
    let input = flow
        .accept_input(
            DeliveryRequest {
                expected_head: None,
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::Flow(flow.state().address().clone()),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::AtBoundary,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection {
                    text: "not a User".into(),
                },
            },
            |_| Ok(()),
        )
        .unwrap()
        .message_id;
    let (lease, receipt) = admit(&mut flow, "not a User");
    let request = BoundaryReferenceRequest {
        observed: observed(&flow),
        cutoff: ConversationCutoff::BeforeUserTurn {
            lease,
            input,
            receipt,
        },
        budget: ReferenceBudget::default(),
    };
    assert!(flow.capture_reference_boundary_plan(&request).is_err());
}
