//! Original, branch-visible byte checkpoints from the background process writer.

use std::{collections::HashSet, path::PathBuf};

use atman_rt::{RunId, TurnId, TurnLease};

use crate::{
    event::Event,
    flow_store::{FlowTransition, trace::ExecutionEvent},
    retained_file::CopyControl,
    tools::bash_bg::BgHandle,
};

use super::*;

mod historical;
use historical::{Claim, HistoricalLogs, RESULT_READ_BUDGET};

const MAX_CHECKPOINTS: usize = 1024;
const MAX_PAGE_LOGS: usize = 64;

#[derive(Clone, Debug)]
pub struct ConversationLogSource {
    pub receipt: CommitReceipt,
    pub record: FlowRecordId,
    pub lease: TurnLease<AtmanUuid>,
    pub handle: String,
    pub bytes: Option<u64>,
    pub drained: Option<bool>,
}

#[derive(PartialEq, Eq)]
pub struct ConversationLogFile {
    pub(crate) path: PathBuf,
    pub(crate) checkpoint: Option<ConversationLogCheckpoint>,
}

#[derive(PartialEq, Eq)]
pub(crate) struct ConversationLogCheckpoint {
    pub(crate) bytes: usize,
    pub(crate) digest: blake3::Hash,
}

pub(super) struct WorkLogIndex {
    flow: FlowRef,
    session: String,
    through: u64,
    max_turns: usize,
    starts: Vec<TurnStart>,
    checkpoints: VecDeque<ConversationLogSource>,
    historical: HistoricalLogs,
    directory: PathBuf,
    truncated: bool,
}

type TurnStart = (TurnId<AtmanUuid>, RunId<AtmanUuid>, u64);

impl WorkLogIndex {
    pub(super) fn new(flow: FlowRef, directory: PathBuf, through: u64, max_turns: usize) -> Self {
        Self {
            session: flow.session_id.to_string(),
            flow,
            through,
            max_turns,
            starts: Vec::new(),
            checkpoints: VecDeque::new(),
            historical: HistoricalLogs::default(),
            directory,
            truncated: false,
        }
    }

    pub(super) fn retained_bytes(&self) -> usize {
        self.starts.capacity() * std::mem::size_of::<TurnStart>()
            + self.checkpoints.capacity() * std::mem::size_of::<ConversationLogSource>()
            + self
                .checkpoints
                .iter()
                .map(|entry| entry.handle.capacity() + entry.receipt.digest.capacity())
                .sum::<usize>()
            + self.historical.retained_bytes()
    }

