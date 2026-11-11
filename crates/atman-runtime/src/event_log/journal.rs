//! Durable batch commits for instance and session facts.
//!
//! This blocking store belongs on the host's storage worker. A successful commit acknowledges the whole batch after synchronization. An I/O failure makes the handle unusable until reopened or reconciled under its existing lock: a complete but unacknowledged batch may still exist, so callers reconcile stable request IDs after replay. Live display frames do not need this durability boundary.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::file_lock::FileLock;

const FORMAT: &str = "atman-fact-journal";
const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    format: String,
    version: u32,
    id: Uuid,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    sequence: u64,
    previous: String,
    // Preserve the exact serialized payload for integrity checks across codec changes.
    payload: String,
    digest: String,
}

/// Position of one durably committed batch, scoped to its journal identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommitReceipt {
    pub journal_id: Uuid,
    pub sequence: u64,
    pub digest: String,
}

/// One complete committed batch. Individual facts do not have independent acknowledgement positions.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CommittedBatch<T> {
    pub receipt: CommitReceipt,
    pub facts: Vec<T>,
}

/// A bounded window from one journal. An empty page preserves the supplied cursor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct JournalPage<T> {
    pub batches: Vec<CommittedBatch<T>>,
    pub next: Option<CommitReceipt>,
    pub has_more: bool,
}

#[derive(Clone)]
struct BatchPosition {
    offset: u64,
    length: usize,
    digest: String,
}

/// Integrity-checked on-disk prefix. Trailing bytes are left untouched, and the cursor does not certify a concurrently active writer's acknowledgement.
#[derive(Clone, Debug)]
pub struct JournalInspection {
    pub cursor: Option<CommitReceipt>,
    pub valid_bytes: u64,
    pub trailing_bytes: u64,
}

/// A validated historical prefix with an independent read handle and compact batch positions.
pub struct JournalReader {
    file: File,
    id: Uuid,
    batches: Vec<BatchPosition>,
}

/// A bounded set of writer-acknowledged batches, read independently of the writer's seek position.
pub(crate) struct CapturedJournal {
    file: File,
    batches: Vec<(CommitReceipt, BatchPosition)>,
    pub(crate) truncated: bool,
}

impl CapturedJournal {
    pub(crate) fn len(&self) -> usize {
        self.batches.len()
    }

    pub(crate) fn read<T: DeserializeOwned>(&self, index: usize) -> io::Result<CommittedBatch<T>> {
        let (receipt, position) = self
            .batches
            .get(index)
            .ok_or_else(|| invalid("captured journal batch is unavailable"))?;
        let mut bytes = vec![0; position.length];
        let mut read = 0;
        while read < bytes.len() {
            #[cfg(unix)]
            let count = std::os::unix::fs::FileExt::read_at(
                &self.file,
                &mut bytes[read..],
                position.offset + read as u64,
            );
            #[cfg(windows)]
            let count = std::os::windows::fs::FileExt::seek_read(
                &self.file,
                &mut bytes[read..],
                position.offset + read as u64,
            );
            #[cfg(not(any(unix, windows)))]
            let count: io::Result<usize> = Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "positioned journal reads are unavailable on this platform",
            ));
            match count {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        decode_batch(&bytes, receipt.journal_id, receipt.sequence, position)
    }
}

impl JournalReader {
    /// Read one inspected batch without rescanning history or observing later appends.
    pub fn read_batch<T: DeserializeOwned>(
        &mut self,
        receipt: &CommitReceipt,
    ) -> io::Result<CommittedBatch<T>> {
        let index = receipt_index(self.id, &self.batches, receipt)?;
        read_batch(&mut self.file, self.id, index, &self.batches[index])
    }
}

fn receipt_index(
    id: Uuid,
    batches: &[BatchPosition],
    receipt: &CommitReceipt,
) -> io::Result<usize> {
    let index = usize::try_from(receipt.sequence)
        .ok()
        .and_then(|sequence| sequence.checked_sub(1))
        .filter(|&index| {
            receipt.journal_id == id
                && batches
                    .get(index)
                    .is_some_and(|batch| batch.digest == receipt.digest)
        });
    index.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "journal cursor is not a committed position in this history",
        )
    })
}

