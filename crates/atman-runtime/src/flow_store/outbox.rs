//! Durable send intents and authenticated receiver acknowledgements.

use std::collections::{BTreeSet, HashMap};
use std::io;

use atman_rt::{FlowInstance, TurnLease};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{Acceptance, DeliveryRequest, SenderIdentity};
use super::waiter::{ReplyStatus, ReplyWaiter};
use super::{FlowFact, FlowRef, FlowStore, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SendIntent {
    pub lease: TurnLease<AtmanUuid>,
    pub authorization: SendAuthorization,
    pub request: DeliveryRequest,
    pub reply: Option<ReplyWaiter>,
    pub queued_at: DateTime<Utc>,
}

/// Authority used for new receiver admission, including after restarting the sender.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SendAuthorization {
    /// Delivery requires the external host's explicit callback on each attempt.
    Host,
    /// The runtime rechecks the sender's current durable Send grants.
    PeerGrants,
}

/// Receiver acceptance only; this does not confirm execution or business completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeliveryReceipt {
    pub message_id: Uuid,
    pub receipt: CommitReceipt,
}

impl From<&Acceptance> for DeliveryReceipt {
    fn from(acceptance: &Acceptance) -> Self {
        Self {
            message_id: acceptance.message_id,
            receipt: acceptance.receipt.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SendStatus {
    PendingDelivery,
    Accepted {
        delivery: DeliveryReceipt,
        by: CommitReceipt,
    },
    Failed {
        failure: DeliveryFailure,
        by: CommitReceipt,
    },
}

/// A definitive receiver rejection, distinct from an unavailable transport or uncertain ACK.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DeliveryFailure {
    RecipientClosed { observed: CommitReceipt },
    RecipientDeleted { observed: CommitReceipt },
}

/// Only receiver admission constructs this error, after checking its acceptance ledger.
#[derive(Debug, thiserror::Error)]
#[error("recipient is closed or deleted and did not accept this request")]
pub(crate) struct RecipientRejected(pub DeliveryFailure);

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SendRecord {
    pub intent: SendIntent,
    pub queued: CommitReceipt,
    pub status: SendStatus,
}

/// Body-free delivery state. The complete intent remains in its queued journal batch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SendSummary {
    pub request_id: Uuid,
    pub recipient: FlowRef,
    pub authorization: SendAuthorization,
    pub scope: Option<super::task_scope::TaskScopeRef>,
    pub reply: Option<Uuid>,
    pub queued: CommitReceipt,
    pub status: SendStatus,
}

impl SendSummary {
    fn new(intent: &SendIntent, queued: CommitReceipt) -> Self {
        Self {
            request_id: intent.request.request_id,
            recipient: intent.request.recipient.clone(),
            authorization: intent.authorization,
            scope: intent.request.scope.clone(),
            reply: intent.reply.as_ref().map(|waiter| waiter.correlation),
            queued,
            status: SendStatus::PendingDelivery,
        }
    }

    pub(super) fn read(
        &self,
        batch: crate::event_log::journal::CommittedBatch<FlowFact>,
    ) -> io::Result<SendRecord> {
        if batch.receipt != self.queued {
            return Err(invalid("send body does not belong to its queued receipt"));
        }
        let intent = batch
            .facts
            .into_iter()
            .find_map(|fact| match fact {
                FlowFact::SendQueued(intent) if intent.request.request_id == self.request_id => {
                    Some(*intent)
                }
                _ => None,
            })
            .ok_or_else(|| invalid("send intent is missing from its queued batch"))?;
        Ok(SendRecord {
            intent,
            queued: self.queued.clone(),
            status: self.status.clone(),
        })
    }
}

impl From<&SendRecord> for SendSummary {
    fn from(record: &SendRecord) -> Self {
        Self {
            status: record.status.clone(),
            ..Self::new(&record.intent, record.queued.clone())
        }
    }
}

#[derive(Default)]
pub(super) struct Outbox {
    records: Vec<SendSummary>,
    indices: HashMap<Uuid, usize>,
    replies: HashMap<Uuid, usize>,
    pending: BTreeSet<usize>,
}

impl Outbox {
    pub fn records(&self) -> &[SendSummary] {
        &self.records
    }

    pub fn pending(&self) -> impl Iterator<Item = &SendSummary> {
        self.pending.iter().map(|&index| &self.records[index])
    }

    pub fn get(&self, request_id: Uuid) -> Option<&SendSummary> {
        self.indices
            .get(&request_id)
            .map(|&index| &self.records[index])
    }

    pub fn by_reply(&self, correlation: Uuid) -> Option<&SendSummary> {
        self.replies
            .get(&correlation)
            .map(|&index| &self.records[index])
    }

