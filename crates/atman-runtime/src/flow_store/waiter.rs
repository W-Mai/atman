//! Correlated reply ownership in the flow's existing inbox.

use std::collections::{HashMap, HashSet};
use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::inbox::{AcceptedEnvelope, DeliveryMode, SenderIdentity};
use super::{FlowFact, FlowStore, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;

/// Register before sending the request. Correlation identities cannot be reused within an instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplyWaiter {
    pub correlation: Uuid,
    pub lease: TurnLease<AtmanUuid>,
    pub sender: SenderIdentity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReplyStatus {
    Pending,
    Delivered { message_id: Uuid, by: CommitReceipt },
    Cancelled { by: CommitReceipt },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReplyRecord {
    pub waiter: ReplyWaiter,
    pub registered: CommitReceipt,
    pub status: ReplyStatus,
}

#[derive(Default)]
pub(super) struct ReplyWaiters {
    records: Vec<ReplyRecord>,
    indices: HashMap<Uuid, usize>,
    pending: HashSet<Uuid>,
}

impl ReplyWaiters {
    pub fn records(&self) -> &[ReplyRecord] {
        &self.records
    }

    pub fn get(&self, correlation: Uuid) -> Option<&ReplyRecord> {
        self.indices
            .get(&correlation)
            .map(|&index| &self.records[index])
    }

    pub fn matches(&self, correlation: Uuid, envelope: &AcceptedEnvelope) -> bool {
        self.get(correlation).is_some_and(|record| {
            record.status == ReplyStatus::Pending
                && envelope.request.correlation == Some(correlation)
                && envelope.request.sender == record.waiter.sender
                && envelope.request.delivery == DeliveryMode::AtBoundary
        })
    }

    pub fn holds(&self, envelope: &AcceptedEnvelope) -> bool {
        envelope
            .request
            .correlation
            .is_some_and(|id| self.matches(id, envelope))
    }

    pub fn register(&mut self, waiter: ReplyWaiter, receipt: CommitReceipt) {
        self.pending.insert(waiter.correlation);
        self.indices.insert(waiter.correlation, self.records.len());
        self.records.push(ReplyRecord {
            waiter,
            registered: receipt,
            status: ReplyStatus::Pending,
        });
    }

    pub fn cancel(&mut self, correlation: Uuid, receipt: CommitReceipt) {
        self.pending.remove(&correlation);
        self.records[self.indices[&correlation]].status = ReplyStatus::Cancelled { by: receipt };
    }

    pub fn deliver(&mut self, correlation: Uuid, message_id: Uuid, receipt: CommitReceipt) {
        self.pending.remove(&correlation);
        self.records[self.indices[&correlation]].status = ReplyStatus::Delivered {
            message_id,
            by: receipt,
        };
    }

    pub fn state_changed(&mut self, state: &FlowInstance<AtmanUuid>, receipt: &CommitReceipt) {
        let live = match state.state() {
            InstanceState::Open(InstanceActivity::Active(active))
                if matches!(active.phase, TurnPhase::Running | TurnPhase::Waiting(_)) =>
            {
                Some(&active.lease)
            }
            _ => None,
        };
        self.pending.retain(|correlation| {
            let record = &mut self.records[self.indices[correlation]];
            if live != Some(&record.waiter.lease) {
                record.status = ReplyStatus::Cancelled {
                    by: receipt.clone(),
                };
                false
            } else {
                true
            }
        });
    }
}

impl FlowStore {
    pub fn reply_waiters(&self) -> &[ReplyRecord] {
        self.inbox.waiters.records()
    }

    /// Persist the receiver binding before making the corresponding request externally visible.
    pub fn register_reply_waiter(&mut self, waiter: ReplyWaiter) -> io::Result<ReplyRecord> {
        if let Some(record) = self.inbox.waiters.get(waiter.correlation) {
            if record.waiter != waiter {
                return Err(invalid("reply correlation was reused with different data"));
            }
            return Ok(record.clone());
        }
        self.inbox.validate_waiter(&self.state, &waiter)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ReplyWaiterRegistered(Box::new(waiter.clone()))])?;
        self.inbox.invalidate();
        self.inbox.waiters.register(waiter.clone(), receipt.clone());
        self.cursor = receipt;
        Ok(self
            .inbox
            .waiters
            .get(waiter.correlation)
            .expect("registered waiter")
            .clone())
    }

    /// Cancel a wait without discarding an accepted reply or releasing any execution pause.
    pub fn cancel_reply_waiter(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        correlation: Uuid,
    ) -> io::Result<ReplyRecord> {
        let record = self
            .inbox
            .waiters
            .get(correlation)
            .ok_or_else(|| invalid("reply waiter does not exist"))?;
        if &record.waiter.lease != lease {
            return Err(invalid("reply waiter belongs to another turn"));
        }
        if record.status != ReplyStatus::Pending {
            return Ok(record.clone());
        }
        let receipt = self.journal.commit(&[FlowFact::ReplyWaiterCancelled {
            lease: lease.clone(),
            correlation,
        }])?;
        self.inbox.invalidate();
        self.inbox.waiters.cancel(correlation, receipt.clone());
        self.cursor = receipt;
        Ok(self
            .inbox
            .waiters
            .get(correlation)
            .expect("cancelled waiter")
            .clone())
    }
}