fn read_batch<T: DeserializeOwned>(
    file: &mut File,
    id: Uuid,
    index: usize,
    position: &BatchPosition,
) -> io::Result<CommittedBatch<T>> {
    file.seek(SeekFrom::Start(position.offset))?;
    let mut bytes = vec![0; position.length];
    file.read_exact(&mut bytes)?;
    decode_batch(&bytes, id, index as u64 + 1, position)
}

fn decode_batch<T: DeserializeOwned>(
    bytes: &[u8],
    id: Uuid,
    sequence: u64,
    position: &BatchPosition,
) -> io::Result<CommittedBatch<T>> {
    let record: Record = serde_json::from_slice(bytes).map_err(invalid)?;
    if !bytes.ends_with(b"\n")
        || record.sequence != sequence
        || record.digest != position.digest
        || record.digest != batch_digest(sequence, &record.previous, &record.payload)
    {
        return Err(invalid("journal page integrity check failed"));
    }
    let facts: Vec<T> = serde_json::from_str(&record.payload).map_err(invalid)?;
    if facts.is_empty() {
        return Err(invalid("empty journal batch"));
    }
    Ok(CommittedBatch {
        receipt: CommitReceipt {
            journal_id: id,
            sequence,
            digest: record.digest,
        },
        facts,
    })
}

struct ScannedJournal {
    sequence: u64,
    digest: String,
    offset: u64,
    batches: Vec<BatchPosition>,
    tail: Option<Vec<u8>>,
}

fn scan<T: DeserializeOwned>(
    file: &mut File,
    expected_id: Uuid,
    apply: &mut impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
) -> io::Result<ScannedJournal> {
    let length = file.metadata()?.len();
    let mut reader = BufReader::new(file.take(length));
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(invalid("incomplete journal header"));
    }
    let header: Header = serde_json::from_slice(&line).map_err(invalid)?;
    if header.format != FORMAT || header.version != VERSION || header.id != expected_id {
        return Err(invalid("journal format, version, or identity mismatch"));
    }
    let mut digest = hex_digest(&line);
    let mut offset = line.len() as u64;
    let mut sequence = 0_u64;
    let mut batches = Vec::new();
    let tail = loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break None;
        }
        if !line.ends_with(b"\n") {
            break Some(line);
        }
        let record: Record = serde_json::from_slice(&line).map_err(invalid)?;
        let next = sequence
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        if record.sequence != next
            || record.previous != digest
            || record.digest != batch_digest(next, &digest, &record.payload)
        {
            return Err(invalid("journal batch integrity check failed"));
        }
        let events: Vec<T> = serde_json::from_str(&record.payload).map_err(invalid)?;
        if events.is_empty() {
            return Err(invalid("empty journal batch"));
        }
        apply(
            CommitReceipt {
                journal_id: expected_id,
                sequence: next,
                digest: record.digest.clone(),
            },
            events,
        )?;
        sequence = next;
        digest = record.digest;
        batches.push(BatchPosition {
            offset,
            length: bytes,
            digest: digest.clone(),
        });
        offset = offset
            .checked_add(bytes as u64)
            .ok_or_else(|| invalid("journal offset exhausted"))?;
    };
    if reader.get_ref().limit() != 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "journal shrank during inspection",
        ));
    }
    Ok(ScannedJournal {
        sequence,
        digest,
        offset,
        batches,
        tail,
    })
}

/// An incomplete final record retained before truncating the journal to its committed prefix.
#[derive(Debug)]
pub struct RecoveredTail {
    pub path: PathBuf,
    pub offset: u64,
    pub bytes: u64,
}

/// A single writer with an exclusive OS file lock and monotonic commit sequence.
pub struct Journal {
    file: FileLock,
    id: Uuid,
    sequence: u64,
    digest: String,
    poisoned: bool,
    end: u64,
    batches: Vec<BatchPosition>,
}

fn complete_replay(
    file: &mut File,
    path: &Path,
    offset: u64,
    tail: Option<Vec<u8>>,
) -> io::Result<Option<RecoveredTail>> {
    let recovered = if let Some(tail) = tail {
        let retained = retain_tail(path, offset, &tail)?;
        file.set_len(offset)?;
        file.sync_all()?;
        Some(retained)
    } else {
        None
    };
    // A failed sync may have left a complete, unacknowledged batch.
    file.sync_all()?;
    sync_parent(path)?;
    file.seek(SeekFrom::Start(offset))?;
    Ok(recovered)
}

