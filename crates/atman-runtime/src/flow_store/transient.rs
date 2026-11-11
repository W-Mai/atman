//! Bounded host-owned retention without a filesystem journal.

use std::io::{self, Write};
use std::num::NonZeroUsize;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use uuid::Uuid;

use super::position::{FlowBatch, FlowPage, FlowPosition, TransientPosition};
use super::{FlowFact, invalid};

const MAX_EMERGENCY_BATCH: usize = 2048;

/// Shared retained-payload budget. Projection and renderer allocations are separate.
#[derive(Clone)]
pub struct TransientBudget(Arc<HostBudget>);

struct HostBudget {
    maximum: usize,
    used: AtomicUsize,
}

impl TransientBudget {
    pub fn new(maximum: usize) -> io::Result<Self> {
        if maximum == 0 {
            return Err(invalid("transient host budget must be nonzero"));
        }
        Ok(Self(Arc::new(HostBudget {
            maximum,
            used: AtomicUsize::new(0),
        })))
    }

    pub fn retained_bytes(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TransientLimits {
    pub maximum_bytes: usize,
    pub terminal_reserve_bytes: usize,
}

impl Default for TransientLimits {
    fn default() -> Self {
        Self {
            maximum_bytes: 32 * 1024 * 1024,
            terminal_reserve_bytes: 256 * 1024,
        }
    }
}

/// One flow's retention account; clones share ownership and byte reservations.
#[derive(Clone)]
pub struct TransientRetention(Arc<FlowBudget>);

struct FlowBudget {
    host: TransientBudget,
    generation: Uuid,
    flow: Uuid,
    limits: TransientLimits,
    data: AtomicUsize,
    terminal: AtomicUsize,
    rejections: AtomicU64,
    admission_closed: AtomicBool,
}

impl Drop for FlowBudget {
    fn drop(&mut self) {
        self.host
            .0
            .used
            .fetch_sub(self.limits.terminal_reserve_bytes, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransientUsage {
    pub retained_bytes: usize,
    pub maximum_bytes: usize,
    pub terminal_reserve_bytes: usize,
    pub rejections: u64,
    pub admission_closed: bool,
}

/// A shared allocation charge, released only after its last owner is dropped.
#[derive(Clone)]
pub struct TransientAllocation(Arc<Allocation>);

struct Allocation {
    owner: Arc<FlowBudget>,
    bytes: usize,
    terminal: bool,
}

impl Drop for Allocation {
    fn drop(&mut self) {
        if self.terminal {
            self.owner.terminal.fetch_sub(self.bytes, Ordering::AcqRel);
        } else {
            self.owner.data.fetch_sub(self.bytes, Ordering::AcqRel);
            self.owner
                .host
                .0
                .used
                .fetch_sub(self.bytes, Ordering::AcqRel);
        }
    }
}

impl TransientAllocation {
    pub fn bytes(&self) -> usize {
        self.0.bytes
    }
}

#[derive(Debug)]
struct CapacityExceeded;

impl std::fmt::Display for CapacityExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "temporary conversation retention limit reached; keep or discard it before continuing",
        )
    }
}

impl std::error::Error for CapacityExceeded {}

pub fn is_capacity_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|cause| cause.is::<CapacityExceeded>())
}

fn reserve_count(counter: &AtomicUsize, bytes: usize, maximum: usize) -> bool {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
            used.checked_add(bytes).filter(|next| *next <= maximum)
        })
        .is_ok()
}

