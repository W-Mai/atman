use std::collections::BTreeSet;
use std::sync::Arc;

use uuid::Uuid;

use crate::error::RuntimeError;
use crate::permission::{
    ApprovalTarget, BatchMode, DecisionAuthority, GrantScope, GroupOwner, PermissionAction,
    PermissionBroker, PermissionGroup, PermissionGroupId, PermissionRequest, PermissionRequestId,
    PermissionSelector, ResolveOutcome,
};
use crate::tool::{BoxFut, InvocationPlane, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::trust::RiskKind;
use crate::value::Value;

pub struct PermissionList;
pub struct PermissionGet;
pub struct PermissionGroupTool;
pub struct PermissionUngroup;
pub struct PermissionApprove;
pub struct PermissionDeny;
pub struct PermissionDefer;
pub struct PermissionBatch;

fn broker_and_actor<'a>(
    ctx: &'a ToolCtx,
    name: &str,
) -> Result<
    (
        &'a Arc<PermissionBroker>,
        &'a Arc<crate::flow_authority::FlowIdentity>,
    ),
    RuntimeError,
> {
    let broker = ctx
        .permission_broker
        .as_ref()
        .ok_or_else(|| failed(name, "permission broker is missing"))?;
    let actor = ctx
        .flow_identity
        .as_ref()
        .ok_or_else(|| failed(name, "permission identity is missing"))?;
    broker
        .authenticate_control_actor(actor)
        .map_err(|error| failed(name, error))?;
    Ok((broker, actor))
}

fn failed(name: &str, error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("{name}: {error}"))
}

