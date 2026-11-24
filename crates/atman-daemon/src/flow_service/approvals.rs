//! Project the loaded Flow owner's current User queue.

use std::{
    collections::{BTreeMap, HashMap},
    io,
    sync::{Arc, OnceLock},
};

use atman_proto::{
    JsonRpcError, PermissionGroupView,
    approvals::{
        ApplyFlowApprovalRequest, ApprovalDecisionAction, ApprovalDecisionView,
        ApprovalExecutionBoundary, ApprovalPathOrigin, ApprovalRisk, ApprovalTier,
        FlowApprovalDecision, FlowApprovalDetail, FlowApprovalOperation,
        FlowApprovalOperationResponse, FlowApprovalQueueResponse, FlowApprovalRow,
        FlowApprovalSelection, ReadFlowApprovalQueueRequest,
    },
    flows::FlowAddress,
};
use atman_runtime::{
    Session,
    flow_runtime::catalog::SessionCatalog,
    permission::{
        BatchResolution, ExecutionBoundary, PermissionAction, PermissionError, PermissionGroup,
        PermissionGroupId, PermissionRequest, PermissionRequestId, UserApprovalOperation,
        UserApprovalSelection,
    },
    redact::Redactor,
    tool::{PathOrigin, Tier},
    trust::RiskKind,
};
use serde_json::Value;
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::core_address;
use crate::DaemonState;

const MAX_ROWS: usize = 64;
const MAX_TARGETS: usize = 12;
const MAX_PREVIEW_BYTES: usize = 8192;
const MAX_TEXT_BYTES: usize = 512;
const MAX_RESPONSE_BYTES: usize = 512 * 1024;
static BUILTIN_REDACTOR: OnceLock<Redactor> = OnceLock::new();
static APPROVAL_SLOTS: Semaphore = Semaphore::const_new(16);

