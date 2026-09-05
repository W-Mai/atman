use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, bail};
use atman_proto::{
    ApprovalActorProjection, ApprovalGroupOwnerProjection, ApprovalRequestProjection,
    ApprovalScopeProjection, ApprovalState, ApprovalTarget, CompactReviewProjection,
    ContextProjection, FormQuestionKind, InterjectionProjection, InterjectionSource,
    LlmCallPurpose, LlmCallScope, McpServerStateProjection, McpTransportProjection,
    SessionProjection, TodoState, TrustEscalation, TrustMode, TrustPolicyAction, TrustTheme,
    WorkflowFanoutMode, WorkflowNodeKind, WorkflowNodeProjection, WorkflowNodeState,
    WorkflowStatementKind,
};
use atman_runtime::event::{FlowRunId, TurnId};
use atman_runtime::form::{CompositeForm, FormKind, FormQuestion, PendingForm};
use atman_runtime::injection::{
    Injection, InjectionId, InjectionLevel, InjectionSource, InjectionState,
};
use atman_runtime::memory::MemoryId;
use atman_runtime::permission::{ExecutionBoundary, PermissionGroupId, PermissionRequestId};
use atman_runtime::permission_audit::{
    PermissionAuditScope, PermissionAuditTarget, PermissionEscalationAuditHop,
    PermissionGroupAudit, PermissionGroupAuditOwner, PermissionPolicyReference,
    PermissionProjectionActor, PermissionProvenanceSummary, PermissionRequestAudit,
};
use atman_runtime::projection::workflow::WorkflowProjection;
use atman_runtime::workflow::{
    ApprovalState as RuntimeApprovalState, LlmStats, NodeStatus, Parallelism, WorkflowGraph,
    WorkflowNode, WorkflowNodeKind as RuntimeWorkflowNodeKind, WorkflowPermissionIdentity,
    WorkflowPermissionRequest, WorkflowPermissionState,
};

use crate::app::{PendingPermission, PendingPermissionGroup};

#[derive(Debug, Clone)]
pub(crate) struct TuiSessionProjection {
    pub(crate) revision: u64,
    pub(crate) session_name: Option<String>,
    pub(crate) project_root: Option<String>,
    pub(crate) goal: Option<String>,
    pub(crate) context: atman_runtime::ContextSnapshot,
    pub(crate) todos: Vec<atman_runtime::memory::todo::Todo>,
    pub(crate) plans: Vec<atman_runtime::memory::plan::Plan>,
    pub(crate) trust: atman_runtime::trust::TrustConfig,
    pub(crate) pending_permissions: BTreeMap<PermissionRequestId, PendingPermission>,
    pub(crate) pending_permission_groups: BTreeMap<PermissionGroupId, PendingPermissionGroup>,
    pub(crate) pending_forms: Vec<PendingForm>,
    pub(crate) pending_compact_reviews: Vec<atman_runtime::PendingCompactReview>,
    pub(crate) pending_injections: Vec<Injection>,
    pub(crate) workflows: Vec<WorkflowProjection>,
}

impl TryFrom<&SessionProjection> for TuiSessionProjection {
    type Error = anyhow::Error;

    fn try_from(projection: &SessionProjection) -> Result<Self> {
        let audits = projection
            .interactions
            .approvals
            .iter()
            .map(permission_request)
            .collect::<Result<Vec<_>>>()?;
        let groups = projection
            .interactions
            .approval_groups
            .iter()
            .map(permission_group)
            .collect::<Vec<_>>();
        let pending_permissions = projection
            .interactions
            .approvals
            .iter()
            .zip(&audits)
            .filter(|(source, _)| {
                matches!(
                    source.state,
                    ApprovalState::Evaluating | ApprovalState::Pending
                ) && matches!(source.target, Some(ApprovalTarget::User))
            })
            .map(|(_, audit)| {
                let request_id = audit
                    .request_id
                    .clone()
                    .expect("public approvals have stable request identities");
                (
                    request_id.clone(),
                    PendingPermission {
                        request_id,
                        revision: audit.revision,
                        payload: audit.clone(),
                    },
                )
            })
            .collect();
        let pending_permission_groups = projection
            .interactions
            .approval_groups
            .iter()
            .zip(&groups)
            .filter(|(source, _)| !source.resolved)
            .map(|(_, group)| {
                (
                    group.group_id.clone(),
                    PendingPermissionGroup {
                        group_id: group.group_id.clone(),
                        revision: group.revision,
                        payload: group.clone(),
                        expanded: false,
                    },
                )
            })
            .collect();

        Ok(Self {
            revision: projection.revision.0,
            session_name: (!projection.metadata.title.is_empty())
                .then(|| projection.metadata.title.clone()),
            project_root: projection.metadata.project_root.clone(),
            goal: projection.goal.clone(),
            context: context(&projection.context),
            todos: projection
                .todos
                .iter()
                .map(todo)
                .collect::<Result<Vec<_>>>()?,
            plans: projection.plans.iter().map(plan).collect(),
            trust: trust(&projection.trust),
            pending_permissions,
            pending_permission_groups,
            pending_forms: projection
                .interactions
                .forms
                .iter()
                .map(pending_form)
                .collect::<Result<Vec<_>>>()?,
            pending_compact_reviews: projection
                .interactions
                .compact_reviews
                .iter()
                .map(compact_review)
                .collect(),
            pending_injections: projection
                .interactions
                .interjections
                .iter()
                .filter(|injection| {
                    matches!(injection.state, atman_proto::InterjectionState::Pending)
                })
                .map(interjection)
                .collect(),
            workflows: projection
                .workflows
                .iter()
                .map(|workflow| workflow_projection(workflow, &audits, &groups, projection))
                .collect::<Result<Vec<_>>>()?,
        })
    }
}

