//! Read registered outputs from their original visible conversation records.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Mutex,
};

use super::*;
use crate::{
    flow_store::{
        branches::{ConversationItemId, HistoryHead},
        source_capture::SourceSelection,
    },
    tools::tool_output::{OutputByteRange, OutputRangeBudget, OutputRangeError},
};
use uuid::Uuid;

mod diff;
mod image;
mod verification;
pub use diff::{ConversationDiffRequest, ConversationDiffResult};
pub use image::{ConversationImageRequest, ConversationImageResult};
use verification::VerificationCache;

const MAX_SOURCES: usize = 4;
pub(super) const MAX_READERS: usize = 2;
const BUDGET: OutputRangeBudget = OutputRangeBudget {
    max_verification_bytes: 8 * 1024 * 1024,
    max_return_bytes: 32 * 1024,
};
pub(super) static READERS: Semaphore = Semaphore::const_new(MAX_READERS);

pub struct ConversationArtifactRequest {
    pub source: SourceSelection,
    pub head: HistoryHead,
    pub item: ConversationItemId,
    pub part: u32,
    pub id: String,
    pub offset: usize,
    pub limit: usize,
    pub verification: Option<ConversationArtifactVerification>,
}

#[derive(Clone, Copy)]
pub enum ConversationArtifactVerification {
    Read(Uuid),
    Cancel(Uuid),
}

pub enum ConversationArtifactResult {
    Ready(OutputByteRange),
    Capacity(ConversationCapacity),
    VerificationBudget {
        required: usize,
        allowed: usize,
        progressive: bool,
    },
    Verifying {
        verified_bytes: usize,
        total_bytes: usize,
    },
    VerificationCancelled,
    SourceChanged,
    ContentChanged,
    InvalidRange,
    Unavailable,
}

/// Host-owned read indexes and temporary output snapshots. Both require fresh authorization.
#[derive(Default)]
pub struct ConversationArtifactSources {
    entries: Mutex<VecDeque<(PathBuf, Box<PreparedConversationSource>)>>,
    verification: Arc<VerificationCache>,
}

impl ConversationArtifactSources {
    fn take(
        &self,
        root: &Path,
        expected: &SourceSelection,
        head: &HistoryHead,
    ) -> Option<Box<PreparedConversationSource>> {
        let mut entries = self.entries.lock().unwrap();
        let index = entries.iter().position(|(data, prepared)| {
            let boundary = prepared.boundary();
            data == root && boundary.source == *expected && boundary.observed_head == *head
        })?;
        entries.remove(index).map(|(_, source)| source)
    }

    fn retain(&self, root: PathBuf, source: Box<PreparedConversationSource>) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|(data, old)| {
            data != &root || old.boundary().source.flow != source.boundary().source.flow
        });
        if entries.len() == MAX_SOURCES {
            entries.pop_back();
        }
        entries.push_front((root, source));
    }
}

impl SessionCatalog {
    async fn prepare_record_source(
        &self,
        cache: &ConversationArtifactSources,
        root: &Path,
        source: &SourceSelection,
        head: &HistoryHead,
        permit: &Arc<SemaphorePermit<'static>>,
        observation: &Observation,
    ) -> io::Result<Result<Box<PreparedConversationSource>, ConversationCapacity>> {
        if let Some(cached) = cache.take(root, source, head) {
            let cached = read_step(permit, move || {
                Ok(cached.storage_is_current().then_some(cached))
            })
            .await?;
            if let Some(cached) = cached {
                return Ok(Ok(cached));
            }
        }
        self.prepare_conversation(
            RecentConversationRequest {
                flow: source.flow.clone(),
                expected: Some(source.clone()),
                expected_head: Some(head.clone()),
                before: None,
                limit: NonZeroUsize::MIN,
            },
            permit,
            &observation.0,
            None,
        )
        .await
    }

    pub async fn read_conversation_artifact(
        &self,
        request: ConversationArtifactRequest,
        cache: Arc<ConversationArtifactSources>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<ConversationArtifactResult> {
        authorize(&request.source.flow)?;
        let root = self.recorded_root()?;
        if let Some(ConversationArtifactVerification::Cancel(id)) = request.verification {
            cache.verification.cancel(&root, &request, id);
            return Ok(ConversationArtifactResult::VerificationCancelled);
        }
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(readers_busy(MAX_READERS));
        };
        let permit = Arc::new(permit);
        let observation = Observation::default();
        let (mut verification, snapshot) = match request.verification {
            Some(ConversationArtifactVerification::Read(id)) => {
                match cache.verification.begin(&root, &request, id) {
                    Ok((verification, snapshot)) => (Some(verification), Some(snapshot)),
                    Err(result) => return Ok(result),
                }
            }
            _ => (None, None),
        };
        let flow = request.source.flow.clone();
        let source = match self
            .prepare_record_source(
                &cache,
                &root,
                &request.source,
                &request.head,
                &permit,
                &observation,
            )
            .await
        {
            Ok(Ok(source)) => source,
            Ok(Err(capacity)) => return Ok(ConversationArtifactResult::Capacity(capacity)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return Ok(ConversationArtifactResult::SourceChanged);
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = authorize(&flow) {
            crate::read_work::retire((source, Arc::clone(&permit)));
            return Err(error);
        }
        let control = observation.0.read_control();
        let result = read_step(&permit, move || {
            let mut source = source;
            if !source.storage_is_current() {
                return Ok(ConversationArtifactResult::SourceChanged);
            }
            let result = match source.read_artifact_record(request.item, request.part, &request.id)
            {
                Ok(Ok(artifact)) => {
                    let store = source.artifact_store();
                    if let Some(snapshot) = snapshot {
                        snapshot.step(&cache.verification, &store, artifact.output, &request)?
                    } else {
                        match store.read_artifact_range_with(
                            &artifact.output,
                            request.offset,
                            request.limit,
                            BUDGET,
                            &control,
                        ) {
                            Ok(range) => ConversationArtifactResult::Ready(range),
                            Err(OutputRangeError::BudgetExceeded { required, allowed }) => {
                                ConversationArtifactResult::VerificationBudget {
                                    required,
                                    allowed,
                                    progressive: true,
                                }
                            }
                            Err(OutputRangeError::Read(error)) => read_error(error)?,
                        }
                    }
                }
                Ok(Err(capacity)) => ConversationArtifactResult::Capacity(capacity),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    ConversationArtifactResult::Unavailable
                }
                Err(error) => return Err(error),
            };
            if !source.storage_is_current() {
                return Ok(ConversationArtifactResult::SourceChanged);
            }
            cache.retain(root, source);
            Ok(result)
        })
        .await?;
        authorize(&flow)?;
        if let Some(verification) = &mut verification {
            verification.finish(&result)?;
        }
        Ok(result)
    }
}

fn readers_busy(allowed: usize) -> ConversationArtifactResult {
    ConversationArtifactResult::Capacity(ConversationCapacity {
        kind: ConversationCapacityKind::ReadersBusy,
        required_at_least: allowed + 1,
        allowed,
        restart_required: false,
    })
}

fn read_error(error: io::Error) -> io::Result<ConversationArtifactResult> {
    match error.kind() {
        io::ErrorKind::InvalidData => Ok(ConversationArtifactResult::ContentChanged),
        io::ErrorKind::InvalidInput => Ok(ConversationArtifactResult::InvalidRange),
        io::ErrorKind::Interrupted => Err(error),
        _ => Ok(ConversationArtifactResult::Unavailable),
    }
}