fn string_arg(args: &ToolArgs, name: &str, position: usize) -> Result<String, RuntimeError> {
    let value = args.named(name).or_else(|| args.positional.get(position));
    match value {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn optional_string(args: &ToolArgs, name: &str) -> Result<Option<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn request_id(args: &ToolArgs) -> Result<PermissionRequestId, RuntimeError> {
    parse_request_id(&string_arg(args, "request_id", 0)?)
}

fn parse_request_id(value: &str) -> Result<PermissionRequestId, RuntimeError> {
    Uuid::parse_str(value)
        .map(PermissionRequestId)
        .map_err(|_| failed("permission", "request_id is not a valid UUID"))
}

fn group_id(args: &ToolArgs) -> Result<PermissionGroupId, RuntimeError> {
    Uuid::parse_str(&string_arg(args, "group_id", 0)?)
        .map(PermissionGroupId)
        .map_err(|_| failed("permission", "group_id is not a valid UUID"))
}

fn request_ids(
    args: &ToolArgs,
    required: bool,
) -> Result<BTreeSet<PermissionRequestId>, RuntimeError> {
    let Some(value) = args
        .named("request_ids")
        .or_else(|| args.positional.first())
    else {
        return if required {
            Err(RuntimeError::MissingArg("request_ids".into()))
        } else {
            Ok(BTreeSet::new())
        };
    };
    let Value::List(values) = value else {
        return Err(RuntimeError::TypeMismatch {
            expected: "list".into(),
            actual: value.kind_name().into(),
        });
    };
    values
        .iter()
        .map(|value| match value {
            Value::Str(value) => parse_request_id(value),
            other => Err(RuntimeError::TypeMismatch {
                expected: "string".into(),
                actual: other.kind_name().into(),
            }),
        })
        .collect()
}

fn request_value(request: PermissionRequest) -> Value {
    let (state, target) = match request.state {
        crate::permission::PermissionRequestState::Evaluating => ("evaluating", Value::Unit),
        crate::permission::PermissionRequestState::Pending { target } => {
            let target = match target {
                crate::permission::ApprovalTarget::Flow(run_id) => run_id.to_string(),
                crate::permission::ApprovalTarget::User => "user".into(),
            };
            ("pending", Value::Str(target))
        }
        crate::permission::PermissionRequestState::Approved { .. } => ("approved", Value::Unit),
        crate::permission::PermissionRequestState::Denied { .. } => ("denied", Value::Unit),
        crate::permission::PermissionRequestState::Cancelled { .. } => ("cancelled", Value::Unit),
    };
    Value::Struct(vec![
        (
            "request_id".into(),
            Value::Str(request.request_id.to_string()),
        ),
        (
            "requesting_run_id".into(),
            Value::Str(request.requesting_run_id.to_string()),
        ),
        ("tool_use_id".into(), Value::Str(request.intent.tool_use_id)),
        ("tool_name".into(), Value::Str(request.intent.tool_name)),
        (
            "tier".into(),
            Value::Str(format!("{:?}", request.intent.tier).to_lowercase()),
        ),
        ("state".into(), Value::Str(state.into())),
        ("target".into(), target),
        (
            "requested_at".into(),
            Value::Str(request.requested_at.to_rfc3339()),
        ),
    ])
}

fn group_value(group: PermissionGroup) -> Value {
    let owner = match group.owner {
        GroupOwner::Flow(run_id) => run_id.to_string(),
        GroupOwner::User => "user".into(),
        GroupOwner::System => "system".into(),
    };
    Value::Struct(vec![
        ("group_id".into(), Value::Str(group.group_id.to_string())),
        ("owner".into(), Value::Str(owner)),
        ("label".into(), Value::Str(group.label)),
        (
            "request_ids".into(),
            Value::List(
                group
                    .request_ids
                    .into_iter()
                    .map(|id| Value::Str(id.to_string()))
                    .collect(),
            ),
        ),
        (
            "created_at".into(),
            Value::Str(group.created_at.to_rfc3339()),
        ),
        ("revision".into(), Value::Int(group.revision as i64)),
    ])
}

fn decision_value(outcome: ResolveOutcome) -> Value {
    let decision = match outcome {
        ResolveOutcome::Resolved(decision) | ResolveOutcome::Deferred(decision) => decision,
    };
    Value::Struct(vec![
        (
            "decision_id".into(),
            Value::Str(decision.decision_id.to_string()),
        ),
        (
            "request_id".into(),
            Value::Str(decision.request_id.to_string()),
        ),
        (
            "action".into(),
            Value::Str(format!("{:?}", decision.action).to_lowercase()),
        ),
    ])
}

fn control_tool_defaults() -> (Tier, InvocationPlane) {
    (Tier::Zero, InvocationPlane::PermissionControl)
}

fn batch_value(result: crate::permission::BatchResolution) -> Value {
    let (status, decision) = match result.outcome {
        crate::permission::BatchRequestOutcome::Approved(decision) => ("approved", Some(decision)),
        crate::permission::BatchRequestOutcome::Denied(decision) => ("denied", Some(decision)),
        crate::permission::BatchRequestOutcome::Deferred(decision) => ("deferred", Some(decision)),
        crate::permission::BatchRequestOutcome::SkippedAlreadyResolved => {
            ("skipped_already_resolved", None)
        }
        crate::permission::BatchRequestOutcome::RejectedNotAncestor => {
            ("rejected_not_ancestor", None)
        }
        crate::permission::BatchRequestOutcome::RejectedOverAuthority => {
            ("rejected_over_authority", None)
        }
        crate::permission::BatchRequestOutcome::RejectedStale => ("rejected_stale", None),
        crate::permission::BatchRequestOutcome::RejectedNotFound => ("rejected_not_found", None),
        crate::permission::BatchRequestOutcome::RejectedNotRunning => {
            ("rejected_not_running", None)
        }
        crate::permission::BatchRequestOutcome::RejectedPermissionManagementRequired => {
            ("rejected_permission_management_required", None)
        }
        crate::permission::BatchRequestOutcome::RejectedUnsupportedGrantScope => {
            ("rejected_unsupported_grant_scope", None)
        }
        crate::permission::BatchRequestOutcome::RejectedNoEscalationTarget => {
            ("rejected_no_escalation_target", None)
        }
        crate::permission::BatchRequestOutcome::Rejected(_) => ("rejected", None),
    };
    let mut fields = vec![
        (
            "request_id".into(),
            Value::Str(result.request_id.to_string()),
        ),
        ("status".into(), Value::Str(status.into())),
    ];
    if let Some(decision) = decision {
        fields.push((
            "decision_id".into(),
            Value::Str(decision.decision_id.to_string()),
        ));
    }
    Value::Struct(fields)
}

impl Tool for PermissionBatch {
    fn name(&self) -> &str {
        "permission.batch"
    }
    fn tier(&self) -> Tier {
        control_tool_defaults().0
    }
    fn invocation_plane(&self) -> InvocationPlane {
        control_tool_defaults().1
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "request_ids":{"type":"array","items":{"type":"string"}},
                "group_id":{"type":"string"},
                "descendant_run_id":{"type":"string"},
                "selector":{"type":"string","enum":["child","tool","tier","risk","path","target"]},
                "selector_value":{"type":"string"},
                "action":{"type":"string","enum":["approve","deny","defer"]},
                "atomic":{"type":"boolean"},
                "group_revision":{"type":"integer","minimum":0},
                "reason":{"type":"string"}
            },
            "required":["action"],
            "oneOf":[
                {"required":["request_ids"]},
                {"required":["group_id"]},
                {"required":["descendant_run_id"]},
                {"required":["selector"]}
            ],
            "additionalProperties":false
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (broker, actor) = broker_and_actor(ctx, self.name())?;
            for field in [
                "session_id",
                "actor",
                "actor_run_id",
                "run_id",
                "requester_run_id",
            ] {
                if args.named(field).is_some() {
                    return Err(failed(
                        self.name(),
                        format!("caller-supplied {field} is not allowed"),
                    ));
                }
            }
            let selector_count = [
                args.named("request_ids").is_some(),
                args.named("group_id").is_some(),
                args.named("descendant_run_id").is_some(),
                args.named("selector").is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if selector_count != 1 {
                return Err(failed(
                    self.name(),
                    "exactly one of request_ids, group_id, descendant_run_id, or selector is required",
                ));
            }
            let selector = if args.named("request_ids").is_some() {
                PermissionSelector::RequestIds(request_ids(&args, true)?.into_iter().collect())
            } else if args.named("group_id").is_some() {
                PermissionSelector::Group(group_id(&args)?)
            } else if args.named("descendant_run_id").is_some() {
                let value = string_arg(&args, "descendant_run_id", 0)?;
                PermissionSelector::DescendantRun(crate::event::FlowRunId(
                    Uuid::parse_str(&value).map_err(|_| {
                        failed(self.name(), "descendant_run_id is not a valid UUID")
                    })?,
                ))
            } else {
                let kind = string_arg(&args, "selector", 0)?;
                let value = optional_string(&args, "selector_value")?;
                match kind.as_str() {
                    "child" if value.is_none() => PermissionSelector::ChildRun,
                    "tool" => PermissionSelector::Tool(value.ok_or_else(|| {
                        failed(self.name(), "tool selector requires selector_value")
                    })?),
                    "tier" => PermissionSelector::Tier(match value.as_deref() {
                        Some("zero") | Some("0") => Tier::Zero,
                        Some("one") | Some("1") => Tier::One,
                        Some("two") | Some("2") => Tier::Two,
                        Some("three") | Some("3") => Tier::Three,
                        Some("four") | Some("4") => Tier::Four,
                        _ => {
                            return Err(failed(
                                self.name(),
                                "tier selector_value must be zero through four",
                            ));
                        }
                    }),
                    "risk" => PermissionSelector::Risk(match value.as_deref() {
                        Some("workspace_external") => RiskKind::WorkspaceExternal,
                        Some("network") => RiskKind::Network,
                        Some("irreversible") => RiskKind::Irreversible,
                        Some("filesystem_write") => RiskKind::FilesystemWrite,
                        Some("process_spawn") => RiskKind::ProcessSpawn,
                        Some("repository_mutation") => RiskKind::RepositoryMutation,
                        _ => return Err(failed(self.name(), "unknown risk selector_value")),
                    }),
                    "path" => {
                        PermissionSelector::PathPrefix(crate::fs_access::canonicalize_stable(
                            std::path::Path::new(&value.ok_or_else(|| {
                                failed(self.name(), "path selector requires selector_value")
                            })?),
                        ))
                    }
                    "target" => PermissionSelector::Target(match value.as_deref() {
                        Some("flow") => ApprovalTarget::Flow(actor.run_id.clone()),
                        _ => {
                            return Err(failed(self.name(), "target selector_value must be flow"));
                        }
                    }),
                    "child" => {
                        return Err(failed(
                            self.name(),
                            "child selector does not accept selector_value",
                        ));
                    }
                    _ => return Err(failed(self.name(), "unknown selector")),
                }
            };
            let action = match string_arg(&args, "action", 1)?.as_str() {
                "approve" => PermissionAction::Approve,
                "deny" => PermissionAction::Deny,
                "defer" => PermissionAction::Defer,
                _ => {
                    return Err(failed(
                        self.name(),
                        "action must be approve, deny, or defer",
                    ));
                }
            };
            let mode = match args.named("atomic") {
                Some(Value::Bool(true)) => BatchMode::Atomic,
                Some(Value::Bool(false)) | None => BatchMode::BestEffort,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let revision = match args.named("group_revision") {
                Some(Value::Int(value)) if *value >= 0 => Some(*value as u64),
                Some(Value::Int(_)) => {
                    return Err(failed(self.name(), "group_revision must be non-negative"));
                }
                None => None,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "int".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            if revision.is_some() && !matches!(selector, PermissionSelector::Group(_)) {
                return Err(failed(
                    self.name(),
                    "group_revision requires the group_id selector",
                ));
            }
            let results = broker
                .resolve_batch(
                    actor,
                    selector,
                    action,
                    None,
                    optional_string(&args, "reason")?,
                    mode,
                    revision,
                )
                .map_err(|e| failed(self.name(), e))?;
            Ok(Value::List(results.into_iter().map(batch_value).collect()))
        })
    }
}

impl Tool for PermissionList {
    fn name(&self) -> &str {
        "permission.list"
    }
    fn tier(&self) -> Tier {
        control_tool_defaults().0
    }
    fn invocation_plane(&self) -> InvocationPlane {
        control_tool_defaults().1
    }
    fn description(&self) -> Option<&str> {
        Some("List permission requests currently targeted to this flow, plus its owned groups.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (broker, actor) = broker_and_actor(ctx, self.name())?;
            let requests = broker
                .visible_list(actor)
                .map_err(|e| failed(self.name(), e))?;
            let groups = broker
                .visible_group_list(actor)
                .map_err(|e| failed(self.name(), e))?;
            Ok(Value::Struct(vec![
                (
                    "requests".into(),
                    Value::List(requests.into_iter().map(request_value).collect()),
                ),
                (
                    "groups".into(),
                    Value::List(groups.into_iter().map(group_value).collect()),
                ),
            ]))
        })
    }
}

impl Tool for PermissionGet {
    fn name(&self) -> &str {
        "permission.get"
    }
    fn tier(&self) -> Tier {
        control_tool_defaults().0
    }
    fn invocation_plane(&self) -> InvocationPlane {
        control_tool_defaults().1
    }
    fn description(&self) -> Option<&str> {
        Some("Inspect one visible permission request or one group owned by this flow.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "request_id":{"type":"string"},
            "group_id":{"type":"string"}
        },"oneOf":[{"required":["request_id"]},{"required":["group_id"]}]})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (broker, actor) = broker_and_actor(ctx, self.name())?;
            if args.named("request_id").is_some() {
                let value = args.named("request_id").expect("checked above");
                let Value::Str(value) = value else {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: value.kind_name().into(),
                    });
                };
                let id = parse_request_id(value)?;
                return broker
                    .visible_get(actor, &id)
                    .map_err(|e| failed(self.name(), e))?
                    .map(request_value)
                    .ok_or_else(|| {
                        failed(
                            self.name(),
                            "permission request was not found or is not visible",
                        )
                    });
            }
            let id = group_id(&args)?;
            broker
                .visible_group_get(actor, &id)
                .map_err(|e| failed(self.name(), e))?
                .map(group_value)
                .ok_or_else(|| {
                    failed(
                        self.name(),
                        "permission group was not found or is not visible",
                    )
                })
        })
    }
}

