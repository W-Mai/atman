//! Durable batch commits for instance and session facts.
//!
//! This blocking store belongs on the host's storage worker. A successful commit acknowledges the whole batch after synchronization. An I/O failure makes the handle unusable until reopened: a complete but unacknowledged batch may still exist, so callers reconcile stable request IDs after replay. Live display frames do not need this durability boundary.

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use uuid::Uuid;

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
    file: File,
    id: Uuid,
    sequence: u64,
    digest: String,
    poisoned: bool,
    end: u64,
    batches: Vec<BatchPosition>,
}

impl Journal {
    /// Create a new journal without overwriting an existing file. The parent must exist durably.
    pub fn create(path: &Path, id: Uuid) -> io::Result<Self> {
        let mut options = private_file_options();
        let mut file = options.create_new(true).read(true).write(true).open(path)?;
        fs2::FileExt::try_lock_exclusive(&file)?;
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
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        fs2::FileExt::try_lock_exclusive(&file)?;
        let ScannedJournal {
            sequence,
            digest,
            offset,
            batches,
            tail,
        } = scan(&mut file, expected_id, &mut apply)?;
        let recovered = if let Some(tail) = tail {
            let retained = retain_tail(path, offset, &tail)?;
            file.set_len(offset)?;
            file.sync_all()?;
            Some(retained)
        } else {
            None
        };
        // A previous owner's failed sync may have left a complete, unacknowledged batch.
        file.sync_all()?;
        sync_parent(path)?;
        file.seek(SeekFrom::Start(offset))?;
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
        mut apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<JournalInspection> {
        let mut file = File::open(path)?;
        let scanned = scan(&mut file, expected_id, &mut apply)?;
        Ok(JournalInspection {
            cursor: (scanned.sequence != 0).then_some(CommitReceipt {
                journal_id: expected_id,
                sequence: scanned.sequence,
                digest: scanned.digest,
            }),
            valid_bytes: scanned.offset,
            trailing_bytes: scanned.tail.map_or(0, |tail| tail.len() as u64),
        })
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
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
            Some(cursor) => {
                let position = usize::try_from(cursor.sequence)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
                let known = position
                    .checked_sub(1)
                    .and_then(|index| self.batches.get(index));
                if cursor.journal_id != self.id
                    || known.is_none_or(|batch| batch.digest != cursor.digest)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "journal cursor is not a committed position in this history",
                    ));
                }
                position
            }
        };
        let end = start.saturating_add(limit.get()).min(self.batches.len());
        let mut batches = Vec::with_capacity(end - start);
        for index in start..end {
            let position = &self.batches[index];
            self.poisoned = true;
            self.file.seek(SeekFrom::Start(position.offset))?;
            let mut bytes = vec![0; position.length];
            self.file.read_exact(&mut bytes)?;
            let record: Record = serde_json::from_slice(&bytes).map_err(invalid)?;
            let sequence = index as u64 + 1;
            if !bytes.ends_with(b"\n")
                || record.sequence != sequence
                || record.digest != position.digest
                || record.digest != batch_digest(sequence, &record.previous, &record.payload)
            {
                return Err(invalid("journal page integrity check failed"));
            }
            self.poisoned = false;
            let facts: Vec<T> = serde_json::from_str(&record.payload).map_err(invalid)?;
            if facts.is_empty() {
                return Err(invalid("empty journal batch"));
            }
            batches.push(CommittedBatch {
                receipt: CommitReceipt {
                    journal_id: self.id,
                    sequence,
                    digest: record.digest,
                },
                facts,
            });
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
        journal.file = File::open(&path).unwrap();
        assert!(journal.commit(&[1]).is_err());
        assert_eq!(journal.sequence(), 0);
        journal.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(journal.commit(&[2]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        drop(journal);
        let (mut journal, _) = Journal::open::<u32>(&path, id, |_, _| Ok(())).unwrap();
        assert_eq!(journal.commit(&[3]).unwrap().sequence, 1);
    }
}
