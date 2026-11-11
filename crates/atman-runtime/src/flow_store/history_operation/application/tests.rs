use super::*;
use crate::{
    flow_store::{
        FlowTransition, RuntimeStore,
        context::{ContextCheckpoint, ContextCheckpointPurpose, context_digest},
        history_operation::tests::{complete, queue, request},
        source_capture::tests::{fixture, publish},
    },
    message::Message,
};
use atman_rt::{RunId, TurnId};

fn turn(flow: &mut FlowStore, text: &str) -> ConversationCutoff {
    use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, InputContent};
    let accepted = flow
        .accept_input(
            DeliveryRequest {
                expected_head: Some(flow.history_head()),
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: flow.state().address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection { text: text.into() },
            },
            |_| Ok(()),
        )
        .unwrap();
    let reserved = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |entry| {
            entry.id == accepted.message_id
        })
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    flow.commit_input(
        &reserved,
        InputContent::InvocationInput {
            command: None,
            arguments: Default::default(),
            invocation: Default::default(),
            messages: vec![Message::user_text(lease.turn.clone(), text)],
        },
    )
    .unwrap();
    publish(
        flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), text),
    );
    complete(flow, lease)
}

fn reviewed() -> (tempfile::TempDir, FlowStore, HistoryOperationRequest) {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "retained"),
    );
    let cutoff = complete(&mut flow, lease);
    turn(&mut flow, "abandoned");
    queue(&mut flow);
    let operation = request(&flow, cutoff);
    (directory, flow, operation)
}

fn apply(flow: &mut FlowStore, operation: &HistoryOperationRequest) -> HistoryOperationRecord {
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap()
}

fn unchanged_branch() -> (tempfile::TempDir, FlowStore, HistoryOperationRequest) {
    let (directory, mut flow, lease) = fixture();
    publish(
        &mut flow,
        &lease,
        Message::assistant_text(lease.turn.clone(), "retained context ".repeat(100)),
    );
    let cutoff = complete(&mut flow, lease);
    let operation = request(&flow, cutoff);
    (directory, flow, operation)
}

fn checkpoint(flow: &mut FlowStore) -> ContextCheckpoint {
    let window = flow.context_messages().unwrap();
    ContextCheckpoint {
        id: Uuid::new_v4(),
        expected_head: Some(flow.history_head()),
        purpose: ContextCheckpointPurpose::Compaction,
        expected: context_digest(&window).unwrap(),
        messages: vec![Message::assistant_text(
            window[0].turn_id.clone(),
            "summary",
        )],
        summary: "summary".into(),
        range: 0..window.len(),
        count: window.len(),
        before_tokens: 100,
        after_tokens: 5,
    }
}

#[tokio::test]
async fn checkpoint_window_retains_captured_head_across_equal_content_selection() {
    let (_directory, flow, operation) = unchanged_branch();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let captured = worker.context().messages();
    assert_eq!(captured.history_head(), Some(&operation.expected_head));
    let mut flow = worker.shutdown().await.unwrap();
    apply(&mut flow, &operation);
    let head = flow.history_head();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let current = context.messages();
    assert_eq!(captured.as_ref(), current.as_ref());
    assert_eq!(current.history_head(), Some(&head));
    let replacement = vec![Message::assistant_text(current[0].turn_id.clone(), "short")];
    assert!(
        context
            .commit_rewritten_window(&captured, replacement.clone(), 100, 1)
            .await
            .is_err()
    );
    assert_eq!(context.messages().as_ref(), current.as_ref());
    context
        .commit_rewritten_window(&current, replacement.clone(), 100, 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.messages().as_ref(), replacement.as_slice());
    assert_eq!(context.messages().history_head(), Some(&head));
    assert_eq!(current.as_ref(), captured.as_ref());
    assert_eq!(captured.history_head(), Some(&operation.expected_head));
    let flow = worker.shutdown().await.unwrap();
    assert_eq!(
        flow.latest_context_checkpoint()
            .unwrap()
            .checkpoint
            .expected_head,
        Some(head)
    );
}