impl Journal {
    /// Return the actual preceding commit after validating the complete writer acknowledgement.
    pub(crate) fn previous_receipt(
        &self,
        receipt: &CommitReceipt,
    ) -> io::Result<Option<CommitReceipt>> {
        if self.poisoned {
            return Err(invalid("journal owner is unavailable after a failed write"));
        }
        let index = receipt_index(self.id, &self.batches, receipt)?;
        Ok(index.checked_sub(1).map(|previous| CommitReceipt {
            journal_id: self.id,
            sequence: receipt.sequence - 1,
            digest: self.batches[previous].digest.clone(),
        }))
    }

    /// Capture only a bounded prefix of the supplied batch selection. No payload is read here.
    pub(crate) fn capture_batches(
        &self,
        through: &CommitReceipt,
        receipts: &[CommitReceipt],
        max_bytes: usize,
    ) -> io::Result<CapturedJournal> {
        if self.poisoned {
            return Err(invalid("journal owner is unavailable after a failed write"));
        }
        receipt_index(self.id, &self.batches, through)?;
        for receipt in receipts {
            receipt_index(self.id, &self.batches, receipt)?;
            if receipt.sequence > through.sequence {
                return Err(invalid("captured batch exceeds the source boundary"));
            }
        }
        let mut bytes = 0usize;
        let mut batches = Vec::new();
        for receipt in receipts {
            let index = receipt_index(self.id, &self.batches, receipt)?;
            let position = &self.batches[index];
            if position.length > max_bytes.saturating_sub(bytes) {
                if batches.is_empty() {
                    return Err(invalid("source batch exceeds the capture byte budget"));
                }
                break;
            }
            bytes += position.length;
            batches.push((receipt.clone(), position.clone()));
        }
        Ok(CapturedJournal {
            file: self.file.try_clone()?,
            truncated: batches.len() != receipts.len(),
            batches,
        })
    }

    /// Create a new journal without overwriting an existing file. The parent must exist durably.
    pub fn create(path: &Path, id: Uuid) -> io::Result<Self> {
        let mut options = private_file_options();
        let mut file =
            FileLock::try_exclusive(options.create_new(true).read(true).write(true).open(path)?)?;
        let header = Header {
            format: FORMAT.into(),
            version: VERSION,
            id,
        };
        let bytes = json_line(&header)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_parent(path)?;
        Ok(Self {
            file,
            id,
            sequence: 0,
            digest: hex_digest(&bytes),
            poisoned: false,
            end: bytes.len() as u64,
            batches: Vec::new(),
        })
    }

