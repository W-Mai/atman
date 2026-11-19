//! Project the loaded Flow owner's current User queue.

use std::{collections::HashMap, sync::OnceLock};

use atman_proto::{
    JsonRpcError, PermissionGroupView,
    approvals::{
        ApprovalExecutionBoundary, ApprovalPathOrigin, ApprovalRisk, ApprovalTier,
        FlowApprovalDetail, FlowApprovalQueueResponse, FlowApprovalRow,
        ReadFlowApprovalQueueRequest,
    },
};
use atman_runtime::{
    permission::{ExecutionBoundary, PermissionGroup, PermissionRequest},
    redact::Redactor,
    tool::{PathOrigin, Tier},
    trust::RiskKind,
};
use serde_json::Value;

use super::core_address;
use crate::DaemonState;

const MAX_ROWS: usize = 64;
const MAX_TARGETS: usize = 12;
const MAX_PREVIEW_BYTES: usize = 8192;
const MAX_TEXT_BYTES: usize = 512;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
static BUILTIN_REDACTOR: OnceLock<Redactor> = OnceLock::new();

pub(super) fn snapshot(
    state: &DaemonState,
    request: ReadFlowApprovalQueueRequest,
) -> Result<Value, JsonRpcError> {
    let flow = core_address(request.flow.clone());
    let worker = state
        .session_service
        .get()
        .and_then(|service| service.host.session(flow.session_id.0).ok())
        .and_then(|session| session.loaded_flow(&flow.instance_id));
    let Some(worker) = worker else {
        return encode(FlowApprovalQueueResponse::Unavailable { flow: request.flow });
    };
    let context = worker.context();
    let Some((mut requests, mut groups)) = context.permission_broker().inspect_user_queue(
        &context.id().to_string(),
        MAX_ROWS,
        copy_queue,
    ) else {
        return encode(FlowApprovalQueueResponse::Capacity { flow: request.flow });
    };
    let configured = context.sink().redactor();
    let redactor = configured
        .as_deref()
        .unwrap_or_else(|| BUILTIN_REDACTOR.get_or_init(Redactor::builtin));
    for request in &mut requests {
        redact_row(request, redactor);
    }
    for group in &mut groups {
        group.label = redacted(redactor, &group.label, MAX_TEXT_BYTES).unwrap_or_default();
    }
    let response = FlowApprovalQueueResponse::Ready {
        flow: request.flow.clone(),
        requests,
        groups,
    };
    let value = encode(response)?;
    if serde_json::to_vec(&value)
        .map_err(|_| JsonRpcError::internal("approval queue could not be encoded"))?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return encode(FlowApprovalQueueResponse::Capacity { flow: request.flow });
    }
    Ok(value)
}

fn encode(response: FlowApprovalQueueResponse) -> Result<Value, JsonRpcError> {
    serde_json::to_value(response)
        .map_err(|_| JsonRpcError::internal("approval queue could not be encoded"))
}

fn copy_queue(
    mut requests: Vec<&PermissionRequest>,
    groups: Vec<&PermissionGroup>,
) -> (Vec<FlowApprovalRow>, Vec<PermissionGroupView>) {
    requests.sort_by_key(|request| (request.requested_at, request.request_id.0));
    let revisions = requests
        .iter()
        .map(|request| (request.request_id.0, request.revision))
        .collect::<HashMap<_, _>>();
    let groups = groups
        .into_iter()
        .filter(|group| {
            group.request_ids.iter().all(|id| {
                revisions
                    .get(&id.0)
                    .is_some_and(|revision| group.member_revisions.get(id) == Some(revision))
            })
        })
        .map(|group| PermissionGroupView {
            group_id: group.group_id.0,
            label: bounded(&group.label, MAX_TEXT_BYTES).unwrap_or_default(),
            request_ids: group.request_ids.iter().map(|id| id.0).collect(),
            revision: group.revision,
        })
        .collect();
    (requests.into_iter().map(row).collect(), groups)
}

fn bounded(text: &str, limit: usize) -> Option<String> {
    (text.len() <= limit).then(|| text.to_owned())
}

fn redacted(redactor: &Redactor, text: &str, limit: usize) -> Option<String> {
    let result = redactor.redact(text).0;
    (result.len() <= limit).then_some(result)
}

