//! Lossless source indexing and conservative ownership inspection of legacy event logs.

mod ownership;

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::event::Event;
use ownership::{Lineage, Subject};

/// Content identity of the entire source, including whitespace and invalid records.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LegacySource {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

impl LegacySource {
    /// Verify the current source bytes. This does not lock or stop an old executor.
    pub fn verify(&self) -> io::Result<()> {
        let mut file = open_regular(&self.path)?;
        if file.metadata()?.len() != self.bytes {
            return Err(source_changed());
        }
        let mut hash = Sha256::new();
        let bytes = io::copy(&mut (&mut file).take(self.bytes), &mut hash)?;
        if bytes != self.bytes
            || file.metadata()?.len() != self.bytes
            || digest(hash) != self.sha256
        {
            return Err(source_changed());
        }
        Ok(())
    }
}

/// Legacy identities are mapping keys, never executable instance addresses.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyOwner {
    Default,
    Agent { owner: String },
    SpawnedRun { run_id: String },
    Unassigned { reason: UnassignedReason },
}

/// Stable grouping for retained history; unassigned reasons remain on individual records.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LegacyFlowKey {
    Default,
    Agent { owner: String },
    SpawnedRun { run_id: String },
    Unassigned,
}

impl LegacyOwner {
    pub fn key(&self) -> LegacyFlowKey {
        match self {
            Self::Default => LegacyFlowKey::Default,
            Self::Agent { owner } => LegacyFlowKey::Agent {
                owner: owner.clone(),
            },
            Self::SpawnedRun { run_id } => LegacyFlowKey::SpawnedRun {
                run_id: run_id.clone(),
            },
            Self::Unassigned { .. } => LegacyFlowKey::Unassigned,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnassignedReason {
    InvalidRecord,
    Unscoped,
    UnknownRun,
    ConflictingRun,
    ConflictingOwner,
    CyclicLineage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordIssue {
    InvalidJson,
    UnsupportedEvent,
    MissingSequence,
    InvalidSequence,
    MissingTimestamp,
    InvalidTimestamp,
    UnterminatedLine,
}

/// A physical nonempty line. Ranges include its original line terminator when present.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct LegacyRecord {
    pub line: u64,
    pub offset: u64,
    pub bytes: u64,
    pub sha256: String,
    pub sequence: Option<u64>,
    pub timestamp: Option<DateTime<Utc>>,
    pub event_type: Option<String>,
    pub owner: LegacyOwner,
    pub issues: Vec<RecordIssue>,
}

impl LegacyRecord {
    /// Read one indexed physical record and reject changed content before publishing it.
    pub(crate) fn read(
        &self,
        file: &mut (impl Read + Seek),
        source_bytes: u64,
    ) -> io::Result<Vec<u8>> {
        if self.bytes == 0
            || self
                .offset
                .checked_add(self.bytes)
                .is_none_or(|end| end > source_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid legacy record range",
            ));
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = vec![0; usize::try_from(self.bytes).map_err(io::Error::other)?];
        file.read_exact(&mut bytes)?;
        if format!("{:x}", Sha256::digest(&bytes)) != self.sha256 {
            return Err(source_changed());
        }
        Ok(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyRun {
    pub run_id: String,
    pub owner: LegacyOwner,
}

/// Read-only mapping. No source records are published, rewritten, or activated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyInspection {
    pub session_id: Uuid,
    pub source: LegacySource,
    pub blank_lines: u64,
    pub records: Vec<LegacyRecord>,
    pub runs: Vec<LegacyRun>,
}

impl LegacyInspection {
    pub(crate) fn grouped(&self) -> BTreeMap<LegacyFlowKey, Vec<&LegacyRecord>> {
        // Empty sessions retain a selectable default history without an executable identity.
        let mut groups = BTreeMap::from([(LegacyFlowKey::Default, Vec::new())]);
        for record in &self.records {
            groups.entry(record.owner.key()).or_default().push(record);
        }
        groups
    }
}

/// Index original bytes without replay's permissive skipping or generated timestamps.
///
/// Inspection retains metadata and byte ranges rather than all message bodies. The source is checked again before returning; a migration writer must verify it again and establish exclusive execution ownership before switching catalogs.
pub fn inspect(path: &Path, session_id: Uuid) -> io::Result<LegacyInspection> {
    let path = path.canonicalize()?;
    let file = open_regular(&path)?;
    let source_bytes = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(source_bytes));
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut line = 0;
    let mut blank_lines = 0;
    let mut buffer = Vec::new();
    let mut records = Vec::new();
    let mut subjects = Vec::new();
    let mut lineage = Lineage::default();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        line += 1;
        let offset = bytes;
        bytes += buffer.len() as u64;
        hash.update(&buffer);
        if buffer.iter().all(u8::is_ascii_whitespace) {
            blank_lines += 1;
            continue;
        }
        let mut record = LegacyRecord {
            line,
            offset,
            bytes: buffer.len() as u64,
            sha256: digest(Sha256::new_with_prefix(&buffer)),
            sequence: None,
            timestamp: None,
            event_type: None,
            owner: LegacyOwner::Unassigned {
                reason: UnassignedReason::InvalidRecord,
            },
            issues: Vec::new(),
        };
        if !buffer.ends_with(b"\n") {
            record.issues.push(RecordIssue::UnterminatedLine);
        }
        let subject = match serde_json::from_slice::<serde_json::Value>(&buffer) {
            Ok(value) => {
                record.sequence = value.get("seq").and_then(|v| v.as_u64());
                if record.sequence.is_none() {
                    record.issues.push(if value.get("seq").is_some() {
                        RecordIssue::InvalidSequence
                    } else {
                        RecordIssue::MissingSequence
                    });
                }
                record.timestamp = value
                    .get("ts")
                    .and_then(|v| v.as_str())
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.with_timezone(&Utc));
                if record.timestamp.is_none() {
                    record.issues.push(if value.get("ts").is_some() {
                        RecordIssue::InvalidTimestamp
                    } else {
                        RecordIssue::MissingTimestamp
                    });
                }
                let event_type = value
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned);
                match serde_json::from_value::<Event>(value) {
                    Ok(event) => {
                        // Only known event tags are printed; arbitrary invalid payloads stay in the source.
                        record.event_type = event_type;
                        lineage.observe(&event);
                        Subject::of(&event)
                    }
                    Err(_) => {
                        record.issues.push(RecordIssue::UnsupportedEvent);
                        Subject::Invalid
                    }
                }
            }
            Err(_) => {
                record.issues.push(RecordIssue::InvalidJson);
                Subject::Invalid
            }
        };
        lineage.reference(&subject);
        records.push(record);
        subjects.push(subject);
    }
    if bytes != source_bytes {
        return Err(source_changed());
    }
    let session_owner = session_id.to_string();
    let resolved = lineage.resolve(&session_owner);
    for (record, subject) in records.iter_mut().zip(subjects) {
        record.owner = subject.owner(&session_owner, &resolved);
    }
    let source = LegacySource {
        path,
        bytes,
        sha256: digest(hash),
    };
    source.verify()?;
    Ok(LegacyInspection {
        session_id,
        source,
        blank_lines,
        records,
        runs: resolved
            .into_iter()
            .map(|(run_id, owner)| LegacyRun { run_id, owner })
            .collect(),
    })
}

fn open_regular(path: &Path) -> io::Result<File> {
    if !path.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "legacy source must be a regular file",
        ));
    }
    File::open(path)
}

fn digest(hash: Sha256) -> String {
    format!("{:x}", hash.finalize())
}

fn source_changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "legacy event source changed; inspect it again",
    )
}