#[test]
fn checkpoint_head_rejects_stale_results_but_preserves_accepted_retry_receipts() {
    for legacy in [false, true] {
        let (directory, mut flow, operation) = unchanged_branch();
        let original = flow.context_messages().unwrap();
        let mut accepted = checkpoint(&mut flow);
        if legacy {
            accepted.expected_head = None;
            assert!(
                serde_json::to_value(&accepted)
                    .unwrap()
                    .get("expected_head")
                    .is_none()
            );
        }
        let saved = flow.checkpoint_context(accepted.clone()).unwrap();
        let mut operation = operation;
        operation.execution_revision = flow.state().revision();
        apply(&mut flow, &operation);
        assert_eq!(flow.context_messages().unwrap(), original);
        let cursor = flow.cursor().clone();
        let mut stale = accepted.clone();
        stale.id = Uuid::new_v4();
        assert!(flow.checkpoint_context(stale).is_err());
        assert_eq!(flow.checkpoint_context(accepted.clone()).unwrap(), saved);
        assert_eq!(flow.cursor(), &cursor);
        assert_eq!(flow.context_messages().unwrap(), original);
        let head = flow.history_head();
        let mut fresh = checkpoint(&mut flow);
        fresh.expected_head = None;
        assert!(flow.checkpoint_context(fresh.clone()).is_err());
        fresh.expected_head = Some(head);
        let applied = flow.checkpoint_context(fresh.clone()).unwrap();
        drop(flow);
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
        assert_eq!(flow.checkpoint_context(accepted).unwrap(), saved);
        assert_eq!(flow.latest_context_checkpoint(), Some(&applied));
        assert_eq!(flow.context_messages().unwrap(), fresh.messages);
    }
}