    /// Validate and replay complete batches under the writer lock, then retain any incomplete tail.
    ///
    /// The callback builds a private projection; publish it only after this method succeeds. Complete malformed records, identity mismatches, and unsupported payloads fail without modifying the journal. Reopening does not run user code or resume an interrupted invocation.
    pub fn open<T: DeserializeOwned>(
        path: &Path,
        expected_id: Uuid,
        mut apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<(Self, Option<RecoveredTail>)> {
        let mut file =
            FileLock::try_exclusive(OpenOptions::new().read(true).write(true).open(path)?)?;
        let ScannedJournal {
            sequence,
            digest,
            offset,
            batches,
            tail,
        } = scan(&mut file, expected_id, &mut apply)?;
        let recovered = complete_replay(&mut file, path, offset, tail)?;
        Ok((
            Self {
                file,
                id: expected_id,
                sequence,
                digest,
                poisoned: false,
                end: offset,
                batches,
            },
            recovered,
        ))
    }

    /// Inspect a fixed-length file snapshot without locking, repairing, or starting execution. Publish the callback's projection only after success. A complete record may still be awaiting its writer's sync; inspection is historical evidence, not an acceptance or delivery acknowledgement. Use the owning host for authoritative live state.
    pub fn inspect<T: DeserializeOwned>(
        path: &Path,
        expected_id: Uuid,
        apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<JournalInspection> {
        Self::inspect_with_reader(path, expected_id, apply).map(|(inspection, _)| inspection)
    }

    /// Inspect once and retain that exact file's validated batch index for later body reads.
    pub fn inspect_with_reader<T: DeserializeOwned>(
        path: &Path,
        expected_id: Uuid,
        mut apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<(JournalInspection, JournalReader)> {
        let mut file = File::open(path)?;
        let scanned = scan(&mut file, expected_id, &mut apply)?;
        let inspection = JournalInspection {
            cursor: (scanned.sequence != 0).then_some(CommitReceipt {
                journal_id: expected_id,
                sequence: scanned.sequence,
                digest: scanned.digest,
            }),
            valid_bytes: scanned.offset,
            trailing_bytes: scanned.tail.map_or(0, |tail| tail.len() as u64),
        };
        Ok((
            inspection,
            JournalReader {
                file,
                id: expected_id,
                batches: scanned.batches,
            },
        ))
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub(crate) fn needs_reconciliation(&self) -> bool {
        self.poisoned
    }

    /// Revalidate and synchronize an uncertain writer without releasing its ownership lock.
    ///
    /// The callback must build a private projection and publish it only on success. Previously acknowledged batches must remain an exact prefix; any failure leaves the writer unavailable.
    pub(crate) fn reconcile_owned<T: DeserializeOwned>(
        &mut self,
        path: &Path,
        mut apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<Option<RecoveredTail>> {
        self.poisoned = true;
        self.file.seek(SeekFrom::Start(0))?;
        let ScannedJournal {
            sequence,
            digest,
            offset,
            batches,
            tail,
        } = scan(&mut self.file, self.id, &mut apply)?;
        if self.sequence > 0 {
            receipt_index(
                self.id,
                &batches,
                &CommitReceipt {
                    journal_id: self.id,
                    sequence: self.sequence,
                    digest: self.digest.clone(),
                },
            )?;
        }
        let recovered = complete_replay(&mut self.file, path, offset, tail)?;
        self.sequence = sequence;
        self.digest = digest;
        self.end = offset;
        self.batches = batches;
        self.poisoned = false;
        Ok(recovered)
    }

    /// Read a committed body through the writer's verified index. Failed reads require reopening.
    pub fn read_batch<T: DeserializeOwned>(
        &mut self,
        receipt: &CommitReceipt,
    ) -> io::Result<CommittedBatch<T>> {
        if self.poisoned {
            return Err(io::Error::other(
                "journal requires reopening after an I/O failure",
            ));
        }
        let index = receipt_index(self.id, &self.batches, receipt)?;
        self.poisoned = true;
        let batch = read_batch(&mut self.file, self.id, index, &self.batches[index])?;
        self.poisoned = false;
        Ok(batch)
    }

    /// Read committed batches without rescanning preceding history. The cursor must match this journal's recorded identity, sequence, and digest; reads never consume input or advance the writer's commit position.
    pub fn read_after<T: DeserializeOwned>(
        &mut self,
        after: Option<&CommitReceipt>,
        limit: NonZeroUsize,
    ) -> io::Result<JournalPage<T>> {
        if self.poisoned {
            return Err(io::Error::other(
                "journal requires reopening after an I/O failure",
            ));
        }
        let start = match after {
            None => 0,
            Some(cursor) => receipt_index(self.id, &self.batches, cursor)? + 1,
        };
        let end = start.saturating_add(limit.get()).min(self.batches.len());
        let mut batches = Vec::with_capacity(end - start);
        for index in start..end {
            let position = &self.batches[index];
            self.poisoned = true;
            let batch = read_batch(&mut self.file, self.id, index, position)?;
            self.poisoned = false;
            batches.push(batch);
        }
        Ok(JournalPage {
            next: batches
                .last()
                .map(|batch| batch.receipt.clone())
                .or_else(|| after.cloned()),
            has_more: end < self.batches.len(),
            batches,
        })
    }

    /// Serialize before touching disk, append one complete batch, and acknowledge only after sync.
    pub fn commit<T: Serialize>(&mut self, events: &[T]) -> io::Result<CommitReceipt> {
        if self.poisoned {
            return Err(io::Error::other(
                "journal requires reopening after an I/O failure",
            ));
        }
        if events.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty journal batch",
            ));
        }
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let payload = serde_json::to_string(events).map_err(invalid)?;
        let digest = batch_digest(sequence, &self.digest, &payload);
        let record = Record {
            sequence,
            previous: self.digest.clone(),
            payload,
            digest: digest.clone(),
        };
        let bytes = json_line(&record)?;
        let end = self
            .end
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("journal offset exhausted"))?;
        // Poison before I/O so unwinding cannot leave an apparently usable uncertain writer.
        self.poisoned = true;
        // Page reads move this same file's offset; append only at the acknowledged boundary.
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        self.sequence = sequence;
        self.digest = digest.clone();
        self.batches.push(BatchPosition {
            offset: self.end,
            length: bytes.len(),
            digest: digest.clone(),
        });
        self.end = end;
        self.poisoned = false;
        Ok(CommitReceipt {
            journal_id: self.id,
            sequence,
            digest,
        })
    }
}

pub(crate) fn private_file_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

pub(crate) fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("journal has no parent directory"))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(())
}