impl Tool for PermissionGroupTool {
    fn name(&self) -> &str {
        "permission.group"
    }
    fn tier(&self) -> Tier {
        control_tool_defaults().0
    }
    fn invocation_plane(&self) -> InvocationPlane {
        control_tool_defaults().1
    }
    fn description(&self) -> Option<&str> {
        Some("Create an owned group from explicit visible permission request IDs.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "request_ids":{"type":"array","items":{"type":"string"},"minItems":1},
            "label":{"type":"string"}
        },"required":["request_ids","label"]})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (broker, actor) = broker_and_actor(ctx, self.name())?;
            let ids = request_ids(&args, true)?;
            let label = string_arg(&args, "label", 1)?;
            broker
                .create_group(actor, ids, label)
                .map(group_value)
                .map_err(|e| failed(self.name(), e))
        })
    }
}

impl Tool for PermissionUngroup {
    fn name(&self) -> &str {
        "permission.ungroup"
    }
    fn tier(&self) -> Tier {
        control_tool_defaults().0
    }
    fn invocation_plane(&self) -> InvocationPlane {
        control_tool_defaults().1
    }
    fn description(&self) -> Option<&str> {
        Some("Remove explicit members from an owned group, or delete it once empty.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{
            "group_id":{"type":"string"},
            "request_ids":{"type":"array","items":{"type":"string"}}
        },"required":["group_id"]})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (broker, actor) = broker_and_actor(ctx, self.name())?;
            let id = group_id(&args)?;
            if args.named("request_ids").is_none() {
                return broker
                    .delete_empty_group(actor, &id)
                    .map(group_value)
                    .map_err(|e| failed(self.name(), e));
            }
            let ids = request_ids(&args, false)?;
            broker
                .ungroup_requests(actor, &id, &ids)
                .map(group_value)
                .map_err(|e| failed(self.name(), e))
        })
    }
}

