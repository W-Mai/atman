//! Durable deadlines whose delivery and terminal status share one journal commit.

use std::{collections::BTreeMap, io};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::inbox::{
    AcceptedEnvelope, DeliveryMode, DeliveryRequest, Inbox, InboxPayload, SenderIdentity,
};
use super::{CommitReceipt, FlowFact, FlowRef, FlowStore, invalid};

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Timer {
    pub id: Uuid,
    pub due_at: DateTime<Utc>,
    pub content: serde_json::Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerStatus {
    Scheduled,
    Fired,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TimerRecord {
    pub timer: Timer,
    pub scheduled_at: DateTime<Utc>,
    pub status: TimerStatus,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TimerDelivery {
    pub at: DateTime<Utc>,
    pub envelopes: Vec<AcceptedEnvelope>,
}

#[derive(Default)]
pub(super) struct Timers {
    records: BTreeMap<Uuid, TimerRecord>,
}

impl Timers {
    pub fn records(&self) -> impl Iterator<Item = &TimerRecord> {
        self.records.values()
    }

    pub fn register(&mut self, record: TimerRecord) -> io::Result<()> {
        if record.timer.id.is_nil()
            || record.status != TimerStatus::Scheduled
            || self.records.contains_key(&record.timer.id)
        {
            return Err(invalid("invalid timer registration"));
        }
        self.records.insert(record.timer.id, record);
        Ok(())
    }

    pub fn cancel(&mut self, id: Uuid) -> io::Result<()> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| invalid("unknown timer"))?;
        if record.status != TimerStatus::Scheduled {
            return Err(invalid("timer is no longer scheduled"));
        }
        record.status = TimerStatus::Cancelled;
        Ok(())
    }

    pub fn close(&mut self) {
        for record in self.records.values_mut() {
            if record.status == TimerStatus::Scheduled {
                record.status = TimerStatus::Cancelled;
            }
        }
    }

    fn due(&self, at: DateTime<Utc>) -> Vec<&Timer> {
        let mut timers: Vec<_> = self
            .records
            .values()
            .filter(|record| record.status == TimerStatus::Scheduled && record.timer.due_at <= at)
            .map(|record| &record.timer)
            .collect();
        timers.sort_by_key(|timer| (timer.due_at, timer.id));
        timers
    }

    pub fn validate_delivery(
        &self,
        delivery: &TimerDelivery,
        inbox: &Inbox,
        address: &FlowRef,
    ) -> io::Result<()> {
        let due = self.due(delivery.at);
        if due.is_empty() || due.len() != delivery.envelopes.len() {
            return Err(invalid("timer delivery does not match due registrations"));
        }
        let mut ids = std::collections::HashSet::new();
        for (timer, envelope) in due.into_iter().zip(&delivery.envelopes) {
            if envelope.request != request(timer, address)
                || envelope.accepted_at != delivery.at
                || !ids.insert(envelope.id)
            {
                return Err(invalid("timer delivery conflicts with registration"));
            }
            inbox.validate(envelope)?;
        }
        Ok(())
    }

    pub fn apply_delivery(
        &mut self,
        delivery: TimerDelivery,
        inbox: &mut Inbox,
        receipt: &CommitReceipt,
    ) {
        for envelope in delivery.envelopes {
            let InboxPayload::Timer { timer, .. } = &envelope.request.payload else {
                unreachable!("validated timer payload")
            };
            self.records.get_mut(timer).expect("validated timer").status = TimerStatus::Fired;
            inbox.apply(envelope, receipt.clone());
        }
    }
}

fn request(timer: &Timer, address: &FlowRef) -> DeliveryRequest {
    let mut digest = Sha256::new();
    digest.update(b"atman.timer.delivery");
    digest.update(address.session_id.0.as_bytes());
    digest.update(address.instance_id.0.0.as_bytes());
    digest.update(timer.id.as_bytes());
    DeliveryRequest {
        request_id: Uuid::from_bytes(digest.finalize()[..16].try_into().expect("digest prefix")),
        sender: SenderIdentity::Host(address.instance_id.0.0),
        recipient: address.clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Timer {
            timer: timer.id,
            due_at: timer.due_at,
            purpose: super::inbox::TimerPurpose::Notification,
            content: timer.content.clone(),
        },
    }
}

impl FlowStore {
    pub fn timers(&self) -> impl Iterator<Item = &TimerRecord> {
        self.timers.records()
    }

    pub fn next_timer_deadline(&self) -> Option<DateTime<Utc>> {
        self.timers()
            .filter(|record| record.status == TimerStatus::Scheduled)
            .map(|record| record.timer.due_at)
            .min()
    }

    /// Retries preserve the recorded status, including delivery or cancellation. Identity reuse with different content is rejected.
    pub fn schedule_timer(&mut self, timer: Timer) -> io::Result<TimerRecord> {
        if let Some(record) = self.timers.records.get(&timer.id) {
            return if record.timer == timer {
                Ok(record.clone())
            } else {
                Err(invalid(
                    "timer identity conflicts with recorded registration",
                ))
            };
        }
        if !self.state.accepts_input() || timer.id.is_nil() {
            return Err(invalid("flow cannot schedule timer"));
        }
        let record = TimerRecord {
            timer,
            scheduled_at: Utc::now(),
            status: TimerStatus::Scheduled,
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::TimerScheduled(Box::new(record.clone()))])?;
        self.timers.register(record.clone())?;
        self.cursor = receipt;
        Ok(record)
    }

    /// Cancellation prevents future delivery; it never retracts an already accepted notification.
    pub fn cancel_timer(&mut self, id: Uuid) -> io::Result<CommitReceipt> {
        let record = self
            .timers
            .records
            .get(&id)
            .ok_or_else(|| invalid("unknown timer"))?;
        if record.status != TimerStatus::Scheduled {
            return Ok(self.cursor.clone());
        }
        let receipt = self.journal.commit(&[FlowFact::TimerCancelled(id)])?;
        self.timers.cancel(id)?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Persist due input even while execution is paused. Reading or reopening storage never fires timers.
    pub fn fire_due_timers(&mut self, at: DateTime<Utc>) -> io::Result<CommitReceipt> {
        if !self.state.accepts_input() {
            return Err(invalid("flow cannot receive timer notifications"));
        }
        let envelopes: Vec<_> = self
            .timers
            .due(at)
            .into_iter()
            .map(|timer| AcceptedEnvelope {
                id: Uuid::now_v7(),
                accepted_at: at,
                request: request(timer, &self.descriptor.address),
            })
            .collect();
        if envelopes.is_empty() {
            return Ok(self.cursor.clone());
        }
        let delivery = TimerDelivery { at, envelopes };
        self.timers
            .validate_delivery(&delivery, &self.inbox, &self.descriptor.address)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::TimersFired(Box::new(delivery.clone()))])?;
        self.timers
            .apply_delivery(delivery, &mut self.inbox, &receipt);
        self.cursor = receipt.clone();
        Ok(receipt)
    }
}
