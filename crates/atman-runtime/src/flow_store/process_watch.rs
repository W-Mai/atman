//! Durable observation of process resources owned by one flow instance.
//!
//! The host authenticates resource ownership and performs output matching. A report groups all matches for one source position before submitting it to the recipient owner. Process handles are not durable identities: every process lifetime has a distinct incarnation.

use std::{collections::BTreeMap, io};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::inbox::{
    AcceptedEnvelope, DeliveryMode, DeliveryRequest, Inbox, InboxPayload, SenderIdentity,
};
use super::{FlowFact, FlowRef, FlowStore, invalid};
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProcessSource {
    Bash { incarnation: Uuid, handle: String },
    Terminal { incarnation: Uuid, handle: String },
}

impl ProcessSource {
    /// Registry handle associated with this process incarnation.
    pub fn handle(&self) -> &str {
        match self {
            Self::Bash { handle, .. } | Self::Terminal { handle, .. } => handle,
        }
    }

    fn valid(&self) -> bool {
        let (incarnation, handle) = match self {
            Self::Bash {
                incarnation,
                handle,
            }
            | Self::Terminal {
                incarnation,
                handle,
            } => (incarnation, handle),
        };
        !incarnation.is_nil() && !handle.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessWatch {
    pub id: Uuid,
    pub source: ProcessSource,
    pub pattern: String,
    pub once: bool,
    /// Absolute expiry; absent means observation has no time limit.
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessWatchStatus {
    Active,
    Matched,
    Cancelled,
    Exited,
    SourceLost,
    HistoryGap,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessWatchRecord {
    pub watch: ProcessWatch,
    pub started_after: u64,
    pub registered_at: DateTime<Utc>,
    pub status: ProcessWatchStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessMatch {
    pub watcher: Uuid,
    /// Only the matched output, not a copy of the source's accumulated history.
    pub text: String,
    pub row: Option<u16>,
    pub col: Option<u16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ProcessSignal {
    Matches(Vec<ProcessMatch>),
    Exited,
    SourceLost,
    HistoryGap,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProcessReport {
    pub source: ProcessSource,
    pub position: u64,
    pub observed_at: DateTime<Utc>,
    pub signal: ProcessSignal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcessObservation {
    pub report: ProcessReport,
    pub updates: Vec<(Uuid, ProcessWatchStatus)>,
    pub envelope: Option<AcceptedEnvelope>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ProcessWatchExpiry {
    pub at: DateTime<Utc>,
    pub envelopes: Vec<AcceptedEnvelope>,
}

#[derive(Default)]
pub(super) struct ProcessWatches {
    records: BTreeMap<Uuid, ProcessWatchRecord>,
    reports: BTreeMap<ProcessSource, Vec<(ProcessObservation, CommitReceipt)>>,
}

impl ProcessWatches {
    pub(super) fn values(&self) -> impl Iterator<Item = &ProcessWatchRecord> {
        self.records.values()
    }
    pub fn contains(&self, id: Uuid) -> bool {
        self.records.contains_key(&id)
    }
    fn position(&self, source: &ProcessSource) -> u64 {
        self.reports
            .get(source)
            .and_then(|reports| reports.last())
            .map_or(0, |(observation, _)| observation.report.position)
    }

    pub fn register(&mut self, record: ProcessWatchRecord) -> io::Result<()> {
        self.validate_registration(&record)?;
        self.records.insert(record.watch.id, record);
        Ok(())
    }

    pub(super) fn validate_registration(&self, record: &ProcessWatchRecord) -> io::Result<()> {
        if record.watch.id.is_nil()
            || !record.watch.source.valid()
            || record.watch.pattern.is_empty()
            || record.status != ProcessWatchStatus::Active
            || self.records.contains_key(&record.watch.id)
            || record.started_after < self.position(&record.watch.source)
            || record
                .watch
                .expires_at
                .is_some_and(|expiry| expiry <= record.registered_at)
            || self
                .reports
                .get(&record.watch.source)
                .and_then(|reports| reports.last())
                .is_some_and(|(last, _)| {
                    matches!(
                        last.report.signal,
                        ProcessSignal::Exited | ProcessSignal::SourceLost
                    )
                })
        {
            return Err(invalid("invalid process watch registration"));
        }
        Ok(())
    }

    pub fn cancel(&mut self, id: Uuid) -> io::Result<()> {
        let record = self
            .records
            .get_mut(&id)
            .ok_or_else(|| invalid("process watch does not exist"))?;
        if record.status != ProcessWatchStatus::Active {
            return Err(invalid("process watch is no longer active"));
        }
        record.status = ProcessWatchStatus::Cancelled;
        Ok(())
    }

    pub fn close(&mut self) {
        for record in self
            .records
            .values_mut()
            .filter(|record| record.status == ProcessWatchStatus::Active)
        {
            record.status = ProcessWatchStatus::Cancelled;
        }
    }

    fn selection(&self, report: &ProcessReport) -> io::Result<Vec<(Uuid, ProcessWatchStatus)>> {
        if !report.source.valid()
            || report.position <= self.position(&report.source)
            || !self
                .records
                .values()
                .any(|record| record.watch.source == report.source)
            || self
                .reports
                .get(&report.source)
                .and_then(|reports| reports.last())
                .is_some_and(|(last, _)| {
                    matches!(
                        last.report.signal,
                        ProcessSignal::Exited | ProcessSignal::SourceLost
                    )
                })
        {
            return Err(invalid("stale process observation position"));
        }
        if !matches!(report.signal, ProcessSignal::Matches(_))
            && self.records.values().any(|record| {
                record.watch.source == report.source
                    && record.status == ProcessWatchStatus::Active
                    && record.started_after >= report.position
            })
        {
            return Err(invalid(
                "process termination precedes an active registration",
            ));
        }
        let matches = match &report.signal {
            ProcessSignal::Matches(matches) => {
                let mut ids = std::collections::HashSet::new();
                for item in matches {
                    let record = self
                        .records
                        .get(&item.watcher)
                        .ok_or_else(|| invalid("unknown process watcher in observation"))?;
                    if !ids.insert(item.watcher)
                        || record.watch.source != report.source
                        || !item.text.contains(&record.watch.pattern)
                    {
                        return Err(invalid(
                            "process match does not belong to its registered source and pattern",
                        ));
                    }
                }
                matches.as_slice()
            }
            _ => &[],
        };
        Ok(self
            .records
            .values()
            .filter_map(|record| {
                if record.watch.source != report.source
                    || record.status != ProcessWatchStatus::Active
                    || record.started_after >= report.position
                    || (matches!(report.signal, ProcessSignal::Matches(_))
                        && record
                            .watch
                            .expires_at
                            .is_some_and(|expiry| report.observed_at >= expiry))
                {
                    return None;
                }
                let status = match report.signal {
                    ProcessSignal::Matches(_)
                        if matches.iter().any(|item| item.watcher == record.watch.id) =>
                    {
                        if record.watch.once {
                            ProcessWatchStatus::Matched
                        } else {
                            ProcessWatchStatus::Active
                        }
                    }
                    ProcessSignal::Matches(_) => return None,
                    ProcessSignal::Exited => ProcessWatchStatus::Exited,
                    ProcessSignal::SourceLost => ProcessWatchStatus::SourceLost,
                    ProcessSignal::HistoryGap => ProcessWatchStatus::HistoryGap,
                };
                Some((record.watch.id, status))
            })
            .collect())
    }

    fn content(
        &self,
        report: &ProcessReport,
        updates: &[(Uuid, ProcessWatchStatus)],
    ) -> serde_json::Value {
        let matches = match &report.signal {
            ProcessSignal::Matches(matches) => matches
                .iter()
                .filter(|item| updates.iter().any(|(id, _)| id == &item.watcher))
                .cloned()
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        serde_json::json!({"source": report.source, "position": report.position, "observed_at": report.observed_at, "signal": match report.signal { ProcessSignal::Matches(_) => "matched", ProcessSignal::Exited => "exited", ProcessSignal::SourceLost => "source_lost", ProcessSignal::HistoryGap => "history_gap" }, "subscriptions": updates, "matches": matches})
    }

    pub fn validate(
        &self,
        observation: &ProcessObservation,
        inbox: &Inbox,
        recipient: &FlowRef,
    ) -> io::Result<()> {
        let updates = self.selection(&observation.report)?;
        if updates != observation.updates || observation.envelope.is_some() == updates.is_empty() {
            return Err(invalid("process observation has incomplete matches"));
        }
        if let Some(envelope) = &observation.envelope {
            let expected = request(
                recipient,
                observation_id(&observation.report)?,
                InboxPayload::ProcessEvent {
                    source: observation.report.source.clone(),
                    content: self.content(&observation.report, &updates),
                },
            );
            if envelope.request != expected {
                return Err(invalid(
                    "process observation envelope does not match its source",
                ));
            }
            inbox.validate(envelope)?;
        }
        Ok(())
    }

    pub fn apply(
        &mut self,
        observation: ProcessObservation,
        inbox: &mut Inbox,
        receipt: &CommitReceipt,
    ) {
        for (id, status) in &observation.updates {
            self.records
                .get_mut(id)
                .expect("validated process watch")
                .status = *status;
        }
        if let Some(envelope) = &observation.envelope {
            inbox.apply(envelope.clone(), receipt.clone());
        }
        self.reports
            .entry(observation.report.source.clone())
            .or_default()
            .push((observation, receipt.clone()));
    }

    fn expiry_requests(&self, recipient: &FlowRef, at: DateTime<Utc>) -> Vec<DeliveryRequest> {
        self.records.values().filter(|record| record.status == ProcessWatchStatus::Active).filter_map(|record| {
            let due_at = record.watch.expires_at.filter(|expiry| *expiry <= at)?;
            Some(request(recipient, record.watch.id, InboxPayload::Timer { timer: record.watch.id, due_at, purpose: super::inbox::TimerPurpose::WatchExpiry, content: serde_json::json!({"kind": "process_watch_expired", "source": record.watch.source, "pattern": record.watch.pattern}) }))
        }).collect()
    }

    pub fn validate_expiry(
        &self,
        expiry: &ProcessWatchExpiry,
        inbox: &Inbox,
        recipient: &FlowRef,
    ) -> io::Result<()> {
        let expected = self.expiry_requests(recipient, expiry.at);
        if expected.is_empty()
            || expected
                != expiry
                    .envelopes
                    .iter()
                    .map(|envelope| envelope.request.clone())
                    .collect::<Vec<_>>()
        {
            return Err(invalid("invalid process watch expiry batch"));
        }
        let mut identities = std::collections::HashSet::new();
        for envelope in &expiry.envelopes {
            if !identities.insert(envelope.id) {
                return Err(invalid("duplicate message identity in expiry batch"));
            }
            inbox.validate(envelope)?;
        }
        Ok(())
    }

    pub fn apply_expiry(
        &mut self,
        expiry: ProcessWatchExpiry,
        inbox: &mut Inbox,
        receipt: &CommitReceipt,
    ) {
        for envelope in expiry.envelopes {
            self.records
                .get_mut(&envelope.request.request_id)
                .expect("validated expiry")
                .status = ProcessWatchStatus::Expired;
            inbox.apply(envelope, receipt.clone());
        }
    }
}

fn request(recipient: &FlowRef, request_id: Uuid, payload: InboxPayload) -> DeliveryRequest {
    DeliveryRequest {
        request_id,
        sender: SenderIdentity::Host(recipient.instance_id.0.0),
        recipient: recipient.clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload,
    }
}

fn observation_id(report: &ProcessReport) -> io::Result<Uuid> {
    let mut digest = Sha256::new();
    digest.update(b"atman.process.observation");
    digest.update(serde_json::to_vec(&report.source).map_err(invalid)?);
    digest.update(report.position.to_be_bytes());
    Ok(Uuid::from_bytes(
        digest.finalize()[..16].try_into().expect("digest prefix"),
    ))
}

fn accepted(request: DeliveryRequest) -> AcceptedEnvelope {
    AcceptedEnvelope {
        id: Uuid::now_v7(),
        accepted_at: Utc::now(),
        request,
    }
}

impl FlowStore {
    pub fn process_watches(&self) -> impl Iterator<Item = &ProcessWatchRecord> {
        self.process_watches.records.values()
    }

    /// The host establishes its source stream and captures the source position before registering.
    pub fn register_process_watch(
        &mut self,
        watch: ProcessWatch,
        started_after: u64,
    ) -> io::Result<ProcessWatchRecord> {
        if let Some(record) = self.process_watches.records.get(&watch.id) {
            return if record.watch == watch {
                Ok(record.clone())
            } else {
                Err(invalid(
                    "process watch identity conflicts with recorded registration",
                ))
            };
        }
        if !self.state.accepts_input() {
            return Err(invalid("recipient cannot register process watches"));
        }
        if self.subscriptions.contains(watch.id) {
            return Err(invalid(
                "watcher identity is already assigned to a flow subscription",
            ));
        }
        let record = ProcessWatchRecord {
            watch,
            started_after,
            registered_at: Utc::now(),
            status: ProcessWatchStatus::Active,
        };
        self.process_watches.validate_registration(&record)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ProcessWatchRegistered(Box::new(record.clone()))])?;
        self.process_watches.register(record.clone())?;
        self.cursor = receipt;
        Ok(record)
    }

    pub fn unregister_process_watch(&mut self, id: Uuid) -> io::Result<CommitReceipt> {
        let record = self
            .process_watches
            .records
            .get(&id)
            .ok_or_else(|| invalid("process watch does not exist"))?;
        if record.status != ProcessWatchStatus::Active {
            return Ok(self.cursor.clone());
        }
        let receipt = self
            .journal
            .commit(&[FlowFact::ProcessWatchCancelled(id)])?;
        self.process_watches.cancel(id)?;
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Commit all matches for a source position with their single inbox envelope. Retried reports retain the original receipt; conflicting content at an existing position is rejected.
    pub fn observe_process(&mut self, report: ProcessReport) -> io::Result<CommitReceipt> {
        if let Some((previous, receipt)) = self
            .process_watches
            .reports
            .get(&report.source)
            .and_then(|reports| {
                reports
                    .binary_search_by_key(&report.position, |(previous, _)| {
                        previous.report.position
                    })
                    .ok()
                    .map(|index| &reports[index])
            })
        {
            return if previous.report == report {
                Ok(receipt.clone())
            } else {
                Err(invalid(
                    "process position conflicts with recorded observation",
                ))
            };
        }
        if !self.state.accepts_input() {
            return Err(invalid("recipient cannot observe process events"));
        }
        let updates = self.process_watches.selection(&report)?;
        let envelope = (!updates.is_empty())
            .then(|| {
                Ok::<_, io::Error>(accepted(request(
                    &self.descriptor.address,
                    observation_id(&report)?,
                    InboxPayload::ProcessEvent {
                        source: report.source.clone(),
                        content: self.process_watches.content(&report, &updates),
                    },
                )))
            })
            .transpose()?;
        let observation = ProcessObservation {
            report,
            updates,
            envelope,
        };
        self.process_watches
            .validate(&observation, &self.inbox, &self.descriptor.address)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ProcessObserved(Box::new(observation.clone()))])?;
        self.process_watches
            .apply(observation, &mut self.inbox, &receipt);
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// Expire outstanding registrations without resetting their deadlines or deleting accepted matches.
    pub fn expire_process_watches(&mut self, at: DateTime<Utc>) -> io::Result<CommitReceipt> {
        if !self.state.accepts_input() {
            return Err(invalid("recipient cannot expire process watches"));
        }
        let envelopes = self
            .process_watches
            .expiry_requests(&self.descriptor.address, at)
            .into_iter()
            .map(accepted)
            .collect::<Vec<_>>();
        if envelopes.is_empty() {
            return Ok(self.cursor.clone());
        }
        let expiry = ProcessWatchExpiry { at, envelopes };
        self.process_watches
            .validate_expiry(&expiry, &self.inbox, &self.descriptor.address)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ProcessWatchesExpired(Box::new(expiry.clone()))])?;
        self.process_watches
            .apply_expiry(expiry, &mut self.inbox, &receipt);
        self.cursor = receipt.clone();
        Ok(receipt)
    }

    /// A new process owner cannot recover old process effects. Preserve pending input and report lost sources once, using the recorded incarnation.
    pub(crate) fn lose_process_sources(&mut self) -> io::Result<()> {
        if !self.state.accepts_input() {
            return Ok(());
        }
        let at = Utc::now();
        self.expire_process_watches(at)?;
        let mut sources = BTreeMap::new();
        for record in self
            .process_watches
            .records
            .values()
            .filter(|record| record.status == ProcessWatchStatus::Active)
        {
            sources
                .entry(record.watch.source.clone())
                .and_modify(|position: &mut u64| *position = (*position).max(record.started_after))
                .or_insert(
                    record
                        .started_after
                        .max(self.process_watches.position(&record.watch.source)),
                );
        }
        for (source, position) in sources {
            self.observe_process(ProcessReport {
                source,
                position: position
                    .checked_add(1)
                    .ok_or_else(|| invalid("process position exhausted"))?,
                observed_at: at,
                signal: ProcessSignal::SourceLost,
            })?;
        }
        Ok(())
    }
}