fn json_line(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(invalid)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn hex_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn batch_digest(sequence: u64, previous: &str, payload: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(sequence.to_be_bytes());
    hash.update(previous.as_bytes());
    hash.update(payload.as_bytes());
    format!("{:x}", hash.finalize())
}

fn retain_tail(path: &Path, offset: u64, bytes: &[u8]) -> io::Result<RecoveredTail> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("journal has no file name"))?;
    let mut name = name.to_os_string();
    name.push(format!(".incomplete-{}", Uuid::new_v4()));
    let retained_path = path.with_file_name(name);
    let mut retained = private_file_options()
        .create_new(true)
        .write(true)
        .open(&retained_path)?;
    retained.write_all(bytes)?;
    retained.sync_all()?;
    sync_parent(&retained_path)?;
    Ok(RecoveredTail {
        path: retained_path,
        offset,
        bytes: bytes.len() as u64,
    })
}

fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predecessor_requires_the_complete_writer_receipt_and_retains_batch_identity() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Journal::create(&dir.path().join("events.jsonl"), Uuid::new_v4()).unwrap();
        let first = writer.commit(&["first"]).unwrap();
        let second = writer.commit(&["turn started", "input committed"]).unwrap();
        assert_eq!(writer.previous_receipt(&first).unwrap(), None);
        assert_eq!(writer.previous_receipt(&second).unwrap(), Some(first));
        let mut forged = second.clone();
        forged.digest = "wrong".into();
        assert!(writer.previous_receipt(&forged).is_err());
        forged = second.clone();
        forged.journal_id = Uuid::new_v4();
        assert!(writer.previous_receipt(&forged).is_err());
        forged = second.clone();
        forged.sequence += 1;
        assert!(writer.previous_receipt(&forged).is_err());
        writer.poisoned = true;
        assert!(writer.previous_receipt(&second).is_err());
    }

    #[test]
    fn detached_capture_fences_receipts_and_never_moves_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut writer = Journal::create(&path, Uuid::new_v4()).unwrap();
        let first = writer.commit(&["first"]).unwrap();
        let second = writer.commit(&["second"]).unwrap();
        let budget = writer.batches[1].length;
        let capture = writer
            .capture_batches(&second, &[second.clone(), first.clone()], budget)
            .unwrap();
        assert!(capture.truncated);
        assert_eq!(capture.len(), 1);
        let third = writer.commit(&["third"]).unwrap();
        let position = writer.file.stream_position().unwrap();
        assert_eq!(capture.read::<String>(0).unwrap().facts, ["second"]);
        assert_eq!(writer.file.stream_position().unwrap(), position);
        let fourth = writer.commit(&["fourth"]).unwrap();
        assert_eq!(
            writer.read_batch::<String>(&fourth).unwrap().facts,
            ["fourth"]
        );
        assert!(
            writer
                .capture_batches(&second, &[third], usize::MAX)
                .is_err()
        );
        assert!(
            writer
                .capture_batches(&second, std::slice::from_ref(&second), budget - 1)
                .is_err()
        );
        for invalid in [
            CommitReceipt {
                journal_id: Uuid::new_v4(),
                ..second.clone()
            },
            CommitReceipt {
                digest: "invalid".into(),
                ..second.clone()
            },
            CommitReceipt {
                sequence: 0,
                ..second.clone()
            },
        ] {
            assert!(writer.capture_batches(&invalid, &[], budget).is_err());
            // Even the omitted suffix must belong to the requested source boundary.
            assert!(
                writer
                    .capture_batches(&second, &[second.clone(), first.clone(), invalid], budget)
                    .is_err()
            );
        }
        let offset = writer.batches[1].offset;
        let mut editor = OpenOptions::new().write(true).open(path).unwrap();
        editor.seek(SeekFrom::Start(offset)).unwrap();
        editor.write_all(b"!").unwrap();
        assert!(capture.read::<String>(0).is_err());
    }

    #[test]
    fn indexed_readers_keep_their_verified_prefix_and_recheck_body_integrity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut writer = Journal::create(&path, id).unwrap();
        let first = writer.commit(&["first", "same batch"]).unwrap();
        let (inspection, mut reader) =
            Journal::inspect_with_reader::<String>(&path, id, |_, _| Ok(())).unwrap();
        assert_eq!(inspection.cursor, Some(first.clone()));
        let second = writer.commit(&["second"]).unwrap();
        assert_eq!(
            reader.read_batch::<String>(&second).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            reader.read_batch::<String>(&first).unwrap().facts,
            ["first", "same batch"]
        );
        assert_eq!(
            writer.read_batch::<String>(&first).unwrap().facts,
            ["first", "same batch"]
        );
        let third = writer.commit(&["third"]).unwrap();
        assert_eq!(
            writer.read_batch::<String>(&third).unwrap().facts,
            ["third"]
        );
        for receipt in [
            CommitReceipt {
                journal_id: Uuid::new_v4(),
                ..first.clone()
            },
            CommitReceipt {
                sequence: 0,
                ..first.clone()
            },
            CommitReceipt {
                digest: "not the inspected batch".into(),
                ..first.clone()
            },
        ] {
            assert_eq!(
                reader.read_batch::<String>(&receipt).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                writer.read_batch::<String>(&receipt).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert!(!writer.poisoned);
        }
        // A successful index lookup never substitutes for checking the current bytes.
        let position = &writer.batches[0];
        let mut editor = OpenOptions::new().write(true).open(&path).unwrap();
        editor.seek(SeekFrom::Start(position.offset)).unwrap();
        editor.write_all(b"!").unwrap();
        assert!(reader.read_batch::<String>(&first).is_err());
        assert!(writer.read_batch::<String>(&first).is_err());
        assert!(writer.poisoned);
        assert!(
            writer
                .commit(&["cannot acknowledge damaged history"])
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn releasing_writer_ownership_does_not_wait_for_inherited_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut writer = Journal::create(&path, id).unwrap();
        writer.commit(&["saved"]).unwrap();
        // A forked command retains this open-file description until exec closes it.
        let inherited = writer.file.try_clone().unwrap();
        assert!(Journal::open::<String>(&path, id, |_, _| Ok(())).is_err());
        drop(writer);
        let mut restored = Vec::new();
        let (next, _) = Journal::open::<String>(&path, id, |_, facts| {
            restored.extend(facts);
            Ok(())
        })
        .unwrap();
        assert_eq!(restored, ["saved"]);
        drop(inherited);
        assert!(Journal::open::<String>(&path, id, |_, _| Ok(())).is_err());
        drop(next);
        Journal::open::<String>(&path, id, |_, _| Ok(())).unwrap();
    }

    #[test]
    fn bounded_pages_resume_across_appends_and_reopening_without_moving_the_commit_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        let empty = journal
            .read_after::<String>(None, NonZeroUsize::new(2).unwrap())
            .unwrap();
        assert!(empty.batches.is_empty() && empty.next.is_none() && !empty.has_more);
        let first = journal.commit(&["first", "same batch"]).unwrap();
        let second = journal.commit(&["second"]).unwrap();
        let third = journal.commit(&["third"]).unwrap();
        let page = journal
            .read_after::<String>(None, NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert_eq!(page.batches.len(), 1);
        assert_eq!(page.batches[0].facts, ["first", "same batch"]);
        assert_eq!(page.next, Some(first.clone()));
        assert!(page.has_more);
        let fourth = journal.commit(&["fourth"]).unwrap();
        let page = journal
            .read_after::<String>(page.next.as_ref(), NonZeroUsize::new(2).unwrap())
            .unwrap();
        assert_eq!(
            page.batches
                .iter()
                .map(|batch| &batch.receipt)
                .collect::<Vec<_>>(),
            [&second, &third]
        );
        assert!(page.has_more);
        drop(journal);
        let (mut journal, _) = Journal::open::<String>(&path, id, |_, _| Ok(())).unwrap();
        let last = journal
            .read_after::<String>(page.next.as_ref(), NonZeroUsize::new(2).unwrap())
            .unwrap();
        assert_eq!(last.batches[0].facts, ["fourth"]);
        assert_eq!(last.next, Some(fourth));
        assert!(!last.has_more);
        let empty = journal
            .read_after::<String>(last.next.as_ref(), NonZeroUsize::new(2).unwrap())
            .unwrap();
        assert!(empty.batches.is_empty() && !empty.has_more);
        assert_eq!(empty.next, last.next);
        let prefix = std::fs::read(&path).unwrap();
        for invalid in [
            CommitReceipt {
                journal_id: Uuid::new_v4(),
                ..first.clone()
            },
            CommitReceipt {
                sequence: 0,
                ..first.clone()
            },
            CommitReceipt {
                sequence: u64::MAX,
                ..first.clone()
            },
            CommitReceipt {
                digest: "different history".into(),
                ..first
            },
        ] {
            assert_eq!(
                journal
                    .read_after::<String>(Some(&invalid), NonZeroUsize::new(2).unwrap())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput,
            );
            assert!(!journal.poisoned);
        }
        assert_eq!(std::fs::read(&path).unwrap(), prefix);
        assert_eq!(journal.commit(&["fifth"]).unwrap().sequence, 5);
        assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
    }

    #[test]
    fn page_integrity_checks_only_the_selected_committed_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut journal = Journal::create(&path, Uuid::new_v4()).unwrap();
        let first = journal.commit(&[1]).unwrap();
        journal.commit(&[2]).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let corrupt = original.replacen("[2]", "[9]", 1);
        std::fs::write(&path, &corrupt).unwrap();
        let page = journal
            .read_after::<u32>(None, NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert_eq!(page.batches[0].facts, [1]);
        assert!(
            journal
                .read_after::<u32>(Some(&first), NonZeroUsize::new(1).unwrap())
                .is_err()
        );
        assert!(journal.poisoned);
        assert!(journal.commit(&[3]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
    }

    #[test]
    fn durable_batches_replay_atomically_in_sequence_and_bind_to_one_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        let first = journal.commit(&["accepted", "context_committed"]).unwrap();
        let second = journal.commit(&["answer"]).unwrap();
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert!(Journal::create(&path, id).is_err());
        assert!(Journal::open::<String>(&path, id, |_, _| Ok(())).is_err());
        drop(journal);
        let mut batches = Vec::new();
        let (mut reopened, tail) = Journal::open(&path, id, |receipt, items: Vec<String>| {
            batches.push((receipt, items));
            Ok(())
        })
        .unwrap();
        assert!(tail.is_none());
        assert_eq!(
            batches,
            vec![
                (first, vec!["accepted".into(), "context_committed".into()]),
                (second, vec!["answer".into()]),
            ]
        );
        assert_eq!(reopened.commit(&["next"]).unwrap().sequence, 3);
        drop(reopened);
        assert!(Journal::open::<String>(&path, Uuid::new_v4(), |_, _| Ok(())).is_err());
    }

    #[test]
    fn incomplete_tail_is_preserved_before_recovery_and_never_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        journal.commit(&[1, 2]).unwrap();
        drop(journal);
        let prefix = std::fs::read(&path).unwrap();
        let torn = b"{\"sequence\":2,\"payload\":\"partial";
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(torn)
            .unwrap();
        let mut seen = Vec::new();
        let (mut journal, tail) = Journal::open(&path, id, |_, items: Vec<u32>| {
            seen.extend(items);
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, [1, 2]);
        let tail = tail.unwrap();
        assert_eq!(tail.offset, prefix.len() as u64);
        assert_eq!(tail.bytes, torn.len() as u64);
        assert_eq!(std::fs::read(tail.path).unwrap(), torn);
        assert_eq!(std::fs::read(&path).unwrap(), prefix);
        journal.commit(&[3]).unwrap();
        drop(journal);
        let mut seen = Vec::new();
        let (_, tail) = Journal::open(&path, id, |_, items: Vec<u32>| {
            seen.extend(items);
            Ok(())
        })
        .unwrap();
        assert!(tail.is_none());
        assert_eq!(seen, [1, 2, 3]);
    }

    #[test]
    fn complete_corruption_and_replay_failure_leave_original_bytes_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        journal.commit(&[1]).unwrap();
        journal.commit(&[2]).unwrap();
        drop(journal);
        let original = std::fs::read_to_string(&path).unwrap();
        for corrupted in [
            original.replacen("[1]", "[9]", 1),
            original.replacen("\"sequence\":1", "\"sequence\":8", 1),
            format!(
                "{}\n{}\n",
                original.lines().next().unwrap(),
                original.lines().nth(2).unwrap()
            ),
            format!("{original}not-json\n"),
        ] {
            std::fs::write(&path, &corrupted).unwrap();
            assert!(Journal::open::<u32>(&path, id, |_, _| Ok(())).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupted);
        }
        std::fs::write(&path, format!("{original}partial")).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(Journal::open::<u32>(&path, id, |_, _| Err(invalid("unsupported fact"))).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejected_batches_and_uncertain_io_never_advance_the_acknowledged_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(journal.commit::<u32>(&[]).is_err());
        let invalid_json_key = std::collections::BTreeMap::from([(vec![1], 2)]);
        assert!(journal.commit(&[invalid_json_key]).is_err());
        assert!(!journal.poisoned);
        assert_eq!(journal.sequence(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Inject a write failure without platform-specific devices or production failpoints.
        fs2::FileExt::unlock(&*journal.file).unwrap();
        journal.file = FileLock::try_exclusive(File::open(&path).unwrap()).unwrap();
        assert!(journal.commit(&[1]).is_err());
        assert_eq!(journal.sequence(), 0);
        fs2::FileExt::unlock(&*journal.file).unwrap();
        journal.file = FileLock::try_exclusive(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        )
        .unwrap();
        assert!(journal.commit(&[2]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(journal);
        let (mut journal, _) = Journal::open::<u32>(&path, id, |_, _| Ok(())).unwrap();
        assert_eq!(journal.commit(&[3]).unwrap().sequence, 1);
    }

    #[test]
    fn retained_owner_reconciliation_syncs_complete_unknown_batches_and_retains_partial_tails() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        let first = journal.commit(&[1_u32]).unwrap();
        let second_digest = batch_digest(2, &first.digest, "[2]");
        let second = json_line(&Record {
            sequence: 2,
            previous: first.digest.clone(),
            payload: "[2]".into(),
            digest: second_digest.clone(),
        })
        .unwrap();
        journal.file.write_all(&second).unwrap();
        journal.file.write_all(b"incomplete").unwrap();
        journal.poisoned = true;
        let mut values = Vec::new();
        let recovered = journal
            .reconcile_owned(&path, |_, facts: Vec<u32>| {
                // No other writer can acquire the owner while replay callbacks run.
                assert!(Journal::open::<u32>(&path, id, |_, _| Ok(())).is_err());
                values.extend(facts);
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert_eq!(values, [1, 2]);
        assert_eq!(std::fs::read(recovered.path).unwrap(), b"incomplete");
        assert_eq!(journal.sequence(), 2);
        assert_eq!(journal.digest, second_digest);
        assert_eq!(journal.commit(&[3_u32]).unwrap().sequence, 3);
    }

    #[test]
    fn retained_owner_reconciliation_rejects_modified_acknowledged_prefix_without_repair() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let id = Uuid::new_v4();
        let mut journal = Journal::create(&path, id).unwrap();
        journal.commit(&[1_u32]).unwrap();
        let replacement = directory.path().join("replacement.jsonl");
        let mut other = Journal::create(&replacement, id).unwrap();
        other.commit(&[9_u32]).unwrap();
        drop(other);
        let mut changed = std::fs::read(replacement).unwrap();
        changed.extend_from_slice(b"partial");
        std::fs::write(&path, &changed).unwrap();
        assert!(
            journal
                .reconcile_owned::<u32>(&path, |_, _| Ok(()))
                .is_err()
        );
        assert!(journal.needs_reconciliation());
        assert_eq!(std::fs::read(path).unwrap(), changed);
        assert!(journal.commit(&[2_u32]).is_err());
    }
}
