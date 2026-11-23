//! Incremental recent conversation reads on the shared storage workers.

use std::{io, num::NonZeroUsize, ops::ControlFlow, sync::Arc};

use tokio::sync::{Semaphore, SemaphorePermit};

use super::SessionCatalog;
use crate::{
    event_log::journal::InspectionBudget,
    flow_store::{
        FlowRef, RuntimeStore,
        branches::{
            ConversationCapacity, ConversationCapacityKind, ConversationPageStep,
            ConversationPreparationStep, PreparedConversationSource, RecentConversationLimits,
            RecentConversationPage, RecentConversationRequest,
        },
        source_capture::SourceCaptureControl,
    },
};

const MAX_READERS: usize = 2;
const MAX_FLOW_SCAN_BYTES: usize = 32 * 1024 * 1024;
static READERS: Semaphore = Semaphore::const_new(MAX_READERS);

mod artifact;
pub use artifact::{
    ConversationArtifactRequest, ConversationArtifactResult, ConversationArtifactSources,
    ConversationArtifactVerification, ConversationImageRequest, ConversationImageResult,
};

#[derive(Default)]
struct Observation(SourceCaptureControl);

impl Drop for Observation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl SessionCatalog {
    /// Inspect one recent display page without opening execution. Each Flow scan/decode step
    /// yields the storage worker. Session catalog registration uses the original complete read.
    pub async fn read_recent_conversation(
        &self,
        request: RecentConversationRequest,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<Result<RecentConversationPage, ConversationCapacity>> {
        authorize(&request.flow)?;
        let Ok(permit) = READERS.try_acquire() else {
            return Ok(Err(ConversationCapacity {
                kind: ConversationCapacityKind::ReadersBusy,
                required_at_least: MAX_READERS + 1,
                allowed: MAX_READERS,
                restart_required: true,
            }));
        };
        let permit = Arc::new(permit);
        let observation = Observation::default();
        let mut source = match self
            .prepare_conversation(request.clone(), &permit, &observation.0)
            .await?
        {
            Ok(source) => source,
            Err(value) => return Ok(Err(value)),
        };

        if let Err(error) = authorize(&request.flow) {
            crate::read_work::retire((source, Arc::clone(&permit)));
            return Err(error);
        }
        let flow = request.flow.clone();
        let control = observation.0.clone();
        let started = read_step(&permit, move || {
            Ok(source
                .start_page(request, control)?
                .map(|page| (source, page)))
        })
        .await?;
        let mut page;
        (source, page) = match started {
            Ok(started) => started,
            Err(value) => return Ok(Err(restart_capacity(value))),
        };
        let budget = inspection_budget(RecentConversationLimits::default());
        loop {
            let result = read_step(&permit, move || {
                Ok(match page.step(&mut source, budget)? {
                    ConversationPageStep::Loading => ControlFlow::Continue((source, page)),
                    ConversationPageStep::Capacity(value) => {
                        ControlFlow::Break(Err(restart_capacity(value)))
                    }
                    ConversationPageStep::Ready(page) => ControlFlow::Break(Ok(page)),
                })
            })
            .await?;
            match result {
                ControlFlow::Continue(next) => (source, page) = next,
                ControlFlow::Break(Err(value)) => return Ok(Err(value)),
                ControlFlow::Break(Ok(page)) => {
                    authorize(&flow)?;
                    return Ok(Ok(page));
                }
            }
        }
    }

    async fn prepare_conversation(
        &self,
        request: RecentConversationRequest,
        permit: &Arc<SemaphorePermit<'static>>,
        control: &SourceCaptureControl,
    ) -> io::Result<Result<Box<PreparedConversationSource>, ConversationCapacity>> {
        request.validate()?;
        let data = self.recorded_root()?;
        let control = control.clone();
        let limits = RecentConversationLimits::default();
        let budget = inspection_budget(limits);
        let prepared = read_step(permit, move || {
            let session = RuntimeStore::inspect_session(&data, request.flow.session_id.0)?;
            let pending = session.prepare_recent_conversation(request, limits, control)?;
            let captured = usize::try_from(pending.captured_bytes()?).unwrap_or(usize::MAX);
            if captured > MAX_FLOW_SCAN_BYTES {
                return Ok(Err(ConversationCapacity {
                    kind: ConversationCapacityKind::ScanBytes,
                    required_at_least: captured,
                    allowed: MAX_FLOW_SCAN_BYTES,
                    restart_required: true,
                }));
            }
            Ok(Ok(Box::new(pending)))
        })
        .await?;
        let mut pending = match prepared {
            Ok(pending) => pending,
            Err(value) => return Ok(Err(value)),
        };
        loop {
            let result = read_step(permit, move || {
                Ok(match pending.step(budget)? {
                    ConversationPreparationStep::Loading(_) => ControlFlow::Continue(pending),
                    ConversationPreparationStep::Capacity(value) => {
                        ControlFlow::Break(Err(restart_capacity(value)))
                    }
                    ConversationPreparationStep::Ready(source) => ControlFlow::Break(Ok(source)),
                })
            })
            .await?;
            match result {
                ControlFlow::Continue(next) => pending = next,
                ControlFlow::Break(result) => return Ok(result),
            }
        }
    }
}

fn inspection_budget(limits: RecentConversationLimits) -> InspectionBudget {
    InspectionBudget {
        work_bytes: limits
            .source_record_bytes
            .saturating_mul(NonZeroUsize::new(2).unwrap()),
        max_batches: NonZeroUsize::new(128).unwrap(),
        max_record_bytes: limits.source_record_bytes,
    }
}

async fn read_step<T: Send + 'static>(
    permit: &Arc<SemaphorePermit<'static>>,
    operation: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let permit = Arc::clone(permit);
    // Abandoned work and unread results retain admission until their worker-side retirement.
    let (result, _permit) = crate::read_work::capture(move || (operation(), permit))
        .await
        .map_err(io::Error::other)?;
    result
}

fn restart_capacity(mut value: ConversationCapacity) -> ConversationCapacity {
    value.restart_required = true;
    value
}
