//! Resumable inspection of the original journal framing and verified batch index.

use std::marker::PhantomData;

use super::*;

const READ_CHUNK: usize = 8192;

/// Per-step admission. Encoded bytes are charged once when read and once before decoding.
///
/// The record limit includes framing and also bounds an incomplete tail. Batch count limits callback invocations. These limits do not bound arbitrary deserializer or callback work, historical reads performed by a callback, or the total retained batch index.
#[derive(Clone, Copy, Debug)]
pub struct InspectionBudget {
    pub work_bytes: NonZeroUsize,
    pub max_batches: NonZeroUsize,
    pub max_record_bytes: NonZeroUsize,
}

impl InspectionBudget {
    pub(super) fn complete() -> Self {
        Self {
            work_bytes: NonZeroUsize::MAX,
            max_batches: NonZeroUsize::MAX,
            max_record_bytes: NonZeroUsize::MAX,
        }
    }
}

/// Preparation progress, distinct from a writer's acceptance or delivery acknowledgement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectionProgress {
    pub captured_bytes: u64,
    pub read_bytes: u64,
    pub decoded_bytes: u64,
    pub verified_bytes: u64,
    pub cursor: Option<CommitReceipt>,
}

/// The admission limit preventing further preparation of the current frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InspectionCapacityKind {
    RecordBytes,
    DecodeWork,
}

/// A resumable capacity boundary. No record is skipped and no receipt advances.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectionCapacity {
    pub kind: InspectionCapacityKind,
    pub record_offset: u64,
    pub required_bytes: usize,
    pub allowed_bytes: usize,
    /// False when only a lower bound is known, without reading the oversized frame.
    pub required_is_exact: bool,
    pub progress: InspectionProgress,
}

/// Index admission is measured in retained positions, independently of record bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectionRetentionCapacity {
    pub required_entries: usize,
    pub allowed_entries: usize,
    pub retained_bytes: usize,
    pub progress: InspectionProgress,
}

/// Incomplete preparation, a distinguishable capacity requirement, or a fully checked prefix.
#[derive(Clone, Debug)]
pub enum InspectionStep {
    Loading(InspectionProgress),
    Capacity(InspectionCapacity),
    RetentionCapacity(InspectionRetentionCapacity),
    Done(JournalInspection),
}

/// A fixed-length observation of one retained file, with no writer lock or recovery.
///
/// Dropping this value cancels observation only. Keep callback projections private until Done. Capacity can resume with a larger budget; I/O, validation and callback errors terminate the observation. Only finish can transfer a complete verified index into a JournalReader.
pub struct JournalInspectionReader<T> {
    file: File,
    scanner: Scanner,
    marker: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> JournalInspectionReader<T> {
    pub fn open(path: &Path, expected_id: Uuid) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let scanner = Scanner::new(&mut file, expected_id)?;
        Ok(Self {
            file,
            scanner,
            marker: PhantomData,
        })
    }

    pub fn progress(&self) -> InspectionProgress {
        self.scanner.progress()
    }

    pub(crate) fn read_stamp(&self) -> io::Result<super::ReadStamp> {
        let stamp = super::ReadStamp::of(&self.file)?;
        if stamp.length != self.progress().captured_bytes {
            return Err(invalid("journal changed while capturing its length"));
        }
        Ok(stamp)
    }

    pub fn step(
        &mut self,
        budget: InspectionBudget,
        mut apply: impl FnMut(CommitReceipt, Vec<T>) -> io::Result<()>,
    ) -> io::Result<InspectionStep> {
        self.step_with_prefix(budget, |receipt, facts, _| apply(receipt, facts))
    }

