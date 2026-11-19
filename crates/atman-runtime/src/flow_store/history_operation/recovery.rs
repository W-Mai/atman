//! Recover authored input data from an applied Revert without replaying its command or settings.

use super::*;
use crate::flow_store::inbox::{DeliveryMode, Inbox, InboxEntry, InboxPayload, UserContent};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecoveredHistoryInput {
    pub operation: Uuid,
    pub head: HistoryHead,
    pub input: Uuid,
    pub recoverable_inputs: Vec<Uuid>,
    pub acceptance: FlowPosition,
    pub delivery: DeliveryMode,
    pub content: UserContent,
}

impl FlowStore {
    pub(super) fn recoverable_turn_inputs(&self, lease: &TurnLease<AtmanUuid>) -> Vec<Uuid> {
        recoverable_turn_inputs(&self.inbox, lease)
    }

    /// Read one original authored body after validating its operation and containing admission.
    pub fn history_recovered_input(
        &mut self,
        sender: SenderIdentity,
        operation: Uuid,
        input: Uuid,
        authorize: impl FnOnce(&HistoryOperationRecord) -> io::Result<()>,
    ) -> io::Result<RecoveredHistoryInput> {
        let record = self
            .history_operation(&sender, operation)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        authorize(record)?;
        let (head, recoverable_inputs) = recovery_selection(&self.inbox, record, input)?;
        recovered_input(
            operation,
            head,
            input,
            recoverable_inputs,
            self.inbox_entry(input)?,
        )
    }
}

fn recoverable_turn_inputs(inbox: &Inbox, lease: &TurnLease<AtmanUuid>) -> Vec<Uuid> {
    inbox
        .inputs()
        .iter()
        .filter(|record| &record.lease == lease)
        .flat_map(|record| record.message_ids.iter().copied())
        .filter(|input| {
            inbox
                .summary(*input)
                .is_some_and(|entry| entry.envelope.request.is_editable_user_input())
        })
        .collect()
}

fn recovery_selection(
    inbox: &Inbox,
    record: &HistoryOperationRecord,
    input: Uuid,
) -> io::Result<(HistoryHead, Vec<Uuid>)> {
    let HistoryOperationStatus::Applied { head, .. } = &record.status else {
        return Err(invalid(
            "input recovery requires an applied history operation",
        ));
    };
    let HistoryOperationTarget::Revert { cutoff, .. } = &record.request.target else {
        return Err(invalid("Restore has no removed user input"));
    };
    let ConversationCutoff::BeforeUserTurn { lease, .. } = cutoff.as_ref() else {
        return Err(invalid("input recovery requires a user-turn boundary"));
    };
    let inputs = recoverable_turn_inputs(inbox, lease);
    if !inputs.contains(&input) {
        return Err(invalid(
            "input is not authored content from the reverted turn",
        ));
    }
    Ok((head.clone(), inputs))
}

fn recovered_input(
    operation: Uuid,
    head: HistoryHead,
    input: Uuid,
    recoverable_inputs: Vec<Uuid>,
    entry: Option<InboxEntry>,
) -> io::Result<RecoveredHistoryInput> {
    let entry = entry.ok_or_else(|| invalid("original input is unavailable"))?;
    let delivery = entry.envelope.request.delivery;
    let content = match entry.envelope.request.payload {
        InboxPayload::Command {
            user: Some(content),
            ..
        }
        | InboxPayload::Invocation {
            user: Some(content),
            ..
        }
        | InboxPayload::UserSubmission { content, .. } => content,
        InboxPayload::Interjection { text } => UserContent {
            text,
            images: vec![],
            presentation: None,
        },
        _ => return Err(invalid("input has no recoverable authored content")),
    };
    Ok(RecoveredHistoryInput {
        operation,
        head,
        input,
        recoverable_inputs,
        acceptance: entry.acceptance.receipt,
        delivery,
        content,
    })
}

impl RecoveredHistoryInput {
    /// Materialize owned attachments under a combined decoded byte budget.
    pub fn retained_images(&self, max_bytes: usize) -> io::Result<Vec<Vec<u8>>> {
        let mut remaining = max_bytes;
        self.content
            .images
            .iter()
            .map(|image| {
                let bytes = crate::attachment_store::retained_image_bytes(image, remaining)
                    .map_err(io::Error::other)?;
                remaining -= bytes.len();
                Ok(bytes)
            })
            .collect()
    }
}

