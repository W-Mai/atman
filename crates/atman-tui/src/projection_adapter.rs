use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::{Duration, Instant};

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

use crate::app::{
    Disclosure, NoteLevel, OutputItem, PendingPermission, PendingPermissionGroup, ToolCallStatus,
};
use crate::history::ToolDisplayMeta;

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
    pub(crate) transcript: Option<Vec<OutputItem>>,
    pub(crate) transcript_revision: u64,
}

impl TuiSessionProjection {
    pub(crate) fn try_from_state(
        state: &atman_client::SessionState,
        current_transcript_revision: Option<u64>,
    ) -> Result<Self> {
        Self::convert(
            state.projection(),
            state.transcript_revision(),
            current_transcript_revision != Some(state.transcript_revision()),
        )
    }

    fn convert(
        projection: &SessionProjection,
        transcript_revision: u64,
        include_transcript: bool,
    ) -> Result<Self> {
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

        let transcript = if include_transcript {
            let workflows = projection
                .workflows
                .iter()
                .map(|workflow| workflow_projection(workflow, &audits, &groups, projection))
                .collect::<Result<Vec<_>>>()?;
            Some(transcript(projection, &workflows, &audits, &groups)?)
        } else {
            None
        };

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
            transcript,
            transcript_revision,
        })
    }
}

impl TryFrom<&SessionProjection> for TuiSessionProjection {
    type Error = anyhow::Error;

    fn try_from(projection: &SessionProjection) -> Result<Self> {
        Self::convert(projection, projection.revision.0, true)
    }
}

