//! Fork source review uses the attached native Flow and its committed history.

use std::io;

use atman_proto::{
    JsonRpcError,
    flows::FlowCursor,
    history::{ConversationCutoff, ForkSourceReviewRequest, ForkSourceReviewResponse},
};
use atman_runtime::{
    flow_runtime::source_capture::{
        FORK_STAGING_CAPACITY_BYTES, ForkBoundaryRequest, ForkReadBudget, SourceCaptureControl,
    },
    flow_store::source_capture::ForkCutoff,
    storage::ProjectContext,
};
use serde_json::Value;

use super::{core_address, events, history_wire, wire};

mod create;
pub(crate) use create::completed_retry;
pub(super) use create::{create, status, validate_create, validate_status};

pub(super) fn validate(request: &ForkSourceReviewRequest) -> Result<(), JsonRpcError> {
    let observed = &request.observed;
    let valid_receipt = |cursor: &FlowCursor| {
        cursor.journal_id == observed.flow.instance_id
            && cursor.sequence > 0
            && cursor.digest.len() == 64
            && cursor.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    };
    if observed.branch.is_nil()
        || observed.branch != request.expected_head.branch
        || !valid_receipt(&observed.cursor)
    {
        return Err(JsonRpcError::invalid_params(
            "Fork review requires the exact displayed source",
        ));
    }
    let boundary = match &request.cutoff {
        None => return Ok(()),
        Some(ConversationCutoff::BeforeUserTurn {
            lease,
            input,
            receipt,
        }) if !input.is_nil() => Some((lease, receipt)),
        Some(ConversationCutoff::AfterCompletedTurn { lease, receipt }) => Some((lease, receipt)),
        _ => None,
    };
    if boundary.is_some_and(|(lease, receipt)| {
        lease.flow == observed.flow
            && valid_receipt(receipt)
            && receipt.sequence <= observed.cursor.sequence
    }) {
        Ok(())
    } else {
        Err(JsonRpcError::invalid_params(
            "Fork review requires a committed boundary of the displayed Flow",
        ))
    }
}

struct ReviewCancellation(SourceCaptureControl);

impl Drop for ReviewCancellation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn review(
    state: &crate::DaemonState,
    request: ForkSourceReviewRequest,
) -> anyhow::Result<Value> {
    anyhow::ensure!(!state.is_stopping(), "daemon is stopping");
    let service = state
        .session_service
        .get()
        .ok_or_else(|| anyhow::anyhow!("open the source Session before reviewing a Fork"))?;
    let flow = core_address(request.observed.flow.clone());
    service.host.flow(&flow).await?;
    let cancel = ReviewCancellation(SourceCaptureControl::default());
    let source = service
        .host
        .review_fork_source(
            ForkBoundaryRequest {
                observed: history_wire::core_source(request.observed),
                expected_head: wire::core_head(request.expected_head),
                cutoff: request.cutoff.map_or(ForkCutoff::LatestSafe, |cutoff| {
                    ForkCutoff::At(Box::new(history_wire::core_cutoff(cutoff)))
                }),
            },
            ForkReadBudget::default(),
            FORK_STAGING_CAPACITY_BYTES,
            cancel.0.clone(),
            move |address| {
                if address != &flow {
                    return Err(io::ErrorKind::PermissionDenied.into());
                }
                Ok(())
            },
        )
        .await?;
    let origin = source.origin;
    let review = source.product;
    let boundary = review.boundary;
    let project_root = |project: ProjectContext| project.root().to_string_lossy().into_owned();
    let response = ForkSourceReviewResponse {
        observed: history_wire::source(&boundary.observed),
        head: wire::head(&boundary.head),
        source: history_wire::source(&source.source),
        selected: boundary
            .boundary
            .as_ref()
            .map(|boundary| history_wire::cutoff(&boundary.selected)),
        admission: boundary
            .boundary
            .and_then(|boundary| boundary.admission)
            .as_ref()
            .map(events::cursor),
        excluded_turns: boundary.excluded_turns,
        catalog: events::cursor(&origin.catalog),
        session_project: origin.session_project.map(project_root),
        flow_project: origin.flow_project.map(project_root),
        working_directory: origin
            .execution
            .and_then(|binding| binding.authority.workspace_root)
            .map(|path| path.to_string_lossy().into_owned()),
        history_records: review.history_records,
        snapshot_bytes: review.snapshot_bytes,
        asset_copy_bytes: review.asset_bytes,
    };
    Ok(serde_json::to_value(response)?)
}
