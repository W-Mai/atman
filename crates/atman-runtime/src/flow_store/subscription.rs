//! Receiver-owned public answer subscriptions and atomic inbox delivery.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    num::NonZeroUsize,
};

use atman_rt::InstanceState;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{
    AcceptedEnvelope, DeliveryMode, DeliveryRequest, FlowEventRef, Inbox, InboxPayload,
    SenderIdentity,
};
use super::publication::PublishedMessage;
use super::{FlowFact, FlowRef, FlowStore, invalid};
use crate::event_log::journal::CommitReceipt;
use crate::message::MessageOrigin;

/// A cursor obtained from an actual source owner, not a caller-selected history offset.
#[derive(Clone, Debug)]
pub struct PublicationAnchor {
    source: FlowRef,
    cursor: CommitReceipt,
}

impl PublicationAnchor {
    pub fn source(&self) -> &FlowRef {
        &self.source
    }
    pub fn cursor(&self) -> &CommitReceipt {
        &self.cursor
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum AnswerFilter {
    All,
    Contains(String),
}

impl AnswerFilter {
    fn matches(&self, message: &crate::message::Message) -> bool {
        match self {
            Self::All => true,
            Self::Contains(text) => message.text_concat().contains(text),
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
pub struct AnswerSubscription {
    pub id: Uuid,
    pub source: FlowRef,
    pub filter: AnswerFilter,
    pub once: bool,
    pub purpose: SubscriptionPurpose,
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
    pub subscription: AnswerSubscription,
    pub started_after: CommitReceipt,
    pub cursor: u64,
    pub status: SubscriptionStatus,
}

/// Source-authenticated page. Records are public final answers only; private context is absent.
#[derive(Clone, Debug)]
pub struct PublicationPage {
    source: FlowRef,
    after: u64,
    through: u64,
    records: Vec<PublishedMessage>,
}

impl PublicationPage {
    pub fn through(&self) -> u64 {
        self.through
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SubscriptionMatch {
    id: Uuid,
    purpose: SubscriptionPurpose,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AnswerNotification {
    subscriptions: Vec<SubscriptionMatch>,
    message: crate::message::Message,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(super) struct Observation {
    source: FlowRef,
    through: u64,
    updates: Vec<(Uuid, SubscriptionStatus)>,
    envelopes: Vec<AcceptedEnvelope>,
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
            || matches!(&record.subscription.filter, AnswerFilter::Contains(text) if text.is_empty())
        {
            return Err(invalid("invalid or stale answer subscription"));
        }
        Ok(())
    }

    pub fn cancel(&mut self, id: Uuid) -> io::Result<()> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| invalid("subscription does not exist"))?;
        if record.status != SubscriptionStatus::Active {
            return Err(invalid("subscription is no longer active"));
        }
        record.status = SubscriptionStatus::Cancelled;
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
                || event != "final_answer"
                || envelope.request.cause.as_ref() != Some(source)
                || envelope.request.delivery != DeliveryMode::AtBoundary
                || envelope.request.correlation.is_some()
                || envelope.request.scope.is_some()
            {
                return Err(invalid("invalid observation envelope"));
            }
            let notification: AnswerNotification =
                serde_json::from_value(content.clone()).map_err(invalid)?;
            if notification.message.origin != MessageOrigin::FinalAnswer {
                return Err(invalid("observation contains non-final output"));
            }
            let mut expected = Vec::new();
            for record in &records {
                if record.cursor < source.sequence
                    && updates[&record.subscription.id] == SubscriptionStatus::Active
                    && record.subscription.filter.matches(&notification.message)
                {
                    expected.push(SubscriptionMatch {
                        id: record.subscription.id,
                        purpose: record.subscription.purpose,
                    });
                    if record.subscription.once {
                        updates.insert(record.subscription.id, SubscriptionStatus::Matched);
                    }
                }
            }
            if expected.is_empty() || notification.subscriptions != expected {
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
    pub fn publication_anchor(&self) -> PublicationAnchor {
        PublicationAnchor {
            source: self.descriptor.address.clone(),
            cursor: self.cursor.clone(),
        }
    }

    /// Read a bounded page without reopening a journal or observing private branch output.
    pub fn publications_after(
        &self,
        after: u64,
        limit: NonZeroUsize,
    ) -> io::Result<PublicationPage> {
        if after > self.cursor.sequence {
            return Err(invalid("source cursor is ahead of its journal"));
        }
        let records: Vec<_> = self
            .published_messages()
            .iter()
            .filter(|record| {
                record.receipt.sequence > after
                    && record.publication.context_run.is_none()
                    && record.publication.message.origin == MessageOrigin::FinalAnswer
            })
            .take(limit.get())
            .cloned()
            .collect();
        // Advance only for observable source facts. Advancing to unrelated commits would make
        // a self-subscription repeatedly observe its own cursor-update transactions.
        let through = records
            .last()
            .map_or(after, |record| record.receipt.sequence);
        Ok(PublicationPage {
            source: self.descriptor.address.clone(),
            after,
            through,
            records,
        })
    }

    pub fn answer_subscriptions(&self) -> impl Iterator<Item = &SubscriptionRecord> {
        self.subscriptions.records.values()
    }

    /// The trusted host checks watch authority before registration, then activates a prepared source.
    pub fn subscribe_answers(
        &mut self,
        subscription: AnswerSubscription,
        anchor: &PublicationAnchor,
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
        let receipt = self
            .journal
            .commit(&[FlowFact::AnswerSubscribed(Box::new(record.clone()))])?;
        self.subscriptions
            .records
            .insert(record.subscription.id, record);
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    pub fn unsubscribe_answers(&mut self, id: Uuid) -> io::Result<CommitReceipt> {
        let record = self
            .subscriptions
            .records
            .get(&id)
            .ok_or_else(|| invalid("subscription does not exist"))?;
        if record.status != SubscriptionStatus::Active {
            return Ok(self.cursor.clone());
        }
        let receipt = self.journal.commit(&[FlowFact::AnswerUnsubscribed(id)])?;
        self.subscriptions.cancel(id)?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Cursor movement and every matched input share one durable receiver commit.
    ///
    /// Register or cancel before this command to determine participation. No active turn is needed;
    /// execution pauses retain these inputs in the common inbox without running the recipient.
    pub fn observe_answers(&mut self, page: &PublicationPage) -> io::Result<CommitReceipt> {
        if matches!(
            self.state.state(),
            InstanceState::Closing { .. } | InstanceState::Closed
        ) {
            return Err(invalid("recipient cannot observe new answers"));
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
            return Err(invalid("source page skips unobserved answers"));
        }
        let mut updates: BTreeMap<_, _> = records
            .iter()
            .map(|record| (record.subscription.id, SubscriptionStatus::Active))
            .collect();
        let mut envelopes = Vec::new();
        for publication in &page.records {
            let mut matches = Vec::new();
            for record in &records {
                if record.cursor < publication.receipt.sequence
                    && updates[&record.subscription.id] == SubscriptionStatus::Active
                    && record
                        .subscription
                        .filter
                        .matches(&publication.publication.message)
                {
                    if record.subscription.once {
                        updates.insert(record.subscription.id, SubscriptionStatus::Matched);
                    }
                    matches.push(SubscriptionMatch {
                        id: record.subscription.id,
                        purpose: record.subscription.purpose,
                    });
                }
            }
            if matches.is_empty() {
                continue;
            }
            let source = FlowEventRef {
                flow: page.source.clone(),
                sequence: publication.receipt.sequence,
                index: 0,
            };
            // All matching subscriptions contribute reasons to one input for this source fact.
            envelopes.push(AcceptedEnvelope {
                id: Uuid::now_v7(), accepted_at: chrono::Utc::now(),
                request: DeliveryRequest {
                    request_id: publication.publication.id,
                    sender: SenderIdentity::Flow(page.source.clone()), recipient: self.descriptor.address.clone(),
                    delivery: DeliveryMode::AtBoundary, cause: Some(source.clone()), correlation: None, scope: None,
                    payload: InboxPayload::FlowEvent { source, event: "final_answer".into(),
                        content: serde_json::json!({ "subscriptions": matches, "message": publication.publication.message }) },
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
            .commit(&[FlowFact::AnswersObserved(Box::new(observation.clone()))])?;
        self.subscriptions
            .apply(&observation, &mut self.inbox, &receipt);
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
