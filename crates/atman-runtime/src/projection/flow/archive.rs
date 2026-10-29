//! Shared display projection for original legacy logs and imported immutable archives.

use std::{collections::BTreeMap, fs::File, io, path::Path};

use uuid::Uuid;

use super::{FlowRecord, FlowTranscript};
use crate::{
    TranscriptEntry,
    event::{Event, EventEnvelope},
    event_log::{
        legacy::{self, LegacyFlowKey, LegacyInspection, LegacySource},
        reader::ReplayRecord,
        replay::FlowOwnership,
    },
    flow_store::{history::LegacyHistoryRecord, view::FlowContentSummary},
    projection::message_window::{MessageProjection, project_transcript_records},
};

pub struct ArchiveTranscript {
    pub entries: Vec<TranscriptEntry>,
    /// The retained model window, including committed compaction and attachment degradation.
    pub context: Vec<crate::message::Message>,
    pub outputs: crate::tools::tool_output::OutputStore,
    /// Records which cannot participate in typed replay, retained for inspection by source line.
    pub unparsed: Vec<LegacyHistoryRecord>,
}

/// A validated directory of the original legacy log. Discovery and reading never migrate data or acquire execution ownership.
pub struct LegacyHistory {
    inspection: LegacyInspection,
}

/// Immutable display snapshot with a source identity, not an executable instance address or journal cursor.
pub struct LegacyFlowView {
    session_id: Uuid,
    source: LegacySource,
    owner: LegacyFlowKey,
    records: u64,
    transcript: ArchiveTranscript,
}

impl LegacyHistory {
    pub fn inspect(path: &Path, session_id: Uuid) -> io::Result<Self> {
        Ok(Self {
            inspection: legacy::inspect(path, session_id)?,
        })
    }

    pub fn inspection(&self) -> &LegacyInspection {
        &self.inspection
    }

    pub fn flows(&self) -> Vec<LegacyFlowKey> {
        self.inspection.grouped().into_keys().collect()
    }

    /// Read one ownership partition. A changed source invalidates the directory; callers must explicitly inspect again before publishing a new snapshot.
    pub fn read_flow(&self, owner: &LegacyFlowKey) -> io::Result<LegacyFlowView> {
        let groups = self.inspection.grouped();
        let records = groups.get(owner).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "legacy flow is not in this source")
        })?;
        self.inspection.source.verify()?;
        let mut file = File::open(&self.inspection.source.path)?;
        let mut transcript = project_records(records.iter().map(|record| {
            Ok(LegacyHistoryRecord::new(
                (*record).clone(),
                &record.read(&mut file, self.inspection.source.bytes)?,
            ))
        }))?;
        self.inspection.source.verify()?;
        transcript.outputs = crate::tools::tool_output::OutputStore::at(
            self.inspection
                .source
                .path
                .parent()
                .ok_or_else(|| io::Error::other("legacy source has no directory"))?,
        );
        Ok(LegacyFlowView {
            session_id: self.inspection.session_id,
            source: self.inspection.source.clone(),
            owner: owner.clone(),
            records: records.len() as u64,
            transcript,
        })
    }
}

impl LegacyFlowView {
    pub fn into_transcript(self) -> ArchiveTranscript {
        self.transcript
    }
    pub fn continuation_source(&self) -> crate::flow_store::history::LegacyArchive {
        crate::flow_store::history::LegacyArchive {
            session_id: self.session_id,
            source: self.source.clone(),
            owner: self.owner.clone(),
            records: self.records,
        }
    }
    pub fn session_id(&self) -> Uuid {
        self.session_id
    }
    pub fn source(&self) -> &LegacySource {
        &self.source
    }
    pub fn owner(&self) -> &LegacyFlowKey {
        &self.owner
    }
    pub fn transcript(&self) -> &ArchiveTranscript {
        &self.transcript
    }
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
        project_records(self.entries().iter().map(|entry| {
            let FlowRecord::Legacy(record) = &entry.record else {
                return Err(io::Error::other("archive contains execution records"));
            };
            Ok((**record).clone())
        }))
    }
}

fn project_records(
    records: impl IntoIterator<Item = io::Result<LegacyHistoryRecord>>,
) -> io::Result<ArchiveTranscript> {
    let mut projected = Vec::new();
    let mut unparsed = Vec::new();
    let mut ownership = FlowOwnership::default();
    let mut outputs = BTreeMap::new();
    for record in records {
        let record = record?;
        let source = &record.source;
        let parsed = source.sequence.zip(record.project_event()?);
        for (id, content) in &record.outputs {
            if outputs
                .insert(id.clone(), content.clone())
                .is_some_and(|previous| previous != *content)
            {
                return Err(io::Error::other("conflicting retained output contents"));
            }
        }
        let Some((seq, mut event)) = parsed else {
            unparsed.push(record);
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
        projected.push(ReplayRecord {
            envelope: EventEnvelope {
                seq,
                // The display reducer reads persisted_ts, never this required wire field.
                ts: source.timestamp.unwrap_or(chrono::DateTime::UNIX_EPOCH),
                event,
            },
            persisted_ts: source.timestamp,
        });
    }
    let entries = project_transcript_records(&projected, &ownership);
    let envelopes = projected
        .into_iter()
        .map(|record| record.envelope)
        .collect::<Vec<_>>();
    Ok(ArchiveTranscript {
        entries,
        context: envelopes.as_slice().to_messages(),
        outputs: crate::tools::tool_output::OutputStore::retained(outputs)
            .map_err(io::Error::other)?,
        unparsed,
    })
}
