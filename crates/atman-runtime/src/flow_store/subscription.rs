//! Receiver-owned flow event subscriptions and atomic inbox delivery.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    num::NonZeroUsize,
};

use atman_rt::InstanceState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::completion::CommittedCompletion;
use super::inbox::{
    AcceptedEnvelope, DeliveryMode, DeliveryRequest, FlowEventRef, Inbox, InboxPayload,
    SenderIdentity,
};
use super::waiter::{ReplyStatus, ReplyWaiter};
use super::{FlowFact, FlowRef, FlowStore, invalid};
use crate::event_log::journal::CommitReceipt;
use crate::message::MessageOrigin;

/// A cursor obtained from an actual source owner, not a caller-selected history offset.
#[derive(Clone, Debug)]
pub struct ObservationAnchor {
    source: FlowRef,
    cursor: CommitReceipt,
}

impl ObservationAnchor {
    pub fn source(&self) -> &FlowRef {
        &self.source
    }
    pub fn cursor(&self) -> &CommitReceipt {
        &self.cursor
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FlowEventFilter {
    FinalAnswers,
    AnswerContains(String),
    AnswersExceptTurn(crate::event::TurnId),
    TurnCompleted { turn: Option<crate::event::TurnId> },
}

impl FlowEventFilter {
    fn matches(&self, event: &ObservedEvent) -> bool {
        match (self, event) {
            (Self::FinalAnswers, ObservedEvent::FinalAnswer { .. }) => true,
            (Self::AnswerContains(text), ObservedEvent::FinalAnswer { message }) => {
                message.text_concat().contains(text)
            }
            (Self::AnswersExceptTurn(turn), ObservedEvent::FinalAnswer { message }) => {
                &message.turn_id != turn
            }
            (Self::TurnCompleted { turn }, ObservedEvent::TurnCompleted { completion }) => turn
                .as_ref()
                .is_none_or(|turn| turn == &completion.completion.lease.turn),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionPurpose {
    Watcher,
    SpawnResult,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowSubscription {
    pub id: Uuid,
    pub source: FlowRef,
    pub filter: FlowEventFilter,
    pub once: bool,
    pub purpose: SubscriptionPurpose,
    /// An exact one-shot completion can belong to an existing continuation.
    pub reply: Option<ReplyWaiter>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionStatus {
    Active,
    Matched,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionRecord {
    pub subscription: FlowSubscription,
    pub started_after: CommitReceipt,
    pub cursor: u64,
    pub status: SubscriptionStatus,
}

/// Proof obtained from a live recipient owner. Callers cannot manufacture it from serialized data.
#[derive(Clone, Debug)]
pub struct ResultSubscriptionProof(pub(super) ResultRegistration);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct ResultRegistration {
    pub recipient: FlowRef,
    pub record: SubscriptionRecord,
    pub completion: Option<SubscriptionRecord>,
    pub receipt: CommitReceipt,
}

/// Source-authenticated page of final answers and entry completions; private context is absent.
#[derive(Clone, Debug)]
pub struct FlowEventPage {
    source: FlowRef,
    after: u64,
    through: u64,
    records: Vec<(CommitReceipt, ObservedEvent)>,
}

impl FlowEventPage {
    pub fn through(&self) -> u64 {
        self.through
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SubscriptionMatch {
    id: Uuid,
    purpose: SubscriptionPurpose,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ObservedEvent {
    FinalAnswer { message: crate::message::Message },
    TurnCompleted { completion: CommittedCompletion },
}

impl ObservedEvent {
    fn name(&self) -> &'static str {
        match self {
            Self::FinalAnswer { .. } => "final_answer",
            Self::TurnCompleted { .. } => "turn_completed",
        }
    }

    fn validate_source(&self, source: &FlowEventRef) -> io::Result<()> {
        let valid = match self {
            Self::FinalAnswer { message } => message.origin == MessageOrigin::FinalAnswer,
            Self::TurnCompleted { completion } => {
                completion.completion.lease.flow == source.flow
                    && completion.receipt.journal_id == source.flow.instance_id.0.0
                    && completion.receipt.sequence == source.sequence
            }
        };
        if valid {
            Ok(())
        } else {
            Err(invalid("observation does not match its source fact"))
        }
    }
}

#[derive(Deserialize, Serialize)]
struct EventNotification {
    subscriptions: Vec<SubscriptionMatch>,
    #[serde(flatten)]
    event: ObservedEvent,
}

pub(crate) fn is_watcher_notification(envelope: &AcceptedEnvelope) -> bool {
    match &envelope.request.payload {
        InboxPayload::Watcher { .. } | InboxPayload::ProcessEvent { .. } => true,
        InboxPayload::Timer { content, .. } => {
            content.get("kind").and_then(|kind| kind.as_str()) == Some("process_watch_expired")
        }
        InboxPayload::FlowEvent { content, .. } => {
            serde_json::from_value::<EventNotification>(content.clone()).is_ok_and(|notification| {
                notification
                    .subscriptions
                    .iter()
                    .any(|reason| reason.purpose == SubscriptionPurpose::Watcher)
            })
        }
        _ => false,
    }
}

pub(crate) fn completion_notification(
    envelope: &AcceptedEnvelope,
) -> io::Result<CommittedCompletion> {
    let InboxPayload::FlowEvent {
        source,
        event,
        content,
    } = &envelope.request.payload
    else {
        return Err(invalid("reply does not contain a source completion"));
    };
    let notification: EventNotification =
        serde_json::from_value(content.clone()).map_err(invalid)?;
    notification.event.validate_source(source)?;
    if event != notification.event.name() {
        return Err(invalid("reply event type disagrees with its content"));
    }
    match notification.event {
        ObservedEvent::TurnCompleted { completion } => Ok(completion),
        _ => Err(invalid("reply is not an entry completion")),
    }
}

fn event_request_id(source: &FlowEventRef) -> Uuid {
    let mut digest = Sha256::new();
    digest.update(b"atman.flow.observation");
    digest.update(source.flow.session_id.0.as_bytes());
    digest.update(source.flow.instance_id.0.0.as_bytes());
    digest.update(source.sequence.to_be_bytes());
    digest.update(source.index.to_be_bytes());
    Uuid::from_bytes(digest.finalize()[..16].try_into().expect("digest prefix"))
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Observation {
    pub source: FlowRef,
    pub through: u64,
    pub updates: Vec<(Uuid, SubscriptionStatus)>,
    pub envelopes: Vec<AcceptedEnvelope>,
}

#[derive(Default)]
pub(super) struct Subscriptions {
    records: BTreeMap<Uuid, SubscriptionRecord>,
    // No subsequently registered subscription can backfill an already observed source position.
    observed: HashMap<FlowRef, u64>,
}

impl Subscriptions {
    pub fn register(&mut self, record: SubscriptionRecord) -> io::Result<()> {
        self.validate_registration(&record)?;
        self.records.insert(record.subscription.id, record);
        Ok(())
    }

    fn validate_registration(&self, record: &SubscriptionRecord) -> io::Result<()> {
        if record.subscription.id.is_nil()
            || record.started_after.journal_id != record.subscription.source.instance_id.0.0
            || record.started_after.sequence == 0
            || record.cursor != record.started_after.sequence
            || record.status != SubscriptionStatus::Active
            || self.records.contains_key(&record.subscription.id)
            || self
                .observed
                .get(&record.subscription.source)
                .is_some_and(|cursor| *cursor > record.cursor)
            || matches!(&record.subscription.filter, FlowEventFilter::AnswerContains(text) if text.is_empty())
        {
            return Err(invalid("invalid or stale flow subscription"));
        }
        if let Some(reply) = &record.subscription.reply {
            if !record.subscription.once
                || !matches!(
                    record.subscription.filter,
                    FlowEventFilter::TurnCompleted { turn: Some(_) }
                )
                || reply.sender != SenderIdentity::Flow(record.subscription.source.clone())
                || self.records.values().any(|existing| {
                    existing.status == SubscriptionStatus::Active
                        && existing.subscription.source == record.subscription.source
                        && existing.subscription.filter == record.subscription.filter
                        && existing
                            .subscription
                            .reply
                            .as_ref()
                            .is_some_and(|old| old.correlation != reply.correlation)
                })
            {
                return Err(invalid(
                    "a source completion cannot belong to conflicting reply waiters",
                ));
            }
        }
        Ok(())
    }

    pub fn validate_reply(
        record: &SubscriptionRecord,
        inbox: &Inbox,
        recipient: &FlowRef,
    ) -> io::Result<()> {
        if let Some(reply) = &record.subscription.reply
            && (reply.lease.flow != *recipient
                || !inbox
                    .waiters
                    .get(reply.correlation)
                    .is_some_and(|registered| {
                        registered.waiter == *reply && registered.status == ReplyStatus::Pending
                    }))
        {
            return Err(invalid(
                "completion subscription has no live matching reply registration",
            ));
        }
        Ok(())
    }

    pub fn cancel(
        &mut self,
        id: Uuid,
        inbox: &mut Inbox,
        receipt: &CommitReceipt,
    ) -> io::Result<()> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| invalid("subscription does not exist"))?;
        if record.status != SubscriptionStatus::Active {
            return Err(invalid("subscription is no longer active"));
        }
        record.status = SubscriptionStatus::Cancelled;
        let correlation = record
            .subscription
            .reply
            .as_ref()
            .map(|reply| reply.correlation);
        if let Some(correlation) = correlation
            && !self.records.values().any(|record| {
                record.status == SubscriptionStatus::Active
                    && record
                        .subscription
                        .reply
                        .as_ref()
                        .is_some_and(|reply| reply.correlation == correlation)
            })
            && inbox
                .waiters
                .get(correlation)
                .is_some_and(|record| record.status == ReplyStatus::Pending)
        {
            inbox.invalidate();
            inbox.waiters.cancel(correlation, receipt.clone());
        }
        Ok(())
    }

    pub fn close(&mut self) {
        for record in self.records.values_mut() {
            if record.status == SubscriptionStatus::Active {
                record.status = SubscriptionStatus::Cancelled;
            }
        }
    }

    pub fn validate(
        &self,
        observation: &Observation,
        inbox: &Inbox,
        recipient: &FlowRef,
    ) -> io::Result<()> {
        if self
            .observed
            .get(&observation.source)
            .is_some_and(|cursor| *cursor >= observation.through)
            || observation.updates.is_empty()
        {
            return Err(invalid("duplicate or empty source observation"));
        }
        let records: Vec<_> = self
            .records
            .values()
            .filter(|record| {
                record.subscription.source == observation.source
                    && record.status == SubscriptionStatus::Active
                    && record.cursor < observation.through
            })
            .collect();
        let mut updates: BTreeMap<_, _> = records
            .iter()
            .map(|record| (record.subscription.id, SubscriptionStatus::Active))
            .collect();
        let mut messages = std::collections::HashSet::new();
        let mut events = std::collections::HashSet::new();
        let mut last_sequence = self.observed.get(&observation.source).copied().unwrap_or(0);
        for envelope in &observation.envelopes {
            let InboxPayload::FlowEvent {
                source,
                event,
                content,
            } = &envelope.request.payload
            else {
                return Err(invalid("observation requires a source event"));
            };
            if !messages.insert(envelope.id)
                || !events.insert(envelope.request.request_id)
                || envelope.request.sender != SenderIdentity::Flow(observation.source.clone())
                || envelope.request.recipient != *recipient
                || source.flow != observation.source
                || source.sequence > observation.through
                || source.sequence <= last_sequence
                || source.index != 0
                || envelope.request.request_id != event_request_id(source)
                || envelope.request.cause.as_ref() != Some(source)
                || envelope.request.delivery != DeliveryMode::AtBoundary
                || envelope.request.scope.is_some()
            {
                return Err(invalid("invalid observation envelope"));
            }
            let notification: EventNotification =
                serde_json::from_value(content.clone()).map_err(invalid)?;
            notification.event.validate_source(source)?;
            if event != notification.event.name() {
                return Err(invalid("observation event type does not match its content"));
            }
            let mut expected = Vec::new();
            let mut correlation = None;
            for record in &records {
                if record.cursor < source.sequence
                    && updates[&record.subscription.id] == SubscriptionStatus::Active
                    && record.subscription.filter.matches(&notification.event)
                {
                    expected.push(SubscriptionMatch {
                        id: record.subscription.id,
                        purpose: record.subscription.purpose,
                    });
                    if let Some(reply) = &record.subscription.reply {
                        correlation = Some(reply.correlation);
                    }
                    if record.subscription.once {
                        updates.insert(record.subscription.id, SubscriptionStatus::Matched);
                    }
                }
            }
            if expected.is_empty()
                || notification.subscriptions != expected
                || envelope.request.correlation != correlation
            {
                return Err(invalid("observation reasons do not match subscriptions"));
            }
            last_sequence = source.sequence;
            inbox.validate(envelope)?;
        }
        if observation.updates != updates.into_iter().collect::<Vec<_>>() {
            return Err(invalid("observation cursor updates are incomplete"));
        }
        Ok(())
    }

    pub fn apply(&mut self, observation: &Observation, inbox: &mut Inbox, receipt: &CommitReceipt) {
        for (id, status) in &observation.updates {
            let record = self.records.get_mut(id).expect("validated subscription");
            record.cursor = observation.through;
            record.status = *status;
        }
        self.observed
            .insert(observation.source.clone(), observation.through);
        for envelope in &observation.envelopes {
            inbox.apply(envelope.clone(), receipt.clone());
        }
    }
}

impl FlowStore {
    pub fn result_subscription_proof(
        &self,
        id: Uuid,
        completion_id: Option<Uuid>,
    ) -> io::Result<ResultSubscriptionProof> {
        let record = self
            .subscriptions
            .records
            .get(&id)
            .ok_or_else(|| invalid("result subscription is not registered"))?;
        if !self.state.accepts_input()
            || record.status != SubscriptionStatus::Active
            || record.subscription.purpose != SubscriptionPurpose::SpawnResult
            || !matches!(
                record.subscription.filter,
                FlowEventFilter::FinalAnswers | FlowEventFilter::AnswersExceptTurn(_)
            )
            || record.subscription.once
            || record.subscription.reply.is_some()
        {
            return Err(invalid(
                "result subscription is not an active continuing answer route",
            ));
        }
        let completion = completion_id
            .map(|id| {
                let completion = self
                    .subscriptions
                    .records
                    .get(&id)
                    .ok_or_else(|| invalid("completion route is not registered"))?;
                if completion.status != SubscriptionStatus::Active
                    || completion.subscription.source != record.subscription.source
                    || completion.subscription.purpose != SubscriptionPurpose::SpawnResult
                    || completion.subscription.reply.is_none()
                {
                    return Err(invalid(
                        "completion route is not an active correlated subscription",
                    ));
                }
                Subscriptions::validate_reply(completion, &self.inbox, &self.descriptor.address)?;
                Ok(completion.clone())
            })
            .transpose()?;
        Ok(ResultSubscriptionProof(ResultRegistration {
            recipient: self.descriptor.address.clone(),
            record: record.clone(),
            completion,
            receipt: self.cursor.clone(),
        }))
    }
    pub fn observation_anchor(&self) -> ObservationAnchor {
        ObservationAnchor {
            source: self.descriptor.address.clone(),
            cursor: self.cursor.clone(),
        }
    }

    /// Read a bounded page without reopening a journal or observing private branch output.
    pub fn events_after(&self, after: u64, limit: NonZeroUsize) -> io::Result<FlowEventPage> {
        if after > self.cursor.sequence {
            return Err(invalid("source cursor is ahead of its journal"));
        }
        let publications = self.published_messages();
        let mut answers = publications
            [publications.partition_point(|p| p.receipt.sequence <= after)..]
            .iter()
            .filter(|record| {
                record.publication.context_run.is_none()
                    && record.publication.message.origin == MessageOrigin::FinalAnswer
            })
            .map(|record| {
                (
                    record.receipt.clone(),
                    ObservedEvent::FinalAnswer {
                        message: record.publication.message.clone(),
                    },
                )
            })
            .peekable();
        let completions = self.completions();
        let mut completions = completions
            [completions.partition_point(|p| p.receipt.sequence <= after)..]
            .iter()
            .map(|record| {
                (
                    record.receipt.clone(),
                    ObservedEvent::TurnCompleted {
                        completion: record.clone(),
                    },
                )
            })
            .peekable();
        let records: Vec<_> = std::iter::from_fn(|| match (answers.peek(), completions.peek()) {
            (Some((a, _)), Some((b, _))) if a.sequence < b.sequence => answers.next(),
            (Some(_), None) => answers.next(),
            (_, Some(_)) => completions.next(),
            (None, None) => None,
        })
        .take(limit.get())
        .collect();
        // Advance only for observable source facts. Advancing to unrelated commits would make
        // a self-subscription repeatedly observe its own cursor-update transactions.
        let through = records
            .last()
            .map_or(after, |(receipt, _)| receipt.sequence);
        Ok(FlowEventPage {
            source: self.descriptor.address.clone(),
            after,
            through,
            records,
        })
    }

    pub fn flow_subscriptions(&self) -> impl Iterator<Item = &SubscriptionRecord> {
        self.subscriptions.records.values()
    }

    /// The trusted host checks watch authority before registration, then activates a prepared source.
    pub fn subscribe_events(
        &mut self,
        subscription: FlowSubscription,
        anchor: &ObservationAnchor,
    ) -> io::Result<CommitReceipt> {
        if subscription.source != anchor.source {
            return Err(invalid("subscription anchor belongs to another source"));
        }
        if let Some(previous) = self.subscriptions.records.get(&subscription.id) {
            return if previous.subscription == subscription
                && previous.started_after == anchor.cursor
            {
                Ok(self.cursor.clone())
            } else {
                Err(invalid(
                    "subscription identity conflicts with recorded registration",
                ))
            };
        }
        if !self.state.accepts_input() {
            return Err(invalid("recipient cannot register subscriptions"));
        }
        let record = SubscriptionRecord {
            subscription,
            started_after: anchor.cursor.clone(),
            cursor: anchor.cursor.sequence,
            status: SubscriptionStatus::Active,
        };
        self.subscriptions.validate_registration(&record)?;
        let mut facts = Vec::new();
        if let Some(reply) = &record.subscription.reply {
            if reply.lease.flow != self.descriptor.address {
                return Err(invalid("reply waiter belongs to another flow"));
            }
            if let Some(existing) = self.inbox.waiters.get(reply.correlation) {
                if existing.waiter != *reply || existing.status != ReplyStatus::Pending {
                    return Err(invalid(
                        "reply waiter conflicts with completion subscription",
                    ));
                }
            } else {
                self.inbox.validate_waiter(&self.state, reply)?;
                facts.push(FlowFact::ReplyWaiterRegistered(Box::new(reply.clone())));
            }
        }
        facts.push(FlowFact::FlowSubscribed(Box::new(record.clone())));
        FlowFact::validate_batch(&facts)?;
        let receipt = self.journal.commit(&facts)?;
        if let FlowFact::ReplyWaiterRegistered(reply) = &facts[0] {
            self.inbox.invalidate();
            self.inbox
                .waiters
                .register(reply.as_ref().clone(), receipt.clone());
        }
        self.subscriptions
            .records
            .insert(record.subscription.id, record);
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    pub fn unsubscribe_events(&mut self, id: Uuid) -> io::Result<CommitReceipt> {
        let record = self
            .subscriptions
            .records
            .get(&id)
            .ok_or_else(|| invalid("subscription does not exist"))?;
        if record.status != SubscriptionStatus::Active {
            return Ok(self.cursor.clone());
        }
        let receipt = self.journal.commit(&[FlowFact::FlowUnsubscribed(id)])?;
        self.subscriptions.cancel(id, &mut self.inbox, &receipt)?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Cursor movement and every matched input share one durable receiver commit.
    ///
    /// Register or cancel before this command to determine participation. No active turn is needed;
    /// execution pauses retain these inputs in the common inbox without running the recipient.
    pub fn observe_events(&mut self, page: &FlowEventPage) -> io::Result<CommitReceipt> {
        if matches!(
            self.state.state(),
            InstanceState::Closing { .. } | InstanceState::Closed
        ) {
            return Err(invalid("recipient cannot observe new flow events"));
        }
        let records: Vec<_> = self
            .subscriptions
            .records
            .values()
            .filter(|record| {
                record.subscription.source == page.source
                    && record.status == SubscriptionStatus::Active
                    && record.cursor < page.through
            })
            .collect();
        if records.is_empty()
            || self
                .subscriptions
                .observed
                .get(&page.source)
                .is_some_and(|cursor| *cursor >= page.through)
        {
            return Ok(self.cursor.clone());
        }
        if records.iter().any(|record| record.cursor < page.after) {
            return Err(invalid("source page skips unobserved flow events"));
        }
        let mut updates: BTreeMap<_, _> = records
            .iter()
            .map(|record| (record.subscription.id, SubscriptionStatus::Active))
            .collect();
        let mut envelopes = Vec::new();
        for (receipt, event) in &page.records {
            let mut matches = Vec::new();
            let mut correlation = None;
            for record in &records {
                if record.cursor < receipt.sequence
                    && updates[&record.subscription.id] == SubscriptionStatus::Active
                    && record.subscription.filter.matches(event)
                {
                    if record.subscription.once {
                        updates.insert(record.subscription.id, SubscriptionStatus::Matched);
                    }
                    matches.push(SubscriptionMatch {
                        id: record.subscription.id,
                        purpose: record.subscription.purpose,
                    });
                    if let Some(reply) = &record.subscription.reply {
                        correlation = Some(reply.correlation);
                    }
                }
            }
            if matches.is_empty() {
                continue;
            }
            let source = FlowEventRef {
                flow: page.source.clone(),
                sequence: receipt.sequence,
                index: 0,
            };
            // All matching subscriptions contribute reasons to one input for this source fact.
            envelopes.push(AcceptedEnvelope {
                id: Uuid::now_v7(),
                accepted_at: chrono::Utc::now(),
                request: DeliveryRequest {
                    request_id: event_request_id(&source),
                    sender: SenderIdentity::Flow(page.source.clone()),
                    recipient: self.descriptor.address.clone(),
                    delivery: DeliveryMode::AtBoundary,
                    cause: Some(source.clone()),
                    correlation,
                    scope: None,
                    payload: InboxPayload::FlowEvent {
                        source,
                        event: event.name().into(),
                        content: serde_json::to_value(EventNotification {
                            subscriptions: matches,
                            event: event.clone(),
                        })
                        .map_err(invalid)?,
                    },
                },
            });
        }
        let observation = Observation {
            source: page.source.clone(),
            through: page.through,
            updates: updates.into_iter().collect(),
            envelopes,
        };
        self.subscriptions
            .validate(&observation, &self.inbox, &self.descriptor.address)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::EventsObserved(Box::new(observation.clone()))])?;
        self.subscriptions
            .apply(&observation, &mut self.inbox, &receipt);
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
