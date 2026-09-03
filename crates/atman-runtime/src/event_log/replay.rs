use std::collections::{HashMap, HashSet, VecDeque};
use std::io::BufRead;
use std::path::Path;

use crate::event::{Event, FlowRunId};
use crate::event_log::reader::{
    ReplayRecord, context_snapshot_from_records, read_replay_records, scan_replay_records,
};
use crate::message::Message;
use crate::projection::message_window::{
    TranscriptEntry, apply_attachment_degradation, apply_envelope_to_messages,
    message_belongs_to_root, project_transcript_records,
};
use crate::session::{ContextSnapshot, SessionOpenError};

pub trait TranscriptReplayObserver {
    fn observe(&mut self, entry: TranscriptEntry);
}

impl<F> TranscriptReplayObserver for F
where
    F: FnMut(TranscriptEntry),
{
    fn observe(&mut self, entry: TranscriptEntry) {
        self(entry);
    }
}

pub struct ReplayBundle {
    pub last_seq: Option<u64>,
    pub compacted_messages: Vec<(u64, Message)>,
    pub all_messages: Vec<(u64, Message)>,
    pub context: ContextSnapshot,
    pub events: Vec<crate::event::EventEnvelope>,
}

#[derive(Debug, Default)]
pub(crate) struct FlowOwnership {
    pub known: HashSet<FlowRunId>,
    pub spawned: HashSet<FlowRunId>,
}

impl FlowOwnership {
    fn from_records(records: &[ReplayRecord]) -> Self {
        let mut known = HashSet::new();
        let mut spawned = HashSet::new();
        let mut children = HashMap::<FlowRunId, Vec<FlowRunId>>::new();
        for record in records {
            let Event::FlowStart {
                run_id,
                parent_run_id,
                spawned: is_spawned,
                ..
            } = &record.envelope.event
            else {
                continue;
            };
            known.insert(run_id.clone());
            if let Some(parent) = parent_run_id {
                children
                    .entry(parent.clone())
                    .or_default()
                    .push(run_id.clone());
            }
            if *is_spawned {
                spawned.insert(run_id.clone());
            }
        }
        let mut queue = spawned.iter().cloned().collect::<VecDeque<_>>();
        while let Some(parent) = queue.pop_front() {
            let Some(descendants) = children.get(&parent) else {
                continue;
            };
            for descendant in descendants {
                if spawned.insert(descendant.clone()) {
                    queue.push_back(descendant.clone());
                }
            }
        }
        Self { known, spawned }
    }
}

pub struct SessionReplay;

impl SessionReplay {
    pub fn from_path(
        path: &Path,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> Result<ReplayBundle, SessionOpenError> {
        let records = read_replay_records(path)?;
        Ok(Self::from_records(records, observer))
    }

    pub fn from_reader<R: BufRead>(
        reader: R,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> std::io::Result<ReplayBundle> {
        let records = scan_replay_records(reader)?;
        Ok(Self::from_records(records, observer))
    }

    fn from_records(
        records: Vec<ReplayRecord>,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> ReplayBundle {
        let ownership = FlowOwnership::from_records(&records);
        let mut compacted_messages = Vec::new();
        let mut compacted_positions = HashMap::new();
        let mut all_messages = Vec::new();
        let mut all_positions = HashMap::new();
        for record in &records {
            apply_envelope_to_messages(
                &record.envelope,
                &ownership.spawned,
                &mut compacted_messages,
                &mut compacted_positions,
            );
            match &record.envelope.event {
                Event::UserMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | Event::AssistantMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | Event::ToolResultMsg {
                    message,
                    flow_run_id,
                    ..
                }
                | Event::SystemMsg {
                    message,
                    flow_run_id,
                    ..
                } if message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned) => {
                    all_positions.insert(record.envelope.seq, all_messages.len());
                    all_messages.push((record.envelope.seq, message.clone()));
                }
                Event::AttachmentDegraded {
                    message_seq,
                    part_index,
                    file_basename,
                    reason,
                    ..
                } => {
                    apply_attachment_degradation(
                        &mut all_messages,
                        &all_positions,
                        *message_seq,
                        *part_index,
                        file_basename,
                        reason,
                    );
                }
                _ => {}
            }
        }
        let context = context_snapshot_from_records(&records);
        let last_seq = records.last().map(|record| record.envelope.seq);
        if let Some(observer) = observer {
            for entry in project_transcript_records(&records, &ownership) {
                observer.observe(entry);
            }
        }
        let events = records.into_iter().map(|record| record.envelope).collect();
        ReplayBundle {
            last_seq,
            compacted_messages,
            all_messages,
            context,
            events,
        }
    }
}

pub fn transcript_from_envelopes(
    envelopes: &[crate::event::EventEnvelope],
) -> Vec<TranscriptEntry> {
    let records = envelopes
        .iter()
        .cloned()
        .map(|envelope| ReplayRecord {
            persisted_ts: Some(envelope.ts),
            envelope,
        })
        .collect::<Vec<_>>();
    let ownership = FlowOwnership::from_records(&records);
    project_transcript_records(&records, &ownership)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_parses_each_nonempty_line_once() {
        let event = crate::event::EventEnvelope::new(
            1,
            Event::TurnStart {
                turn_id: crate::event::TurnId::now(),
            },
        );
        let line = serde_json::to_string(&event).unwrap();
        let input = format!("{line}\nnot-json\n\n{line}\n");
        crate::event_log::reader::reset_parse_attempts();

        let bundle = SessionReplay::from_reader(input.as_bytes(), None).unwrap();

        assert_eq!(crate::event_log::reader::parse_attempts(), 3);
        assert_eq!(bundle.last_seq, Some(1));
    }
}
