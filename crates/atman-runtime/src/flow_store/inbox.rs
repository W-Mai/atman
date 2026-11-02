//! Typed delivery facts and their single inbox projection.

use std::collections::{BTreeMap, HashMap};
use std::io;

use atman_rt::inbox::{Batch, PendingBatch};
use atman_rt::{FlowInstance, TurnLease};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::input_observation::{InputDisposition, InputObservation};
use super::waiter::{ReplyStatus, ReplyWaiter, ReplyWaiters};
use super::{FlowRef, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::message::{ImageSource, Message};
use crate::user_input::UserInputPresentation;

mod edit;
pub use edit::{InboxChange, InboxEditRequest, InboxMove};
mod body;
mod message;
pub use message::context_message;
mod metadata;
pub(in crate::flow_store) use metadata::PreparedInput;
pub use metadata::{InboxSummary, InputEnvelope, InputKind, InputRequest};
mod scope;
pub use scope::{InputScopeInvalidation, InvalidatedInputScope};

/// Compact retry evidence with the same object-order and signed-zero equality as JSON values.
pub(super) fn request_fingerprint(request: &impl Serialize) -> io::Result<blake3::Hash> {
    fn normalize(value: &mut Value) {
        match value {
            Value::Object(fields) => {
                fields.sort_keys();
                fields.values_mut().for_each(normalize);
            }
            Value::Array(items) => items.iter_mut().for_each(normalize),
            Value::Number(number) if number.is_f64() && number.as_f64() == Some(0.0) => {
                *value = serde_json::json!(0.0);
            }
            _ => {}
        }
    }
    let mut value = serde_json::to_value(request).map_err(invalid)?;
    normalize(&mut value);
    let mut digest = blake3::Hasher::new();
    serde_json::to_writer(&mut digest, &value).map_err(invalid)?;
    Ok(digest.finalize())
}

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

/// Human-authored content; roles and turn identities are assigned after authenticated admission.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct UserContent {
    pub text: String,
    pub images: Vec<ImageSource>,
    pub presentation: Option<UserInputPresentation>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum InboxPayload {
    /// A user- or host-selected command source captured before durable acceptance.
    Command {
        entry: Box<super::EntrySnapshot>,
        arguments: BTreeMap<String, Value>,
        invocation: BTreeMap<String, Value>,
        user: Option<UserContent>,
    },
    /// Explicit user control, recorded as internal context rather than a new human message.
    GoalResume {
        goal: super::goal::GoalVersion,
        objective: String,
    },
    /// An entry-authored next turn, conditional on its successful completion and goal version.
    GoalContinuation {
        source: TurnLease<AtmanUuid>,
        goal: super::goal::GoalVersion,
        text: String,
    },
    /// Explicit entry arguments and invocation configuration; never a mid-turn text injection.
    Invocation {
        arguments: BTreeMap<String, Value>,
        invocation: BTreeMap<String, Value>,
        user: Option<UserContent>,
    },
    UserSubmission {
        content: UserContent,
        invocation: BTreeMap<String, Value>,
    },
    Interjection {
        text: String,
    },
    /// A host-validated answer to a prior form; never a permission grant.
    FormReply {
        answer: crate::form::DeferredFormAnswer,
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
    ProcessEvent {
        source: super::process_watch::ProcessSource,
        content: Value,
    },
    Timer {
        timer: Uuid,
        due_at: DateTime<Utc>,
        purpose: TimerPurpose,
        content: Value,
    },
    Internal {
        kind: String,
        content: Value,
    },
}

/// Set by the notification producer, never inferred from model-authored content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerPurpose {
    Notification,
    WatchExpiry,
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
    pub scope: Option<super::task_scope::TaskScopeRef>,
    pub payload: InboxPayload,
}

impl DeliveryRequest {
    /// Authenticated authored input can resume an interrupted conversation, but notifications cannot.
    pub(super) fn resumes_interruption(&self) -> bool {
        matches!(self.sender, SenderIdentity::User(_))
            && self.scope.is_none()
            && matches!(
                self.payload,
                InboxPayload::UserSubmission { .. }
                    | InboxPayload::Command { user: Some(_), .. }
                    | InboxPayload::Invocation { user: Some(_), .. }
                    | InboxPayload::Interjection { .. }
            )
    }

    pub(crate) fn validate_source(&self) -> io::Result<()> {
        if let Some(scope) = &self.scope {
            scope.validate()?;
        }
        if let InboxPayload::Invocation { user, .. } = &self.payload {
            return if self.delivery == DeliveryMode::NextTurn
                && self.correlation.is_none()
                && (user.is_none() || matches!(self.sender, SenderIdentity::User(_)))
            {
                Ok(())
            } else {
                Err(invalid(
                    "an explicit invocation requires uncorrelated next-turn delivery and authenticated user content",
                ))
            };
        }
        let valid = match (&self.sender, &self.payload) {
            (
                SenderIdentity::User(_) | SenderIdentity::Host(_),
                InboxPayload::Command { entry, user, .. },
            ) => {
                self.delivery == DeliveryMode::NextTurn
                    && self.correlation.is_none()
                    && self.scope.is_none()
                    && self.cause.is_none()
                    && !entry.name.is_empty()
                    && entry.path.is_absolute()
                    && (user.is_none() || matches!(self.sender, SenderIdentity::User(_)))
            }
            (SenderIdentity::User(_), InboxPayload::GoalResume { goal, objective }) => {
                self.delivery == DeliveryMode::NextTurn
                    && self.correlation.is_none()
                    && self.scope.is_none()
                    && self.cause.is_none()
                    && goal.revision > 0
                    && !goal.id.is_empty()
                    && !objective.trim().is_empty()
            }
            (SenderIdentity::Host(_), InboxPayload::FormReply { answer }) => {
                self.delivery == DeliveryMode::AtBoundary
                    && self.correlation.is_none()
                    && self.scope.is_none()
                    && self.cause.is_none()
                    && !answer.prompt_id.is_empty()
                    && matches!(
                        answer.submission,
                        crate::form::FormSubmission::Submitted { .. }
                    )
                    && answer.form.accepts(&answer.submission)
            }
            (SenderIdentity::Flow(sender), InboxPayload::GoalContinuation { source, .. }) => {
                sender == &source.flow
                    && sender == &self.recipient
                    && self.delivery == DeliveryMode::NextTurn
                    && self.correlation.is_none()
                    && self.scope.is_none()
                    && self.cause.is_none()
            }
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
                | InboxPayload::ProcessEvent { .. }
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
        /// Accepted command source, or the instance's default entry when absent.
        command: Option<Uuid>,
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
    pub(crate) fn messages(&self) -> &[Message] {
        match self {
            Self::Context { messages } | Self::InvocationInput { messages, .. } => messages,
            Self::WaitingContinuation { .. } => &[],
        }
    }

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

/// Input admission metadata without retained conversation or invocation payloads.
#[derive(Clone, Debug, Serialize)]
pub struct InputRecordRef {
    pub lease: TurnLease<AtmanUuid>,
    pub message_ids: Vec<Uuid>,
    pub target: InputTarget,
    pub command: Option<Uuid>,
    pub message_count: usize,
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
    /// Last content or ordering mutation, initially the acceptance receipt.
    pub revision: CommitReceipt,
}

#[derive(Default)]
pub(super) struct Inbox {
    cancelling: Option<(TurnLease<AtmanUuid>, CommitReceipt)>,
    entries: Vec<InboxSummary>,
    requests: HashMap<(SenderIdentity, Uuid), usize>,
    messages: HashMap<Uuid, usize>,
    pending: PendingBatch<Uuid>,
    inputs: Vec<InputRecordRef>,
    last_invocation: Option<usize>,
    pub(super) waiters: ReplyWaiters,
    replacements: HashMap<Uuid, CommitReceipt>,
    edits: HashMap<(SenderIdentity, Uuid), (blake3::Hash, CommitReceipt)>,
    invalid_scopes: HashMap<super::task_scope::TaskScopeRef, InvalidatedInputScope>,
}

impl Inbox {
    pub(super) fn observe_request(
        &self,
        sender: &SenderIdentity,
        request_id: Uuid,
        observed: &CommitReceipt,
    ) -> io::Result<Option<InputObservation>> {
        let Some(&index) = self.requests.get(&(sender.clone(), request_id)) else {
            return Ok(None);
        };
        let entry = &self.entries[index];
        let input = match &entry.acceptance.disposition {
            InboxDisposition::Pending => InputDisposition::Pending,
            InboxDisposition::Cancelled { by } => InputDisposition::Cancelled {
                receipt: by.clone(),
            },
            InboxDisposition::InputCommitted { by } => {
                let index = self
                    .inputs
                    .binary_search_by_key(&by.sequence, |input| input.receipt.sequence)
                    .map_err(|_| invalid("consumed input has no admission record"))?;
                let record = &self.inputs[index];
                if &record.receipt != by || !record.message_ids.contains(&entry.envelope.id) {
                    return Err(invalid(
                        "consumed input does not match its recorded admission",
                    ));
                }
                InputDisposition::Committed {
                    lease: record.lease.clone(),
                    receipt: by.clone(),
                }
            }
        };
        Ok(Some(InputObservation {
            message_id: entry.envelope.id,
            accepted: entry.acceptance.receipt.clone(),
            input,
            observed_receipt: observed.clone(),
        }))
    }

    pub fn inputs(&self) -> &[InputRecordRef] {
        &self.inputs
    }

    pub fn last_invocation(&self) -> Option<&InputRecordRef> {
        self.last_invocation.map(|index| &self.inputs[index])
    }

    pub fn last_command(&self) -> Option<Uuid> {
        self.last_invocation().and_then(|record| record.command)
    }

    /// Select source from actual input, including a continuation of a previously admitted command.
    pub fn command_for_input(&self, ids: &[Uuid]) -> io::Result<Option<Uuid>> {
        let mut selected = ids.iter().map(|id| {
            self.messages
                .get(id)
                .map(|index| &self.entries[*index].envelope)
                .ok_or_else(|| invalid("selected invocation input is unavailable"))
        });
        let first = selected.next().transpose()?;
        let command = match first.map(|envelope| (&envelope.request.payload, envelope.id)) {
            Some((InputKind::Command, id)) => Some(id),
            Some((InputKind::GoalResume { .. } | InputKind::GoalContinuation { .. }, _)) => {
                self.last_command()
            }
            _ => None,
        };
        for envelope in selected {
            if command.is_some() || matches!(envelope?.request.payload, InputKind::Command) {
                return Err(invalid("command invocation requires a separate turn"));
            }
        }
        Ok(command)
    }

    pub(super) fn candidates(
        &self,
        correlation: Option<Uuid>,
    ) -> impl Iterator<Item = &InputEnvelope> {
        self.pending().filter(move |envelope| {
            !self.pending.has_reservation()
                && match correlation {
                    Some(correlation) => self.waiters.matches(correlation, envelope),
                    None => !self.waiters.holds(envelope),
                }
        })
    }

    pub fn reserve(
        &mut self,
        mut eligible: impl FnMut(&InputEnvelope) -> bool,
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

    pub fn reserve_reply(
        &mut self,
        correlation: Uuid,
        mut eligible: impl FnMut(&InputEnvelope) -> bool,
    ) -> Option<Batch<Uuid>> {
        let Self {
            pending,
            entries,
            messages,
            waiters,
            ..
        } = self;
        let mut selected = false;
        pending.reserve_matching(|id| {
            if !selected
                && waiters.matches(correlation, &entries[messages[id]].envelope)
                && eligible(&entries[messages[id]].envelope)
            {
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
        if !state.can_receive_reply(&waiter.lease).map_err(invalid)? {
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
        match state.state() {
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active))
                if active.phase == atman_rt::TurnPhase::Cancelling =>
            {
                if self
                    .cancelling
                    .as_ref()
                    .is_none_or(|(lease, _)| lease != &active.lease)
                {
                    self.cancelling = Some((active.lease.clone(), receipt.clone()));
                }
            }
            _ => self.cancelling = None,
        }
        self.invalidate();
        self.waiters.state_changed(state, receipt);
        if matches!(
            state.state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. })
        ) || matches!(state.state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) if active.phase == atman_rt::TurnPhase::Cancelling)
        {
            self.cancel_matching(receipt, |envelope| {
                matches!(envelope.request.payload, InputKind::GoalContinuation { .. })
            });
        }
    }

    pub fn owns(&self, batch: &Batch<Uuid>) -> bool {
        self.pending.owns(batch)
    }

    pub(super) fn user_requested_after_cancel(&self, lease: &TurnLease<AtmanUuid>) -> bool {
        self.cancelling
            .as_ref()
            .is_some_and(|(cancelled, receipt)| {
                cancelled == lease
                    && self.pending_entries().any(|entry| {
                        entry.acceptance.receipt.sequence > receipt.sequence
                            && entry.envelope.request.resumes_interruption()
                    })
            })
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
        if let InputContent::InvocationInput { command, .. } = &input.content
            && *command != self.command_for_input(&input.message_ids)?
        {
            return Err(invalid("invocation source does not match selected input"));
        }
        Ok(())
    }

    /// Durable publication and FIFO consumption share the same ownership check.
    pub fn commit_input(
        &mut self,
        batch: &Batch<Uuid>,
        input: &CommittedInput,
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
        if input.content.target() == InputTarget::InvocationInput {
            // Admitted work supersedes unselected suggestions from the previous turn.
            self.cancel_matching(&receipt, |envelope| {
                matches!(envelope.request.payload, InputKind::GoalContinuation { .. })
            });
            self.last_invocation = Some(self.inputs.len());
        }
        self.inputs.push(InputRecordRef {
            lease: input.lease.clone(),
            message_ids: input.message_ids.clone(),
            target: input.content.target(),
            command: match &input.content {
                InputContent::InvocationInput { command, .. } => *command,
                _ => None,
            },
            message_count: input.content.messages().len(),
            receipt: receipt.clone(),
        });
        Ok(receipt)
    }

    pub fn replay_input(
        &mut self,
        state: &FlowInstance<AtmanUuid>,
        input: &CommittedInput,
        receipt: CommitReceipt,
    ) -> io::Result<()> {
        self.validate_input(state, input)?;
        let selected: std::collections::HashSet<_> = input.message_ids.iter().copied().collect();
        let batch = self
            .pending
            .reserve_matching(|id| selected.contains(id))
            .ok_or_else(|| invalid("input replay cannot reserve its pending messages"))?;
        self.commit_input(&batch, input, || Ok(receipt))?;
        Ok(())
    }

    pub fn entries(&self) -> &[InboxSummary] {
        &self.entries
    }

    pub fn pending(&self) -> impl Iterator<Item = &InputEnvelope> {
        self.pending_entries().map(|entry| &entry.envelope)
    }

    pub fn pending_entries(&self) -> impl Iterator<Item = &InboxSummary> {
        self.pending
            .iter()
            .map(|id| &self.entries[self.messages[id]])
    }

    pub fn previous(&self, request: &DeliveryRequest) -> io::Result<Option<Acceptance>> {
        let Some(&index) = self
            .requests
            .get(&(request.sender.clone(), request.request_id))
        else {
            return Ok(None);
        };
        let entry = &self.entries[index];
        let matches = entry.original == request_fingerprint(request)?;
        if !matches {
            return Err(invalid(
                "delivery request ID was reused with different data",
            ));
        }
        Ok(Some(entry.acceptance.clone()))
    }

    pub(super) fn original_fingerprint(&self, entry: &InboxSummary) -> io::Result<blake3::Hash> {
        Ok(entry.original)
    }

    pub fn validate(&self, envelope: &AcceptedEnvelope) -> io::Result<PreparedInput> {
        envelope.request.validate_source()?;
        if envelope
            .request
            .scope
            .as_ref()
            .is_some_and(|scope| self.invalid_scopes.contains_key(scope))
        {
            return Err(invalid("input task scope was invalidated"));
        }
        if self.messages.contains_key(&envelope.id) || self.previous(&envelope.request)?.is_some() {
            return Err(invalid("duplicate acceptance fact"));
        }
        PreparedInput::new(envelope)
    }

    /// Called only after validation and a successful durable append, or during validated replay.
    pub fn apply(&mut self, prepared: PreparedInput, receipt: CommitReceipt) -> Acceptance {
        let PreparedInput {
            envelope,
            original,
            content,
        } = prepared;
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
        self.entries.push(InboxSummary {
            envelope,
            original,
            content,
            revision: acceptance.receipt.clone(),
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

    /// Reconcile cancellation from an already committed owner fact, preserving original bodies.
    pub(super) fn cancel_matching(
        &mut self,
        receipt: &CommitReceipt,
        mut matches: impl FnMut(&InputEnvelope) -> bool,
    ) {
        let selected: std::collections::HashSet<_> = self
            .pending()
            .filter(|envelope| matches(envelope))
            .map(|envelope| envelope.id)
            .collect();
        if selected.is_empty() {
            return;
        }
        self.invalidate();
        let Some(batch) = self.pending.reserve_matching(|id| selected.contains(id)) else {
            return;
        };
        for id in batch.items() {
            self.entries[self.messages[id]].acceptance.disposition = InboxDisposition::Cancelled {
                by: receipt.clone(),
            };
        }
        assert!(
            self.pending
                .commit(&batch, || Ok::<_, std::convert::Infallible>(()))
                .unwrap()
        );
    }
}