fn context(source: &ContextProjection) -> atman_runtime::ContextSnapshot {
    atman_runtime::ContextSnapshot {
        model: source.model.clone(),
        provider: source.provider.clone(),
        tokens_in: source.input_tokens,
        tokens_out: source.output_tokens,
        cost_usd: source.cost_usd,
        mcp_servers: source
            .mcp_servers
            .iter()
            .map(|server| atman_runtime::mcp::McpServerStatus {
                name: server.name.clone(),
                transport: match server.transport {
                    McpTransportProjection::Stdio => atman_runtime::mcp::TransportKind::Stdio,
                    McpTransportProjection::Http => atman_runtime::mcp::TransportKind::Http,
                    McpTransportProjection::Sse => atman_runtime::mcp::TransportKind::Sse,
                },
                state: match &server.state {
                    McpServerStateProjection::Disabled => {
                        atman_runtime::mcp::McpServerState::Disabled
                    }
                    McpServerStateProjection::Pending => {
                        atman_runtime::mcp::McpServerState::Pending
                    }
                    McpServerStateProjection::Connecting => {
                        atman_runtime::mcp::McpServerState::Connecting
                    }
                    McpServerStateProjection::Connected { tools } => {
                        atman_runtime::mcp::McpServerState::Connected {
                            tool_count: tools.len(),
                            tools: tools
                                .iter()
                                .map(|tool| atman_runtime::mcp::McpToolInfo {
                                    name: tool.name.clone(),
                                    description: tool.description.clone(),
                                })
                                .collect(),
                        }
                    }
                    McpServerStateProjection::Error { message } => {
                        atman_runtime::mcp::McpServerState::Error {
                            message: message.clone(),
                        }
                    }
                    McpServerStateProjection::Disconnected { message } => {
                        atman_runtime::mcp::McpServerState::Disconnected {
                            message: message.clone(),
                        }
                    }
                    McpServerStateProjection::Timeout { message } => {
                        atman_runtime::mcp::McpServerState::Timeout {
                            message: message.clone(),
                        }
                    }
                },
            })
            .collect(),
        memory_recent_count: source.memory_recent_count,
        window_tokens: source.window_tokens,
        window_budget: source.window_budget,
        cache_read: source.cache_read_tokens,
        cache_write: source.cache_write_tokens,
        last_ttft_ms: source.last_ttft_ms,
        last_tokens_per_sec: source.last_tokens_per_second,
        usage_buckets: source
            .usage_buckets
            .iter()
            .map(|bucket| atman_runtime::session::ContextUsageBucket {
                provider: bucket.provider.clone(),
                model: bucket.model.clone(),
                call_purpose: call_purpose(bucket.call_purpose),
                call_scope: call_scope(bucket.call_scope),
                calls: bucket.calls,
                tokens_in: bucket.input_tokens,
                tokens_out: bucket.output_tokens,
                cache_read: bucket.cache_read_tokens,
                cache_write: bucket.cache_write_tokens,
            })
            .collect(),
    }
}

fn call_purpose(source: LlmCallPurpose) -> atman_runtime::context_plan::ContextCallPurpose {
    match source {
        LlmCallPurpose::General => atman_runtime::context_plan::ContextCallPurpose::General,
        LlmCallPurpose::Classification => {
            atman_runtime::context_plan::ContextCallPurpose::Classification
        }
        LlmCallPurpose::Extraction => atman_runtime::context_plan::ContextCallPurpose::Extraction,
        LlmCallPurpose::BranchGeneration => {
            atman_runtime::context_plan::ContextCallPurpose::BranchGeneration
        }
        LlmCallPurpose::Compaction => atman_runtime::context_plan::ContextCallPurpose::Compaction,
        LlmCallPurpose::InterjectionClassification => {
            atman_runtime::context_plan::ContextCallPurpose::InterjectionClassification
        }
    }
}

fn call_scope(source: LlmCallScope) -> atman_runtime::context_plan::ContextCallScope {
    match source {
        LlmCallScope::Root => atman_runtime::context_plan::ContextCallScope::Root,
        LlmCallScope::Child => atman_runtime::context_plan::ContextCallScope::Child,
        LlmCallScope::Detached => atman_runtime::context_plan::ContextCallScope::Detached,
    }
}

