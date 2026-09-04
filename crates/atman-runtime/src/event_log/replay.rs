use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use crate::event::Event;
use crate::event_log::reader::{
    ReplayRecord, context_snapshot_from_records, read_replay_records, scan_replay_records,
};
use crate::message::Message;
use crate::projection::message_window::{
    FlowOwnership, TranscriptEntry, apply_attachment_degradation, apply_envelope_to_messages,
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
    /// Digest of the last root checkpoint before subsequent message mutations.
    pub checkpoint_epoch: Option<String>,
    pub compacted_messages: Vec<(u64, Message)>,
    pub all_messages: Vec<(u64, Message)>,
    pub context: ContextSnapshot,
    pub events: Vec<crate::event::EventEnvelope>,
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
        let ownership =
            FlowOwnership::from_events(records.iter().map(|record| &record.envelope.event));
        let mut compacted_messages = Vec::new();
        let mut compacted_positions = HashMap::new();
        let mut all_messages = Vec::new();
        let mut all_positions = HashMap::new();
        let mut checkpoint = None;
        for record in &records {
            if record.envelope.context_id.is_some() {
                continue;
            }
            if let Event::Checkpoint {
                flow_run_id,
                messages,
                ..
            } = &record.envelope.event
                && message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned)
            {
                checkpoint = Some(messages.as_slice());
            }
            apply_envelope_to_messages(
                &record.envelope,
                &ownership.spawned,
                &mut compacted_messages,
                &mut compacted_positions,
            );
            if let Some((message, flow_run_id)) = record.envelope.event.context_message()
                && message_belongs_to_root(flow_run_id, &ownership.spawned)
            {
                all_positions.insert(record.envelope.seq, all_messages.len());
                all_messages.push((
                    record.envelope.seq,
                    message.replayed(record.envelope.seq, None),
                ));
            }
            if let Event::AttachmentDegraded {
                flow_run_id, patch, ..
            } = &record.envelope.event
                && message_belongs_to_root(flow_run_id.as_ref(), &ownership.spawned)
            {
                apply_attachment_degradation(&mut all_messages, &all_positions, patch);
            }
        }
        let context = context_snapshot_from_records(&records);
        let last_seq = records.last().map(|record| record.envelope.seq);
        if let Some(observer) = observer {
            for entry in project_transcript_records(&records, &ownership) {
                observer.observe(entry);
            }
        }
        let checkpoint_epoch = checkpoint.map(crate::context_state::checkpoint_epoch_digest);
        let events = records.into_iter().map(|record| record.envelope).collect();
        ReplayBundle {
            last_seq,
            checkpoint_epoch,
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
    let ownership = FlowOwnership::from_events(records.iter().map(|record| &record.envelope.event));
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