fn transcript(
    projection: &SessionProjection,
    workflows: &[WorkflowProjection],
    approvals: &[PermissionRequestAudit],
    groups: &[PermissionGroupAudit],
) -> Result<Vec<OutputItem>> {
    let messages = projection
        .transcript
        .iter()
        .map(|item| match item {
            atman_proto::TranscriptItem::Message { message, .. } => {
                message_from_projection(message).map(Some)
            }
            _ => Ok(None),
        })
        .collect::<Result<Vec<_>>>()?;
    let tool_map = messages
        .iter()
        .flatten()
        .flat_map(|message| &message.parts)
        .filter_map(|part| {
            let atman_runtime::message::MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } = part
            else {
                return None;
            };
            Some((
                id.clone(),
                ToolDisplayMeta::from_tool_use(name, input, intent.as_ref()),
            ))
        })
        .collect::<HashMap<_, _>>();
    let subflows = subflow_roots(projection);
    let mut subflow_messages =
        HashMap::<atman_proto::FlowRunId, Vec<atman_runtime::Message>>::new();
    let mut workflow_slots = projection
        .workflows
        .iter()
        .zip(workflows)
        .enumerate()
        .map(|(index, (source, workflow))| (source.turn_id.0, (index, workflow.clone())))
        .collect::<HashMap<_, _>>();
    let mut out = Vec::new();

    for (item, message) in projection.transcript.iter().zip(messages) {
        match item {
            atman_proto::TranscriptItem::Message {
                run_id,
                message: source,
                ..
            } => {
                let message = message.expect("message projections were converted above");
                if matches!(source.origin, atman_proto::MessageOrigin::Interjection) {
                    continue;
                }
                if let Some(root) = run_id.as_ref().and_then(|run_id| subflows.get(run_id)) {
                    subflow_messages
                        .entry(root.clone())
                        .or_default()
                        .push(message);
                    continue;
                }
                let workflow = workflow_slots.remove(&source.turn_id.0);
                let after_user = matches!(
                    (source.role, source.origin),
                    (
                        atman_proto::MessageRole::User,
                        atman_proto::MessageOrigin::User
                    )
                );
                if !after_user && let Some((index, workflow)) = workflow.as_ref() {
                    out.push(workflow_item(*index, workflow));
                }
                crate::history::flatten_message(&message, &mut out, &tool_map);
                if after_user && let Some((index, workflow)) = workflow.as_ref() {
                    out.push(workflow_item(*index, workflow));
                }
            }
            atman_proto::TranscriptItem::Diff {
                tool_use_id,
                title,
                old_content,
                new_content,
                unified_diff,
                ..
            } => {
                let detail = OutputItem::DiffPreview {
                    title: title.clone(),
                    old_content: old_content.clone(),
                    new_content: new_content.clone(),
                    unified_diff: unified_diff.clone(),
                    expanded: false,
                };
                if tool_use_id
                    .as_deref()
                    .is_none_or(|id| !crate::history::attach_detail(&mut out, id, detail.clone()))
                {
                    out.push(detail);
                }
            }
            atman_proto::TranscriptItem::FileEdit {
                tool_use_id,
                path,
                added_lines,
                removed_lines,
                hunks,
                ..
            } => {
                let metrics = atman_runtime::activity::EditMetrics {
                    hunks: usize::try_from(*hunks).context("file edit hunk count exceeds usize")?,
                    insertions: usize::try_from(*added_lines)
                        .context("file edit insertion count exceeds usize")?,
                    deletions: usize::try_from(*removed_lines)
                        .context("file edit deletion count exceeds usize")?,
                };
                if let Some(tool_use_id) = tool_use_id {
                    for item in out.iter_mut().rev() {
                        let OutputItem::ToolDispatch { calls } = item else {
                            continue;
                        };
                        if let Some(call) = calls.iter_mut().find(|call| call.id == *tool_use_id) {
                            call.applied_edit = Some((path.clone(), metrics));
                            break;
                        }
                    }
                }
            }
            atman_proto::TranscriptItem::Compaction {
                operation_id,
                context_id,
                run_id,
                outcome,
                range_start,
                range_end,
                compacted_count,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => {
                let range_start = usize::try_from(*range_start)
                    .context("compaction range start exceeds usize")?;
                let range_end =
                    usize::try_from(*range_end).context("compaction range end exceeds usize")?;
                let item = OutputItem::CompactionSummary {
                    operation_id: operation_id.as_ref().map(ToString::to_string),
                    context_id: context_id.as_ref().map(|id| id.0.to_string()),
                    run_id: run_id.as_ref().map(|id| id.0.to_string()),
                    phase: match outcome {
                        atman_proto::CompactionOutcome::Finished => {
                            atman_runtime::stream::CompactionPhase::Finished
                        }
                        atman_proto::CompactionOutcome::Failed
                        | atman_proto::CompactionOutcome::Abandoned => {
                            atman_runtime::stream::CompactionPhase::Failed
                        }
                    },
                    range_start,
                    range_end,
                    summary: summary.clone(),
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    compacted_count: usize::try_from(*compacted_count)
                        .context("compaction message count exceeds usize")?,
                    disclosure: Disclosure::Summary,
                };
                if matches!(
                    out.last(),
                    Some(OutputItem::CompactionSummary {
                        phase: atman_runtime::stream::CompactionPhase::Finished,
                        range_start: last_start,
                        range_end: last_end,
                        summary: last_summary,
                        ..
                    }) if *last_start == range_start
                        && *last_end == range_end
                        && last_summary == summary
                ) {
                    *out.last_mut().expect("matched the last item") = item;
                } else {
                    out.push(item);
                }
            }
            atman_proto::TranscriptItem::Mermaid { source, .. } => {
                out.push(OutputItem::MermaidDiagram {
                    source: source.clone(),
                });
            }
            atman_proto::TranscriptItem::Notice { level, text, .. } => {
                out.push(OutputItem::SystemNote {
                    text: text.clone(),
                    level: note_level(*level),
                });
            }
            atman_proto::TranscriptItem::Extension { kind, payload, .. } => {
                out.push(OutputItem::SystemNote {
                    text: format!("{kind}: {payload}"),
                    level: NoteLevel::Debug,
                });
            }
        }
    }

    let mut remaining = workflow_slots.into_values().collect::<Vec<_>>();
    remaining.sort_by_key(|(index, _)| *index);
    out.extend(
        remaining
            .into_iter()
            .map(|(index, workflow)| workflow_item(index, &workflow)),
    );
    for compaction in &projection.compactions {
        out.push(OutputItem::CompactionSummary {
            operation_id: Some(compaction.id.to_string()),
            context_id: compaction.context_id.map(|id| id.0.to_string()),
            run_id: compaction.run_id.as_ref().map(|id| id.0.to_string()),
            phase: atman_runtime::stream::CompactionPhase::Running,
            range_start: usize::try_from(compaction.range_start)
                .context("active compaction range start exceeds usize")?,
            range_end: usize::try_from(compaction.range_end)
                .context("active compaction range end exceeds usize")?,
            summary: compaction.summary.clone(),
            before_tokens: compaction.before_tokens,
            after_tokens: 0,
            compacted_count: usize::try_from(compaction.compacted_count)
                .context("active compaction message count exceeds usize")?,
            disclosure: Disclosure::Summary,
        });
    }
    attach_subflows(
        &mut out,
        projection,
        &subflow_messages,
        &subflows,
        approvals,
        groups,
    )?;
    apply_workflow_tool_state(&mut out, workflows);
    Ok(out)
}

