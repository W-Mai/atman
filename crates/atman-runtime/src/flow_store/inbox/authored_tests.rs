use super::*;
use crate::flow_runtime::FlowWorker;
use crate::flow_store::{
    FlowStore, RuntimeStore,
    completion::InvocationResult,
    history_operation::{HistoryOperationTarget, HistoryReviewRequest},
    local_operator::LocalOperator,
    source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection, tests::fixture},
    task_scope::TaskScopeRef,
};
use atman_rt::TurnOutcome;

fn request(flow: &FlowStore) -> DeliveryRequest {
    DeliveryRequest {
        expected_head: None,
        request_id: Uuid::new_v4(),
        sender: LocalOperator::NATIVE.sender(),
        recipient: flow.state().address().clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::UserSubmission {
            content: UserContent {
                text: "prepared authored content".into(),
                images: vec![],
                presentation: None,
            },
            invocation: BTreeMap::new(),
        },
    }
}

#[tokio::test]
async fn authored_acceptance_keeps_original_receipt_across_prepare_edits_head_and_reopen() {
    let (directory, mut flow, lease) = fixture();
    let completed = flow
        .finish_turn(
            lease.clone(),
            InvocationResult::Unobserved(TurnOutcome::Completed),
        )
        .unwrap();
    let original = request(&flow);
    let fingerprint = AuthoredInputFingerprint::for_request(&serde_json::json!({
        "flow": original.recipient,
        "text": "original authored text before preparation"
    }))
    .unwrap();
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    alternate.payload = InboxPayload::UserSubmission {
        content: UserContent {
            text: "another prepared projection of the same authored request".into(),
            images: vec![],
            presentation: None,
        },
        invocation: BTreeMap::new(),
    };
    let worker = FlowWorker::start(flow).await.unwrap();
    let (native, rpc) = tokio::join!(
        worker.accept_authored_input(
            LocalOperator::NATIVE,
            original.clone(),
            fingerprint,
            |_| Ok(())
        ),
        worker.accept_authored_input(
            LocalOperator::LEGACY_RPC,
            alternate.clone(),
            fingerprint,
            |_| Ok(())
        ),
    );
    let accepted = native.unwrap();
    assert_eq!(accepted, rpc.unwrap());
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    assert_eq!(
        worker
            .observe_authored_input(LocalOperator::LEGACY_RPC, original.request_id, fingerprint,)
            .await
            .unwrap(),
        Some(accepted.clone()),
    );
    let mut flow = worker.shutdown().await.unwrap();
    let sender = flow.inbox_entries()[0].envelope.request.sender.clone();
    flow.edit_input(
        InboxEditRequest {
            expected_head: None,
            id: Uuid::new_v4(),
            sender,
            message: accepted.message_id,
            expected: accepted.receipt.clone(),
            change: InboxChange::Replace {
                content: UserContent {
                    text: "edited pending content".into(),
                    images: vec![],
                    presentation: None,
                },
                delivery: DeliveryMode::NextTurn,
            },
        },
        |_| Ok(()),
    )
    .unwrap();
    let before_head = flow.history_head();
    let address = flow.state().address().clone();
    let operation = flow
        .review_history_operation(
            HistoryReviewRequest {
                id: Uuid::new_v4(),
                sender: LocalOperator::NATIVE.sender(),
                flow: address.clone(),
                expected_head: before_head.clone(),
                target: HistoryOperationTarget::Revert {
                    observed: SourceSelection {
                        flow: address.clone(),
                        branch: before_head.branch,
                        cursor: flow.cursor().require_durable().unwrap().clone(),
                    },
                    cutoff: Box::new(ConversationCutoff::AfterCompletedTurn {
                        lease,
                        receipt: completed.receipt.require_durable().unwrap().clone(),
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
    flow.apply_history_operation(operation.sender, operation.id, |_| Ok(()))
        .unwrap();
    assert_ne!(flow.history_head(), before_head);
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(address.session_id.0).unwrap();
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_ne!(flow.history_head(), before_head);
    let before_retry = flow.cursor().clone();
    assert_eq!(
        flow.accept_authored_input(
            LocalOperator::LEGACY_RPC,
            alternate,
            fingerprint,
            |_| panic!("accepted retry is not new input"),
        )
        .unwrap(),
        accepted,
    );
    assert_eq!(flow.cursor(), &before_retry);
    let metadata = serde_json::to_string(flow.inbox_entries()).unwrap();
    assert!(!metadata.contains("prepared authored content"));
    assert!(!metadata.contains("edited pending content"));
    let saved = flow.inbox_entry(accepted.message_id).unwrap().unwrap();
    assert_eq!(saved.envelope.authored, Some(fingerprint));
    assert!(matches!(saved.envelope.request.payload,
        InboxPayload::UserSubmission {content, ..} if content.text == "edited pending content"));
    let mut fresh = original;
    fresh.request_id = Uuid::new_v4();
    assert!(
        flow.accept_authored_input(LocalOperator::NATIVE, fresh, fingerprint, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &before_retry);
}

#[test]
fn authored_identity_rejects_conflicts_unproven_legacy_and_scoped_or_forged_input() {
    let (_directory, mut flow, _) = fixture();
    let original = request(&flow);
    let fingerprint = AuthoredInputFingerprint::for_request(&original).unwrap();
    let accepted = flow
        .accept_authored_input(LocalOperator::NATIVE, original.clone(), fingerprint, |_| {
            Ok(())
        })
        .unwrap();
    let before = flow.cursor().clone();
    let conflict = AuthoredInputFingerprint::for_request(&"different authored input").unwrap();
    assert!(
        flow.observe_authored_input(LocalOperator::NATIVE, original.request_id, conflict)
            .is_err()
    );
    assert!(
        flow.accept_authored_input(LocalOperator::NATIVE, original.clone(), conflict, |_| {
            panic!("conflicting input reached new authorization")
        })
        .is_err()
    );
    assert!(
        flow.observe_authored_input(
            LocalOperator::new(Uuid::new_v4()),
            original.request_id,
            fingerprint,
        )
        .unwrap()
        .is_none()
    );
    let mut invalid = Vec::new();
    let mut scoped = original.clone();
    scoped.scope = Some(TaskScopeRef {
        owner: original.recipient.clone(),
        id: Uuid::new_v4(),
    });
    invalid.push(scoped);
    let mut caused = original.clone();
    caused.cause = Some(FlowEventRef {
        flow: original.recipient.clone(),
        sequence: 1,
        index: 0,
    });
    invalid.push(caused);
    let mut correlated = original.clone();
    correlated.correlation = Some(Uuid::new_v4());
    invalid.push(correlated);
    let mut forged = original.clone();
    forged.sender = SenderIdentity::User(Uuid::new_v4());
    invalid.push(forged);
    let mut foreign = original.clone();
    foreign.recipient.instance_id = crate::flow_store::FlowInstanceId::now();
    invalid.push(foreign);
    for mut request in invalid {
        request.request_id = Uuid::new_v4();
        assert!(
            flow.accept_authored_input(LocalOperator::NATIVE, request, fingerprint, |_| {
                panic!("invalid authored input reached new authorization")
            })
            .is_err()
        );
    }
    assert_eq!(flow.cursor(), &before);
    assert_eq!(flow.inbox_entries().len(), 1);
    assert_eq!(flow.inbox_entries()[0].acceptance, accepted);
    let mut legacy = original;
    legacy.request_id = Uuid::new_v4();
    flow.accept_operator_input(LocalOperator::NATIVE, legacy.clone(), |_| Ok(()))
        .unwrap();
    let before = flow.cursor().clone();
    assert!(
        flow.observe_authored_input(LocalOperator::NATIVE, legacy.request_id, fingerprint)
            .is_err()
    );
    assert_eq!(flow.cursor(), &before);
}

#[test]
fn authored_fingerprints_preserve_json_equality_and_absent_legacy_wire() {
    let left = serde_json::json!({"a": [1, {"b": -0.0}], "c": "literal text"});
    let right = serde_json::json!({"c": "literal text", "a": [1, {"b": 0.0}]});
    assert_eq!(
        AuthoredInputFingerprint::for_request(&left).unwrap(),
        AuthoredInputFingerprint::for_request(&right).unwrap(),
    );
    let (_directory, flow, _) = fixture();
    let envelope = AcceptedEnvelope {
        id: Uuid::new_v4(),
        accepted_at: Utc::now(),
        request: request(&flow),
        authored: None,
    };
    let legacy = serde_json::to_value(&envelope).unwrap();
    assert!(legacy.get("authored").is_none());
    assert_eq!(
        serde_json::from_value::<AcceptedEnvelope>(legacy).unwrap(),
        envelope,
    );
}

#[tokio::test]
async fn narrow_operator_edits_preserve_authored_fields_and_reconcile_after_restart() {
    use crate::user_input::QuoteSnapshot;

    let (directory, mut flow, _) = fixture();
    let mut original = request(&flow);
    let InboxPayload::UserSubmission { content, .. } = &mut original.payload else {
        unreachable!()
    };
    let quote = QuoteSnapshot {
        text: "Reference".into(),
    };
    let presentation = UserInputPresentation {
        prompt: "Original".into(),
        quote: Some(quote.clone()),
    };
    content.text = presentation.model_text();
    content.presentation = Some(presentation);
    let address = original.recipient.clone();
    let accepted = flow.accept_input(original.clone(), |_| Ok(())).unwrap();
    let mut hidden = original;
    hidden.request_id = Uuid::new_v4();
    hidden.sender = SenderIdentity::User(Uuid::new_v4());
    let hidden = flow.accept_input(hidden, |_| Ok(())).unwrap();
    let page = flow
        .operator_inbox_page(LocalOperator::LEGACY_RPC, 0)
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.items[0].text, "Original");
    assert!(page.items[0].has_quote);
    let edit = OperatorInboxEdit {
        expected_head: flow.history_head(),
        id: Uuid::new_v4(),
        message: accepted.message_id,
        expected: accepted.receipt,
        change: OperatorInboxChange::EditText("Edited".into()),
    };
    let worker = FlowWorker::start(flow).await.unwrap();
    let hidden_edit = OperatorInboxEdit {
        message: hidden.message_id,
        expected: hidden.receipt,
        ..edit.clone()
    };
    assert!(
        worker
            .edit_operator_inbox(LocalOperator::LEGACY_RPC, hidden_edit)
            .await
            .is_err()
    );
    let receipt = worker
        .edit_operator_inbox(LocalOperator::LEGACY_RPC, edit.clone())
        .await
        .unwrap();
    let different_action = OperatorInboxEdit {
        change: OperatorInboxChange::SetDelivery(DeliveryMode::NextTurn),
        ..edit.clone()
    };
    assert!(
        worker
            .edit_operator_inbox(LocalOperator::NATIVE, different_action)
            .await
            .is_err()
    );
    let mut flow = worker.shutdown().await.unwrap();
    let body = flow
        .inbox_entry(accepted.message_id)
        .unwrap()
        .unwrap()
        .envelope
        .request
        .editable_content()
        .unwrap();
    assert_eq!(body.authored_text(), "Edited");
    assert_eq!(body.text, "> Reference\n\nEdited");
    assert_eq!(body.presentation.unwrap().quote, Some(quote));
    let worker = FlowWorker::start(flow).await.unwrap();
    let cancel = OperatorInboxEdit {
        id: Uuid::new_v4(),
        expected: receipt.clone(),
        change: OperatorInboxChange::Cancel,
        ..edit.clone()
    };
    worker
        .edit_operator_inbox(LocalOperator::NATIVE, cancel)
        .await
        .unwrap();
    drop(worker.shutdown().await.unwrap());
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(address.session_id.0).unwrap();
    let worker = FlowWorker::start(session.open_flow(&address.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(
        worker
            .edit_operator_inbox(LocalOperator::NATIVE, edit)
            .await
            .unwrap(),
        receipt
    );
    worker.shutdown().await.unwrap();
}