    pub(super) fn observe(
        &mut self,
        receipt: &CommitReceipt,
        fact: usize,
        value: &FlowFact,
    ) -> io::Result<()> {
        if receipt.sequence > self.through {
            return Ok(());
        }
        match value {
            FlowFact::Transition(FlowTransition::TurnStarted { turn, run }) => {
                if self.starts.len() == self.max_turns {
                    return Err(io::Error::other(ConversationCapacity {
                        kind: ConversationCapacityKind::RetainedIndex,
                        required_at_least: self.max_turns + 1,
                        allowed: self.max_turns,
                        restart_required: true,
                    }));
                }
                self.starts
                    .push((turn.clone(), run.clone(), receipt.sequence));
            }
            FlowFact::ExecutionEvents(events) => {
                for (item, event) in events.iter().enumerate() {
                    if let Some(lease) = event
                        .scope
                        .lease()
                        .filter(|lease| self.admission(lease).is_some())
                    {
                        self.historical
                            .observe_node(lease, receipt.sequence, &event.event);
                    }
                    let Some((lease, handle, bytes, _, drained)) = checkpoint(event) else {
                        continue;
                    };
                    if self.admission(lease).is_none()
                        || BgHandle::managed_log_name(handle, &self.session).is_none()
                    {
                        continue;
                    }
                    if self.checkpoints.len() == MAX_CHECKPOINTS {
                        self.truncated = true;
                        self.checkpoints.pop_front();
                    }
                    self.checkpoints.push_back(ConversationLogSource {
                        receipt: receipt.clone(),
                        record: FlowRecordId {
                            commit: receipt.sequence,
                            fact,
                            item,
                        },
                        lease: lease.clone(),
                        handle: handle.to_owned(),
                        bytes: Some(bytes),
                        drained: Some(drained),
                    });
                }
            }
            FlowFact::MessagePublished(publication)
                if self.admission(&publication.lease).is_some() =>
            {
                self.historical.observe_publication(
                    receipt,
                    fact,
                    &self.directory,
                    &self.session,
                    publication,
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn admission(&self, lease: &TurnLease<AtmanUuid>) -> Option<u64> {
        let (turn, run, sequence) = self
            .starts
            .get(usize::try_from(lease.epoch.checked_sub(1)?).ok()?)?;
        (lease.flow == self.flow && lease.turn == *turn && lease.run == *run).then_some(*sequence)
    }
}

type Checkpoint<'a> = (&'a TurnLease<AtmanUuid>, &'a str, u64, &'a [u8; 32], bool);

fn checkpoint(event: &ExecutionEvent) -> Option<Checkpoint<'_>> {
    let Event::BashLogCheckpoint {
        handle,
        bytes,
        digest,
        drained,
        ..
    } = &event.event
    else {
        return None;
    };
    Some((event.scope.lease()?, handle, *bytes, digest, *drained))
}

impl PreparedConversationSource {
    pub(crate) fn has_work_log_index(&self) -> bool {
        self.markers.work_logs.is_some()
    }

    pub(super) fn work_logs(
        &self,
        items: &[ConversationItem],
        control: &CopyControl,
    ) -> (Vec<ConversationLogSource>, bool) {
        let Some(index) = &self.markers.work_logs else {
            return (Vec::new(), false);
        };
        let leases = items
            .iter()
            .filter_map(|item| item.work.as_ref().map(|work| &work.lease))
            .collect::<Vec<_>>();
        let mut handles = HashSet::new();
        let mut matches = index.checkpoints.iter().rev().filter(|entry| {
            self.visible(entry.record.commit)
                && index
                    .admission(&entry.lease)
                    .is_some_and(|sequence| self.visible(sequence))
                && leases.contains(&&entry.lease)
                && handles.insert(entry.handle.as_str())
        });
        let mut entries = matches
            .by_ref()
            .take(MAX_PAGE_LOGS)
            .cloned()
            .collect::<Vec<_>>();
        let mut truncated = matches.next().is_some()
            || ((index.truncated || index.historical.truncated) && !leases.is_empty());
        let mut historical_handles = handles
            .into_iter()
            .map(str::to_owned)
            .collect::<HashSet<_>>();
        let mut budget = RESULT_READ_BUDGET;
        for claim in index.historical.entries().rev() {
            if !leases.contains(&&claim.lease) || !self.owns_historical_log(index, claim) {
                continue;
            }
            match claim.resolve(&self.directory, &index.session, &mut budget, control) {
                Ok(Some((handle, _))) if historical_handles.insert(handle.clone()) => {
                    if entries.len() == MAX_PAGE_LOGS {
                        truncated = true;
                        break;
                    }
                    entries.push(claim.project(handle));
                }
                Ok(_) => {}
                Err(_) => truncated = true,
            }
        }
        entries.sort_by_key(|entry| (entry.record.commit, entry.record.fact, entry.record.item));
        (entries, truncated)
    }

    fn owns_historical_log(&self, index: &WorkLogIndex, claim: &Claim) -> bool {
        self.visible(claim.record.commit)
            && index
                .admission(&claim.lease)
                .is_some_and(|sequence| self.visible(sequence))
            && index
                .historical
                .owns(claim, |sequence| self.visible(sequence))
    }

    pub(crate) fn read_log_source(
        &mut self,
        record: FlowRecordId,
        receipt: &CommitReceipt,
        control: &CopyControl,
    ) -> io::Result<Result<ConversationLogFile, ConversationCapacity>> {
        if record.commit != receipt.sequence || self.reader.receipt_at(record.commit)? != *receipt {
            return Err(missing_record());
        }
        let facts = match self.read_visible_batch(record.commit)? {
            Ok(facts) => facts,
            Err(capacity) => return Ok(Err(capacity)),
        };
        let index = self.markers.work_logs.as_ref().ok_or_else(missing_record)?;
        let events = match facts.get(record.fact) {
            Some(FlowFact::ExecutionEvents(events)) => events,
            Some(FlowFact::MessagePublished(_)) => {
                let claim = index
                    .historical
                    .entries()
                    .find(|claim| claim.record == record)
                    .filter(|claim| self.owns_historical_log(index, claim))
                    .ok_or_else(missing_record)?;
                let (_, path) = claim
                    .resolve(
                        &self.directory,
                        &index.session,
                        &mut { RESULT_READ_BUDGET },
                        control,
                    )?
                    .ok_or_else(missing_record)?;
                return Ok(Ok(ConversationLogFile {
                    path,
                    checkpoint: None,
                }));
            }
            _ => return Err(missing_record()),
        };
        let (lease, handle, bytes, digest, _) = events
            .get(record.item)
            .and_then(checkpoint)
            .ok_or_else(missing_record)?;
        index
            .admission(lease)
            .filter(|sequence| *sequence <= record.commit && self.visible(*sequence))
            .ok_or_else(missing_record)?;
        let name = BgHandle::managed_log_name(handle, &index.session).ok_or_else(missing_record)?;
        Ok(Ok(ConversationLogFile {
            path: self.directory.join(name),
            checkpoint: Some(ConversationLogCheckpoint {
                bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
                digest: blake3::Hash::from_bytes(*digest),
            }),
        }))
    }
}