impl TransientRetention {
    pub fn new(
        host: TransientBudget,
        host_generation: Uuid,
        flow_id: Uuid,
        limits: TransientLimits,
    ) -> io::Result<Self> {
        if host_generation.is_nil()
            || flow_id.is_nil()
            || limits.terminal_reserve_bytes < 16 * 1024
            || limits.maximum_bytes <= limits.terminal_reserve_bytes
        {
            return Err(invalid("invalid transient identity or retention limits"));
        }
        if !reserve_count(&host.0.used, limits.terminal_reserve_bytes, host.0.maximum) {
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, CapacityExceeded));
        }
        Ok(Self(Arc::new(FlowBudget {
            host,
            generation: host_generation,
            flow: flow_id,
            limits,
            data: AtomicUsize::new(0),
            terminal: AtomicUsize::new(0),
            rejections: AtomicU64::new(0),
            admission_closed: AtomicBool::new(false),
        })))
    }

    pub fn host_generation(&self) -> Uuid {
        self.0.generation
    }
    pub fn flow_id(&self) -> Uuid {
        self.0.flow
    }

    pub fn usage(&self) -> TransientUsage {
        TransientUsage {
            retained_bytes: self.0.data.load(Ordering::Acquire)
                + self.0.terminal.load(Ordering::Acquire),
            maximum_bytes: self.0.limits.maximum_bytes,
            terminal_reserve_bytes: self.0.limits.terminal_reserve_bytes,
            rejections: self.0.rejections.load(Ordering::Acquire),
            admission_closed: self.0.admission_closed.load(Ordering::Acquire),
        }
    }

    fn capacity_error(&self) -> io::Error {
        self.0.rejections.fetch_add(1, Ordering::AcqRel);
        io::Error::new(io::ErrorKind::OutOfMemory, CapacityExceeded)
    }

    fn try_data(&self, bytes: usize) -> Option<TransientAllocation> {
        if self.0.admission_closed.load(Ordering::Acquire) {
            return None;
        }
        let maximum = self.0.limits.maximum_bytes - self.0.limits.terminal_reserve_bytes;
        if !reserve_count(&self.0.data, bytes, maximum) {
            return None;
        }
        if !reserve_count(&self.0.host.0.used, bytes, self.0.host.0.maximum) {
            self.0.data.fetch_sub(bytes, Ordering::AcqRel);
            return None;
        }
        if self.0.admission_closed.load(Ordering::Acquire) {
            self.0.data.fetch_sub(bytes, Ordering::AcqRel);
            self.0.host.0.used.fetch_sub(bytes, Ordering::AcqRel);
            return None;
        }
        Some(TransientAllocation(Arc::new(Allocation {
            owner: self.0.clone(),
            bytes,
            terminal: false,
        })))
    }

    /// Reserve immutable output or artifact bytes before publishing them to readers.
    pub fn reserve(&self, bytes: usize) -> io::Result<TransientAllocation> {
        self.try_data(bytes).ok_or_else(|| self.capacity_error())
    }

    fn reserve_terminal(&self, bytes: usize) -> io::Result<TransientAllocation> {
        if let Some(allocation) = self.try_data(bytes) {
            return Ok(allocation);
        }
        if bytes > MAX_EMERGENCY_BATCH
            || !reserve_count(
                &self.0.terminal,
                bytes,
                self.0.limits.terminal_reserve_bytes,
            )
        {
            return Err(self.capacity_error());
        }
        self.0.admission_closed.store(true, Ordering::Release);
        Ok(TransientAllocation(Arc::new(Allocation {
            owner: self.0.clone(),
            bytes,
            terminal: true,
        })))
    }

    fn available(&self, terminal: bool) -> usize {
        let data = (self.0.limits.maximum_bytes - self.0.limits.terminal_reserve_bytes)
            .saturating_sub(self.0.data.load(Ordering::Acquire));
        let host = self
            .0
            .host
            .0
            .maximum
            .saturating_sub(self.0.host.0.used.load(Ordering::Acquire));
        let data = if self.0.admission_closed.load(Ordering::Acquire) {
            0
        } else {
            data.min(host)
        };
        if terminal {
            data.max(
                self.0
                    .limits
                    .terminal_reserve_bytes
                    .saturating_sub(self.0.terminal.load(Ordering::Acquire))
                    .min(MAX_EMERGENCY_BATCH),
            )
        } else {
            data
        }
    }
}

struct RetainedBatch {
    receipt: FlowPosition,
    payload: Vec<u8>,
    _allocation: TransientAllocation,
}

/// Immutable retention capture. Keeping it alive retains its existing byte charges.
pub struct TransientSnapshot {
    pub id: Uuid,
    pub cursor: FlowPosition,
    batches: Vec<Arc<RetainedBatch>>,
}

impl TransientSnapshot {
    pub fn len(&self) -> usize {
        self.batches.len()
    }
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    pub fn read_updates(&self, index: usize) -> io::Result<FlowBatch<super::view::FlowUpdate>> {
        let batch = self.map_batch(index, |_| Ok(()))?;
        Ok(FlowBatch {
            receipt: batch.receipt,
            facts: batch.facts.into_iter().map(Into::into).collect(),
        })
    }

    pub(super) fn map_batch(
        &self,
        index: usize,
        mut map: impl FnMut(&mut FlowPosition) -> io::Result<()>,
    ) -> io::Result<FlowBatch<FlowFact>> {
        let batch = self
            .batches
            .get(index)
            .ok_or_else(|| invalid("transient snapshot batch is unavailable"))?;
        let mut facts: Vec<FlowFact> = serde_json::from_slice(&batch.payload).map_err(invalid)?;
        for fact in &mut facts {
            fact.visit_positions(&mut map)?;
        }
        Ok(FlowBatch {
            receipt: batch.receipt.clone(),
            facts,
        })
    }
}

pub(super) struct MemoryJournal {
    retention: TransientRetention,
    batches: Vec<Arc<RetainedBatch>>,
    frozen: Option<Uuid>,
    emergency_used: u8,
}

impl MemoryJournal {
    pub fn new(retention: TransientRetention) -> Self {
        Self {
            retention,
            batches: Vec::new(),
            frozen: None,
            emergency_used: 0,
        }
    }

    pub fn retention(&self) -> &TransientRetention {
        &self.retention
    }

