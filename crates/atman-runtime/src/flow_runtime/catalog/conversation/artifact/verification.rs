//! Short-lived verification observations, scoped to one durable tool result.

use std::{sync::OnceLock, time::Duration};

use tokio::{sync::OwnedMutexGuard, time::Instant};

use crate::{
    retained_file::{CopyControl, StagingCapacity, TextVerificationStep},
    tools::tool_output::{OutputArtifactIdentity, OutputStore},
};

use super::*;

const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
const STEP_BYTES: NonZeroUsize = NonZeroUsize::new(1024 * 1024).unwrap();
const MAX_SNAPSHOTS: usize = 4;
const IDLE: Duration = Duration::from_secs(60);
const REAP_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Default)]
pub(super) struct VerificationCache {
    active: Mutex<Active>,
    capacity: OnceLock<Arc<StagingCapacity>>,
}

#[derive(Default)]
struct Active {
    slots: Vec<Arc<Slot>>,
    reaping: bool,
}

struct Slot {
    id: Uuid,
    root: PathBuf,
    source: SourceSelection,
    head: HistoryHead,
    item: ConversationItemId,
    part: u32,
    output: String,
    touched: Mutex<Instant>,
    state: Arc<tokio::sync::Mutex<Option<Snapshot>>>,
    control: CopyControl,
}

impl Slot {
    fn matches(&self, root: &Path, request: &ConversationArtifactRequest) -> bool {
        self.root == root
            && self.source == request.source
            && self.head == request.head
            && self.item == request.item
            && self.part == request.part
            && self.output == request.id
    }
}

struct Snapshot {
    identity: OutputArtifactIdentity,
    state: TextVerificationStep,
}

pub(super) struct Verification {
    cache: Arc<VerificationCache>,
    slot: Arc<Slot>,
    keep: bool,
}

/// Exclusive across a request, including the original source preparation.
pub(super) struct SnapshotRead {
    state: OwnedMutexGuard<Option<Snapshot>>,
    control: CopyControl,
}

impl VerificationCache {
    pub(super) fn begin(
        self: &Arc<Self>,
        root: &Path,
        request: &ConversationArtifactRequest,
        id: Uuid,
    ) -> Result<(Verification, SnapshotRead), ConversationArtifactResult> {
        let mut active = self.active.lock().unwrap();
        let slot = if let Some(slot) = active.slots.iter().find(|slot| slot.id == id) {
            if !slot.matches(root, request) {
                return Err(ConversationArtifactResult::InvalidRange);
            }
            Arc::clone(slot)
        } else {
            if active.slots.len() == MAX_SNAPSHOTS {
                return Err(readers_busy(MAX_SNAPSHOTS));
            }
            let slot = Arc::new(Slot {
                id,
                root: root.to_owned(),
                source: request.source.clone(),
                head: request.head.clone(),
                item: request.item,
                part: request.part,
                output: request.id.clone(),
                touched: Mutex::new(Instant::now()),
                state: Arc::default(),
                control: CopyControl::default(),
            });
            active.slots.push(Arc::clone(&slot));
            slot
        };
        let state = Arc::clone(&slot.state)
            .try_lock_owned()
            .map_err(|_| readers_busy(1))?;
        *slot.touched.lock().unwrap() = Instant::now();
        if !active.reaping {
            active.reaping = true;
            let weak = Arc::downgrade(self);
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(REAP_INTERVAL).await;
                    let Some(cache) = weak.upgrade() else { break };
                    let mut active = cache.active.lock().unwrap();
                    active.slots.retain(|slot| {
                        let live = slot.touched.lock().unwrap().elapsed() < IDLE;
                        if !live {
                            slot.control.cancel();
                            crate::read_work::retire(Arc::clone(slot));
                        }
                        live
                    });
                    if active.slots.is_empty() {
                        active.reaping = false;
                        break;
                    }
                }
            });
        }
        let read = SnapshotRead {
            state,
            control: slot.control.clone(),
        };
        Ok((
            Verification {
                cache: Arc::clone(self),
                slot,
                keep: false,
            },
            read,
        ))
    }

    pub(super) fn cancel(&self, root: &Path, request: &ConversationArtifactRequest, id: Uuid) {
        self.remove(|slot| slot.id == id && slot.matches(root, request));
    }

    fn remove(&self, matches: impl Fn(&Arc<Slot>) -> bool) {
        let mut active = self.active.lock().unwrap();
        if let Some(index) = active.slots.iter().position(matches) {
            let slot = active.slots.swap_remove(index);
            slot.control.cancel();
            crate::read_work::retire(slot);
        }
    }

    fn capacity(&self) -> io::Result<&Arc<StagingCapacity>> {
        if let Some(capacity) = self.capacity.get() {
            return Ok(capacity);
        }
        let capacity =
            StagingCapacity::new(std::env::temp_dir(), MAX_SNAPSHOTS * MAX_SNAPSHOT_BYTES)?;
        Ok(self.capacity.get_or_init(|| capacity))
    }
}