impl crate::flow_store::SessionInspection {
    /// Recover from one immutable inspection without creating an execution owner.
    pub fn operator_history_recovered_input(
        &self,
        id: &crate::flow_store::FlowInstanceId,
        operator: crate::flow_store::local_operator::LocalOperator,
        operation: Uuid,
        input: Uuid,
    ) -> io::Result<RecoveredHistoryInput> {
        let mut recorded = crate::flow_store::read_registered_flow(
            &self.dir,
            &self.projection,
            id,
            |_, _| Ok(()),
        )?;
        let operations = &recorded.replay.history_operations;
        let sender =
            operator.resolve(None, |sender| operations.find(sender, operation).is_some())?;
        let record = operations
            .find(&sender, operation)
            .ok_or_else(|| invalid("history operation is unavailable for this user"))?;
        let (_, _, inbox) = recorded
            .replay
            .projection
            .as_ref()
            .ok_or_else(|| invalid("flow is unavailable"))?;
        let (head, candidates) = recovery_selection(inbox, record, input)?;
        let flow_dir = self.dir.join("flows").join(id.to_string());
        let entry = inbox.read_entry(input, |receipt| {
            crate::flow_store::images::resolve_batch(
                &flow_dir,
                recorded
                    .reader
                    .read_batch(receipt.require_durable()?)?
                    .into(),
            )
        })?;
        recovered_input(operation, head, input, candidates, entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{
        RuntimeStore,
        history_operation::tests::{complete, request},
        inbox::{DeliveryRequest, InputContent},
        source_capture::tests::fixture,
    };
    use atman_rt::{RunId, TurnId};

    #[test]
    fn recovered_inputs_keep_authored_content_and_admission_identity_after_restart() {
        let (directory, mut flow, prior) = fixture();
        complete(&mut flow, prior);
        let user = SenderIdentity::User(Uuid::new_v4());
        let external = tempfile::tempdir().unwrap();
        let image = crate::attachment_store::AttachmentStore::at(external.path()).import_base64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a7p8AAAAASUVORK5CYII=", Some("pixel.png")).unwrap();
        let image_bytes = crate::attachment_store::image_bytes(&image).unwrap();
        let content = UserContent {
            text: "> quote\n\noriginal input".into(),
            images: vec![image],
            presentation: Some(crate::user_input::UserInputPresentation {
                prompt: "original input".into(),
                quote: Some(crate::user_input::QuoteSnapshot {
                    text: "quote".into(),
                }),
            }),
        };
        let mut authored = Vec::new();
        for index in 0..3 {
            authored.push(
                flow.accept_input(
                    DeliveryRequest {
                        expected_head: Some(flow.history_head()),
                        request_id: Uuid::new_v4(),
                        sender: user.clone(),
                        recipient: flow.state().address().clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::UserSubmission {
                            content: if index == 0 {
                                content.clone()
                            } else {
                                UserContent {
                                    text: format!("input {index}"),
                                    images: vec![],
                                    presentation: None,
                                }
                            },
                            invocation: Default::default(),
                        },
                    },
                    |_| Ok(()),
                )
                .unwrap(),
            );
        }
        let host_input = flow
            .accept_input(
                DeliveryRequest {
                    expected_head: Some(flow.history_head()),
                    request_id: Uuid::new_v4(),
                    sender: SenderIdentity::Host(Uuid::new_v4()),
                    recipient: flow.state().address().clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Internal {
                        kind: "notice".into(),
                        content: serde_json::json!("original input"),
                    },
                },
                |_| Ok(()),
            )
            .unwrap();
        let reserved = flow
            .reserve_invocation_input(TurnId::now(), RunId::now(), |entry| {
                entry.id != authored[2].message_id
            })
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
        complete(&mut flow, lease.clone());
        let operation = request(
            &flow,
            ConversationCutoff::BeforeUserTurn {
                lease,
                input: authored[0].message_id,
                receipt: admission.require_durable().unwrap().clone(),
            },
        );
        let review = flow
            .review_history_operation(
                HistoryReviewRequest {
                    id: operation.id,
                    sender: operation.sender.clone(),
                    flow: operation.flow.clone(),
                    expected_head: operation.expected_head.clone(),
                    target: operation.target.clone(),
                },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            review.recoverable_inputs,
            vec![authored[0].message_id, authored[1].message_id]
        );
        assert!(review.admitted_inputs.contains(&host_input.message_id));
        flow.begin_history_operation(operation.clone(), |_| Ok(()))
            .unwrap();
        assert!(
            flow.history_recovered_input(
                operation.sender.clone(),
                operation.id,
                authored[0].message_id,
                |_| Ok(())
            )
            .is_err()
        );
        flow.stop_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        flow.apply_history_operation(operation.sender.clone(), operation.id, |_| Ok(()))
            .unwrap();
        let before = flow.cursor().clone();
        drop(external);
        let recovered = flow
            .history_recovered_input(
                operation.sender.clone(),
                operation.id,
                authored[0].message_id,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(recovered.content.text, content.text);
        assert_eq!(recovered.content.presentation, content.presentation);
        assert_eq!(
            crate::attachment_store::image_bytes(&recovered.content.images[0]).unwrap(),
            image_bytes
        );
        assert_eq!(recovered.acceptance, authored[0].receipt);
        assert_eq!(recovered.head, flow.history_head());
        assert_eq!(recovered.recoverable_inputs, review.recoverable_inputs);
        assert_eq!(recovered.delivery, DeliveryMode::NextTurn);
        let second = flow
            .history_recovered_input(
                operation.sender.clone(),
                operation.id,
                authored[1].message_id,
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(second.content.text, "input 1");
        assert_ne!(second.input, recovered.input);
        assert!(
            flow.history_recovered_input(
                operation.sender.clone(),
                operation.id,
                host_input.message_id,
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            flow.history_recovered_input(
                operation.sender.clone(),
                operation.id,
                authored[2].message_id,
                |_| Ok(())
            )
            .is_err()
        );
        assert!(
            flow.history_recovered_input(user, operation.id, authored[0].message_id, |_| Ok(()))
                .is_err()
        );
        assert!(
            flow.history_recovered_input(
                operation.sender.clone(),
                operation.id,
                authored[0].message_id,
                |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            )
            .is_err()
        );
        assert_eq!(flow.cursor(), &before);
        drop(flow);
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
        assert_eq!(
            flow.history_recovered_input(
                operation.sender,
                operation.id,
                authored[0].message_id,
                |_| Ok(())
            )
            .unwrap(),
            recovered
        );
        assert_eq!(flow.cursor(), &before);
    }
}
