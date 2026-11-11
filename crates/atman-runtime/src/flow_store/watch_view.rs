//! Recorded watcher registrations and delivery progress at one journal boundary.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowFact,
    inbox::{Inbox, InboxDisposition, InboxPayload, InboxSummary},
    process_watch::{ProcessWatchRecord, ProcessWatches},
    subscription::{SubscriptionPurpose, SubscriptionRecord, Subscriptions},
};

/// Only observation facts establish watcher delivery provenance. Generic inbox
/// messages with similar JSON cannot impersonate an observed match.
#[derive(Default)]
pub(super) struct WatchDeliveries(BTreeMap<Uuid, (Vec<Uuid>, String, bool)>);

impl WatchDeliveries {
    pub(super) fn observe(&mut self, fact: &FlowFact) -> std::io::Result<()> {
        match fact {
            FlowFact::EventsObserved(observation) => {
                for envelope in &observation.envelopes {
                    if let InboxPayload::FlowEvent { content, event, .. } =
                        &envelope.request.payload
                    {
                        self.0.insert(
                            envelope.id,
                            (
                                super::subscription::watcher_matches(content)?,
                                event.clone(),
                                true,
                            ),
                        );
                    }
                }
            }
            FlowFact::ProcessObserved(observation) => {
                for envelope in &observation.envelopes {
                    use super::process_watch::ProcessSignal;
                    let InboxPayload::ProcessEvent { content, .. } = &envelope.request.payload
                    else {
                        return Err(super::invalid("process observation has no process payload"));
                    };
                    let updates: Vec<(Uuid, super::process_watch::ProcessWatchStatus)> =
                        serde_json::from_value(content["subscriptions"].clone())
                            .map_err(super::invalid)?;
                    let event = match observation.report.signal {
                        ProcessSignal::Matches(_) => "matched",
                        ProcessSignal::Exited => "exited",
                        ProcessSignal::SourceLost => "source_lost",
                        ProcessSignal::HistoryGap => "history_gap",
                    };
                    self.0.insert(
                        envelope.id,
                        (
                            updates.iter().map(|(id, _)| *id).collect(),
                            event.into(),
                            event == "matched",
                        ),
                    );
                }
            }
            FlowFact::FlowWatchesExpired(expiry) => self.expiry(&expiry.envelopes),
            FlowFact::ProcessWatchesExpired(expiry) => self.expiry(&expiry.envelopes),
            _ => {}
        }
        Ok(())
    }

    fn expiry(&mut self, envelopes: &[super::inbox::AcceptedEnvelope]) {
        for envelope in envelopes {
            if let InboxPayload::Timer { timer, .. } = envelope.request.payload {
                self.0
                    .insert(envelope.id, (vec![timer], "expired".into(), false));
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "record", rename_all = "snake_case")]
pub enum WatchRegistration {
    Flow(SubscriptionRecord),
    Process(ProcessWatchRecord),
}

impl WatchRegistration {
    pub fn id(&self) -> Uuid {
        match self {
            Self::Flow(record) => record.subscription.id,
            Self::Process(record) => record.watch.id,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WatchDelivery {
    pub message_id: Uuid,
    pub accepted_at: DateTime<Utc>,
    pub event: String,
    pub disposition: InboxDisposition,
}

/// Input commitment is not proof that an invocation finished processing a notification.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WatchProgress {
    pub matched: usize,
    pub pending: usize,
    pub input_committed: usize,
    pub cancelled: usize,
    pub last_delivery: Option<WatchDelivery>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WatchRecord {
    pub registration: WatchRegistration,
    pub progress: WatchProgress,
}

/// Build from validated replay state; no writer, source access, or queue reservation is needed.
pub(super) fn project(
    flows: &Subscriptions,
    processes: &ProcessWatches,
    inbox: &Inbox,
    deliveries: &WatchDeliveries,
) -> Vec<WatchRecord> {
    let mut records: BTreeMap<_, _> = flows
        .values()
        .filter(|record| record.subscription.purpose == SubscriptionPurpose::Watcher)
        .cloned()
        .map(WatchRegistration::Flow)
        .chain(processes.values().cloned().map(WatchRegistration::Process))
        .map(|registration| {
            (
                registration.id(),
                WatchRecord {
                    registration,
                    progress: WatchProgress::default(),
                },
            )
        })
        .collect();
    for entry in inbox.entries() {
        if let Some((ids, event, matched)) = deliveries.0.get(&entry.envelope.id) {
            for id in ids {
                record_delivery(&mut records, *id, entry, event, *matched);
            }
        }
    }
    records.into_values().collect()
}

fn record_delivery(
    records: &mut BTreeMap<Uuid, WatchRecord>,
    id: Uuid,
    entry: &InboxSummary,
    event: &str,
    matched: bool,
) {
    let Some(record) = records.get_mut(&id) else {
        return;
    };
    let progress = &mut record.progress;
    progress.matched += usize::from(matched);
    match entry.acceptance.disposition {
        InboxDisposition::Pending => progress.pending += 1,
        InboxDisposition::InputCommitted { .. } => progress.input_committed += 1,
        InboxDisposition::Cancelled { .. } => progress.cancelled += 1,
    }
    // Journal order, rather than wall-clock order, determines the latest delivery.
    progress.last_delivery = Some(WatchDelivery {
        message_id: entry.envelope.id,
        accepted_at: entry.envelope.accepted_at,
        event: event.into(),
        disposition: entry.acceptance.disposition.clone(),
    });
}
