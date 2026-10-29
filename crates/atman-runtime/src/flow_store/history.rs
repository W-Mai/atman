//! Retained legacy records in the same journal and view stream as executable instances.

use std::{collections::BTreeMap, io};

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{FlowContent, FlowFact, FlowStore, invalid};
use crate::event_log::{
    journal::CommitReceipt,
    legacy::{LegacyFlowKey, LegacyRecord, LegacySource},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyArchive {
    pub session_id: Uuid,
    pub source: LegacySource,
    pub owner: LegacyFlowKey,
    pub records: u64,
}

impl LegacyArchive {
    /// Stable executable continuation identity for one historical ownership partition.
    pub fn continuation_id(session: Uuid, owner: &LegacyFlowKey) -> io::Result<Uuid> {
        let mut hash = Sha256::new();
        hash.update(b"atman-history-continuation\0");
        hash.update(serde_json::to_vec(&(session, owner)).map_err(invalid)?);
        let bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("digest prefix");
        Ok(uuid::Builder::from_custom_bytes(bytes).into_uuid())
    }

    pub(super) fn validate(&self) -> io::Result<()> {
        if !self.source.path.is_absolute()
            || self.source.sha256.len() != 64
            || !self.source.sha256.bytes().all(|c| c.is_ascii_hexdigit())
            || self.records > self.source.bytes
        {
            return Err(invalid("invalid legacy archive source identity"));
        }
        Ok(())
    }

    pub fn title(&self) -> &str {
        match &self.owner {
            LegacyFlowKey::Default => "Default flow history",
            LegacyFlowKey::Agent { owner } => owner,
            LegacyFlowKey::SpawnedRun { run_id } => run_id,
            LegacyFlowKey::Unassigned => "Unassigned history",
        }
    }
}

/// Exact original bytes, including malformed UTF-8/JSON and line terminators.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyHistoryRecord {
    pub source: LegacyRecord,
    pub raw_base64: String,
    /// Managed image contents in base64, keyed by the original artifact content ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub images: BTreeMap<String, String>,
    /// Complete registered tool outputs referenced by this record.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, String>,
}

impl LegacyHistoryRecord {
    pub fn new(source: LegacyRecord, bytes: &[u8]) -> Self {
        Self {
            source,
            raw_base64: STANDARD.encode(bytes),
            images: BTreeMap::new(),
            outputs: BTreeMap::new(),
        }
    }

    pub fn bytes(&self) -> io::Result<Vec<u8>> {
        STANDARD.decode(&self.raw_base64).map_err(invalid)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HistoryProgress {
    pub records: u64,
    pub message_count: usize,
    pub last_line: u64,
    pub end_offset: u64,
}

impl HistoryProgress {
    pub(super) fn advance(
        &self,
        archive: &LegacyArchive,
        records: &[LegacyHistoryRecord],
    ) -> io::Result<Self> {
        if records.is_empty() {
            return Err(invalid("legacy history batch is empty"));
        }
        let mut next = self.clone();
        for record in records {
            let source = &record.source;
            let bytes = record.bytes()?;
            let end = source
                .offset
                .checked_add(source.bytes)
                .ok_or_else(|| invalid("legacy source range overflow"))?;
            if source.owner.key() != archive.owner
                || source.line <= next.last_line
                || source.offset < next.end_offset
                || source.bytes == 0
                || source.bytes != bytes.len() as u64
                || end > archive.source.bytes
                || source.sha256 != format!("{:x}", Sha256::digest(&bytes))
                || next.records >= archive.records
            {
                return Err(invalid(
                    "legacy record has invalid ownership, order, range, or digest",
                ));
            }
            next.records += 1;
            if matches!(
                record.project_event()?,
                Some(
                    crate::event::Event::UserMsg { .. }
                        | crate::event::Event::DeferredFormApplied { .. }
                        | crate::event::Event::AssistantMsg { .. }
                        | crate::event::Event::ToolResultMsg { .. }
                )
            ) {
                next.message_count += 1;
            }
            next.last_line = source.line;
            next.end_offset = end;
        }
        Ok(next)
    }
}

impl FlowStore {
    pub fn history_progress(&self) -> &HistoryProgress {
        &self.history
    }

    /// Append a verified migration batch; execution facts cannot enter this journal.
    pub fn append_legacy_history(
        &mut self,
        records: Vec<LegacyHistoryRecord>,
    ) -> io::Result<CommitReceipt> {
        let FlowContent::History(archive) = &self.descriptor.content else {
            return Err(invalid("instance does not contain legacy history"));
        };
        let next = self.history.advance(archive, &records)?;
        let receipt = self.journal.commit(&[FlowFact::LegacyRecords(records)])?;
        self.history = next;
        self.cursor = receipt.clone();
        self.refresh_projection();
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{FlowJournal, FlowTransition};

    #[test]
    fn journal_content_boundary_rejects_execution_in_archives_and_history_in_entries() {
        for fact in [
            FlowFact::Transition(FlowTransition::Activated),
            FlowFact::ExecutionEvents(Vec::new()),
        ] {
            assert!(FlowJournal::validate(true, &[fact]).is_err());
        }
        assert!(FlowJournal::validate(false, &[FlowFact::LegacyRecords(Vec::new())]).is_err());
    }
}