impl Verification {
    pub(super) fn finish(&mut self, result: &ConversationArtifactResult) -> io::Result<()> {
        self.slot.control.check()?;
        self.keep = matches!(
            result,
            ConversationArtifactResult::Verifying { .. } | ConversationArtifactResult::Ready(_)
        );
        Ok(())
    }
}

impl Drop for Verification {
    fn drop(&mut self) {
        if !self.keep {
            self.cache.remove(|slot| Arc::ptr_eq(slot, &self.slot));
        }
    }
}

impl SnapshotRead {
    pub(super) fn step(
        mut self,
        cache: &VerificationCache,
        store: &OutputStore,
        identity: OutputArtifactIdentity,
        request: &ConversationArtifactRequest,
    ) -> io::Result<ConversationArtifactResult> {
        self.control.check()?;
        let limit = request.limit.min(BUDGET.max_return_bytes);
        if request.offset > identity.bytes || limit < 4 {
            return Ok(ConversationArtifactResult::InvalidRange);
        }
        if identity.bytes > MAX_SNAPSHOT_BYTES {
            return Ok(ConversationArtifactResult::VerificationBudget {
                required: identity.bytes,
                allowed: MAX_SNAPSHOT_BYTES,
                progressive: false,
            });
        }
        match self.advance(cache, store, identity, request.offset, limit) {
            Ok(result) => Ok(result),
            Err(error) => read_error(error),
        }
    }

    fn advance(
        &mut self,
        cache: &VerificationCache,
        store: &OutputStore,
        identity: OutputArtifactIdentity,
        offset: usize,
        limit: usize,
    ) -> io::Result<ConversationArtifactResult> {
        let total_bytes = identity.bytes;
        let state = match self.state.take() {
            Some(snapshot) if snapshot.identity == identity => snapshot.state,
            Some(_) => return Ok(ConversationArtifactResult::ContentChanged),
            None => TextVerificationStep::Pending(Box::new(store.verify_artifact_snapshot(
                &identity,
                cache.capacity()?,
                &self.control,
            )?)),
        };
        let mut state = match state {
            TextVerificationStep::Pending(scan) => scan.advance(STEP_BYTES, &self.control)?,
            TextVerificationStep::Ready(ready) => TextVerificationStep::Ready(ready),
        };
        let result = match &mut state {
            TextVerificationStep::Pending(scan) => ConversationArtifactResult::Verifying {
                verified_bytes: scan.processed_bytes(),
                total_bytes,
            },
            TextVerificationStep::Ready(ready) => {
                ConversationArtifactResult::Ready(OutputByteRange::new(
                    ready.read_window(offset, limit, &self.control)?,
                    offset,
                    total_bytes,
                ))
            }
        };
        *self.state = Some(Snapshot { identity, state });
        Ok(result)
    }
}