fn todo(source: &atman_proto::TodoProjection) -> Result<atman_runtime::memory::todo::Todo> {
    Ok(atman_runtime::memory::todo::Todo {
        id: MemoryId::parse(&source.id)
            .with_context(|| format!("invalid todo id `{}`", source.id))?,
        where_: source.where_.clone(),
        why: source.why.clone(),
        how: source.how.clone(),
        expected_result: source.expected_result.clone(),
        status: match source.state {
            TodoState::Pending => atman_runtime::memory::todo::TodoStatus::Pending,
            TodoState::InProgress => atman_runtime::memory::todo::TodoStatus::InProgress,
            TodoState::Done => atman_runtime::memory::todo::TodoStatus::Done,
            TodoState::Cancelled => atman_runtime::memory::todo::TodoStatus::Cancelled,
        },
    })
}

fn plan(source: &atman_proto::PlanProjection) -> atman_runtime::memory::plan::Plan {
    atman_runtime::memory::plan::Plan {
        id: source.id.clone(),
        title: source.title.clone(),
        steps: source
            .steps
            .iter()
            .map(|step| atman_runtime::memory::plan::PlanStep {
                index: step.index,
                text: step.text.clone(),
                done: step.done,
                done_at: step.done_at,
            })
            .collect(),
        created_at: source.created_at,
        updated_at: source.updated_at,
    }
}

fn trust(source: &atman_proto::TrustProjection) -> atman_runtime::trust::TrustConfig {
    let action = |source: Option<TrustPolicyAction>| {
        source.map(|action| match action {
            TrustPolicyAction::Auto => atman_runtime::trust::PolicyAction::Auto,
            TrustPolicyAction::Ask => atman_runtime::trust::PolicyAction::Ask,
            TrustPolicyAction::Deny => atman_runtime::trust::PolicyAction::Deny,
        })
    };
    atman_runtime::trust::TrustConfig {
        mode: match source.mode {
            TrustMode::Calm => atman_runtime::trust::TrustMode::Calm,
            TrustMode::Steady => atman_runtime::trust::TrustMode::Steady,
            TrustMode::Eager => atman_runtime::trust::TrustMode::Eager,
            TrustMode::Reckless => atman_runtime::trust::TrustMode::Reckless,
        },
        theme: match source.theme {
            TrustTheme::Default => atman_runtime::trust::Theme::Default,
            TrustTheme::Wuxia => atman_runtime::trust::Theme::Wuxia,
            TrustTheme::Animal => atman_runtime::trust::Theme::Animal,
            TrustTheme::Weather => atman_runtime::trust::Theme::Weather,
            TrustTheme::Drink => atman_runtime::trust::Theme::Drink,
        },
        escalation: match source.escalation {
            TrustEscalation::Deny => atman_runtime::trust::EscalationPolicy::Deny,
            TrustEscalation::Ask => atman_runtime::trust::EscalationPolicy::Ask,
            TrustEscalation::Allow => atman_runtime::trust::EscalationPolicy::Allow,
        },
        tiers: atman_runtime::trust::TierPolicyConfig {
            eager: atman_runtime::trust::TierPolicyOverrides {
                tier0: action(source.eager_tiers.tier0),
                tier1: action(source.eager_tiers.tier1),
                tier2: action(source.eager_tiers.tier2),
                tier3: action(source.eager_tiers.tier3),
                tier4: action(source.eager_tiers.tier4),
            },
        },
        risks: atman_runtime::trust::RiskPolicyConfig {
            eager: atman_runtime::trust::RiskPolicyOverrides {
                outside_workspace: action(source.eager_risks.outside_workspace),
                network: action(source.eager_risks.network),
                irreversible: action(source.eager_risks.irreversible),
                filesystem_write: action(source.eager_risks.filesystem_write),
                process_spawn: action(source.eager_risks.process_spawn),
                repository_mutation: action(source.eager_risks.repository_mutation),
            },
        },
    }
}

fn pending_form(source: &atman_proto::PendingFormProjection) -> Result<PendingForm> {
    let questions = source
        .questions
        .iter()
        .map(|question| FormQuestion {
            id: question.id.clone(),
            kind: match question.kind {
                FormQuestionKind::Confirm => FormKind::Confirm {
                    prompt: question.prompt.clone(),
                },
                FormQuestionKind::SingleSelect => FormKind::SingleSelect {
                    prompt: question.prompt.clone(),
                    options: question.options.clone(),
                },
                FormQuestionKind::MultiSelect => FormKind::MultiSelect {
                    prompt: question.prompt.clone(),
                    options: question.options.clone(),
                    min: question.min,
                    max: question.max,
                },
                FormQuestionKind::Text => FormKind::Text {
                    prompt: question.prompt.clone(),
                    placeholder: question.placeholder.clone(),
                    multiline: question.multiline,
                },
            },
        })
        .collect::<Vec<_>>();
    let kind = questions
        .first()
        .map(|question| question.kind.clone())
        .with_context(|| format!("form `{}` has no questions", source.id))?;
    Ok(PendingForm {
        form_id: source.id.clone(),
        run_id: FlowRunId(source.run_id.0),
        tool_use_id: source.tool_use_id.clone(),
        form: CompositeForm { questions },
        kind,
        emitted_at: source.emitted_at,
    })
}

