//! Typed delivery facts and their single inbox projection.

use std::collections::{BTreeMap, HashMap};
use std::io;

use atman_rt::inbox::{Batch, PendingBatch};
use atman_rt::{FlowInstance, TurnLease};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::waiter::{ReplyStatus, ReplyWaiter, ReplyWaiters};
use super::{FlowRef, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::message::{ImageSource, Message};
use crate::user_input::UserInputPresentation;

/// Authenticated provenance supplied by the host, never an authorization grant by itself.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum SenderIdentity {
    User(Uuid),
    Flow(FlowRef),
    Host(Uuid),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    AtBoundary,
    NextTurn,
}

/// Stable source position; separate events in one committed batch retain separate identities.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowEventRef {
    pub flow: FlowRef,
    pub sequence: u64,
    pub index: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum InboxPayload {
    UserSubmission {
        text: String,
        images: Vec<ImageSource>,
        invocation: BTreeMap<String, Value>,
        presentation: Option<UserInputPresentation>,
    },
    Interjection {
        text: String,
    },
    FlowEvent {
        source: FlowEventRef,
        event: String,
        content: Value,
    },
    Watcher {
        subscription: Uuid,
        source: FlowEventRef,
        content: Value,
    },
    Timer {
        timer: Uuid,
        due_at: DateTime<Utc>,
        content: Value,
    },
    Internal {
        kind: String,
        content: Value,
    },
}

/// The host authenticates `sender` before calling storage, including on retries.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeliveryRequest {
    pub request_id: Uuid,
    pub sender: SenderIdentity,
    pub recipient: FlowRef,
    pub delivery: DeliveryMode,
    pub cause: Option<FlowEventRef>,
    pub correlation: Option<Uuid>,
    pub scope: Option<Uuid>,
    pub payload: InboxPayload,
}