fn message_from_projection(
    source: &atman_proto::MessageProjection,
) -> Result<atman_runtime::Message> {
    use atman_runtime::message::{MessageOrigin, MessagePart, MessageRole};

    let mut parts = Vec::with_capacity(source.parts.len());
    for part in &source.parts {
        match part {
            atman_proto::MessagePart::ContextRecord { .. } => {}
            atman_proto::MessagePart::CompactSummary {
                summary,
                seq_start,
                seq_end,
                count,
            } => parts.push(MessagePart::CompactSummary {
                summary: summary.clone(),
                seq_start: *seq_start,
                seq_end: *seq_end,
                count: *count,
            }),
            atman_proto::MessagePart::Text { text } => {
                parts.push(MessagePart::Text { text: text.clone() });
            }
            atman_proto::MessagePart::Thinking { thinking } => {
                parts.push(MessagePart::Thinking {
                    thinking: thinking.clone(),
                    signature: None,
                });
            }
            atman_proto::MessagePart::Image {
                media_type,
                artifact_id,
                name,
                ..
            } => {
                let label = name
                    .as_deref()
                    .or(artifact_id.as_deref())
                    .unwrap_or(media_type);
                let separator = if parts
                    .iter()
                    .any(|part| matches!(part, MessagePart::Text { .. }))
                {
                    "\n"
                } else {
                    ""
                };
                parts.push(MessagePart::Text {
                    text: format!("{separator}[image: {label}]"),
                });
            }
            atman_proto::MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } => parts.push(MessagePart::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
                intent: intent
                    .as_deref()
                    .map(|intent| {
                        atman_runtime::message::ToolCallIntent::new(intent)
                            .context("transcript projection has an empty tool intent")
                    })
                    .transpose()?,
            }),
            atman_proto::MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => parts.push(MessagePart::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
        }
    }
    Ok(atman_runtime::Message {
        role: match source.role {
            atman_proto::MessageRole::User => MessageRole::User,
            atman_proto::MessageRole::Assistant => MessageRole::Assistant,
            atman_proto::MessageRole::System => MessageRole::System,
            atman_proto::MessageRole::Tool => MessageRole::Tool,
        },
        parts,
        turn_id: TurnId(source.turn_id.0),
        origin: match source.origin {
            atman_proto::MessageOrigin::User => MessageOrigin::User,
            atman_proto::MessageOrigin::Watcher => MessageOrigin::Watcher,
            atman_proto::MessageOrigin::Interjection => MessageOrigin::Interjection,
            atman_proto::MessageOrigin::Internal => MessageOrigin::Internal,
        },
    })
}

fn workflow_item(turn_index: usize, workflow: &WorkflowProjection) -> OutputItem {
    let terminal = !workflow.graph().root.is_empty()
        && workflow
            .graph()
            .root
            .iter()
            .all(|node| !matches!(node.status, NodeStatus::Pending | NodeStatus::Running));
    OutputItem::WorkflowPanel {
        turn_index,
        graph: workflow.clone(),
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: Instant::now(),
        ended_at: terminal.then(Instant::now),
        cancelled: workflow
            .graph()
            .root
            .iter()
            .any(|node| matches!(node.status, NodeStatus::Cancelled)),
    }
}

