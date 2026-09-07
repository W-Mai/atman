use std::io::BufRead;
use std::path::Path;

#[cfg(test)]
use crate::event::Event;
use crate::event_log::reader::{
    ReplayRecord, context_snapshot_from_records, context_snapshot_from_selected,
    read_replay_records, scan_replay_records,
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
    pub fn from_indexed_checkpoint(
        path: &Path,
        index: &crate::index::AnchorIndex,
        session_id: &str,
    ) -> Result<Option<ReplayBundle>, SessionOpenError> {
        let Some(coverage) =
            index
                .recover_timeline_coverage(session_id, path)
                .map_err(|source| SessionOpenError::Replay {
                    path: path.to_path_buf(),
                    source: std::io::Error::other(source.to_string()),
                })?
        else {
            return Ok(None);
        };
        let head = index
            .read_events_before_descending(
                session_id,
                None,
                1,
                crate::index::EventFilter::Kinds(&["context_head_selected"]),
            )
            .map_err(|source| SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: std::io::Error::other(source.to_string()),
            })?
            .into_iter()
            .next()
            .map(|row| serde_json::from_str::<crate::event::EventEnvelope>(&row.payload))
            .transpose()
            .map_err(|source| SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
            })?;
        let context_id = head.and_then(|event| event.context_id);
        let mut before = None;
        let checkpoint = loop {
            let rows = index
                .read_events_before_descending(
                    session_id,
                    before,
                    64,
                    crate::index::EventFilter::Kinds(&["checkpoint"]),
                )
                .map_err(|source| SessionOpenError::Replay {
                    path: path.to_path_buf(),
                    source: std::io::Error::other(source.to_string()),
                })?;
            if rows.is_empty() {
                break None;
            }
            before = rows.last().map(|row| row.seq);
            let mut found = None;
            for row in rows {
                let envelope = serde_json::from_str::<crate::event::EventEnvelope>(&row.payload)
                    .map_err(|source| SessionOpenError::Replay {
                        path: path.to_path_buf(),
                        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
                    })?;
                let root_checkpoint = matches!(
                    &envelope.event,
                    crate::event::Event::Checkpoint { flow_run_id, .. }
                        if envelope.context_id.is_some() || flow_run_id.is_none()
                );
                if envelope.context_id == context_id && root_checkpoint {
                    found = Some(envelope);
                    break;
                }
            }
            if found.is_some() {
                break found;
            }
        };
        let Some(checkpoint) = checkpoint else {
            return Ok(None);
        };
        if !index
            .has_contiguous_event_range(session_id, checkpoint.seq, coverage.seq)
            .map_err(|source| SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: std::io::Error::other(source.to_string()),
            })?
        {
            return Ok(None);
        }
        let rows = index
            .read_events_from_seq(session_id, checkpoint.seq)
            .map_err(|source| SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: std::io::Error::other(source.to_string()),
            })?;
        let events = rows
            .into_iter()
            .map(|row| {
                serde_json::from_str::<crate::event::EventEnvelope>(&row.payload).map_err(
                    |source| SessionOpenError::Replay {
                        path: path.to_path_buf(),
                        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
                    },
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        if events.last().map(|event| event.seq) != Some(coverage.seq) {
            return Ok(None);
        }
        let view = crate::projection::context::replay_checkpoint_suffix(&events, context_id)
            .map_err(|source| SessionOpenError::Replay {
                path: path.to_path_buf(),
                source,
            })?;
        let context = context_snapshot_from_selected(events.iter(), &view.selection);
        Ok(Some(ReplayBundle {
            last_seq: Some(coverage.seq),
            view,
            context,
            events,
        }))
    }

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
    fn indexed_checkpoint_replay_reads_only_the_checkpoint_suffix() {
        use crate::event::{EventEnvelope, TurnId};
        use crate::index::{AnchorIndex, EventLogBoundary, ProjectEventInsert};

        let turn = TurnId::now();
        let events = [
            EventEnvelope::new(
                1,
                Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: Message::user_text(turn.clone(), "old raw message"),
                },
            ),
            EventEnvelope::new(
                2,
                Event::Checkpoint {
                    session_id: "session".into(),
                    flow_run_id: None,
                    messages: vec![Message::assistant_text(turn.clone(), "checkpoint window")],
                    window_tokens: 4,
                },
            ),
            EventEnvelope::new(
                3,
                Event::UserMsg {
                    turn_id: turn.clone(),
                    flow_run_id: None,
                    message: Message::user_text(turn.clone(), "latest suffix"),
                },
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let index = AnchorIndex::open_project(dir.path()).unwrap();
        let mut log = Vec::new();
        for (event, kind) in events.iter().zip(["user_msg", "checkpoint", "user_msg"]) {
            let payload = serde_json::to_string(event).unwrap();
            let line_start = log.len() as u64;
            log.extend_from_slice(payload.as_bytes());
            let line_end = log.len() as u64;
            log.push(b'\n');
            let line_digest = blake3::hash(payload.as_bytes()).to_hex().to_string();
            index
                .insert_project_event_at_boundary(
                    ProjectEventInsert {
                        session_id: "session",
                        seq: event.seq as i64,
                        ts: &event.ts.to_rfc3339(),
                        kind,
                        turn_id: Some(&turn.to_string()),
                        flow_run_id: None,
                        text_content: "",
                        payload_json: &payload,
                    },
                    EventLogBoundary {
                        line_start,
                        line_end,
                        log_offset: log.len() as u64,
                        line_digest: &line_digest,
                    },
                )
                .unwrap();
        }
        std::fs::write(&path, log).unwrap();
        crate::event_log::reader::reset_parse_attempts();

        let replay = SessionReplay::from_indexed_checkpoint(&path, &index, "session")
            .unwrap()
            .unwrap();

        assert_eq!(
            replay
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert_eq!(
            replay
                .view
                .window()
                .iter()
                .map(|(_, message)| message.text_concat())
                .collect::<Vec<_>>(),
            ["checkpoint window", "latest suffix"]
        );
        assert_eq!(crate::event_log::reader::parse_attempts(), 0);
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