#[test]
fn stale_checkpoint_head_replay_fails_before_tail_repair() {
    for missing in [false, true] {
        let (directory, mut flow, operation) = unchanged_branch();
        let mut stale = checkpoint(&mut flow);
        apply(&mut flow, &operation);
        if missing {
            stale.expected_head = None;
        }
        flow.journal
            .commit(&[FlowFact::ContextCheckpoint(Box::new(stale))])
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
                .inspect_flow(&operation.flow.instance_id)
                .is_err()
        );
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        assert!(session.open_flow(&operation.flow.instance_id).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[tokio::test]
async fn late_forms_keep_registered_heads_and_accepted_retries_across_history_selection() {
    use crate::form::{CompositeForm, FormAnswer, FormKind, FormQuestion, FormSubmission};
    use crate::rendezvous::{PromptId, PromptResolver};
    let (_directory, flow, operation) = unchanged_branch();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let forms = context.forms();
    let _view = forms.subscribe();
    let resolver = crate::rendezvous::FormPromptResolver::new(forms.clone());
    let form = CompositeForm {
        questions: vec![FormQuestion {
            id: "scope".into(),
            kind: FormKind::Confirm {
                prompt: "Proceed?".into(),
            },
        }],
    };
    let answer = FormSubmission::Submitted {
        answers: vec![FormAnswer::Confirmed { value: true }],
    };
    let accepted = PromptId::now();
    let waiting =
        resolver.register_with_payload(accepted, "form_ask", serde_json::to_value(&form).unwrap());
    assert!(resolver.expire_pending(&accepted));
    assert!(waiting.await.is_err());
    assert!(
        forms
            .submit(&accepted.to_string(), answer.clone())
            .await
            .unwrap()
    );
    let snapshot = worker.snapshot().await.unwrap();
    let crate::flow_store::inbox::InboxPayload::FormReply {
        answer: accepted_answer,
    } = &snapshot.pending[0].request.payload
    else {
        panic!("form answer is missing");
    };
    assert_eq!(
        accepted_answer.expected_head.as_ref(),
        Some(&operation.expected_head)
    );
    assert_eq!(
        snapshot.pending[0].request.expected_head,
        accepted_answer.expected_head
    );
    let accepted_answer = accepted_answer.clone();
    let late = PromptId::now();
    let waiting =
        resolver.register_with_payload(late, "form_ask", serde_json::to_value(&form).unwrap());
    assert!(resolver.expire_pending(&late));
    assert!(waiting.await.is_err());
    let HistoryOperationTarget::Revert { cutoff, .. } = operation.target else {
        unreachable!()
    };
    worker
        .request(move |store| {
            let reviewed = request(store, *cutoff);
            apply(store, &reviewed);
            Ok(())
        })
        .await
        .unwrap();
    let cursor = worker.snapshot().await.unwrap().cursor;
    assert!(
        forms
            .submit(&late.to_string(), answer.clone())
            .await
            .is_err()
    );
    assert_eq!(forms.list_pending().len(), 1);
    context
        .deferred_form_inbox()
        .record(accepted_answer.clone())
        .await
        .unwrap();
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    let mut missing = accepted_answer.clone();
    missing.prompt_id = Uuid::new_v4().to_string();
    missing.expected_head = None;
    assert!(context.deferred_form_inbox().record(missing).await.is_err());
    let flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    assert!(flow.pending_inputs_at_head().next().is_none());
    let head = flow.history_head();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    context
        .deferred_form_inbox()
        .record(accepted_answer)
        .await
        .unwrap();
    let forms = context.forms();
    let _view = forms.subscribe();
    let resolver = crate::rendezvous::FormPromptResolver::new(forms.clone());
    let fresh = PromptId::now();
    let waiting =
        resolver.register_with_payload(fresh, "form_ask", serde_json::to_value(form).unwrap());
    assert!(resolver.expire_pending(&fresh));
    assert!(waiting.await.is_err());
    assert!(forms.submit(&fresh.to_string(), answer).await.unwrap());
    let flow = worker.shutdown().await.unwrap();
    let eligible: Vec<_> = flow.pending_inputs_at_head().collect();
    assert_eq!(eligible.len(), 1);
    assert_eq!(eligible[0].request.expected_head, Some(head));
}

fn goal_request(flow: &FlowStore, edit: GoalEdit) -> GoalRequest {
    GoalRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        expected: flow.goal().map(GoalVersion::from),
        expected_head: None,
        edit,
        start_turn: false,
    }
}

#[tokio::test]
async fn atomic_application_selects_context_pauses_goal_and_preserves_old_queue_across_reopen() {
    let (directory, mut flow, mut operation) = reviewed();
    let full = flow.history_messages().unwrap();
    flow.edit_goal(
        goal_request(
            &flow,
            GoalEdit::Create {
                text: "retained objective".into(),
            },
        ),
        |_| Ok(()),
    )
    .unwrap();
    operation.goal = flow.goal().map(GoalVersion::from);
    let pause = Uuid::new_v4().into();
    flow.transition(
        flow.state().revision(),
        FlowTransition::ExecutionPaused { key: pause },
    )
    .unwrap();
    operation.execution_revision = flow.state().revision();
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    queue(&mut flow);
    let queued: Vec<_> = flow.pending_input_entries().cloned().collect();
    flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    let applied = flow
        .apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    let HistoryOperationStatus::Applied { receipt, head } = &applied.status else {
        panic!("application receipt")
    };
    assert_eq!(flow.history_head(), *head);
    assert_eq!(head.version.get(), 1);
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Paused);
    assert_eq!(flow.goal_updates().last().unwrap().receipt, *receipt);
    assert_eq!(
        flow.goal().unwrap().revision,
        operation.goal.as_ref().unwrap().revision + 1
    );
    assert!(flow.state().pauses().contains(&pause));
    assert!(flow.pending_history_operation().is_none());
    assert_eq!(flow.history_messages().unwrap(), full[..1]);
    assert_eq!(flow.context_messages().unwrap(), full[..1]);
    assert!(flow.pending_inputs_at_head().next().is_none());
    assert_eq!(flow.pending_inputs().count(), 2);
    for (before, after) in queued.iter().zip(flow.pending_input_entries()) {
        assert_eq!(before.envelope.id, after.envelope.id);
        assert_eq!(before.revision, after.revision);
        assert_eq!(before.envelope.history_head, after.envelope.history_head);
    }
    assert!(
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Err(
            io::ErrorKind::PermissionDenied.into()
        ))
        .is_err()
    );
    assert_eq!(
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        applied
    );
    let inspection =
        RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0).unwrap();
    assert_eq!(
        inspection
            .history_operation(&operation.flow.instance_id, &operation.sender, operation.id)
            .unwrap(),
        Some(applied.clone())
    );
    let source = SourceSelection {
        flow: operation.flow.clone(),
        branch: head.branch,
        cursor: receipt.require_durable().unwrap().clone(),
    };
    assert_eq!(
        inspection
            .read_branch_conversation(&source, ConversationReadKind::History)
            .unwrap()
            .messages,
        full[..1]
    );
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
    assert_eq!(flow.history_messages().unwrap(), full[..1]);
    assert_eq!(
        flow.history_operation(&operation.sender, operation.id),
        Some(&applied)
    );
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Paused);
    assert!(flow.pending_inputs_at_head().next().is_none());
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    assert_eq!(worker.context().messages().to_vec(), full[..1]);
    assert_eq!(&**worker.context().messages_full(), &full[..1]);
    assert_eq!(worker.goal().await.unwrap().history_head, *head);
    assert_eq!(
        worker.goal().await.unwrap().goal.unwrap().status,
        GoalStatus::Paused
    );
    worker.shutdown().await.unwrap();
}