fn compact_review(source: &CompactReviewProjection) -> atman_runtime::PendingCompactReview {
    atman_runtime::PendingCompactReview {
        review_id: source.id.clone(),
        context_id: source
            .context_id
            .map(|context_id| atman_runtime::event::ContextId(context_id.0)),
        summary: source.summary.clone(),
        slice_preview: source.slice_preview.clone(),
        slice_count: source.slice_count,
        range_start: source.range_start,
        range_end: source.range_end,
        tokens_before: source.tokens_before,
        emitted_at: source.emitted_at,
    }
}

fn interjection(source: &InterjectionProjection) -> Injection {
    Injection {
        id: InjectionId(source.id),
        text: source.text.clone(),
        turn_id: TurnId(source.turn_id.0),
        flow_run_id: source.run_id.as_ref().map(|run_id| FlowRunId(run_id.0)),
        created_at: source.created_at,
        state: match source.state {
            atman_proto::InterjectionState::Pending => InjectionState::Pending,
            atman_proto::InterjectionState::Injected => InjectionState::Injected,
            atman_proto::InterjectionState::Cancelled => InjectionState::Cancelled,
        },
        level: match source.level {
            atman_proto::InterjectionLevel::Nudge => InjectionLevel::L1Nudge,
            atman_proto::InterjectionLevel::CourseCorrect => InjectionLevel::L2CourseCorrect,
            atman_proto::InterjectionLevel::Redirect => InjectionLevel::L3Redirect,
            atman_proto::InterjectionLevel::HardStop => InjectionLevel::L4HardStop,
        },
        redirect_target: source.redirect_target.clone(),
        source: match &source.source {
            InterjectionSource::User => InjectionSource::User,
            InterjectionSource::Watcher {
                watcher_id,
                kind,
                handle,
            } => InjectionSource::Watcher {
                watcher_id: watcher_id.clone(),
                kind: kind.clone(),
                handle: handle.clone(),
            },
        },
    }
}