fn approve_scope(
    args: &ToolArgs,
    request: &PermissionRequest,
) -> Result<Option<GrantScope>, RuntimeError> {
    let Some(scope) = optional_string(args, "scope")? else {
        return Ok(None);
    };
    match scope.as_str() {
        "current_call" => Ok(Some(GrantScope::CurrentCall)),
        "child_run_same_tool" => Ok(Some(GrantScope::ChildRunSameTool {
            run_id: request.requesting_run_id.clone(),
            tool_name: request.intent.tool_name.clone(),
        })),
        "child_run_same_path_rule" => {
            let provenance = &request.intent.provenance;
            let root = provenance.workspace_root.as_deref().ok_or_else(|| {
                failed(
                    "permission.approve",
                    "request has no structured workspace-relative path",
                )
            })?;
            if provenance.authorized_targets().count() != 1 {
                return Err(failed(
                    "permission.approve",
                    "request must have exactly one structured path target",
                ));
            }
            let path = provenance
                .authorized_targets()
                .next()
                .and_then(|path| path.strip_prefix(root).ok())
                .and_then(|path| path.to_str())
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    failed(
                        "permission.approve",
                        "request has no structured workspace-relative path",
                    )
                })?
                .to_string();
            Ok(Some(GrantScope::ChildRunSamePathRule {
                run_id: request.requesting_run_id.clone(),
                tool_name: request.intent.tool_name.clone(),
                workspace_relative_path: path,
            }))
        }
        _ => Err(failed(
            "permission.approve",
            "scope must be current_call, child_run_same_tool, or child_run_same_path_rule",
        )),
    }
}

