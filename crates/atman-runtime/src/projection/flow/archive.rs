//! Display projection for already partitioned, immutable legacy archives.

use std::io;

use super::{FlowRecord, FlowTranscript};
use crate::{
    TranscriptEntry,
    event::{Event, EventEnvelope},
    event_log::{reader::ReplayRecord, replay::FlowOwnership},
    flow_store::{history::LegacyHistoryRecord, view::FlowContentSummary},
    projection::message_window::project_transcript_records,
};

pub struct ArchiveTranscript {
    pub entries: Vec<TranscriptEntry>,
    /// Records which cannot participate in typed replay, retained for inspection by source line.
    pub unparsed: Vec<LegacyHistoryRecord>,
}

impl FlowTranscript {
    /// Reuse legacy presentation semantics after durable ownership partitioning. Spawned runs in this archive belong to this view; they are not hidden again as nested agents. Missing timestamps stay absent, and unknown records are retained separately.
    pub fn archived_transcript(&self) -> io::Result<ArchiveTranscript> {
        let Some(FlowContentSummary::History(archive)) = self.content() else {
            return Err(io::Error::other("transcript is not an archive"));
        };
        if self.entries().len() as u64 != archive.records {
            return Err(io::Error::other("archive is incomplete"));
        }
        let mut records = Vec::new();
        let mut unparsed = Vec::new();
        let mut ownership = FlowOwnership::default();
        for entry in self.entries() {
            let FlowRecord::Legacy(record) = &entry.record else {
                return Err(io::Error::other("archive contains execution records"));
            };
            let bytes = record.bytes()?;
            let parsed = record
                .source
                .sequence
                .zip(serde_json::from_slice::<Event>(&bytes).ok());
            let Some((seq, mut event)) = parsed else {
                unparsed.push((**record).clone());
                continue;
            };
            if let Event::FlowStart {
                run_id, spawned, ..
            } = &mut event
            {
                ownership.known.insert(run_id.clone());
                // Raw lineage remains unchanged in the retained record.
                *spawned = false;
            }
            records.push(ReplayRecord {
                envelope: EventEnvelope {
                    seq,
                    // The display reducer reads persisted_ts, never this required wire field.
                    ts: record
                        .source
                        .timestamp
                        .unwrap_or(chrono::DateTime::UNIX_EPOCH),
                    event,
                },
                persisted_ts: record.source.timestamp,
            });
        }
        Ok(ArchiveTranscript {
            entries: project_transcript_records(&records, &ownership),
            unparsed,
        })
    }
}
