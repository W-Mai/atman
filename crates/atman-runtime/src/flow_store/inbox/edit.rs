//! Durable edits to unconsumed human input. Execution and notification ownership stay unchanged.

use super::*;
use crate::flow_store::{FlowFact, FlowStore};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxMove {
    Earlier,
    Later,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum InboxChange {
    Replace {
        content: UserContent,
        delivery: DeliveryMode,
    },
    Move {
        direction: InboxMove,
        expected_order: Vec<Uuid>,
    },
    Cancel,
    OperatorReplace {
        content: UserContent,
        delivery: DeliveryMode,
        action: OperatorReplacement,
    },
}

/// The host authenticates the sender and authorizes every request, including receipt retries.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct InboxEditRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<HistoryHead>,
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub message: Uuid,
    pub expected: FlowPosition,
    pub change: InboxChange,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorReplacement {
    Text,
    Delivery,
}

/// A local operator can edit prompt text or delivery without replacing other authored fields.
#[derive(Clone, Debug)]
pub enum OperatorInboxChange {
    EditText(String),
    SetDelivery(DeliveryMode),
    Move {
        direction: InboxMove,
        expected_order: Vec<Uuid>,
    },
    Cancel,
}

#[derive(Clone, Debug)]
pub struct OperatorInboxEdit {
    pub expected_head: HistoryHead,
    pub id: Uuid,
    pub message: Uuid,
    pub expected: FlowPosition,
    pub change: OperatorInboxChange,
}

struct PreparedEdit {
    fingerprint: blake3::Hash,
    change: PreparedChange,
}

enum PreparedChange {
    Replace(DeliveryMode, blake3::Hash),
    Move(usize, usize),
    Cancel,
}

impl Inbox {
    fn previous_edit(&self, edit: &InboxEditRequest) -> io::Result<Option<FlowPosition>> {
        match self.edits.get(&(edit.sender.clone(), edit.id)) {
            Some((previous, receipt)) if *previous == request_fingerprint(edit)? => {
                Ok(Some(receipt.clone()))
            }
            Some(_) => Err(invalid(
                "inbox edit identity conflicts with recorded request",
            )),
            None => Ok(None),
        }
    }

    fn prepare_edit(&self, edit: &InboxEditRequest) -> io::Result<PreparedEdit> {
        if self.previous_edit(edit)?.is_some() {
            return Err(invalid("duplicate inbox edit fact"));
        }
        self.head.validate_write(edit.expected_head.as_ref())?;
        if self.pending.has_reservation() {
            return Err(invalid("inbox input is reserved; refresh before editing"));
        }
        let entry = self
            .messages
            .get(&edit.message)
            .map(|index| &self.entries[*index])
            .ok_or_else(|| invalid("inbox message is unavailable"))?;
        let request = &entry.envelope.request;
        if !matches!(edit.sender, SenderIdentity::User(_)) || edit.sender != request.sender {
            return Err(invalid(
                "inbox edits require the original authenticated user",
            ));
        }
        if entry.acceptance.disposition != InboxDisposition::Pending
            || entry.revision != edit.expected
        {
            return Err(invalid("inbox message was consumed, cancelled, or changed"));
        }
        if !request.is_editable_user_input() {
            return Err(invalid("this input is not an editable user submission"));
        }
        if entry.envelope.history_head != self.head && !matches!(edit.change, InboxChange::Cancel) {
            return Err(invalid("input is held for an earlier history head"));
        }
        let fingerprint = request_fingerprint(edit)?;
        let change = match &edit.change {
            InboxChange::Cancel => PreparedChange::Cancel,
            InboxChange::Replace { content, delivery }
            | InboxChange::OperatorReplace {
                content, delivery, ..
            } => {
                request.payload.validate_replacement(content, *delivery)?;
                let content_hash = request_fingerprint(&(content, delivery))?;
                if entry.content == Some(content_hash) {
                    return Err(invalid("inbox edit does not change the input"));
                }
                PreparedChange::Replace(*delivery, content_hash)
            }
            InboxChange::Move {
                direction,
                expected_order,
            } => {
                let lane: Vec<_> = self
                    .pending()
                    .filter(|envelope| {
                        envelope.request.sender == edit.sender
                            && envelope.history_head == self.head
                            && envelope.request.delivery == DeliveryMode::NextTurn
                            && envelope.request.is_editable_user_input()
                    })
                    .map(|envelope| envelope.id)
                    .collect();
                if &lane != expected_order {
                    return Err(invalid(
                        "next-turn queue order changed; refresh before moving",
                    ));
                }
                let position = lane
                    .iter()
                    .position(|id| *id == edit.message)
                    .ok_or_else(|| invalid("only next-turn user input can be reordered"))?;
                let neighbor = match direction {
                    InboxMove::Earlier => position.checked_sub(1),
                    InboxMove::Later => position.checked_add(1),
                }
                .and_then(|index| lane.get(index))
                .ok_or_else(|| invalid("input is already at the queue boundary"))?;
                let first = self
                    .pending
                    .iter()
                    .position(|id| *id == edit.message)
                    .unwrap();
                let second = self.pending.iter().position(|id| id == neighbor).unwrap();
                PreparedChange::Move(first, second)
            }
        };
        Ok(PreparedEdit {
            fingerprint,
            change,
        })
    }

    fn apply_edit(
        &mut self,
        edit: InboxEditRequest,
        prepared: PreparedEdit,
        receipt: FlowPosition,
    ) {
        let index = self.messages[&edit.message];
        match prepared.change {
            PreparedChange::Replace(delivery, content) => {
                self.replacements.insert(edit.message, receipt.clone());
                let entry = &mut self.entries[index];
                entry.envelope.request.delivery = delivery;
                entry.content = Some(content);
            }
            PreparedChange::Move(first, second) => {
                let neighbor = *self.pending.iter().nth(second).unwrap();
                assert!(self.pending.swap(first, second));
                self.entries[self.messages[&neighbor]].revision = receipt.clone();
            }
            PreparedChange::Cancel => {
                self.cancel_matching(&receipt, |envelope| envelope.id == edit.message)
            }
        }
        self.entries[index].revision = receipt.clone();
        self.edits
            .insert((edit.sender, edit.id), (prepared.fingerprint, receipt));
    }

    pub(in crate::flow_store) fn replay_edit(
        &mut self,
        edit: InboxEditRequest,
        receipt: FlowPosition,
    ) -> io::Result<()> {
        let prepared = self.prepare_edit(&edit)?;
        self.apply_edit(edit, prepared, receipt);
        Ok(())
    }
}

fn editable(request: &DeliveryRequest) -> bool {
    matches!(
        &request.payload,
        InboxPayload::UserSubmission { .. }
            | InboxPayload::Interjection { .. }
            | InboxPayload::Command { user: Some(_), .. }
            | InboxPayload::Invocation { user: Some(_), .. }
    )
}

impl UserContent {
    pub fn authored_text(&self) -> &str {
        self.presentation
            .as_ref()
            .map_or(&self.text, |presentation| &presentation.prompt)
    }
}

impl DeliveryRequest {
    pub(crate) fn editable_content(&self) -> Option<UserContent> {
        match &self.payload {
            InboxPayload::UserSubmission { content, .. } => Some(content.clone()),
            InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => {
                user.clone()
            }
            InboxPayload::Interjection { text } => Some(UserContent {
                text: text.clone(),
                images: vec![],
                presentation: None,
            }),
            _ => None,
        }
    }

    pub(super) fn replace_content(
        &self,
        content: &UserContent,
        delivery: DeliveryMode,
    ) -> io::Result<Self> {
        InputKind::from(&self.payload).validate_replacement(content, delivery)?;
        let mut updated = self.clone();
        updated.delivery = delivery;
        match &mut updated.payload {
            InboxPayload::UserSubmission { content: value, .. } => *value = content.clone(),
            InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => {
                *user = Some(content.clone())
            }
            InboxPayload::Interjection { text }
                if content.images.is_empty() && content.presentation.is_none() =>
            {
                *text = content.text.clone()
            }
            _ => return Err(invalid("input does not support this content replacement")),
        }
        updated.validate_source()?;
        Ok(updated)
    }

    /// Whether this input belongs to the human-editable queue, independent of its current consumption state.
    pub fn is_editable_user_input(&self) -> bool {
        matches!(self.sender, SenderIdentity::User(_))
            && self.scope.is_none()
            && self.correlation.is_none()
            && self.cause.is_none()
            && editable(self)
    }
}

impl FlowStore {
    /// Build a source-bound edit in the original owner. Retry returns the recorded edit without reading a now-consumed body.
    pub(crate) fn operator_inbox_edit(
        &mut self,
        operator: super::super::local_operator::LocalOperator,
        edit: OperatorInboxEdit,
    ) -> io::Result<InboxEditRequest> {
        let sender = operator.resolve(None, |sender| {
            self.inbox.edits.contains_key(&(sender.clone(), edit.id))
        })?;
        if let Some((_, receipt)) = self.inbox.edits.get(&(sender, edit.id)) {
            let receipt = receipt.clone();
            let recorded = self
                .journal
                .read_unresolved(&receipt)?
                .facts
                .into_iter()
                .find_map(|fact| match fact {
                    FlowFact::InboxEdited(value)
                        if value.id == edit.id && operator.owns(&value.sender) =>
                    {
                        Some(*value)
                    }
                    _ => None,
                })
                .ok_or_else(|| invalid("inbox edit is missing from its recorded batch"))?;
            if edit.message == recorded.message
                && edit.expected == recorded.expected
                && Some(&edit.expected_head) == recorded.expected_head.as_ref()
                && match (&edit.change, &recorded.change) {
                    (
                        OperatorInboxChange::EditText(text),
                        InboxChange::OperatorReplace {
                            content,
                            action: OperatorReplacement::Text,
                            ..
                        },
                    ) => content.authored_text() == text,
                    (
                        OperatorInboxChange::SetDelivery(mode),
                        InboxChange::OperatorReplace {
                            delivery,
                            action: OperatorReplacement::Delivery,
                            ..
                        },
                    ) => mode == delivery,
                    (
                        OperatorInboxChange::Move {
                            direction,
                            expected_order,
                        },
                        InboxChange::Move {
                            direction: previous,
                            expected_order: order,
                        },
                    ) => direction == previous && expected_order == order,
                    (OperatorInboxChange::Cancel, InboxChange::Cancel) => true,
                    _ => false,
                }
            {
                return Ok(recorded);
            }
            return Err(invalid(
                "inbox edit identity conflicts with recorded request",
            ));
        }
        let summary = self
            .inbox
            .messages
            .get(&edit.message)
            .map(|index| &self.inbox.entries[*index])
            .filter(|entry| {
                operator.owns(&entry.envelope.request.sender)
                    && entry.envelope.request.is_editable_user_input()
            })
            .ok_or_else(|| invalid("editable inbox message is unavailable"))?;
        let sender = summary.envelope.request.sender.clone();
        if summary.revision != edit.expected
            || summary.acceptance.disposition != InboxDisposition::Pending
        {
            return Err(invalid("inbox message was consumed, cancelled, or changed"));
        }
        let change = match edit.change {
            OperatorInboxChange::Cancel => InboxChange::Cancel,
            OperatorInboxChange::Move {
                direction,
                expected_order,
            } => InboxChange::Move {
                direction,
                expected_order,
            },
            change => {
                let entry = self
                    .inbox_entry(edit.message)?
                    .ok_or_else(|| invalid("editable inbox message is unavailable"))?;
                let request = entry.envelope.request;
                let mut content = request
                    .editable_content()
                    .ok_or_else(|| invalid("input has no editable content"))?;
                let (delivery, action) = match change {
                    OperatorInboxChange::EditText(text) => {
                        content.text = match &mut content.presentation {
                            Some(presentation) => {
                                presentation.prompt = text;
                                presentation.model_text()
                            }
                            None => text,
                        };
                        (request.delivery, OperatorReplacement::Text)
                    }
                    OperatorInboxChange::SetDelivery(delivery) => {
                        (delivery, OperatorReplacement::Delivery)
                    }
                    _ => unreachable!(),
                };
                InboxChange::OperatorReplace {
                    content,
                    delivery,
                    action,
                }
            }
        };
        Ok(InboxEditRequest {
            expected_head: Some(edit.expected_head),
            id: edit.id,
            sender,
            message: edit.message,
            expected: edit.expected,
            change,
        })
    }

    /// Look up an exact committed edit without reapplying it. Authenticate and authorize before returning this receipt to a caller.
    pub fn input_edit_receipt(&self, edit: &InboxEditRequest) -> io::Result<Option<FlowPosition>> {
        self.inbox.previous_edit(edit)
    }

    /// Persist a user queue change without starting a turn or consuming input.
    ///
    /// The host must authenticate `sender`. Authorization runs before receipt lookup; changing delivery to `AtBoundary` also requires host validation of invocation compatibility. Attachment capability is checked at the actual model boundary. A successful edit only changes eligibility at a later safe point.
    pub fn edit_input(
        &mut self,
        edit: InboxEditRequest,
        authorize: impl FnOnce(&InboxEditRequest) -> io::Result<()>,
    ) -> io::Result<FlowPosition> {
        authorize(&edit)?;
        if let Some(receipt) = self.inbox.previous_edit(&edit)? {
            return Ok(receipt);
        }
        if !self.state.accepts_input() {
            return Err(invalid("flow does not accept input edits"));
        }
        let prepared = self.inbox.prepare_edit(&edit)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::InboxEdited(Box::new(edit.clone()))])?;
        self.inbox.apply_edit(edit, prepared, receipt.clone());
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