fn note_level(source: atman_proto::NoticeLevel) -> NoteLevel {
    match source {
        atman_proto::NoticeLevel::Debug => NoteLevel::Debug,
        atman_proto::NoticeLevel::Info => NoteLevel::Info,
        atman_proto::NoticeLevel::Success => NoteLevel::Success,
        atman_proto::NoticeLevel::Warning => NoteLevel::Warn,
        atman_proto::NoticeLevel::Error => NoteLevel::Error,
    }
}

fn subflow_roots(
    projection: &SessionProjection,
) -> HashMap<atman_proto::FlowRunId, atman_proto::FlowRunId> {
    fn collect(nodes: &[WorkflowNodeProjection], roots: &mut HashSet<atman_proto::FlowRunId>) {
        for node in nodes {
            if let WorkflowNodeKind::Subflow { run_id, .. } = &node.kind {
                roots.insert(run_id.clone());
            }
            collect(&node.children, roots);
        }
    }

    let mut roots = HashSet::new();
    for workflow in &projection.workflows {
        collect(&workflow.roots, &mut roots);
    }
    let mut by_run = roots
        .iter()
        .cloned()
        .map(|run_id| (run_id.clone(), run_id))
        .collect::<HashMap<_, _>>();
    loop {
        let mut changed = false;
        for run in &projection.runs {
            let Some(parent) = run.parent_run_id.as_ref() else {
                continue;
            };
            if let Some(root) = by_run.get(parent).cloned()
                && by_run.insert(run.id.clone(), root).is_none()
            {
                changed = true;
            }
        }
        if !changed {
            return by_run;
        }
    }
}