impl DeliveryRequest {
    pub(crate) fn validate_source(&self) -> io::Result<()> {
        let valid = match (&self.sender, &self.payload) {
            (
                SenderIdentity::User(_),
                InboxPayload::UserSubmission { .. } | InboxPayload::Interjection { .. },
            ) => true,
            (SenderIdentity::Flow(_), InboxPayload::Interjection { .. }) => true,
            (SenderIdentity::Flow(sender), InboxPayload::FlowEvent { source, .. }) => {
                sender == &source.flow
            }
            (
                SenderIdentity::Host(_),
                InboxPayload::Watcher { .. }
                | InboxPayload::Timer { .. }
                | InboxPayload::Internal { .. },
            ) => true,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(invalid(
                "delivery payload does not match authenticated sender",
            ))
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AcceptedEnvelope {
    pub id: Uuid,
    pub accepted_at: DateTime<Utc>,
    pub request: DeliveryRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InboxDisposition {
    Pending,
    InputCommitted { by: CommitReceipt },
    Cancelled { by: CommitReceipt },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTarget {
    Context,
    InvocationInput,
    WaitingContinuation { correlation: Uuid },
}

/// Exact host-prepared input, persisted before it is exposed to an invocation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "target", rename_all = "snake_case")]
pub enum InputContent {
    Context {
        messages: Vec<Message>,
    },
    InvocationInput {
        arguments: BTreeMap<String, Value>,
        invocation: BTreeMap<String, Value>,
        messages: Vec<Message>,
    },
    WaitingContinuation {
        correlation: Uuid,
        value: Value,
    },
}

impl InputContent {
    pub fn target(&self) -> InputTarget {
        match self {
            Self::Context { .. } => InputTarget::Context,
            Self::InvocationInput { .. } => InputTarget::InvocationInput,
            Self::WaitingContinuation { correlation, .. } => InputTarget::WaitingContinuation {
                correlation: *correlation,
            },
        }
    }

    fn validate(&self, lease: &TurnLease<AtmanUuid>) -> io::Result<()> {
        let messages = match self {
            Self::Context { messages } if messages.is_empty() => {
                return Err(invalid("context input requires actual messages"));
            }
            Self::Context { messages } | Self::InvocationInput { messages, .. } => messages,
            Self::WaitingContinuation { .. } => return Ok(()),
        };
        if messages.iter().any(|message| message.turn_id != lease.turn) {
            return Err(invalid("input message belongs to another turn"));
        }
        Ok(())
    }
}

/// A committed input is not proof of model inclusion or successful business handling.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedInput {
    pub lease: TurnLease<AtmanUuid>,
    pub message_ids: Vec<Uuid>,
    pub content: InputContent,
}

#[derive(Clone, Debug, Serialize)]
pub struct InputRecord {
    pub input: CommittedInput,
    pub receipt: CommitReceipt,
}

/// Dropping or releasing a reservation retains the original FIFO inputs.
pub struct InputReservation {
    pub(super) lease: TurnLease<AtmanUuid>,
    pub(super) revision: u64,
    pub(super) target: InputTarget,
    pub(super) batch: Batch<Uuid>,
}

impl InputReservation {
    pub fn lease(&self) -> &TurnLease<AtmanUuid> {
        &self.lease
    }
    pub fn message_ids(&self) -> &[Uuid] {
        self.batch.items()
    }
    pub fn target(&self) -> InputTarget {
        self.target
    }
}

/// Acceptance is distinct from handling; retries retain the original acceptance receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Acceptance {
    pub message_id: Uuid,
    pub receipt: CommitReceipt,
    pub disposition: InboxDisposition,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxEntry {
    pub envelope: AcceptedEnvelope,
    pub acceptance: Acceptance,
}

#[derive(Default)]
pub(super) struct Inbox {
    entries: Vec<InboxEntry>,
    requests: HashMap<(SenderIdentity, Uuid), usize>,
    messages: HashMap<Uuid, usize>,
    pending: PendingBatch<Uuid>,
    inputs: Vec<InputRecord>,
    pub(super) waiters: ReplyWaiters,
}

impl Inbox {
    pub fn inputs(&self) -> &[InputRecord] {
        &self.inputs
    }

    pub fn reserve(
        &mut self,
        mut eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> Option<Batch<Uuid>> {
        let Self {
            pending,
            entries,
            messages,
            waiters,
            ..
        } = self;
        pending.reserve_matching(|id| {
            let envelope = &entries[messages[id]].envelope;
            !waiters.holds(envelope) && eligible(envelope)
        })
    }

    pub fn reserve_reply(&mut self, correlation: Uuid) -> Option<Batch<Uuid>> {
        let Self {
            pending,
            entries,
            messages,
            waiters,
            ..
        } = self;
        let mut selected = false;
        pending.reserve_matching(|id| {
            if !selected && waiters.matches(correlation, &entries[messages[id]].envelope) {
                selected = true;
                true
            } else {
                false
            }
        })
    }

    pub fn validate_waiter(
        &self,
        state: &FlowInstance<AtmanUuid>,
        waiter: &ReplyWaiter,
    ) -> io::Result<()> {
        if self.waiters.get(waiter.correlation).is_some() {
            return Err(invalid("duplicate reply waiter"));
        }
        if !(state
            .can_receive_at_boundary(&waiter.lease)
            .map_err(invalid)?
            || state.can_receive_reply(&waiter.lease).map_err(invalid)?)
        {
            return Err(invalid("flow cannot register a reply waiter"));
        }
        if self.entries.iter().any(|entry| {
            entry.envelope.request.correlation == Some(waiter.correlation)
                && entry.envelope.request.sender == waiter.sender
        }) {
            return Err(invalid(
                "reply waiter must be registered before its replies arrive",
            ));
        }
        Ok(())
    }

    pub fn validate_reply(
        &self,
        state: &FlowInstance<AtmanUuid>,
        lease: &TurnLease<AtmanUuid>,
        correlation: Uuid,
    ) -> io::Result<()> {
        let record = self
            .waiters
            .get(correlation)
            .ok_or_else(|| invalid("reply waiter does not exist"))?;
        if record.status != ReplyStatus::Pending
            || &record.waiter.lease != lease
            || !state.can_receive_reply(lease).map_err(invalid)?
        {
            return Err(invalid("reply waiter is not eligible for delivery"));
        }
        Ok(())
    }

    pub fn state_changed(&mut self, state: &FlowInstance<AtmanUuid>, receipt: &CommitReceipt) {
        self.invalidate();
        self.waiters.state_changed(state, receipt);
    }

    pub fn owns(&self, batch: &Batch<Uuid>) -> bool {
        self.pending.owns(batch)
    }

    pub fn release(&mut self, batch: &Batch<Uuid>) -> bool {
        self.pending.release(batch)
    }

    pub fn invalidate(&mut self) {
        self.pending.invalidate_reservation();
    }

    pub fn validate_input(
        &self,
        state: &FlowInstance<AtmanUuid>,
        input: &CommittedInput,
    ) -> io::Result<()> {
        match input.content.target() {
            InputTarget::WaitingContinuation { correlation } => {
                self.validate_reply(state, &input.lease, correlation)?;
                if input.message_ids.len() != 1 {
                    return Err(invalid("a reply waiter consumes one reply"));
                }
                if self
                    .pending()
                    .find(|envelope| self.waiters.matches(correlation, envelope))
                    .map(|envelope| envelope.id)
                    != input.message_ids.first().copied()
                {
                    return Err(invalid(
                        "reply input must consume the oldest matching message",
                    ));
                }
            }
            _ if !state
                .can_receive_at_boundary(&input.lease)
                .map_err(invalid)? =>
            {
                return Err(invalid("flow cannot receive input at this boundary"));
            }
            _ => {}
        }
        input.content.validate(&input.lease)?;
        if input.message_ids.is_empty() {
            return Err(invalid("input commit has no accepted messages"));
        }
        let mut pending = self.pending.iter();
        for id in &input.message_ids {
            if !pending.any(|candidate| candidate == id) {
                return Err(invalid("input commit is not an ordered pending subset"));
            }
            let envelope = &self.entries[self.messages[id]].envelope;
            match input.content.target() {
                InputTarget::WaitingContinuation { correlation }
                    if !self.waiters.matches(correlation, envelope) =>
                {
                    return Err(invalid(
                        "input does not match the registered reply sender and correlation",
                    ));
                }
                InputTarget::Context | InputTarget::InvocationInput
                    if self.waiters.holds(envelope) =>
                {
                    return Err(invalid("input belongs to a waiting continuation"));
                }
                _ => {}
            }
            if input.content.target() == InputTarget::Context
                && self.entries[self.messages[id]].envelope.request.delivery
                    != DeliveryMode::AtBoundary
            {
                return Err(invalid("next-turn input cannot enter an active turn"));
            }
        }
        Ok(())
    }

    /// Durable publication and FIFO consumption share the same ownership check.
    pub fn commit_input(
        &mut self,
        batch: &Batch<Uuid>,
        input: CommittedInput,
        publish: impl FnOnce() -> io::Result<CommitReceipt>,
    ) -> io::Result<CommitReceipt> {
        if batch.items() != input.message_ids {
            return Err(invalid("input commit does not match its reservation"));
        }
        let mut receipt = None;
        if !self.pending.commit(batch, || {
            receipt = Some(publish()?);
            Ok::<_, io::Error>(())
        })? {
            return Err(invalid("input reservation is no longer owned"));
        }
        let receipt = receipt.expect("owned publication supplies a receipt");
        if let InputTarget::WaitingContinuation { correlation } = input.content.target() {
            self.waiters
                .deliver(correlation, input.message_ids[0], receipt.clone());
        }
        for id in &input.message_ids {
            self.entries[self.messages[id]].acceptance.disposition =
                InboxDisposition::InputCommitted {
                    by: receipt.clone(),
                };
        }
        self.inputs.push(InputRecord {
            input,
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }

    pub fn replay_input(
        &mut self,
        state: &FlowInstance<AtmanUuid>,
        input: CommittedInput,
        receipt: CommitReceipt,
    ) -> io::Result<()> {
        self.validate_input(state, &input)?;
        let selected: std::collections::HashSet<_> = input.message_ids.iter().copied().collect();
        let batch = self
            .pending
            .reserve_matching(|id| selected.contains(id))
            .ok_or_else(|| invalid("input replay cannot reserve its pending messages"))?;
        self.commit_input(&batch, input, || Ok(receipt))?;
        Ok(())
    }

    pub fn entries(&self) -> &[InboxEntry] {
        &self.entries
    }

    pub fn pending(&self) -> impl Iterator<Item = &AcceptedEnvelope> {
        self.pending
            .iter()
            .map(|id| &self.entries[self.messages[id]].envelope)
    }

    pub fn previous(&self, request: &DeliveryRequest) -> io::Result<Option<Acceptance>> {
        let Some(&index) = self
            .requests
            .get(&(request.sender.clone(), request.request_id))
        else {
            return Ok(None);
        };
        let entry = &self.entries[index];
        if entry.envelope.request != *request {
            return Err(invalid(
                "delivery request ID was reused with different data",
            ));
        }
        Ok(Some(entry.acceptance.clone()))
    }

    pub fn validate(&self, envelope: &AcceptedEnvelope) -> io::Result<()> {
        envelope.request.validate_source()?;
        if self.messages.contains_key(&envelope.id) || self.previous(&envelope.request)?.is_some() {
            return Err(invalid("duplicate acceptance fact"));
        }
        Ok(())
    }

    /// Called only after validation and a successful durable append, or during validated replay.
    pub fn apply(&mut self, envelope: AcceptedEnvelope, receipt: CommitReceipt) -> Acceptance {
        let id = envelope.id;
        let acceptance = Acceptance {
            message_id: id,
            receipt,
            disposition: InboxDisposition::Pending,
        };
        self.requests.insert(
            (envelope.request.sender.clone(), envelope.request.request_id),
            self.entries.len(),
        );
        self.messages.insert(id, self.entries.len());
        self.entries.push(InboxEntry {
            envelope,
            acceptance: acceptance.clone(),
        });
        self.pending.push_back(id);
        acceptance
    }

    pub fn close(&mut self, receipt: &CommitReceipt) {
        for id in self.pending.iter() {
            self.entries[self.messages[id]].acceptance.disposition = InboxDisposition::Cancelled {
                by: receipt.clone(),
            };
        }
        self.pending.clear();
    }
}
