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

const MAX_SOURCES: usize = 4;
const MAX_READERS: usize = 2;
const BUDGET: OutputRangeBudget = OutputRangeBudget {
    max_verification_bytes: 8 * 1024 * 1024,
    max_return_bytes: 32 * 1024,
};
static READERS: Semaphore = Semaphore::const_new(MAX_READERS);

pub struct ConversationArtifactRequest {
    pub source: SourceSelection,
    pub head: HistoryHead,
    pub item: ConversationItemId,
    pub part: u32,
    pub id: String,
    pub offset: usize,
    pub limit: usize,
}

pub enum ConversationArtifactResult {
    Ready(OutputByteRange),
    Capacity(ConversationCapacity),
    VerificationBudget { required: usize, allowed: usize },
    SourceChanged,
    ContentChanged,
    InvalidRange,
    Unavailable,
}

/// Host-owned cache of bounded, verified indexes. It grants no authority and stores no output bytes.
#[derive(Default)]
pub struct ConversationArtifactSources {
    entries: Mutex<VecDeque<(PathBuf, Box<PreparedConversationSource>)>>,
}

impl ConversationArtifactSources {
    fn take(
        &self,
        root: &Path,
        request: &ConversationArtifactRequest,
    ) -> Option<Box<PreparedConversationSource>> {
        let mut entries = self.entries.lock().unwrap();
        let index = entries.iter().position(|(data, prepared)| {
            let boundary = prepared.boundary();
            data == root
                && boundary.source == request.source
                && boundary.observed_head == request.head
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
    pub async fn read_conversation_artifact(
        &self,
        request: ConversationArtifactRequest,
        cache: Arc<ConversationArtifactSources>,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<ConversationArtifactResult> {
        authorize(&request.source.flow)?;
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(ConversationArtifactResult::Capacity(ConversationCapacity {
                kind: ConversationCapacityKind::ReadersBusy,
                required_at_least: MAX_READERS + 1,
                allowed: MAX_READERS,
                restart_required: false,
            }));
        };
        let permit = Arc::new(permit);
        let observation = Observation::default();
        let root = self.recorded_root()?;
        let cached = cache.take(&root, &request);
        let cached = if let Some(source) = cached {
            read_step(&permit, move || {
                Ok(source.storage_is_current().then_some(source))
            })
            .await?
        } else {
            None
        };
        let flow = request.source.flow.clone();
        let source = match cached {
            Some(source) => source,
            None => {
                let query = RecentConversationRequest {
                    flow: flow.clone(),
                    expected: Some(request.source.clone()),
                    expected_head: Some(request.head.clone()),
                    before: None,
                    limit: NonZeroUsize::MIN,
                };
                match self
                    .prepare_conversation(query, &permit, &observation.0)
                    .await
                {
                    Ok(Ok(source)) => source,
                    Ok(Err(capacity)) => return Ok(ConversationArtifactResult::Capacity(capacity)),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        return Ok(ConversationArtifactResult::SourceChanged);
                    }
                    Err(error) => return Err(error),
                }
            }
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
                Ok(Ok(artifact)) => match source.artifact_store().read_artifact_range_with(
                    &artifact.output,
                    request.offset,
                    request.limit,
                    BUDGET,
                    &control,
                ) {
                    Ok(range) => ConversationArtifactResult::Ready(range),
                    Err(OutputRangeError::BudgetExceeded { required, allowed }) => {
                        ConversationArtifactResult::VerificationBudget { required, allowed }
                    }
                    Err(OutputRangeError::Read(error)) => match error.kind() {
                        io::ErrorKind::InvalidData => ConversationArtifactResult::ContentChanged,
                        io::ErrorKind::InvalidInput => ConversationArtifactResult::InvalidRange,
                        io::ErrorKind::Interrupted => return Err(error),
                        _ => ConversationArtifactResult::Unavailable,
                    },
                },
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
        Ok(result)
    }
}