fn attach_subflows(
    out: &mut Vec<OutputItem>,
    projection: &SessionProjection,
    messages: &HashMap<atman_proto::FlowRunId, Vec<atman_runtime::Message>>,
    roots_by_run: &HashMap<atman_proto::FlowRunId, atman_proto::FlowRunId>,
    approvals: &[PermissionRequestAudit],
    groups: &[PermissionGroupAudit],
) -> Result<()> {
    fn locate(
        turn_id: &atman_proto::TurnId,
        nodes: &[WorkflowNodeProjection],
        parent_tool: Option<&str>,
        found: &mut HashMap<
            atman_proto::FlowRunId,
            (atman_proto::TurnId, WorkflowNodeProjection, Option<String>),
        >,
    ) {
        for node in nodes {
            let parent_tool = match &node.kind {
                WorkflowNodeKind::ToolCall { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => parent_tool,
            };
            if let WorkflowNodeKind::Subflow { run_id, .. } = &node.kind {
                found.insert(
                    run_id.clone(),
                    (
                        turn_id.clone(),
                        node.clone(),
                        parent_tool.map(str::to_owned),
                    ),
                );
            }
            locate(turn_id, &node.children, parent_tool, found);
        }
    }

    let mut locations = HashMap::new();
    for workflow in &projection.workflows {
        locate(&workflow.turn_id, &workflow.roots, None, &mut locations);
    }
    let mut roots = roots_by_run.values().cloned().collect::<Vec<_>>();
    roots.sort_by_key(|run_id| {
        projection
            .runs
            .iter()
            .find(|run| run.id == *run_id)
            .map(|run| run.started_at)
    });
    roots.dedup();
    for root in roots {
        let root_messages = messages.get(&root).cloned().unwrap_or_default();
        let run = projection.runs.iter().find(|run| run.id == root);
        let (status, done) = run.map_or_else(
            || ("running".to_owned(), false),
            |run| match run.state {
                atman_proto::RunLifecycle::Succeeded => ("ok".to_owned(), true),
                atman_proto::RunLifecycle::Failed | atman_proto::RunLifecycle::Lost => {
                    ("error".to_owned(), true)
                }
                atman_proto::RunLifecycle::Cancelled => ("killed".to_owned(), true),
                _ => ("running".to_owned(), false),
            },
        );
        let (workflow_graph, tool_use_id) = match locations.get(&root) {
            Some((turn_id, node, tool_use_id)) => (
                workflow_projection(
                    &atman_proto::WorkflowProjection {
                        turn_id: turn_id.clone(),
                        roots: vec![node.clone()],
                    },
                    approvals,
                    groups,
                    projection,
                )?,
                tool_use_id.clone(),
            ),
            None => (WorkflowProjection::new(TurnId::now()), None),
        };
        let detail = OutputItem::SubAgentActivity {
            handle: root.0.to_string(),
            goal: root_messages
                .iter()
                .find(|message| matches!(message.role, atman_runtime::message::MessageRole::User))
                .map(atman_runtime::Message::text_concat)
                .unwrap_or_default(),
            child_run_id: root.0.to_string(),
            model: run.and_then(|run| run.model.clone()).unwrap_or_default(),
            status,
            output: root_messages
                .iter()
                .filter(|message| {
                    matches!(message.role, atman_runtime::message::MessageRole::Assistant)
                })
                .map(atman_runtime::Message::text_concat)
                .collect::<Vec<_>>()
                .join("\n"),
            iteration: 0,
            done,
            expanded: false,
            messages: root_messages,
            workflow_graph,
            expanded_nodes: HashSet::new(),
            workflow_expanded: false,
        };
        if tool_use_id
            .as_deref()
            .is_none_or(|id| !crate::history::attach_detail(out, id, detail.clone()))
        {
            out.push(detail);
        }
    }
    Ok(())
}

fn apply_workflow_tool_state(out: &mut [OutputItem], workflows: &[WorkflowProjection]) {
    fn collect<'a>(nodes: &'a [WorkflowNode], out: &mut HashMap<&'a str, &'a WorkflowNode>) {
        for node in nodes {
            if let RuntimeWorkflowNodeKind::ToolCall { tool_use_id, .. } = &node.kind {
                out.insert(tool_use_id, node);
            }
            collect(&node.children, out);
        }
    }

    let mut nodes = HashMap::new();
    for workflow in workflows {
        collect(&workflow.graph().root, &mut nodes);
    }
    let now = chrono::Utc::now();
    let instant = Instant::now();
    for item in out {
        let OutputItem::ToolDispatch { calls } = item else {
            continue;
        };
        for call in calls {
            let Some(node) = nodes.get(call.id.as_str()) else {
                continue;
            };
            call.status = match node.status {
                NodeStatus::Pending | NodeStatus::Running => ToolCallStatus::Running,
                NodeStatus::Ok => ToolCallStatus::Ok,
                NodeStatus::Err | NodeStatus::Cancelled => ToolCallStatus::Error,
            };
            if let Some(started_at) = node.started_at {
                let elapsed = node
                    .ended_at
                    .unwrap_or(now)
                    .signed_duration_since(started_at)
                    .num_milliseconds()
                    .max(0) as u64;
                call.started_at = instant
                    .checked_sub(Duration::from_millis(elapsed))
                    .unwrap_or(instant);
            }
            call.ended_at = node.ended_at.map(|_| instant);
        }
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
        let workflow = converted
            .transcript
            .as_deref()
            .unwrap()
            .iter()
            .find_map(|item| match item {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .expect("daemon workflow is placed in the transcript");
        assert_eq!(workflow.graph().permission_requests.len(), 1);
        assert_eq!(workflow.graph().permission_groups.len(), 1);
        assert_eq!(
            workflow.graph().root[0].children[0]
                .llm_stats
                .as_ref()
                .unwrap()
                .cache_read,
            80
        );
    }

    #[test]
    fn converts_durable_transcript_into_existing_output_items() {
        let mut source = projection();
        let now = chrono::Utc::now();
        let turn_id = source.workflows[0].turn_id.clone();
        let run_id = match &source.workflows[0].roots[0].kind {
            WorkflowNodeKind::Flow { run_id, .. } => run_id.clone(),
            _ => unreachable!(),
        };
        source.workflows[0].roots[0]
            .children
            .push(WorkflowNodeProjection {
                id: "tool-node".into(),
                kind: WorkflowNodeKind::ToolCall {
                    tool_use_id: "call-1".into(),
                    tool_name: "fs.read".into(),
                    args_preview: "README.md".into(),
                    intent: Some("Read project documentation".into()),
                    result_preview: Some("contents".into()),
                },
                label: "Read project documentation".into(),
                state: WorkflowNodeState::Succeeded,
                started_at: Some(now - chrono::Duration::milliseconds(250)),
                finished_at: Some(now),
                output_preview: Some("contents".into()),
                children: Vec::new(),
                parallel: false,
                approval: None,
                llm_usage: None,
            });
        let message = |role, origin, parts| atman_proto::MessageProjection {
            role,
            origin,
            turn_id: turn_id.clone(),
            parts,
        };
        source.transcript = vec![
            atman_proto::TranscriptItem::Message {
                seq: 1,
                ts: now,
                run_id: None,
                context_id: None,
                checkpoint_index: None,
                message: message(
                    atman_proto::MessageRole::User,
                    atman_proto::MessageOrigin::User,
                    vec![
                        atman_proto::MessagePart::Text {
                            text: "Inspect the project".into(),
                        },
                        atman_proto::MessagePart::Image {
                            id: None,
                            media_type: "image/png".into(),
                            artifact_id: Some("artifact-1".into()),
                            name: Some("layout.png".into()),
                            detail: atman_proto::ImageDetail::High,
                        },
                    ],
                ),
            },
            atman_proto::TranscriptItem::Message {
                seq: 2,
                ts: now,
                run_id: Some(run_id.clone()),
                context_id: None,
                checkpoint_index: None,
                message: message(
                    atman_proto::MessageRole::Assistant,
                    atman_proto::MessageOrigin::User,
                    vec![
                        atman_proto::MessagePart::Thinking {
                            thinking: "Checking files".into(),
                        },
                        atman_proto::MessagePart::Text {
                            text: "I will inspect the documentation.".into(),
                        },
                        atman_proto::MessagePart::ToolUse {
                            id: "call-1".into(),
                            name: "fs.read".into(),
                            input: serde_json::json!({"path": "README.md"}),
                            intent: Some("Read project documentation".into()),
                        },
                    ],
                ),
            },
            atman_proto::TranscriptItem::Message {
                seq: 3,
                ts: now,
                run_id: Some(run_id),
                context_id: None,
                checkpoint_index: None,
                message: message(
                    atman_proto::MessageRole::Tool,
                    atman_proto::MessageOrigin::User,
                    vec![atman_proto::MessagePart::ToolResult {
                        tool_use_id: "call-1".into(),
                        content: "contents".into(),
                        is_error: false,
                    }],
                ),
            },
            atman_proto::TranscriptItem::Diff {
                seq: 4,
                ts: now,
                run_id: None,
                tool_use_id: Some("call-1".into()),
                title: "README.md".into(),
                old_content: Some("old".into()),
                new_content: Some("new".into()),
                unified_diff: None,
            },
            atman_proto::TranscriptItem::FileEdit {
                seq: 5,
                ts: now,
                turn_id: Some(turn_id.clone()),
                run_id: None,
                tool_use_id: Some("call-1".into()),
                tool_name: "fs.edit".into(),
                path: "README.md".into(),
                added_lines: 2,
                removed_lines: 1,
                hunks: 1,
            },
            atman_proto::TranscriptItem::Message {
                seq: 6,
                ts: now,
                run_id: None,
                context_id: None,
                checkpoint_index: None,
                message: message(
                    atman_proto::MessageRole::User,
                    atman_proto::MessageOrigin::Interjection,
                    vec![atman_proto::MessagePart::Text {
                        text: "hidden correction".into(),
                    }],
                ),
            },
            atman_proto::TranscriptItem::Message {
                seq: 7,
                ts: now,
                run_id: None,
                context_id: None,
                checkpoint_index: None,
                message: message(
                    atman_proto::MessageRole::System,
                    atman_proto::MessageOrigin::Internal,
                    vec![atman_proto::MessagePart::CompactSummary {
                        summary: "summary".into(),
                        seq_start: 0,
                        seq_end: 4,
                        count: 5,
                    }],
                ),
            },
            atman_proto::TranscriptItem::Compaction {
                seq: 8,
                ts: now,
                operation_id: Some(CompactionOperationId(uuid::Uuid::now_v7())),
                context_id: None,
                run_id: None,
                outcome: atman_proto::CompactionOutcome::Finished,
                range_start: 0,
                range_end: 4,
                compacted_count: 5,
                before_tokens: 2_000,
                after_tokens: 500,
                summary: "summary".into(),
            },
            atman_proto::TranscriptItem::Notice {
                seq: 9,
                ts: now,
                level: atman_proto::NoticeLevel::Warning,
                text: "watch warning".into(),
            },
            atman_proto::TranscriptItem::Mermaid {
                seq: 10,
                ts: now,
                source: "graph TD; A-->B".into(),
            },
        ];

        let converted = TuiSessionProjection::try_from(&source).unwrap();
        let transcript = converted.transcript.as_deref().unwrap();
        assert_eq!(converted.transcript_revision, source.revision.0);
        assert!(matches!(
            transcript.first(),
            Some(OutputItem::UserTurn { text })
                if text.contains("Inspect the project") && text.contains("[image: layout.png]")
        ));
        assert!(matches!(
            transcript.get(1),
            Some(OutputItem::WorkflowPanel { .. })
        ));
        let call = transcript
            .iter()
            .find_map(|item| match item {
                OutputItem::ToolDispatch { calls } => calls.first(),
                _ => None,
            })
            .expect("tool dispatch is restored");
        assert_eq!(call.status, ToolCallStatus::Ok);
        assert_eq!(call.applied_edit.as_ref().unwrap().1.insertions, 2);
        assert!(matches!(
            call.detail.as_deref(),
            Some(OutputItem::DiffPreview { title, .. }) if title == "README.md"
        ));
        assert!(transcript.iter().all(|item| {
            !matches!(item, OutputItem::UserTurn { text } if text.contains("hidden correction"))
        }));
        assert_eq!(
            transcript
                .iter()
                .filter(|item| matches!(item, OutputItem::CompactionSummary { .. }))
                .count(),
            1
        );
        assert!(matches!(
            transcript
                .iter()
                .find(|item| matches!(item, OutputItem::CompactionSummary { .. })),
            Some(OutputItem::CompactionSummary {
                compacted_count: 5,
                before_tokens: 2_000,
                after_tokens: 500,
                ..
            })
        ));
        assert!(transcript.iter().any(|item| matches!(
            item,
            OutputItem::SystemNote { text, level: NoteLevel::Warn }
                if text == "watch warning"
        )));
        assert!(transcript.iter().any(|item| matches!(
            item,
            OutputItem::MermaidDiagram { source } if source == "graph TD; A-->B"
        )));
    }

    #[test]
    fn routes_spawned_messages_into_the_existing_subagent_detail() {
        let mut source = projection();
        let now = chrono::Utc::now();
        let turn_id = source.workflows[0].turn_id.clone();
        let root_run_id = match &source.workflows[0].roots[0].kind {
            WorkflowNodeKind::Flow { run_id, .. } => run_id.clone(),
            _ => unreachable!(),
        };
        let child_run_id = atman_proto::FlowRunId(uuid::Uuid::now_v7());
        source.runs = vec![
            atman_proto::RunProjection {
                id: root_run_id.clone(),
                turn_id: Some(turn_id.clone()),
                flow_name: "agent".into(),
                model: Some("reasoning-model".into()),
                provider: Some("openai-compatible".into()),
                parent_run_id: None,
                parent_node_id: None,
                state: atman_proto::RunLifecycle::Running,
                started_at: now,
                finished_at: None,
                error: None,
            },
            atman_proto::RunProjection {
                id: child_run_id.clone(),
                turn_id: Some(turn_id.clone()),
                flow_name: "implementation".into(),
                model: Some("child-model".into()),
                provider: Some("openai-compatible".into()),
                parent_run_id: Some(root_run_id.clone()),
                parent_node_id: Some("spawn-call".into()),
                state: atman_proto::RunLifecycle::Succeeded,
                started_at: now,
                finished_at: Some(now),
                error: None,
            },
        ];
        source.workflows[0].roots[0]
            .children
            .push(WorkflowNodeProjection {
                id: "spawn-call".into(),
                kind: WorkflowNodeKind::ToolCall {
                    tool_use_id: "spawn-1".into(),
                    tool_name: "agent.at".into(),
                    args_preview: "implementation".into(),
                    intent: Some("Implement the change".into()),
                    result_preview: Some("done".into()),
                },
                label: "Implement the change".into(),
                state: WorkflowNodeState::Succeeded,
                started_at: Some(now),
                finished_at: Some(now),
                output_preview: Some("done".into()),
                children: vec![WorkflowNodeProjection {
                    id: child_run_id.0.to_string(),
                    kind: WorkflowNodeKind::Subflow {
                        run_id: child_run_id.clone(),
                        flow_name: "implementation".into(),
                    },
                    label: "implementation".into(),
                    state: WorkflowNodeState::Succeeded,
                    started_at: Some(now),
                    finished_at: Some(now),
                    output_preview: Some("implemented".into()),
                    children: Vec::new(),
                    parallel: false,
                    approval: None,
                    llm_usage: None,
                }],
                parallel: false,
                approval: None,
                llm_usage: None,
            });
        let message = |role, run_id, text: &str| atman_proto::TranscriptItem::Message {
            seq: 1,
            ts: now,
            run_id,
            context_id: None,
            checkpoint_index: None,
            message: atman_proto::MessageProjection {
                role,
                origin: atman_proto::MessageOrigin::User,
                turn_id: turn_id.clone(),
                parts: vec![atman_proto::MessagePart::Text { text: text.into() }],
            },
        };
        source.transcript = vec![
            message(atman_proto::MessageRole::User, None, "Ship the change"),
            atman_proto::TranscriptItem::Message {
                seq: 2,
                ts: now,
                run_id: Some(root_run_id),
                context_id: None,
                checkpoint_index: None,
                message: atman_proto::MessageProjection {
                    role: atman_proto::MessageRole::Assistant,
                    origin: atman_proto::MessageOrigin::User,
                    turn_id: turn_id.clone(),
                    parts: vec![atman_proto::MessagePart::ToolUse {
                        id: "spawn-1".into(),
                        name: "agent.at".into(),
                        input: serde_json::json!({"flow": "implementation"}),
                        intent: Some("Implement the change".into()),
                    }],
                },
            },
            message(
                atman_proto::MessageRole::User,
                Some(child_run_id.clone()),
                "Implement carefully",
            ),
            message(
                atman_proto::MessageRole::Assistant,
                Some(child_run_id),
                "Implemented",
            ),
        ];

        let converted = TuiSessionProjection::try_from(&source).unwrap();
        let transcript = converted.transcript.as_deref().unwrap();
        let detail = transcript
            .iter()
            .find_map(|item| match item {
                OutputItem::ToolDispatch { calls } => calls
                    .iter()
                    .find(|call| call.id == "spawn-1")
                    .and_then(|call| call.detail.as_deref()),
                _ => None,
            })
            .expect("spawned flow is attached to its tool call");
        assert!(matches!(
            detail,
            OutputItem::SubAgentActivity {
                model,
                output,
                done: true,
                ..
            } if model == "child-model" && output == "Implemented"
        ));
        assert!(transcript.iter().all(|item| {
            !matches!(item, OutputItem::AssistantMd { md, .. } if md == "Implemented")
        }));
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
