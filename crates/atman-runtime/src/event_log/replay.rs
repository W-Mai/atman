use std::io::BufRead;
use std::path::Path;

#[cfg(test)]
use crate::event::Event;
use crate::event_log::reader::{
    ReplayRecord, context_snapshot_from_records, read_replay_records, scan_replay_records,
};
#[cfg(test)]
use crate::message::Message;
use crate::projection::context::{ContextReplay, replay_default_context};
use crate::projection::message_window::{
    FlowOwnership, TranscriptEntry, project_transcript_records,
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
    pub view: ContextReplay,
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
        Self::from_records(records, observer).map_err(|source| SessionOpenError::Replay {
            path: path.to_path_buf(),
            source,
        })
    }

    pub fn from_reader<R: BufRead>(
        reader: R,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> std::io::Result<ReplayBundle> {
        let records = scan_replay_records(reader)?;
        Self::from_records(records, observer)
    }

    fn from_records(
        records: Vec<ReplayRecord>,
        observer: Option<&mut dyn TranscriptReplayObserver>,
    ) -> std::io::Result<ReplayBundle> {
        let view = replay_default_context(records.iter().map(|record| &record.envelope))?;
        let ownership =
            FlowOwnership::from_events(records.iter().map(|record| &record.envelope.event));
        let context = context_snapshot_from_records(&records, &view.selection);
        let last_seq = records.last().map(|record| record.envelope.seq);
        if let Some(observer) = observer {
            for entry in project_transcript_records(&records, &ownership) {
                observer.observe(entry);
            }
        }
        let events = records.into_iter().map(|record| record.envelope).collect();
        Ok(ReplayBundle {
            last_seq,
            view,
            context,
            events,
        })
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

    #[test]
    fn invalid_head_lineage_fails_before_transcript_publication() {
        use crate::event::{ContextId, EventEnvelope, TurnId};
        let turn = TurnId::now();
        let initial = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn.clone(), "retained"),
            },
        );
        for scope in [None, Some(ContextId::now())] {
            let mut selection = EventEnvelope::new(
                2,
                Event::ContextHeadSelected {
                    turn_id: turn.clone(),
                },
            );
            selection.context_id = scope;
            let events = [initial.clone(), selection];
            let input = events
                .iter()
                .map(|event| serde_json::to_string(event).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            let mut observed = Vec::new();
            let error = SessionReplay::from_reader(
                input.as_bytes(),
                Some(&mut |entry| observed.push(entry)),
            )
            .err()
            .unwrap();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(observed.is_empty());
            assert!(crate::event_log::reader::context_snapshot_from_envelopes(&events).is_err());
        }
    }

    #[test]
    fn malformed_context_records_fail_before_transcript_publication() {
        use crate::event::{ContextId, EventEnvelope, TurnId};

        let turn = TurnId::now();
        let legacy = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn, "retained"),
            },
        );
        let head = serde_json::to_value(EventEnvelope::new(
            2,
            Event::ContextHeadSelected {
                turn_id: TurnId::now(),
            },
        ))
        .unwrap();
        let mut missing_turn = head.clone();
        missing_turn.as_object_mut().unwrap().remove("turn_id");
        let mut invalid_identity = head;
        invalid_identity["context_id"] = serde_json::json!("not-a-context-id");
        for malformed in [
            missing_turn,
            invalid_identity,
            serde_json::json!({
                "seq": 2, "type": "context_created", "context_id": ContextId::now(),
                "base": null,
            }),
        ] {
            let input = format!(
                "{}\n\n{malformed}\n",
                serde_json::to_string(&legacy).unwrap()
            );
            let mut observed = Vec::new();
            let error = SessionReplay::from_reader(
                input.as_bytes(),
                Some(&mut |entry| observed.push(entry)),
            )
            .err()
            .expect("invalid typed records must not disappear");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(error.to_string().contains("line 3"));
            assert!(observed.is_empty());
        }
    }
}