fn workflow_projection(
    source: &atman_proto::WorkflowProjection,
    approvals: &[PermissionRequestAudit],
    groups: &[PermissionGroupAudit],
    session: &SessionProjection,
) -> Result<WorkflowProjection> {
    let root = source
        .roots
        .iter()
        .map(workflow_node)
        .collect::<Result<Vec<_>>>()?;
    let mut run_ids = HashSet::new();
    collect_run_ids(&root, &mut run_ids);
    let source_approvals = session
        .interactions
        .approvals
        .iter()
        .zip(approvals)
        .filter(|(_, audit)| run_ids.contains(&audit.requesting_run_id.to_string()))
        .collect::<Vec<_>>();
    let permission_requests = source_approvals
        .iter()
        .map(|(source, audit)| {
            let request_id = audit
                .request_id
                .clone()
                .expect("public approvals have stable request identities");
            (
                WorkflowPermissionIdentity::Canonical {
                    request_id: request_id.clone(),
                },
                WorkflowPermissionRequest {
                    payload: (*audit).clone(),
                    state: match source.state {
                        ApprovalState::Evaluating | ApprovalState::Pending => {
                            WorkflowPermissionState::Pending
                        }
                        ApprovalState::Approved => WorkflowPermissionState::Approved,
                        ApprovalState::Denied => WorkflowPermissionState::Denied,
                        ApprovalState::Cancelled => WorkflowPermissionState::Cancelled,
                    },
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let request_ids = permission_requests
        .keys()
        .filter_map(|identity| match identity {
            WorkflowPermissionIdentity::Canonical { request_id } => Some(request_id.clone()),
            WorkflowPermissionIdentity::Legacy { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let permission_groups = groups
        .iter()
        .filter(|group| group.request_ids.iter().any(|id| request_ids.contains(id)))
        .map(|group| (group.group_id.clone(), group.clone()))
        .collect::<BTreeMap<_, _>>();
    let resolved_permission_groups = session
        .interactions
        .approval_groups
        .iter()
        .filter(|group| group.resolved)
        .map(|group| PermissionGroupId(group.id))
        .filter(|id| permission_groups.contains_key(id))
        .collect();
    Ok(WorkflowGraph {
        turn_id: TurnId(source.turn_id.0),
        root,
        permission_requests,
        permission_groups,
        resolved_permission_groups,
    }
    .into())
}

fn collect_run_ids(nodes: &[WorkflowNode], output: &mut HashSet<String>) {
    for node in nodes {
        if let RuntimeWorkflowNodeKind::Flow { run_id, .. }
        | RuntimeWorkflowNodeKind::Subflow { run_id, .. } = &node.kind
        {
            output.insert(run_id.clone());
        }
        collect_run_ids(&node.children, output);
    }
}

fn workflow_node(source: &WorkflowNodeProjection) -> Result<WorkflowNode> {
    Ok(WorkflowNode {
        id: source.id.clone(),
        kind: match &source.kind {
            WorkflowNodeKind::Flow { run_id, flow_name } => RuntimeWorkflowNodeKind::Flow {
                run_id: run_id.0.to_string(),
                flow_name: flow_name.clone(),
            },
            WorkflowNodeKind::Statement { kind } => RuntimeWorkflowNodeKind::Stmt {
                node_kind: workflow_statement(kind),
            },
            WorkflowNodeKind::ToolCall {
                tool_use_id,
                tool_name,
                args_preview,
                intent,
                result_preview,
            } => RuntimeWorkflowNodeKind::ToolCall {
                tool_use_id: tool_use_id.clone(),
                tool: tool_name.clone(),
                args_preview: args_preview.clone(),
                call_intent: match intent.as_deref() {
                    Some(intent) => Some(
                        atman_runtime::message::ToolCallIntent::new(intent)
                            .context("workflow projection has an empty tool intent")?,
                    ),
                    None => None,
                },
                result_preview: result_preview.clone(),
            },
            WorkflowNodeKind::Subflow { run_id, flow_name } => RuntimeWorkflowNodeKind::Subflow {
                run_id: run_id.0.to_string(),
                flow_name: flow_name.clone(),
            },
            WorkflowNodeKind::FanoutBranch { branch_index } => {
                RuntimeWorkflowNodeKind::FanoutBranch {
                    branch_index: *branch_index,
                }
            }
        },
        label: source.label.clone(),
        status: match source.state {
            WorkflowNodeState::Pending => NodeStatus::Pending,
            WorkflowNodeState::Running => NodeStatus::Running,
            WorkflowNodeState::Succeeded => NodeStatus::Ok,
            WorkflowNodeState::Failed => NodeStatus::Err,
            WorkflowNodeState::Cancelled => NodeStatus::Cancelled,
        },
        started_at: source.started_at,
        ended_at: source.finished_at,
        output_preview: source.output_preview.clone(),
        children: source
            .children
            .iter()
            .map(workflow_node)
            .collect::<Result<Vec<_>>>()?,
        parallelism: if source.parallel {
            Parallelism::Parallel
        } else {
            Parallelism::Serial
        },
        approval: source.approval.as_ref().map(|approval| match approval {
            atman_proto::ApprovalProjection::Pending { level, preview } => {
                RuntimeApprovalState::Pending {
                    level: level.clone(),
                    preview: preview.clone(),
                }
            }
            atman_proto::ApprovalProjection::Approved => RuntimeApprovalState::Approved,
            atman_proto::ApprovalProjection::Denied { reason } => RuntimeApprovalState::Denied {
                reason: reason.clone(),
            },
        }),
        llm_stats: source.llm_usage.as_ref().map(|usage| LlmStats {
            model: usage.model.clone(),
            provider: usage.provider.clone(),
            context_call_purpose: call_purpose(usage.call_purpose),
            context_call_scope: call_scope(usage.call_scope),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            ttft_ms: usage.ttft_ms,
            tokens_per_second: usage.tokens_per_second,
            wallclock_ms: usage.wallclock_ms,
        }),
    })
}

fn workflow_statement(source: &WorkflowStatementKind) -> atman_runtime::nodegraph::NodeKind {
    match source {
        WorkflowStatementKind::Llm { model } => atman_runtime::nodegraph::NodeKind::Llm {
            model: model.clone(),
        },
        WorkflowStatementKind::ToolCall { path } => {
            atman_runtime::nodegraph::NodeKind::ToolCall { path: path.clone() }
        }
        WorkflowStatementKind::Fanout { collect } => atman_runtime::nodegraph::NodeKind::Fanout {
            collect: match collect {
                WorkflowFanoutMode::All => atman_runtime::nodegraph::FanoutMode::All,
                WorkflowFanoutMode::First => atman_runtime::nodegraph::FanoutMode::First,
            },
        },
        WorkflowStatementKind::UserConfirm => atman_runtime::nodegraph::NodeKind::UserConfirm,
        WorkflowStatementKind::Subflow { name } => {
            atman_runtime::nodegraph::NodeKind::Subflow { name: name.clone() }
        }
        WorkflowStatementKind::Message { role } => {
            atman_runtime::nodegraph::NodeKind::Message { role: role.clone() }
        }
        WorkflowStatementKind::FixUntilTest => atman_runtime::nodegraph::NodeKind::FixUntilTest,
        WorkflowStatementKind::When { condition_preview } => {
            atman_runtime::nodegraph::NodeKind::When {
                condition_preview: condition_preview.clone(),
            }
        }
        WorkflowStatementKind::Loop => atman_runtime::nodegraph::NodeKind::Loop,
        WorkflowStatementKind::Return => atman_runtime::nodegraph::NodeKind::Return,
    }
}

fn permission_request(source: &ApprovalRequestProjection) -> Result<PermissionRequestAudit> {
    let target = source
        .target
        .as_ref()
        .context("approval projection has no target")?;
    Ok(PermissionRequestAudit {
        request_id: Some(PermissionRequestId(source.id)),
        revision: source.revision,
        session_id: source.session_id.clone(),
        requesting_run_id: FlowRunId(source.requesting_run_id.0),
        parent_run_id: source.parent_run_id.as_ref().map(|id| FlowRunId(id.0)),
        root_run_id: FlowRunId(source.root_run_id.0),
        tool_use_id: source.tool_use_id.clone(),
        tool: source.tool_name.clone(),
        call_intent: match source.intent.as_deref() {
            Some(intent) => Some(
                atman_runtime::message::ToolCallIntent::new(intent)
                    .context("approval projection has an empty intent")?,
            ),
            None => None,
        },
        tier: match source.tier {
            0 => atman_runtime::tool::Tier::Zero,
            1 => atman_runtime::tool::Tier::One,
            2 => atman_runtime::tool::Tier::Two,
            3 => atman_runtime::tool::Tier::Three,
            4 => atman_runtime::tool::Tier::Four,
            tier => bail!("approval projection has invalid tier {tier}"),
        },
        execution_boundary: source.execution_boundary.map(|boundary| match boundary {
            atman_proto::ApprovalExecutionBoundary::Sandboxed => ExecutionBoundary::Sandboxed,
            atman_proto::ApprovalExecutionBoundary::Direct => ExecutionBoundary::Direct,
        }),
        provenance: PermissionProvenanceSummary {
            cwd: source.provenance.cwd.clone(),
            path: source.provenance.path.clone(),
            path_origin: source.provenance.path_origin.clone(),
            workspace_id: source.provenance.workspace_id.clone(),
            workspace_root: source.provenance.workspace_root.clone(),
            repository_root: source.provenance.repository_root.clone(),
            network: source.provenance.network,
            risks: source.provenance.risks.clone(),
            targets: source.provenance.targets.clone(),
        },
        target: permission_target(target),
        group_ids: source
            .group_ids
            .iter()
            .copied()
            .map(PermissionGroupId)
            .collect(),
        policy: PermissionPolicyReference {
            snapshot_id: source.policy.snapshot_id.clone(),
            rule_id: source.policy.rule_id.clone(),
        },
        escalation_path: source
            .escalation_path
            .iter()
            .map(|hop| PermissionEscalationAuditHop {
                target: permission_target(&hop.target),
                actor: hop.actor.as_ref().map(permission_actor),
                action: hop.action.clone(),
                reason: hop.reason.clone(),
                at: hop.at,
            })
            .collect(),
        decision_id: source.decision_id.clone(),
        actor: source.actor.as_ref().map(permission_actor),
        scope: source.scope.as_ref().map(permission_scope),
        reason: source.reason.clone(),
        at: source.at,
    })
}

fn permission_target(source: &ApprovalTarget) -> PermissionAuditTarget {
    match source {
        ApprovalTarget::Flow { run_id } => PermissionAuditTarget::Flow {
            run_id: FlowRunId(run_id.0),
        },
        ApprovalTarget::User => PermissionAuditTarget::User,
    }
}

fn permission_actor(source: &ApprovalActorProjection) -> PermissionProjectionActor {
    match source {
        ApprovalActorProjection::Policy {
            policy_version,
            rule_id,
        } => PermissionProjectionActor::Policy {
            policy_version: policy_version.clone(),
            rule_id: rule_id.clone(),
        },
        ApprovalActorProjection::Flow { session_id, run_id } => PermissionProjectionActor::Flow {
            session_id: session_id.clone(),
            run_id: FlowRunId(run_id.0),
        },
        ApprovalActorProjection::User {
            session_id,
            principal_id,
        } => PermissionProjectionActor::User {
            session_id: session_id.clone(),
            principal_id: principal_id.clone(),
        },
        ApprovalActorProjection::System { component } => PermissionProjectionActor::System {
            component: component.clone(),
        },
        ApprovalActorProjection::UnknownLegacy { label } => {
            PermissionProjectionActor::UnknownLegacy {
                label: label.clone(),
            }
        }
    }
}

fn permission_scope(source: &ApprovalScopeProjection) -> PermissionAuditScope {
    match source {
        ApprovalScopeProjection::CurrentCall => PermissionAuditScope::CurrentCall,
        ApprovalScopeProjection::ChildRunSameTool { run_id, tool_name } => {
            PermissionAuditScope::ChildRunSameTool {
                run_id: FlowRunId(run_id.0),
                tool_name: tool_name.clone(),
            }
        }
        ApprovalScopeProjection::ChildRunSamePathRule {
            run_id,
            tool_name,
            workspace_relative_path,
        } => PermissionAuditScope::ChildRunSamePathRule {
            run_id: FlowRunId(run_id.0),
            tool_name: tool_name.clone(),
            workspace_relative_path: workspace_relative_path.clone(),
        },
    }
}

fn permission_group(source: &atman_proto::ApprovalGroupProjection) -> PermissionGroupAudit {
    PermissionGroupAudit {
        group_id: PermissionGroupId(source.id),
        owner: match &source.owner {
            ApprovalGroupOwnerProjection::Flow { run_id } => PermissionGroupAuditOwner::Flow {
                run_id: FlowRunId(run_id.0),
            },
            ApprovalGroupOwnerProjection::User { session_id } => PermissionGroupAuditOwner::User {
                session_id: session_id.clone(),
            },
            ApprovalGroupOwnerProjection::System => PermissionGroupAuditOwner::System,
        },
        label: source.label.clone(),
        request_ids: source
            .request_ids
            .iter()
            .copied()
            .map(PermissionRequestId)
            .collect(),
        revision: source.revision,
        at: source.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_proto::{
        ApprovalExecutionBoundary, ApprovalGroupProjection, ApprovalPolicyProjection,
        ApprovalProvenanceProjection, CompactionOperationId, ContextUsageBucketProjection,
        FormQuestionProjection, InteractionProjection, McpServerProjection,
        McpServerStateProjection, McpToolProjection, PlanProjection, PlanStepProjection, Revision,
        SessionId, SessionLifecycle, SessionMetadataProjection, TodoProjection, TrustProjection,
        UsageProjection, WorkflowProjection as PublicWorkflowProjection,
    };

    fn projection() -> SessionProjection {
        let now = chrono::Utc::now();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let turn_id = atman_proto::TurnId(uuid::Uuid::now_v7());
        let run_id = atman_proto::FlowRunId(uuid::Uuid::now_v7());
        let request_id = uuid::Uuid::now_v7();
        let group_id = uuid::Uuid::now_v7();
        SessionProjection {
            revision: Revision(9),
            metadata: SessionMetadataProjection {
                id: session_id,
                title: "remote session".into(),
                name_source: atman_proto::NameSource::User,
                project_root: Some("/workspace".into()),
                created_at: Some(now),
                updated_at: Some(now),
            },
            lifecycle: SessionLifecycle::Active,
            runs: Vec::new(),
            transcript: Vec::new(),
            workflows: vec![PublicWorkflowProjection {
                turn_id: turn_id.clone(),
                roots: vec![WorkflowNodeProjection {
                    id: run_id.0.to_string(),
                    kind: WorkflowNodeKind::Flow {
                        run_id: run_id.clone(),
                        flow_name: "agent".into(),
                    },
                    label: "agent".into(),
                    state: WorkflowNodeState::Running,
                    started_at: Some(now),
                    finished_at: None,
                    output_preview: Some("working".into()),
                    children: vec![WorkflowNodeProjection {
                        id: "llm".into(),
                        kind: WorkflowNodeKind::Statement {
                            kind: WorkflowStatementKind::Llm {
                                model: Some("reasoning-model".into()),
                            },
                        },
                        label: "think".into(),
                        state: WorkflowNodeState::Succeeded,
                        started_at: Some(now),
                        finished_at: Some(now),
                        output_preview: None,
                        children: Vec::new(),
                        parallel: false,
                        approval: None,
                        llm_usage: Some(atman_proto::LlmUsageProjection {
                            model: "reasoning-model".into(),
                            provider: "openai-compatible".into(),
                            call_purpose: LlmCallPurpose::General,
                            call_scope: LlmCallScope::Root,
                            input_tokens: 100,
                            output_tokens: 20,
                            cache_read_tokens: 80,
                            cache_write_tokens: 10,
                            wallclock_ms: 500,
                            ttft_ms: 40,
                            tokens_per_second: 40.0,
                        }),
                    }],
                    parallel: false,
                    approval: None,
                    llm_usage: None,
                }],
            }],
            compactions: Vec::new(),
            goal: Some("finish migration".into()),
            todos: vec![TodoProjection {
                id: uuid::Uuid::now_v7().to_string(),
                where_: "client".into(),
                why: "converge".into(),
                how: "project".into(),
                expected_result: "same state".into(),
                state: TodoState::InProgress,
            }],
            plans: vec![PlanProjection {
                id: "plan".into(),
                title: "Migration".into(),
                steps: vec![PlanStepProjection {
                    index: 0,
                    text: "Attach".into(),
                    done: true,
                    done_at: Some(now),
                }],
                created_at: now,
                updated_at: now,
            }],
            context: ContextProjection {
                model: "reasoning-model".into(),
                provider: "openai-compatible".into(),
                input_tokens: 100,
                output_tokens: 20,
                window_tokens: 120,
                window_budget: 4096,
                cost_usd: 0.5,
                cache_read_tokens: 80,
                cache_write_tokens: 10,
                last_ttft_ms: 40,
                last_tokens_per_second: 40.0,
                memory_recent_count: 3,
                usage_buckets: vec![ContextUsageBucketProjection {
                    provider: "openai-compatible".into(),
                    model: "reasoning-model".into(),
                    call_purpose: LlmCallPurpose::General,
                    call_scope: LlmCallScope::Root,
                    calls: 1,
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_read_tokens: 80,
                    cache_write_tokens: 10,
                }],
                mcp_servers: vec![McpServerProjection {
                    name: "catalog".into(),
                    transport: McpTransportProjection::Http,
                    state: McpServerStateProjection::Connected {
                        tools: vec![McpToolProjection {
                            name: "search".into(),
                            description: Some("Search".into()),
                        }],
                    },
                }],
            },
            trust: TrustProjection {
                mode: TrustMode::Eager,
                escalation: TrustEscalation::Allow,
                ..Default::default()
            },
            interactions: InteractionProjection {
                prompts: Vec::new(),
                approvals: vec![ApprovalRequestProjection {
                    id: request_id,
                    session_id: "session".into(),
                    requesting_run_id: run_id.clone(),
                    parent_run_id: None,
                    root_run_id: run_id.clone(),
                    tool_use_id: "tool-1".into(),
                    tool_name: "bash.spawn".into(),
                    intent: Some("Inspect processes".into()),
                    tier: 4,
                    execution_boundary: Some(ApprovalExecutionBoundary::Direct),
                    provenance: ApprovalProvenanceProjection {
                        cwd: Some("/workspace".into()),
                        risks: BTreeSet::from(["ProcessSpawn".into()]),
                        targets: vec!["/workspace".into()],
                        ..Default::default()
                    },
                    state: ApprovalState::Pending,
                    target: Some(ApprovalTarget::User),
                    group_ids: vec![group_id],
                    policy: ApprovalPolicyProjection {
                        snapshot_id: "policy".into(),
                        rule_id: "rule".into(),
                    },
                    escalation_path: Vec::new(),
                    decision_id: None,
                    actor: None,
                    scope: None,
                    reason: None,
                    at: now,
                    revision: 4,
                }],
                approval_groups: vec![ApprovalGroupProjection {
                    id: group_id,
                    owner: ApprovalGroupOwnerProjection::User {
                        session_id: "session".into(),
                    },
                    label: "commands".into(),
                    request_ids: vec![request_id],
                    revision: 2,
                    resolved: false,
                    at: now,
                }],
                forms: vec![atman_proto::PendingFormProjection {
                    id: "form".into(),
                    run_id: run_id.clone(),
                    tool_use_id: "form-tool".into(),
                    emitted_at: now,
                    questions: vec![FormQuestionProjection {
                        id: "choice".into(),
                        kind: FormQuestionKind::SingleSelect,
                        prompt: "Choose".into(),
                        options: vec!["A".into(), "B".into()],
                        min: None,
                        max: None,
                        placeholder: None,
                        multiline: false,
                    }],
                }],
                compact_reviews: vec![CompactReviewProjection {
                    id: CompactionOperationId(uuid::Uuid::now_v7()).0.to_string(),
                    context_id: None,
                    summary: "summary".into(),
                    slice_preview: "preview".into(),
                    slice_count: 2,
                    range_start: 1,
                    range_end: 2,
                    tokens_before: 1000,
                    emitted_at: now,
                }],
                interjections: vec![InterjectionProjection {
                    id: uuid::Uuid::now_v7(),
                    turn_id,
                    run_id: Some(run_id),
                    text: "focus".into(),
                    level: atman_proto::InterjectionLevel::CourseCorrect,
                    state: atman_proto::InterjectionState::Pending,
                    redirect_target: None,
                    created_at: now,
                    source: InterjectionSource::User,
                }],
            },
            resources: Vec::new(),
            usage: UsageProjection::default(),
        }
    }

    #[test]
    fn converts_daemon_state_without_losing_ui_domain_details() {
        let source = projection();
        let converted = TuiSessionProjection::try_from(&source).unwrap();

        assert_eq!(converted.revision, 9);
        assert_eq!(converted.context.last_ttft_ms, 40);
        assert_eq!(converted.context.usage_buckets[0].cache_read, 80);
        let atman_runtime::mcp::McpServerState::Connected { tools, .. } =
            &converted.context.mcp_servers[0].state
        else {
            panic!("expected connected MCP server");
        };
        assert_eq!(tools[0].description.as_deref(), Some("Search"));
        assert_eq!(converted.pending_permissions.len(), 1);
        assert_eq!(converted.pending_permission_groups.len(), 1);
        assert_eq!(converted.pending_forms[0].form.questions.len(), 1);
        assert_eq!(converted.pending_compact_reviews[0].summary, "summary");
        assert_eq!(
            converted.pending_injections[0].level,
            InjectionLevel::L2CourseCorrect
        );
        assert_eq!(converted.workflows[0].graph().permission_requests.len(), 1);
        assert_eq!(converted.workflows[0].graph().permission_groups.len(), 1);
        assert_eq!(
            converted.workflows[0].graph().root[0].children[0]
                .llm_stats
                .as_ref()
                .unwrap()
                .cache_read,
            80
        );
    }

    #[test]
    fn rejects_invalid_projected_identities_instead_of_inventing_local_ones() {
        let mut source = projection();
        source.todos[0].id = "not-a-uuid".into();
        assert!(
            TuiSessionProjection::try_from(&source)
                .unwrap_err()
                .to_string()
                .contains("invalid todo id")
        );

        let mut source = projection();
        source.interactions.approvals[0].tier = 9;
        assert!(
            TuiSessionProjection::try_from(&source)
                .unwrap_err()
                .to_string()
                .contains("invalid tier")
        );
    }
}