    pub(crate) fn step_with_prefix(
        &mut self,
        budget: InspectionBudget,
        apply: impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<InspectionStep> {
        self.step_with_prefix_retained(budget, NonZeroUsize::MAX, apply)
    }

    pub(crate) fn step_with_prefix_retained(
        &mut self,
        budget: InspectionBudget,
        maximum_positions: NonZeroUsize,
        mut apply: impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<InspectionStep> {
        Ok(
            match self.scanner.step_retained(
                &mut self.file,
                budget,
                maximum_positions.get(),
                &mut apply,
            )? {
                ScanStep::Loading => InspectionStep::Loading(self.scanner.progress()),
                ScanStep::Capacity(capacity) => InspectionStep::Capacity(capacity),
                ScanStep::RetentionCapacity(capacity) => {
                    InspectionStep::RetentionCapacity(capacity)
                }
                ScanStep::Done => {
                    InspectionStep::Done(self.scanner.scanned.inspection(self.scanner.id))
                }
            },
        )
    }

    pub fn finish(self) -> io::Result<(JournalInspection, JournalReader)> {
        let id = self.scanner.id;
        let scanned = self.scanner.finish()?;
        let inspection = scanned.inspection(id);
        Ok((
            inspection,
            JournalReader {
                file: self.file,
                id,
                batches: scanned.batches,
            },
        ))
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Loading,
    Done,
    Failed,
}

pub(super) enum ScanStep {
    Loading,
    Capacity(InspectionCapacity),
    RetentionCapacity(InspectionRetentionCapacity),
    Done,
}

/// Shared by bounded read-only preparation and the original complete recovery driver.
pub(super) struct Scanner {
    id: Uuid,
    captured: u64,
    consumed: u64,
    decoded: u64,
    scanned: ScannedJournal,
    input: [u8; READ_CHUNK],
    start: usize,
    end: usize,
    line: Vec<u8>,
    header_complete: bool,
    phase: Phase,
}

impl Scanner {
    pub(super) fn new(file: &mut File, id: Uuid) -> io::Result<Self> {
        let captured = file.metadata()?.len();
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            id,
            captured,
            consumed: 0,
            decoded: 0,
            scanned: ScannedJournal {
                sequence: 0,
                digest: String::new(),
                offset: 0,
                batches: Vec::new(),
                tail: None,
            },
            input: [0; READ_CHUNK],
            start: 0,
            end: 0,
            line: Vec::new(),
            header_complete: false,
            phase: Phase::Loading,
        })
    }

    fn progress(&self) -> InspectionProgress {
        InspectionProgress {
            captured_bytes: self.captured,
            read_bytes: self.consumed,
            decoded_bytes: self.decoded,
            verified_bytes: self.scanned.offset,
            cursor: self.scanned.inspection(self.id).cursor,
        }
    }

    fn capacity(
        &self,
        kind: InspectionCapacityKind,
        required_bytes: usize,
        allowed_bytes: usize,
        required_is_exact: bool,
    ) -> ScanStep {
        ScanStep::Capacity(InspectionCapacity {
            kind,
            record_offset: self.scanned.offset,
            required_bytes,
            allowed_bytes,
            required_is_exact,
            progress: self.progress(),
        })
    }

    pub(super) fn step<T: DeserializeOwned>(
        &mut self,
        file: &mut File,
        budget: InspectionBudget,
        apply: &mut impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<ScanStep> {
        self.step_retained(file, budget, usize::MAX, apply)
    }

    fn step_retained<T: DeserializeOwned>(
        &mut self,
        file: &mut File,
        budget: InspectionBudget,
        maximum_positions: usize,
        apply: &mut impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<ScanStep> {
        match self.phase {
            Phase::Done => return Ok(ScanStep::Done),
            Phase::Failed => return Err(io::Error::other("journal inspection has failed")),
            Phase::Loading => {}
        }
        let result = self.advance(file, budget, maximum_positions, apply);
        if result.is_err() {
            self.phase = Phase::Failed;
        }
        result
    }

    fn advance<T: DeserializeOwned>(
        &mut self,
        file: &mut File,
        budget: InspectionBudget,
        maximum_positions: usize,
        apply: &mut impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<ScanStep> {
        let mut remaining = budget.work_bytes.get();
        let mut batches = 0;
        let record_limit = budget.max_record_bytes.get();
        loop {
            if self.line.len() > record_limit {
                return Ok(self.capacity(
                    InspectionCapacityKind::RecordBytes,
                    self.line.len(),
                    record_limit,
                    self.line.ends_with(b"\n"),
                ));
            }
            if self.start == self.end
                && self.consumed == self.captured
                && !self.line.ends_with(b"\n")
            {
                if !self.header_complete {
                    return Err(invalid("incomplete journal header"));
                }
                if !self.line.is_empty() {
                    self.scanned.tail = Some(std::mem::take(&mut self.line));
                }
                self.phase = Phase::Done;
                return Ok(ScanStep::Done);
            }
            if batches == budget.max_batches.get() {
                return Ok(ScanStep::Loading);
            }
            if self.line.ends_with(b"\n") {
                let length = self.line.len();
                if length > budget.work_bytes.get() {
                    return Ok(self.capacity(
                        InspectionCapacityKind::DecodeWork,
                        length,
                        budget.work_bytes.get(),
                        true,
                    ));
                }
                if length > remaining {
                    return Ok(ScanStep::Loading);
                }
                if self.header_complete {
                    if self.scanned.batches.len() >= maximum_positions {
                        return Ok(ScanStep::RetentionCapacity(InspectionRetentionCapacity {
                            required_entries: self.scanned.batches.len().saturating_add(1),
                            allowed_entries: maximum_positions,
                            retained_bytes: index_bytes(
                                &self.scanned.batches,
                                self.scanned.batches.capacity(),
                            ),
                            progress: self.progress(),
                        }));
                    }
                    if self.scanned.batches.len() == self.scanned.batches.capacity() {
                        let target = self
                            .scanned
                            .batches
                            .capacity()
                            .saturating_mul(2)
                            .max(4)
                            .min(maximum_positions);
                        self.scanned
                            .batches
                            .try_reserve_exact(target - self.scanned.batches.len())
                            .map_err(io::Error::other)?;
                    }
                }
                remaining -= length;
                self.decoded += length as u64;
                if self.header_complete {
                    self.apply_batch(file, apply)?;
                    batches += 1;
                } else {
                    let header: Header = serde_json::from_slice(&self.line).map_err(invalid)?;
                    if header.format != FORMAT || header.version != VERSION || header.id != self.id
                    {
                        return Err(invalid("journal format, version, or identity mismatch"));
                    }
                    self.scanned.digest = hex_digest(&self.line);
                    self.scanned.offset = length as u64;
                    self.header_complete = true;
                }
                self.line.clear();
                continue;
            }
            if self.start != self.end {
                if self.line.len() == record_limit {
                    return Ok(self.capacity(
                        InspectionCapacityKind::RecordBytes,
                        record_limit.saturating_add(1),
                        record_limit,
                        false,
                    ));
                }
                let available = &self.input[self.start..self.end];
                let length = available
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(available.len(), |index| index + 1)
                    .min(record_limit - self.line.len());
                let needed = self.line.len() + length;
                if needed > self.line.capacity() {
                    let capacity = needed
                        .max(self.line.capacity().saturating_mul(2))
                        .min(record_limit);
                    self.line
                        .try_reserve_exact(capacity - self.line.len())
                        .map_err(io::Error::other)?;
                }
                self.line.extend_from_slice(&available[..length]);
                self.start += length;
                continue;
            }
            if remaining == 0 {
                return Ok(ScanStep::Loading);
            }
            let source_remaining =
                usize::try_from(self.captured - self.consumed).unwrap_or(usize::MAX);
            let length = remaining
                .min(READ_CHUNK)
                .min(source_remaining)
                .min((record_limit - self.line.len()).saturating_add(1));
            let read = match file.read(&mut self.input[..length]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "journal shrank during inspection",
                    ));
                }
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            self.start = 0;
            self.end = read;
            self.consumed += read as u64;
            remaining -= read;
        }
    }

    fn apply_batch<T: DeserializeOwned>(
        &mut self,
        file: &File,
        apply: &mut impl FnMut(CommitReceipt, Vec<T>, JournalPrefix<'_>) -> io::Result<()>,
    ) -> io::Result<()> {
        let next = self
            .scanned
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("journal sequence exhausted"))?;
        let batch =
            decode_checked_batch(&self.line, self.id, next, Some(&self.scanned.digest), None)?;
        apply(
            batch.receipt.clone(),
            batch.facts,
            JournalPrefix {
                file,
                id: self.id,
                batches: &self.scanned.batches,
            },
        )?;
        self.scanned.sequence = next;
        self.scanned.digest = batch.receipt.digest;
        self.scanned.batches.push(BatchPosition {
            offset: self.scanned.offset,
            length: self.line.len(),
            digest: self.scanned.digest.clone(),
        });
        self.scanned.offset = self
            .scanned
            .offset
            .checked_add(self.line.len() as u64)
            .ok_or_else(|| invalid("journal offset exhausted"))?;
        Ok(())
    }

    pub(super) fn finish(self) -> io::Result<ScannedJournal> {
        if self.phase != Phase::Done {
            return Err(io::Error::other("journal inspection is not complete"));
        }
        Ok(self.scanned)
    }
}

pub(super) fn index_bytes(positions: &[BatchPosition], capacity: usize) -> usize {
    capacity
        .saturating_mul(std::mem::size_of::<BatchPosition>())
        .saturating_add(
            positions
                .iter()
                .map(|position| position.digest.capacity())
                .sum::<usize>(),
        )
}

#[cfg(test)]
mod tests;