#[test]
fn restore_preserves_divergent_history_and_selects_only_its_own_checkpoint() {
    let (directory, mut flow, operation) = reviewed();
    let full = flow.history_messages().unwrap();
    let window = flow.context_messages().unwrap();
    let saved = flow
        .checkpoint_context(ContextCheckpoint {
            expected_head: Some(flow.history_head()),
            id: Uuid::new_v4(),
            purpose: ContextCheckpointPurpose::Compaction,
            expected: context_digest(&window).unwrap(),
            messages: vec![full[1].clone()],
            summary: "retained summary".into(),
            range: 0..window.len(),
            count: window.len(),
            before_tokens: 10,
            after_tokens: 5,
        })
        .unwrap();
    let first = apply(&mut flow, &operation);
    let branch = flow.history_head().branch;
    assert!(flow.latest_context_checkpoint().is_none());
    assert_eq!(flow.context_messages().unwrap(), full[..1]);
    let new_cutoff = turn(&mut flow, "divergent");
    let divergent = flow.history_messages().unwrap();
    let mut restore = request(&flow, new_cutoff.clone());
    restore.target = HistoryOperationTarget::Restore {
        branch: operation.expected_head.branch,
    };
    apply(&mut flow, &restore);
    assert_eq!(flow.history_messages().unwrap(), full);
    assert_eq!(flow.context_messages().unwrap(), saved.checkpoint.messages);
    assert_eq!(flow.latest_context_checkpoint(), Some(&saved));
    assert!(flow.pending_inputs_at_head().next().is_none());
    let cursor = flow.cursor().clone();
    assert_eq!(
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap(),
        first
    );
    assert_eq!(flow.cursor(), &cursor);
    let mut back = request(&flow, new_cutoff);
    back.target = HistoryOperationTarget::Restore { branch };
    apply(&mut flow, &back);
    assert_eq!(flow.history_messages().unwrap(), divergent);
    assert!(flow.latest_context_checkpoint().is_none());
    assert!(flow.pending_inputs_at_head().next().is_none());
    drop(flow);
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let session = runtime.open_session(operation.flow.session_id.0).unwrap();
    let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
    assert_eq!(flow.history_messages().unwrap(), divergent);
    assert!(flow.latest_context_checkpoint().is_none());
}

#[test]
fn application_conflicts_leave_goal_head_queue_and_operation_unchanged() {
    let (_directory, mut flow, operation) = reviewed();
    flow.begin_history_operation(operation.clone(), |_| Ok(()))
        .unwrap();
    assert!(
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .is_err()
    );
    flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
        .unwrap();
    flow.edit_goal(
        goal_request(
            &flow,
            GoalEdit::Create {
                text: "new objective".into(),
            },
        ),
        |_| Ok(()),
    )
    .unwrap();
    let cursor = flow.cursor().clone();
    assert!(
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.history_head(), operation.expected_head);
    assert_eq!(flow.goal().unwrap().status, GoalStatus::Active);
    assert!(matches!(
        flow.pending_history_operation().unwrap().status,
        HistoryOperationStatus::Ready { .. }
    ));
    assert_eq!(flow.pending_inputs().count(), 1);
}

#[test]
fn discarded_turns_cannot_be_selected_as_boundaries_of_the_current_branch() {
    let (_directory, mut flow, operation) = reviewed();
    let excluded = turn(&mut flow, "excluded turn");
    let mut updated = operation.clone();
    updated.execution_revision = flow.state().revision();
    apply(&mut flow, &updated);
    let cursor = flow.cursor().clone();
    let invalid = request(&flow, excluded);
    assert!(flow.begin_history_operation(invalid, |_| Ok(())).is_err());
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.pending_history_operation().is_none());
}

#[test]
fn malformed_atomic_application_is_rejected_without_repairing_any_tail() {
    for mutation in 0..3 {
        let (directory, mut flow, mut operation) = reviewed();
        flow.edit_goal(
            goal_request(
                &flow,
                GoalEdit::Create {
                    text: "unchanged objective".into(),
                },
            ),
            |_| Ok(()),
        )
        .unwrap();
        operation.goal = flow.goal().map(GoalVersion::from);
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        let (change, pause) = flow
            .history_operations
            .application(
                &operation.sender,
                operation.id,
                &flow.state,
                flow.inbox.head(),
                flow.goal(),
            )
            .unwrap();
        let prepared = flow
            .branches
            .as_ref()
            .unwrap()
            .prepare(change.clone(), flow.cursor.sequence() + 1, |receipt| {
                flow.journal.previous_durable_receipt(receipt).map(drop)
            })
            .unwrap();
        let mut head = prepared.head;
        if mutation == 1 {
            head.branch = ConversationBranchId::random();
        }
        let at = Utc::now();
        let event = FlowFact::HistoryOperation(Box::new(HistoryOperationEvent::Applied {
            sender: operation.sender.clone(),
            id: operation.id,
            head,
            at,
        }));
        assert!(FlowFact::validate_batch(std::slice::from_ref(&event)).is_err());
        let mut facts = vec![event, FlowFact::HistoryHeadChanged(Box::new(change))];
        if mutation != 0 {
            let mut request = pause.unwrap();
            if mutation == 2 {
                request.expected.as_mut().unwrap().revision += 1;
            }
            facts.push(FlowFact::GoalUpdated(Box::new(GoalUpdate {
                request,
                new_goal_id: Uuid::new_v4(),
                at,
                input: None,
            })));
        }
        flow.journal.commit(&facts).unwrap();
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
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}