fn resolve(
    ctx: &ToolCtx,
    name: &str,
    args: &ToolArgs,
    action: PermissionAction,
) -> Result<Value, RuntimeError> {
    let (broker, actor) = broker_and_actor(ctx, name)?;
    let id = request_id(args)?;
    let authority = DecisionAuthority::Flow(
        broker
            .flow_authority(Arc::clone(actor))
            .map_err(|e| failed(name, e))?,
    );
    let request = broker
        .visible_get(actor, &id)
        .map_err(|e| failed(name, e))?
        .ok_or_else(|| failed(name, "permission request was not found or is not visible"))?;
    let scope = if action == PermissionAction::Approve {
        approve_scope(args, &request)?
    } else {
        None
    };
    let reason = if action == PermissionAction::Deny {
        Some(string_arg(args, "reason", 1)?)
    } else {
        optional_string(args, "reason")?
    };
    broker
        .resolve(&id, &authority, action, scope, reason)
        .map(decision_value)
        .map_err(|e| failed(name, e))
}

macro_rules! decision_tool {
    ($ty:ty, $name:literal, $action:expr, $schema:expr) => {
        impl Tool for $ty {
            fn name(&self) -> &str {
                $name
            }
            fn tier(&self) -> Tier {
                control_tool_defaults().0
            }
            fn invocation_plane(&self) -> InvocationPlane {
                control_tool_defaults().1
            }
            fn input_schema(&self) -> serde_json::Value {
                $schema
            }
            fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
                Box::pin(async move { resolve(ctx, self.name(), &args, $action) })
            }
        }
    };
}