    pub fn frozen(&self) -> bool {
        self.frozen.is_some()
    }

    pub fn commit(&mut self, facts: &[FlowFact]) -> io::Result<FlowPosition> {
        if self.frozen.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "temporary history is frozen for retention",
            ));
        }
        let terminal_mask = facts.iter().try_fold(0_u8, |mask, fact| {
            fact.terminal_reservation().map(|next| mask | next)
        });
        let terminal_mask =
            terminal_mask.filter(|mask| *mask != 0 && *mask & self.emergency_used == 0);
        let maximum = self
            .retention
            .available(terminal_mask.is_some())
            .saturating_sub(std::mem::size_of::<RetainedBatch>());
        let mut payload = BoundedBytes {
            bytes: Vec::new(),
            maximum,
            exceeded: false,
        };
        if let Err(error) = serde_json::to_writer(&mut payload, facts) {
            return Err(if payload.exceeded {
                self.retention.capacity_error()
            } else {
                invalid(error)
            });
        }
        let bytes = payload
            .bytes
            .len()
            .checked_add(std::mem::size_of::<RetainedBatch>())
            .ok_or_else(|| self.retention.capacity_error())?;
        let allocation = if terminal_mask.is_some() {
            self.retention.reserve_terminal(bytes)?
        } else {
            self.retention.reserve(bytes)?
        };
        let sequence = u64::try_from(self.batches.len())
            .ok()
            .and_then(|count| count.checked_add(1))
            .ok_or_else(|| invalid("transient publication sequence exhausted"))?;
        let mut digest = blake3::Hasher::new();
        digest.update(b"atman.transient.publication\0");
        digest.update(self.retention.host_generation().as_bytes());
        digest.update(self.retention.flow_id().as_bytes());
        digest.update(&sequence.to_be_bytes());
        if let Some(previous) = self.batches.last() {
            digest.update(previous.receipt.digest().as_bytes());
        }
        digest.update(&payload.bytes);
        let receipt = FlowPosition::Transient {
            transient: TransientPosition {
                host_generation: self.retention.host_generation(),
                flow_id: self.retention.flow_id(),
                sequence,
                digest: digest.finalize().to_hex().to_string(),
            },
        };
        if allocation.0.terminal {
            self.emergency_used |= terminal_mask.unwrap_or(0);
        }
        self.batches.push(Arc::new(RetainedBatch {
            receipt: receipt.clone(),
            payload: payload.bytes,
            _allocation: allocation,
        }));
        Ok(receipt)
    }

    fn index(&self, position: &FlowPosition) -> io::Result<usize> {
        let index = position
            .sequence()
            .checked_sub(1)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|index| {
                self.batches
                    .get(*index)
                    .is_some_and(|batch| batch.receipt == *position)
            });
        index.ok_or_else(|| {
            invalid("position is not an accepted publication in this transient history")
        })
    }

    pub fn validate_reference(&self, position: &FlowPosition) -> io::Result<()> {
        self.index(position).map(|_| ())
    }

    pub fn read(&self, position: &FlowPosition) -> io::Result<FlowBatch<FlowFact>> {
        let batch = &self.batches[self.index(position)?];
        Ok(FlowBatch {
            receipt: batch.receipt.clone(),
            facts: serde_json::from_slice(&batch.payload).map_err(invalid)?,
        })
    }

    pub fn read_after(
        &self,
        after: Option<&FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<FlowPage<FlowFact>> {
        let start = after
            .map(|position| self.index(position).map(|index| index + 1))
            .transpose()?
            .unwrap_or(0);
        let end = start.saturating_add(limit.get()).min(self.batches.len());
        let batches = self.batches[start..end]
            .iter()
            .map(|batch| self.read(&batch.receipt))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(FlowPage {
            next: batches
                .last()
                .map(|batch| batch.receipt.clone())
                .or_else(|| after.cloned()),
            batches,
            has_more: end < self.batches.len(),
        })
    }

    pub fn freeze(&mut self) -> io::Result<TransientSnapshot> {
        if self.frozen.is_some() {
            return Err(invalid("temporary history already has a retention capture"));
        }
        let cursor = self
            .batches
            .last()
            .ok_or_else(|| invalid("temporary history has no creation"))?
            .receipt
            .clone();
        let id = Uuid::now_v7();
        self.frozen = Some(id);
        Ok(TransientSnapshot {
            id,
            cursor,
            batches: self.batches.clone(),
        })
    }

    pub fn thaw(&mut self, id: Uuid) -> io::Result<()> {
        if self.frozen != Some(id) {
            return Err(invalid(
                "retention capture does not own this temporary history",
            ));
        }
        self.frozen = None;
        Ok(())
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    maximum: usize,
    exceeded: bool,
}

#[cfg(test)]
mod tests;

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::new(io::ErrorKind::OutOfMemory, CapacityExceeded));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
