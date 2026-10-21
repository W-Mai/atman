//! Durable send intents and authenticated receiver acknowledgements.

use std::collections::{BTreeSet, HashMap};
use std::io;

use atman_rt::{FlowInstance, TurnLease};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{Acceptance, DeliveryRequest, SenderIdentity};
use super::waiter::{ReplyStatus, ReplyWaiter};
use super::{FlowFact, FlowStore, invalid};
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

#[derive(Default)]
pub(super) struct Outbox {
    records: Vec<SendRecord>,
    indices: HashMap<Uuid, usize>,
    replies: HashMap<Uuid, usize>,
    pending: BTreeSet<usize>,
}

impl Outbox {
    pub fn records(&self) -> &[SendRecord] {
        &self.records
    }

    pub fn pending(&self) -> impl Iterator<Item = &SendRecord> {
        self.pending.iter().map(|&index| &self.records[index])
    }

    pub fn get(&self, request_id: Uuid) -> Option<&SendRecord> {
        self.indices
            .get(&request_id)
            .map(|&index| &self.records[index])
    }

    pub fn by_reply(&self, correlation: Uuid) -> Option<&SendRecord> {
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

    pub fn apply(&mut self, intent: SendIntent, receipt: CommitReceipt) -> SendRecord {
        let record = SendRecord {
            intent,
            queued: receipt,
            status: SendStatus::PendingDelivery,
        };
        self.pending.insert(self.records.len());
        self.indices
            .insert(record.intent.request.request_id, self.records.len());
        if let Some(waiter) = &record.intent.reply {
            self.replies.insert(waiter.correlation, self.records.len());
        }
        self.records.push(record.clone());
        record
    }

    pub fn validate_confirmation(
        &self,
        request_id: Uuid,
        delivery: &DeliveryReceipt,
    ) -> io::Result<()> {
        let record = self
            .get(request_id)
            .ok_or_else(|| invalid("send intent does not exist"))?;
        if record.intent.request.recipient.instance_id.0.0 != delivery.receipt.journal_id
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
    ) -> SendRecord {
        let index = self.indices[&request_id];
        self.pending.remove(&index);
        let record = &mut self.records[index];
        record.status = SendStatus::Accepted {
            delivery,
            by: receipt,
        };
        record.clone()
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
    ) -> SendRecord {
        let index = self.indices[&request_id];
        self.pending.remove(&index);
        let record = &mut self.records[index];
        record.status = SendStatus::Failed {
            failure,
            by: receipt.clone(),
        };
        if let Some(reply) = &record.intent.reply
            && inbox
                .waiters
                .get(reply.correlation)
                .is_some_and(|waiter| waiter.status == ReplyStatus::Pending)
        {
            inbox.invalidate();
            inbox.waiters.cancel(reply.correlation, receipt);
        }
        record.clone()
    }
}

impl DeliveryFailure {
    fn validate_recipient(&self, record: &SendRecord) -> io::Result<()> {
        let (observed, journal_id) = match self {
            Self::RecipientClosed { observed } => {
                (observed, record.intent.request.recipient.instance_id.0.0)
            }
            Self::RecipientDeleted { observed } => {
                (observed, record.intent.request.recipient.session_id.0)
            }
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
            return Ok(record.clone());
        }
        self.outbox.validate_failure(request_id, &failure)?;
        let receipt = self.journal.commit(&[FlowFact::DeliveryFailed {
            request_id,
            failure: failure.clone(),
        }])?;
        let record = self
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
            .and_then(|record| record.intent.request.scope.as_ref())
    }

    pub fn send_record(&self, request_id: Uuid) -> Option<&SendRecord> {
        self.outbox.get(request_id)
    }

    pub fn sends(&self) -> &[SendRecord] {
        self.outbox.records()
    }

    pub fn pending_sends(&self) -> impl Iterator<Item = &SendRecord> {
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
        let record = self.outbox.apply(intent, receipt.clone());
        self.cursor = receipt;
        Ok(record)
    }

    pub(crate) fn send_retry(
        &self,
        request: &DeliveryRequest,
        reply: Option<&ReplyWaiter>,
        authorization: SendAuthorization,
    ) -> io::Result<Option<SendRecord>> {
        if request.sender != SenderIdentity::Flow(self.descriptor.address.clone()) {
            return Err(invalid("send intent does not belong to this flow"));
        }
        if let Some(previous) = self.outbox.get(request.request_id) {
            if previous.intent.request != *request
                || previous.intent.reply.as_ref() != reply
                || previous.intent.authorization != authorization
            {
                return Err(invalid("send request ID was reused with different data"));
            }
            return Ok(Some(previous.clone()));
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
                    return Ok(record.clone());
                }
                return Err(invalid(
                    "delivery confirmation conflicts with the original receipt",
                ));
            }
        }
        self.outbox.validate_confirmation(request_id, &delivery)?;
        let receipt = self.journal.commit(&[FlowFact::DeliveryConfirmed {
            request_id,
            delivery: delivery.clone(),
        }])?;
        let record = self.outbox.confirm(request_id, delivery, receipt.clone());
        self.cursor = receipt;
        Ok(record)
    }
}