decision_tool!(
    PermissionApprove,
    "permission.approve",
    PermissionAction::Approve,
    serde_json::json!({"type":"object","properties":{
        "request_id":{"type":"string"},
        "scope":{"type":"string","enum":["current_call","child_run_same_tool","child_run_same_path_rule"]},
        "reason":{"type":"string"}
    },"required":["request_id"]})
);
decision_tool!(
    PermissionDeny,
    "permission.deny",
    PermissionAction::Deny,
    serde_json::json!({"type":"object","properties":{
        "request_id":{"type":"string"},"reason":{"type":"string"}
    },"required":["request_id","reason"]})
);
decision_tool!(
    PermissionDefer,
    "permission.defer",
    PermissionAction::Defer,
    serde_json::json!({"type":"object","properties":{
        "request_id":{"type":"string"},"reason":{"type":"string"}
    },"required":["request_id"]})
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_authority::{ChildWorkspaceAuthority, EffectiveAuthority, InvocationKind};
    use crate::permission::{
        ApprovalTarget, PermissionIntent, PermissionRequestState, SubmissionOutcome,
    };
    use crate::tools::agent_ctrl::FlowRegistry;
    use crate::trust::{TrustConfig, TrustMode};

    fn managed_ctx() -> (
        ToolCtx,
        Arc<PermissionBroker>,
        Arc<crate::flow_authority::FlowIdentity>,
    ) {
        let flows = Arc::new(FlowRegistry::new());
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let root = flows
            .register_root(
                "permission-tools".into(),
                crate::event::FlowRunId::now(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let requester = flows
            .register_child(
                &root.run_id,
                crate::event::FlowRunId::now(),
                InvocationKind::InlineSubflow,
                true,
                ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let mut ctx = ToolCtx::new()
            .with_flow_registry(flows)
            .with_permission_broker(Arc::clone(&broker))
            .with_trust(trust)
            .with_anchors(None, Some(root.run_id.clone()), None);
        ctx.flow_identity = Some(Arc::clone(&root));
        (ctx, broker, requester)
    }

    fn submit_pending(
        broker: &PermissionBroker,
        requester: &crate::flow_authority::FlowIdentity,
    ) -> crate::permission::PendingPermission {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let SubmissionOutcome::Pending(pending) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                PermissionIntent::minimal("tool-use", "fs.write", Tier::Two),
                false,
                &trust,
            )
            .unwrap()
        else {
            panic!("expected pending request");
        };
        *pending
    }

    #[test]
    fn all_permission_tools_are_registered_on_the_control_plane() {
        let registry = crate::tool::ToolRegistry::new();
        crate::tools::register_tier_zero(&registry);
        for name in [
            "permission.list",
            "permission.get",
            "permission.group",
            "permission.ungroup",
            "permission.approve",
            "permission.deny",
            "permission.defer",
            "permission.batch",
        ] {
            let tool = registry
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert_eq!(tool.invocation_plane(), InvocationPlane::PermissionControl);
            assert_eq!(tool.tier(), Tier::Zero);
            assert_eq!(tool.input_schema()["type"], "object");
        }
    }

    #[tokio::test]
    async fn control_gate_fails_closed_without_broker_or_identity() {
        let ctx = ToolCtx::new();
        let before = ctx
            .permission_broker
            .as_ref()
            .map(|broker| broker.list().len());
        let result = crate::approval::authorize_tool_invocation(
            &ctx,
            "control-call",
            "permission.list",
            &ToolArgs::default(),
            &PermissionList,
        )
        .await;
        assert!(result.is_err());
        assert_eq!(before, None);
    }

    #[tokio::test]
    async fn list_and_approve_use_authenticated_ctx_without_recursive_request() {
        let (ctx, broker, requester) = managed_ctx();
        let pending = submit_pending(&broker, &requester);
        assert!(matches!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(_)
            }
        ));
        let before = broker.list().len();
        let call_ctx = crate::approval::authorize_tool_invocation(
            &ctx,
            "control-list",
            "permission.list",
            &ToolArgs::default(),
            &PermissionList,
        )
        .await
        .unwrap();
        let listed = PermissionList
            .call(ToolArgs::default(), &call_ctx)
            .await
            .unwrap();
        let Value::Struct(fields) = listed else {
            panic!("expected list result");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "requests"),
            Some((_, Value::List(requests))) if requests.len() == 1
        ));
        assert_eq!(broker.list().len(), before);

        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![(
                "request_id".into(),
                Value::Str(pending.request.request_id.to_string()),
            )],
        };
        let call_ctx = crate::approval::authorize_tool_invocation(
            &ctx,
            "control-approve",
            "permission.approve",
            &args,
            &PermissionApprove,
        )
        .await
        .unwrap();
        PermissionApprove.call(args, &call_ctx).await.unwrap();

        assert_eq!(broker.list().len(), before);
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Approved { .. }
        ));
    }

    #[tokio::test]
    async fn get_rejects_a_non_string_request_id_without_falling_back_to_group() {
        let (ctx, _broker, _requester) = managed_ctx();
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![("request_id".into(), Value::Int(7))],
        };

        let error = PermissionGet.call(args, &ctx).await.unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::TypeMismatch { expected, actual }
                if expected == "string" && actual == "int"
        ));
    }

    #[tokio::test]
    async fn batch_target_selector_does_not_expose_invisible_user_requests() {
        let (ctx, broker, requester) = managed_ctx();
        let pending = submit_pending(&broker, &requester);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("selector".into(), Value::Str("target".into())),
                ("selector_value".into(), Value::Str("user".into())),
                ("action".into(), Value::Str("approve".into())),
            ],
        };

        let error = PermissionBatch.call(args, &ctx).await.unwrap_err();
        assert!(error.to_string().contains("must be flow"));
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Pending { .. }
        ));
    }

    #[test]
    fn batch_schema_rejects_unknown_fields() {
        assert_eq!(
            PermissionBatch.input_schema()["additionalProperties"],
            false
        );
    }

    #[tokio::test]
    async fn batch_rejects_malformed_controls_without_mutating_the_broker() {
        let cases = [
            ("atomic", Value::Int(1)),
            ("atomic", Value::Str("true".into())),
            ("atomic", Value::Unit),
            ("group_revision", Value::Int(-1)),
            ("group_revision", Value::Str("0".into())),
            ("group_revision", Value::Unit),
            ("group_revision", Value::Int(0)),
        ];
        for (field, value) in cases {
            let (ctx, broker, requester) = managed_ctx();
            let pending = submit_pending(&broker, &requester);
            let args = ToolArgs {
                positional: Vec::new(),
                named: vec![
                    (
                        "request_ids".into(),
                        Value::List(vec![Value::Str(pending.request.request_id.to_string())]),
                    ),
                    ("action".into(), Value::Str("approve".into())),
                    (field.into(), value),
                ],
            };

            PermissionBatch.call(args, &ctx).await.unwrap_err();
            assert!(matches!(
                broker.get(&pending.request.request_id).unwrap().state,
                PermissionRequestState::Pending { .. }
            ));
        }
    }

    #[tokio::test]
    async fn batch_rejects_caller_identity_fields_without_mutating_the_broker() {
        for field in [
            "session_id",
            "actor",
            "actor_run_id",
            "run_id",
            "requester_run_id",
        ] {
            let (ctx, broker, requester) = managed_ctx();
            let pending = submit_pending(&broker, &requester);
            let args = ToolArgs {
                positional: Vec::new(),
                named: vec![
                    (
                        "request_ids".into(),
                        Value::List(vec![Value::Str(pending.request.request_id.to_string())]),
                    ),
                    ("action".into(), Value::Str("approve".into())),
                    (field.into(), Value::Str("forged".into())),
                ],
            };

            let error = PermissionBatch.call(args, &ctx).await.unwrap_err();
            assert!(error.to_string().contains("caller-supplied"));
            assert!(matches!(
                broker.get(&pending.request.request_id).unwrap().state,
                PermissionRequestState::Pending { .. }
            ));
        }
    }

    #[tokio::test]
    async fn deny_requires_the_reason_declared_by_its_schema() {
        let (ctx, broker, requester) = managed_ctx();
        let pending = submit_pending(&broker, &requester);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![(
                "request_id".into(),
                Value::Str(pending.request.request_id.to_string()),
            )],
        };

        let error = PermissionDeny.call(args, &ctx).await.unwrap_err();
        assert!(matches!(error, RuntimeError::MissingArg(name) if name == "reason"));
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Pending { .. }
        ));
    }
}