pub(super) fn tool_call_key(
    flow: &atman_proto::flows::FlowAddress,
    run: uuid::Uuid,
    id: &str,
) -> Option<String> {
    if id.is_empty() || id.len() > MAX_TEXT_BYTES {
        return None;
    }
    // Keys are presentation-only and change when the host process restarts.
    // Never make provider tool IDs part of persisted transcript responses.
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    let mut hash = blake3::Hasher::new_keyed(KEY.get_or_init(|| {
        let mut key = [0; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key
    }));
    hash.update(flow.session_id.0.as_bytes());
    hash.update(flow.instance_id.as_bytes());
    hash.update(run.as_bytes());
    hash.update(id.as_bytes());
    Some(hash.finalize().to_hex().to_string())
}

pub(super) fn snapshot(
    state: &DaemonState,
    request: ReadFlowApprovalQueueRequest,
) -> Result<Value, JsonRpcError> {
    project_queue(state, request, false)
}

pub(crate) fn device_snapshot(
    state: &DaemonState,
    request: ReadFlowApprovalQueueRequest,
) -> Result<Value, JsonRpcError> {
    project_queue(state, request, true)
}

fn loaded_context(state: &DaemonState, target: &FlowAddress) -> Option<Arc<Session>> {
    let flow = core_address(target.clone());
    state
        .session_service
        .get()
        .and_then(|service| service.host.session(flow.session_id.0).ok())
        .and_then(|session| session.loaded_flow(&flow.instance_id))
        .map(|worker| worker.context())
}

fn project_queue(
    state: &DaemonState,
    request: ReadFlowApprovalQueueRequest,
    scoped: bool,
) -> Result<Value, JsonRpcError> {
    let Some(context) = loaded_context(state, &request.flow) else {
        return encode(FlowApprovalQueueResponse::Unavailable { flow: request.flow });
    };
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
    let request_addr = &request.flow;
    for request in &mut requests {
        redact_row(request, redactor);
        if scoped {
            request.same_tool_grant = None;
            request.workspace_relative_grant_path = None;
        }
        request.tool_call_key = request
            .tool_use_id
            .as_deref()
            .and_then(|id| tool_call_key(request_addr, request.requesting_run_id.0, id));
    }
    for group in &mut groups {
        group.label = redacted(redactor, &group.label, MAX_TEXT_BYTES).unwrap_or_default();
    }
    let response = FlowApprovalQueueResponse::Ready {
        flow: request.flow.clone(),
        decision_receipts: Some(true),
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

fn encode(response: impl serde::Serialize) -> Result<Value, JsonRpcError> {
    serde_json::to_value(response)
        .map_err(|_| JsonRpcError::internal("approval response could not be encoded"))
}

pub(crate) async fn apply(
    state: Arc<DaemonState>,
    request: ApplyFlowApprovalRequest,
    principal: Uuid,
    authorize: impl FnOnce() -> io::Result<()> + Send + 'static,
) -> Result<Value, JsonRpcError> {
    validate_operation(&request.operation)?;
    let slot = APPROVAL_SLOTS
        .try_acquire()
        .map_err(|_| JsonRpcError::application("approval decisions are busy"))?;
    tokio::spawn(async move {
        let _slot = slot;
        let host = state
            .session_service
            .get()
            .map(|service| &service.host)
            .ok_or_else(owner_unavailable)?;
        let _operation = host
            .retain_host_operation()
            .map_err(crate::permission_error)?;
        let session = host
            .session(request.operation.flow.session_id.0)
            .map_err(|_| owner_unavailable())?;
        let _owner = session
            .retain_operation()
            .await
            .map_err(crate::permission_error)?;
        let context = session
            .loaded_flow(&core_address(request.operation.flow.clone()).instance_id)
            .map(|worker| worker.context())
            .ok_or_else(owner_unavailable)?;
        let retained = context.clone();
        let action = crate::permission_action(request.action);
        let selection = match request.selection {
            FlowApprovalSelection::Request {
                request_id,
                revision,
            } => UserApprovalSelection::Requests(BTreeMap::from([(
                PermissionRequestId(request_id),
                revision,
            )])),
            FlowApprovalSelection::Group { group_id, revision } => {
                UserApprovalSelection::Group(PermissionGroupId(group_id), revision)
            }
        };
        let operation = UserApprovalOperation {
            principal,
            id: request.operation.operation_id,
            selection,
            action,
        };
        let results = context
            .apply_instance_effect(move || {
                authorize().map_err(|_| {
                    JsonRpcError::application("approval authorization is no longer valid")
                })?;
                let configured = retained.sink().redactor();
                let redactor = configured
                    .as_deref()
                    .unwrap_or_else(|| BUILTIN_REDACTOR.get_or_init(Redactor::builtin));
                retained
                    .permission_broker()
                    .resolve_user_approval(&retained.id().to_string(), operation, |request| {
                        if action == PermissionAction::Approve {
                            let mut review = row(request);
                            redact_row(&mut review, redactor);
                            if review.detail.incomplete {
                                return Err(PermissionError::ActorNotAuthorized);
                            }
                        }
                        Ok(())
                    })
                    .map_err(crate::permission_error)
            })
            .await
            .map_err(crate::permission_error)??;
        operation_response(request.operation, Some(live_decisions(results)?))
    })
    .await
    .map_err(|_| JsonRpcError::application("approval outcome is unknown"))?
}

pub(crate) async fn status(
    state: &DaemonState,
    operation: FlowApprovalOperation,
    principal: Uuid,
) -> Result<Value, JsonRpcError> {
    validate_operation(&operation)?;
    let live = loaded_context(state, &operation.flow).and_then(|context| {
        context.permission_broker().user_approval_status(
            &context.id().to_string(),
            principal,
            operation.operation_id,
        )
    });
    if let Some(results) = live {
        return operation_response(operation, Some(live_decisions(results)?));
    }
    let catalog = SessionCatalog::new(None, Some(state.data_dir().to_path_buf()));
    let results = match catalog
        .observe_approval_receipt(
            &core_address(operation.flow.clone()),
            principal,
            operation.operation_id,
        )
        .await
    {
        Ok(results) => results,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => return Err(JsonRpcError::application("approval receipt is unavailable")),
    };
    let decisions = results.map(|results| {
        results
            .into_iter()
            .map(|result| FlowApprovalDecision {
                request_id: result.request_id,
                decision: ApprovalDecisionView {
                    decision_id: result.decision_id,
                    action: match result.action {
                        PermissionAction::Approve => ApprovalDecisionAction::Approved,
                        PermissionAction::Deny => ApprovalDecisionAction::Denied,
                        PermissionAction::Defer => ApprovalDecisionAction::Cancelled,
                    },
                },
            })
            .collect()
    });
    operation_response(operation, decisions)
}

fn validate_operation(operation: &FlowApprovalOperation) -> Result<(), JsonRpcError> {
    if operation.operation_id.is_nil()
        || operation.flow.session_id.0.is_nil()
        || operation.flow.instance_id.is_nil()
    {
        return Err(JsonRpcError::invalid_params(
            "approval identities must not be nil",
        ));
    }
    Ok(())
}

fn owner_unavailable() -> JsonRpcError {
    JsonRpcError::application("approval owner is unavailable")
}

fn live_decisions(
    results: Vec<BatchResolution>,
) -> Result<Vec<FlowApprovalDecision>, JsonRpcError> {
    results
        .into_iter()
        .map(|result| {
            let decision = crate::approval_decision_view(&result.outcome)
                .ok_or_else(|| JsonRpcError::internal("approval decision receipt is missing"))?;
            Ok(FlowApprovalDecision {
                request_id: result.request_id.0,
                decision,
            })
        })
        .collect()
}

fn operation_response(
    operation: FlowApprovalOperation,
    decisions: Option<Vec<FlowApprovalDecision>>,
) -> Result<Value, JsonRpcError> {
    encode(FlowApprovalOperationResponse {
        operation,
        decisions,
    })
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
    let exact_tool = !row.tool.is_empty() && redactor.scan(&row.tool).is_empty();
    row.same_tool_grant = exact_tool.then_some(true);
    row.workspace_relative_grant_path = row
        .workspace_relative_grant_path
        .take()
        .filter(|path| exact_tool && redactor.scan(path).is_empty());
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
        tool_call_key: None,
        workspace_relative_grant_path: intent
            .provenance
            .workspace_relative_path()
            .filter(|path| path.len() <= MAX_TEXT_BYTES),
        same_tool_grant: None,
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