fn redact_row(row: &mut FlowApprovalRow, redactor: &Redactor) {
    // A redacted ID cannot correlate with the original tool call.
    row.tool_use_id = row
        .tool_use_id
        .take()
        .filter(|id| redactor.scan(id).is_empty());
    let detail = &mut row.detail;
    let tool = redacted(redactor, &row.tool, MAX_TEXT_BYTES);
    detail.incomplete |= tool.is_none();
    row.tool = tool.unwrap_or_default();
    let fields = [
        (&mut detail.intent, MAX_TEXT_BYTES),
        (&mut detail.preview, MAX_PREVIEW_BYTES),
        (&mut detail.cwd, MAX_TEXT_BYTES),
    ];
    for (field, limit) in fields {
        if let Some(text) = field.take() {
            *field = redacted(redactor, &text, limit);
            detail.incomplete |= field.is_none();
        }
    }
    let target_count = detail.targets.len();
    detail.targets.retain_mut(|path| {
        if let Some(text) = redacted(redactor, path, MAX_TEXT_BYTES) {
            *path = text;
            true
        } else {
            false
        }
    });
    detail.incomplete |= detail.targets.len() != target_count;
}

/// Copy limited display values; redaction runs after the broker lock is released.
#[derive(Default)]
struct BoundedReview {
    incomplete: bool,
}

impl BoundedReview {
    fn text(&mut self, value: &str, limit: usize) -> Option<String> {
        let text = bounded(value, limit);
        self.incomplete |= text.is_none();
        text
    }

    fn path(&mut self, path: &std::path::Path) -> Option<String> {
        if path.as_os_str().len() > MAX_TEXT_BYTES {
            self.incomplete = true;
            return None;
        }
        self.text(&path.to_string_lossy(), MAX_TEXT_BYTES)
    }
}

fn row(request: &PermissionRequest) -> FlowApprovalRow {
    let execution_boundary = request
        .pending_process_boundary()
        .map(|boundary| match boundary {
            ExecutionBoundary::Direct => ApprovalExecutionBoundary::Direct,
            ExecutionBoundary::Sandboxed => ApprovalExecutionBoundary::Sandboxed,
        });
    let intent = &request.intent;
    let provenance = &intent.provenance;
    let mut review = BoundedReview::default();
    let tool_use_id = review.text(&intent.tool_use_id, MAX_TEXT_BYTES);
    let tool = review
        .text(&intent.tool_name, MAX_TEXT_BYTES)
        .unwrap_or_default();
    let preview = intent
        .preview
        .as_deref()
        .and_then(|preview| review.text(preview, MAX_PREVIEW_BYTES));
    let intent_text = intent
        .call_intent
        .as_ref()
        .and_then(|intent| review.text(intent.as_str(), MAX_TEXT_BYTES));
    let cwd = provenance.cwd.as_deref().and_then(|path| review.path(path));
    let mut targets = Vec::new();
    for (index, path) in provenance.authorized_targets().enumerate() {
        if index == MAX_TARGETS {
            review.incomplete = true;
            break;
        }
        if let Some(path) = review.path(path) {
            targets.push(path);
        }
    }
    let risks = intent.risks.iter().copied().map(risk).collect();
    FlowApprovalRow {
        request_id: request.request_id.0,
        revision: request.revision,
        requesting_run_id: request.requesting_run_id.clone().into(),
        tool_use_id,
        tool,
        tier: match intent.tier {
            Tier::Zero => ApprovalTier::Zero,
            Tier::One => ApprovalTier::One,
            Tier::Two => ApprovalTier::Two,
            Tier::Three => ApprovalTier::Three,
            Tier::Four => ApprovalTier::Four,
        },
        detail: FlowApprovalDetail {
            intent: intent_text,
            preview,
            cwd,
            targets,
            path_origin: provenance.path_origin.map(path_origin),
            risks,
            network: provenance.network,
            execution_boundary,
            incomplete: review.incomplete,
        },
    }
}

fn risk(value: RiskKind) -> ApprovalRisk {
    match value {
        RiskKind::WorkspaceExternal => ApprovalRisk::WorkspaceExternal,
        RiskKind::Network => ApprovalRisk::Network,
        RiskKind::Irreversible => ApprovalRisk::Irreversible,
        RiskKind::FilesystemWrite => ApprovalRisk::FilesystemWrite,
        RiskKind::ProcessSpawn => ApprovalRisk::ProcessSpawn,
        RiskKind::RepositoryMutation => ApprovalRisk::RepositoryMutation,
    }
}

fn path_origin(value: PathOrigin) -> ApprovalPathOrigin {
    match value {
        PathOrigin::Omitted => ApprovalPathOrigin::Omitted,
        PathOrigin::Relative => ApprovalPathOrigin::Relative,
        PathOrigin::ExplicitInside => ApprovalPathOrigin::ExplicitInside,
        PathOrigin::ExplicitExternal => ApprovalPathOrigin::ExplicitExternal,
        PathOrigin::Unbound => ApprovalPathOrigin::Unbound,
    }
}