    pub fn validate(&self, state: &FlowInstance<AtmanUuid>, intent: &SendIntent) -> io::Result<()> {
        if self.get(intent.request.request_id).is_some() {
            return Err(invalid("duplicate send intent"));
        }
        if intent.request.sender != SenderIdentity::Flow(state.address().clone()) {
            return Err(invalid("send intent does not belong to this flow"));
        }
        intent.request.validate_source()?;
        if !state.can_receive_reply(&intent.lease).map_err(invalid)? {
            return Err(invalid("flow cannot admit a new send"));
        }
        if let Some(reply) = &intent.reply {
            if reply.lease != intent.lease
                || Some(reply.correlation) != intent.request.correlation
                || reply.sender != SenderIdentity::Flow(intent.request.recipient.clone())
            {
                return Err(invalid("send intent and reply waiter disagree"));
            }
            if self.replies.contains_key(&reply.correlation) {
                return Err(invalid("reply waiter already belongs to another send"));
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, intent: &SendIntent, receipt: CommitReceipt) {
        let record = SendSummary::new(intent, receipt);
        self.pending.insert(self.records.len());
        self.indices.insert(record.request_id, self.records.len());
        if let Some(correlation) = record.reply {
            self.replies.insert(correlation, self.records.len());
        }
        self.records.push(record);
    }

    pub fn validate_confirmation(
        &self,
        request_id: Uuid,
        delivery: &DeliveryReceipt,
    ) -> io::Result<()> {
        let record = self
            .get(request_id)
            .ok_or_else(|| invalid("send intent does not exist"))?;
        if record.recipient.instance_id.0.0 != delivery.receipt.journal_id
            || delivery.receipt.sequence == 0
            || delivery.message_id.is_nil()
        {
            return Err(invalid(
                "delivery receipt does not identify the recipient journal",
            ));
        }
        if record.status != SendStatus::PendingDelivery {
            return Err(invalid("send intent already has a delivery confirmation"));
        }
        Ok(())
    }

    pub fn confirm(
        &mut self,
        request_id: Uuid,
        delivery: DeliveryReceipt,
        receipt: CommitReceipt,
    ) -> SendStatus {
        let index = self.indices[&request_id];
        self.pending.remove(&index);
        let record = &mut self.records[index];
        record.status = SendStatus::Accepted {
            delivery,
            by: receipt,
        };
        record.status.clone()
    }

    pub fn validate_failure(&self, request_id: Uuid, failure: &DeliveryFailure) -> io::Result<()> {
        let record = self
            .get(request_id)
            .ok_or_else(|| invalid("send intent does not exist"))?;
        failure.validate_recipient(record)?;
        if record.status != SendStatus::PendingDelivery {
            return Err(invalid("send already has a terminal delivery outcome"));
        }
        Ok(())
    }

    pub fn fail(
        &mut self,
        request_id: Uuid,
        failure: DeliveryFailure,
        receipt: CommitReceipt,
        inbox: &mut super::inbox::Inbox,
    ) -> SendStatus {
        let index = self.indices[&request_id];
        self.pending.remove(&index);
        let record = &mut self.records[index];
        record.status = SendStatus::Failed {
            failure,
            by: receipt.clone(),
        };
        if let Some(correlation) = record.reply
            && inbox
                .waiters
                .get(correlation)
                .is_some_and(|waiter| waiter.status == ReplyStatus::Pending)
        {
            inbox.invalidate();
            inbox.waiters.cancel(correlation, receipt);
        }
        record.status.clone()
    }
}

impl DeliveryFailure {
    fn validate_recipient(&self, record: &SendSummary) -> io::Result<()> {
        let (observed, journal_id) = match self {
            Self::RecipientClosed { observed } => (observed, record.recipient.instance_id.0.0),
            Self::RecipientDeleted { observed } => (observed, record.recipient.session_id.0),
        };
        if observed.journal_id != journal_id || observed.sequence == 0 {
            return Err(invalid(
                "delivery rejection does not identify its receiver journal",
            ));
        }
        Ok(())
    }
}

impl FlowStore {
    /// Record an authenticated, definitive rejection. An uncertain transport error is not a rejection.
    ///
    /// The failure and any pending reply cancellation share one commit. Retries cannot replace acceptance or revive a failed send.
    pub fn fail_delivery(
        &mut self,
        request_id: Uuid,
        failure: DeliveryFailure,
    ) -> io::Result<SendRecord> {
        if let Some(record) = self.outbox.get(request_id)
            && matches!(record.status, SendStatus::Failed { .. })
        {
            // Concurrent attempts may observe later commits of the same permanently closed receiver.
            failure.validate_recipient(record)?;
            return self
                .send_record(request_id)?
                .ok_or_else(|| invalid("send intent is missing"));
        }
        self.outbox.validate_failure(request_id, &failure)?;
        let mut record = self
            .send_record(request_id)?
            .ok_or_else(|| invalid("send intent is missing"))?;
        let receipt = self.journal.commit(&[FlowFact::DeliveryFailed {
            request_id,
            failure: failure.clone(),
        }])?;
        record.status = self
            .outbox
            .fail(request_id, failure, receipt.clone(), &mut self.inbox);
        self.cursor = receipt;
        Ok(record)
    }

    pub(crate) fn pending_send_scopes(&self) -> Vec<super::task_scope::TaskScopeRef> {
        let mut seen = std::collections::HashSet::new();
        self.inbox
            .waiters
            .pending()
            .filter_map(|record| self.reply_scope(record.waiter.correlation))
            .filter(|scope| seen.insert(*scope))
            .cloned()
            .collect()
    }

    pub(crate) fn reply_scope(
        &self,
        correlation: Uuid,
    ) -> Option<&super::task_scope::TaskScopeRef> {
        self.outbox
            .by_reply(correlation)
            .and_then(|record| record.scope.as_ref())
    }

    /// Load one complete intent from its committed batch, retaining the current delivery outcome.
    pub fn send_record(&mut self, request_id: Uuid) -> io::Result<Option<SendRecord>> {
        let Some(summary) = self.outbox.get(request_id) else {
            return Ok(None);
        };
        summary
            .read(self.journal.inner.read_batch(&summary.queued)?)
            .map(Some)
    }

    /// Lightweight delivery history; use send_record to load an intent body.
    pub fn sends(&self) -> &[SendSummary] {
        self.outbox.records()
    }

    pub fn pending_sends(&self) -> impl Iterator<Item = &SendSummary> {
        self.outbox.pending()
    }

    /// Authenticate the invoking flow before calling. Persist intent before contacting the recipient.
    ///
    /// An attached reply waiter is registered in the same commit. Recorded send retries return their original record before checking new-send authorization or execution admission.
    pub fn queue_send(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<SendRecord> {
        self.queue_send_authorized(
            lease,
            request,
            reply,
            SendAuthorization::Host,
            authorize_new,
        )
    }

    pub(crate) fn queue_send_authorized(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        request: DeliveryRequest,
        reply: Option<ReplyWaiter>,
        authorization: SendAuthorization,
        authorize_new: impl FnOnce(&DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<SendRecord> {
        if let Some(previous) = self.send_retry(&request, reply.as_ref(), authorization)? {
            return Ok(previous);
        }
        let intent = SendIntent {
            lease: lease.clone(),
            authorization,
            request,
            reply,
            queued_at: Utc::now(),
        };
        self.outbox.validate(&self.state, &intent)?;
        let mut facts = Vec::new();
        if let Some(reply) = &intent.reply {
            match self.inbox.waiters.get(reply.correlation) {
                Some(record)
                    if record.waiter == *reply && record.status == ReplyStatus::Pending => {}
                Some(_) => {
                    return Err(invalid(
                        "send reply waiter conflicts with existing registration",
                    ));
                }
                None => {
                    self.inbox.validate_waiter(&self.state, reply)?;
                    facts.push(FlowFact::ReplyWaiterRegistered(Box::new(reply.clone())));
                }
            }
        }
        authorize_new(&intent.request)?;
        facts.push(FlowFact::SendQueued(Box::new(intent.clone())));
        let receipt = self.journal.commit(&facts)?;
        if let [FlowFact::ReplyWaiterRegistered(waiter), ..] = facts.as_slice() {
            self.inbox.invalidate();
            self.inbox
                .waiters
                .register((**waiter).clone(), receipt.clone());
        }
        self.outbox.apply(&intent, receipt.clone());
        self.cursor = receipt.clone();
        Ok(SendRecord {
            intent,
            queued: receipt,
            status: SendStatus::PendingDelivery,
        })
    }

    pub(crate) fn send_retry(
        &mut self,
        request: &DeliveryRequest,
        reply: Option<&ReplyWaiter>,
        authorization: SendAuthorization,
    ) -> io::Result<Option<SendRecord>> {
        if request.sender != SenderIdentity::Flow(self.descriptor.address.clone()) {
            return Err(invalid("send intent does not belong to this flow"));
        }
        if let Some(previous) = self.send_record(request.request_id)? {
            if previous.intent.request != *request
                || previous.intent.reply.as_ref() != reply
                || previous.intent.authorization != authorization
            {
                return Err(invalid("send request ID was reused with different data"));
            }
            return Ok(Some(previous));
        }
        Ok(None)
    }

    /// Confirm an authenticated receiver ACK, including reconciliation after interruption or closure.
    ///
    /// The transport must bind this receipt to the requested recipient. A journal digest is an integrity check, not a signature. Uncertain delivery remains pending until the receiver's idempotent acceptance is reconciled.
    pub fn confirm_delivery(
        &mut self,
        request_id: Uuid,
        delivery: DeliveryReceipt,
    ) -> io::Result<SendRecord> {
        if let Some(record) = self.outbox.get(request_id) {
            if let SendStatus::Accepted {
                delivery: existing, ..
            } = &record.status
            {
                if existing == &delivery {
                    return self
                        .send_record(request_id)?
                        .ok_or_else(|| invalid("send intent is missing"));
                }
                return Err(invalid(
                    "delivery confirmation conflicts with the original receipt",
                ));
            }
        }
        self.outbox.validate_confirmation(request_id, &delivery)?;
        let mut record = self
            .send_record(request_id)?
            .ok_or_else(|| invalid("send intent is missing"))?;
        let receipt = self.journal.commit(&[FlowFact::DeliveryConfirmed {
            request_id,
            delivery: delivery.clone(),
        }])?;
        record.status = self.outbox.confirm(request_id, delivery, receipt.clone());
        self.cursor = receipt;
        Ok(record)
    }
}
