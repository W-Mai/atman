use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{Event, FlowNodeStatus, FlowStatus, TurnId};
use crate::permission::{PermissionGroupId, PermissionRequestId};
use crate::permission_audit::{PermissionGroupAudit, PermissionRequestAudit};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowGraph {
    pub turn_id: TurnId,
    pub root: Vec<WorkflowNode>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub permission_requests: BTreeMap<WorkflowPermissionIdentity, WorkflowPermissionRequest>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub permission_groups: BTreeMap<PermissionGroupId, PermissionGroupAudit>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub resolved_permission_groups: BTreeSet<PermissionGroupId>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowPermissionIdentity {
    Canonical {
        request_id: PermissionRequestId,
    },
    Legacy {
        seq: u64,
        run_id: String,
        tool_use_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkflowPermissionRequest {
    pub payload: PermissionRequestAudit,
    pub state: WorkflowPermissionState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPermissionState {
    Pending,
    Approved,
    Denied,
    Cancelled,
    Interrupted,
    Unrestricted,
}

impl WorkflowPermissionState {
    pub fn is_pending(self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowNode {
    pub id: String,
    pub kind: WorkflowNodeKind,
    pub label: String,
    pub status: NodeStatus,
    pub started_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub output_preview: Option<String>,
    pub children: Vec<WorkflowNode>,
    pub parallelism: Parallelism,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_stats: Option<LlmStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmStats {
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub context_call_purpose: crate::context_plan::ContextCallPurpose,
    #[serde(default)]
    pub context_call_scope: crate::context_plan::ContextCallScope,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub ttft_ms: u64,
    pub tokens_per_second: f64,
    pub wallclock_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkflowNodeKind {
    Flow {
        run_id: String,
        flow_name: String,
    },
    Stmt {
        node_kind: crate::nodegraph::NodeKind,
    },
    ToolCall {
        tool_use_id: String,
        tool: String,
        args_preview: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call_intent: Option<crate::message::ToolCallIntent>,
        result_preview: Option<String>,
    },
    Subflow {
        run_id: String,
        flow_name: String,
    },
    FanoutBranch {
        branch_index: usize,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Running,
    Ok,
    Err,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ApprovalState {
    Pending {
        level: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    Approved,
    Denied {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Parallelism {
    Serial,
    Parallel,
}

impl WorkflowGraph {
    pub fn new(turn_id: TurnId) -> Self {
        Self {
            turn_id,
            root: Vec::new(),
            permission_requests: BTreeMap::new(),
            permission_groups: BTreeMap::new(),
            resolved_permission_groups: BTreeSet::new(),
        }
    }

    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ..
            } => {
                let run_id_str = run_id.0.to_string();
                let node = WorkflowNode {
                    id: run_id_str.clone(),
                    kind: if parent_run_id.is_some() {
                        WorkflowNodeKind::Subflow {
                            run_id: run_id_str,
                            flow_name: flow_name.clone(),
                        }
                    } else {
                        WorkflowNodeKind::Flow {
                            run_id: run_id_str,
                            flow_name: flow_name.clone(),
                        }
                    },
                    label: flow_name.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(chrono::Utc::now()),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                match (parent_run_id.as_ref(), parent_node_id.as_deref()) {
                    (Some(prid), Some(pid)) => {
                        let scoped = scope_id(&prid.0.to_string(), pid);
                        if let Some(parent) = find_node_mut(&mut self.root, &scoped) {
                            parent.children.push(node);
                        }
                    }
                    _ => self.root.push(node),
                }
            }
            Event::FlowEnd { run_id, status, .. } => {
                let id = run_id.0.to_string();
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    let new_status = match status {
                        FlowStatus::Ok => NodeStatus::Ok,
                        FlowStatus::Errored { .. } => NodeStatus::Err,
                        FlowStatus::Cancelled => NodeStatus::Cancelled,
                    };
                    n.status = new_status;
                    n.ended_at = Some(chrono::Utc::now());
                    for child in n.children.iter_mut() {
                        if matches!(child.status, NodeStatus::Running | NodeStatus::Pending) {
                            child.status = new_status;
                            child.ended_at = Some(chrono::Utc::now());
                        }
                    }
                }
            }
            Event::FlowNodeStart {
                run_id,
                node_id,
                kind: nk,
                label,
                parent_node_id,
                ..
            } => {
                let rid = run_id.0.to_string();
                let scoped_id = scope_id(&rid, node_id);
                let parent_id = parent_node_id
                    .as_deref()
                    .map(|p| scope_id(&rid, p))
                    .unwrap_or_else(|| rid.clone());
                let kind = if let Some(idx) = parse_branch_index(node_id) {
                    WorkflowNodeKind::FanoutBranch { branch_index: idx }
                } else {
                    WorkflowNodeKind::Stmt {
                        node_kind: nk.clone(),
                    }
                };
                let node = WorkflowNode {
                    id: scoped_id,
                    kind,
                    label: label.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(chrono::Utc::now()),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &parent_id) {
                    if matches!(node.kind, WorkflowNodeKind::FanoutBranch { .. }) {
                        parent.parallelism = Parallelism::Parallel;
                    }
                    parent.children.push(node);
                }
            }
            Event::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ..
            } => {
                let scoped = scope_id(&run_id.0.to_string(), node_id);
                if let Some(n) = find_node_mut(&mut self.root, &scoped) {
                    let new_status = match status {
                        FlowNodeStatus::Ok => NodeStatus::Ok,
                        FlowNodeStatus::Err => NodeStatus::Err,
                        FlowNodeStatus::Cancelled => NodeStatus::Cancelled,
                    };
                    n.status = new_status;
                    n.ended_at = Some(chrono::Utc::now());
                    if let Some(p) = output_preview {
                        n.output_preview = Some(p.clone());
                    }
                    for child in n.children.iter_mut() {
                        if matches!(child.status, NodeStatus::Running | NodeStatus::Pending) {
                            child.status = new_status;
                            child.ended_at = Some(chrono::Utc::now());
                        }
                    }
                }
            }
            Event::LlmCall {
                run_id,
                node_id,
                model,
                provider,
                context_call_purpose,
                context_call_identity,
                usage,
                wallclock_ms,
                ttft_ms,
                tokens_per_second,
                ..
            } => {
                if let (Some(rid), Some(nid)) = (run_id, node_id) {
                    let scoped = scope_id(&rid.0.to_string(), nid);
                    if let Some(n) = find_node_mut(&mut self.root, &scoped) {
                        n.llm_stats = Some(LlmStats {
                            model: model.clone(),
                            provider: provider.clone(),
                            context_call_purpose: context_call_purpose.unwrap_or_default(),
                            context_call_scope: context_call_identity
                                .as_ref()
                                .map(|identity| identity.scope)
                                .unwrap_or_else(|| {
                                    if run_id.is_some() {
                                        crate::context_plan::ContextCallScope::Root
                                    } else {
                                        crate::context_plan::ContextCallScope::Detached
                                    }
                                }),
                            input_tokens: usage.input,
                            output_tokens: usage.output,
                            cache_read: usage.cached_input,
                            cache_write: usage.cache_write,
                            ttft_ms: ttft_ms.unwrap_or(0),
                            tokens_per_second: tokens_per_second.unwrap_or(0.0),
                            wallclock_ms: *wallclock_ms,
                        });
                    }
                }
            }
            Event::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                call_intent,
                ..
            } => {
                let rid = run_id.0.to_string();
                let scoped_parent = scope_id(&rid, parent_node_id);
                let id = tool_node_id(&rid, tool_use_id);
                if find_node(&self.root, &id).is_some() {
                    return;
                }
                let node = WorkflowNode {
                    id,
                    kind: WorkflowNodeKind::ToolCall {
                        tool_use_id: tool_use_id.clone(),
                        tool: tool_name.clone(),
                        args_preview: args_preview.clone(),
                        call_intent: call_intent.clone(),
                        result_preview: None,
                    },
                    label: tool_name.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(chrono::Utc::now()),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &scoped_parent) {
                    parent.children.push(node);
                    self.refresh_permission_tool_approvals();
                }
            }
            Event::AssistantMsg { .. } => {}
            Event::ToolResultMsg {
                flow_run_id,
                message,
                ..
            } => {
                let flow_id = flow_run_id.as_ref().map(|r| r.0.to_string());
                for part in &message.parts {
                    if let crate::message::MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = part
                    {
                        let node = match flow_id.as_deref() {
                            Some(rid) => {
                                let id = tool_node_id(rid, tool_use_id);
                                find_node_mut(&mut self.root, &id)
                            }
                            None => find_tool_node_by_tool_use_id(&mut self.root, tool_use_id),
                        };
                        if let Some(n) = node {
                            n.status = if *is_error {
                                NodeStatus::Err
                            } else {
                                NodeStatus::Ok
                            };
                            n.ended_at = Some(chrono::Utc::now());
                            let preview: String = content.chars().take(300).collect();
                            n.output_preview = Some(preview.clone());
                            if let WorkflowNodeKind::ToolCall { result_preview, .. } = &mut n.kind {
                                *result_preview = Some(preview);
                            }
                        }
                    }
                }
            }
            Event::ToolPendingApproval {
                run_id,
                tool_use_id,
                level,
                preview,
                ..
            } => {
                let rid = run_id.0.to_string();
                let id = tool_node_id(&rid, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Pending {
                        level: level.clone(),
                        preview: preview.clone(),
                    });
                }
            }
            Event::ToolApproved {
                run_id,
                tool_use_id,
                ..
            } => {
                let rid = run_id.0.to_string();
                let id = tool_node_id(&rid, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Approved);
                }
            }
            Event::ToolDenied {
                run_id,
                tool_use_id,
                reason,
                ..
            } => {
                let rid = run_id.0.to_string();
                let id = tool_node_id(&rid, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Denied {
                        reason: reason.clone(),
                    });
                }
            }
            Event::PermissionRequestCreated { payload }
            | Event::PermissionRequestTargeted { payload }
            | Event::PermissionRequestDeferred { payload } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Pending);
            }
            Event::PermissionRequestApproved { payload } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Approved);
            }
            Event::PermissionRequestDenied { payload } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Denied);
            }
            Event::PermissionRequestCancelled { payload } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Cancelled);
            }
            Event::UnrestrictedExecution { payload } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Unrestricted);
            }
            Event::PermissionGroupCreated { payload }
            | Event::PermissionGroupUpdated { payload } => {
                self.apply_permission_group(payload, false);
            }
            Event::PermissionGroupResolved { payload } => {
                self.apply_permission_group(payload, true);
            }
            _ => {}
        }
    }

    pub fn find_node(&self, id: &str) -> Option<&WorkflowNode> {
        find_node(&self.root, id)
    }

    pub fn find_node_mut(&mut self, id: &str) -> Option<&mut WorkflowNode> {
        find_node_mut(&mut self.root, id)
    }

    pub fn descendant_pending_permissions(&self, flow_node_id: &str) -> usize {
        let Some(node) = find_node(&self.root, flow_node_id) else {
            return 0;
        };
        let mut run_ids = Vec::new();
        collect_flow_run_ids(node, &mut run_ids);
        self.permission_requests
            .values()
            .filter(|request| {
                request.state.is_pending()
                    && run_ids.contains(&request.payload.requesting_run_id.0.to_string())
            })
            .count()
    }

    pub fn interrupt_pending_permissions(&mut self) {
        for request in self.permission_requests.values_mut() {
            if request.state.is_pending() {
                request.state = WorkflowPermissionState::Interrupted;
                request.payload.reason = Some("interrupted at end of persisted history".into());
            }
        }
        self.refresh_permission_tool_approvals();
    }

    pub fn apply_permission_request(
        &mut self,
        payload: &PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) {
        let Some(request_id) = payload.request_id.clone() else {
            return;
        };
        self.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Canonical { request_id },
            payload,
            state,
        );
    }

    pub fn apply_permission_request_with_identity(
        &mut self,
        identity: WorkflowPermissionIdentity,
        payload: &PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) {
        self.apply_permission_requests([(identity, payload.clone(), state)]);
    }

    pub fn apply_permission_requests(
        &mut self,
        requests: impl IntoIterator<
            Item = (
                WorkflowPermissionIdentity,
                PermissionRequestAudit,
                WorkflowPermissionState,
            ),
        >,
    ) {
        for (identity, payload, state) in requests {
            self.permission_requests
                .insert(identity, WorkflowPermissionRequest { payload, state });
        }
        self.refresh_permission_tool_approvals();
    }

    pub fn apply_permission_group(&mut self, payload: &PermissionGroupAudit, resolved: bool) {
        if resolved {
            self.permission_groups.remove(&payload.group_id);
            self.resolved_permission_groups
                .insert(payload.group_id.clone());
        } else if !self.resolved_permission_groups.contains(&payload.group_id) {
            self.permission_groups
                .insert(payload.group_id.clone(), payload.clone());
        }
    }

    fn refresh_permission_tool_approvals(&mut self) {
        let mut exact: BTreeMap<(String, String), &WorkflowPermissionRequest> = BTreeMap::new();
        for request in self.permission_requests.values() {
            let key = (
                request.payload.requesting_run_id.0.to_string(),
                request.payload.tool_use_id.clone(),
            );
            exact
                .entry(key)
                .and_modify(|current| {
                    if (!current.state.is_pending() && request.state.is_pending())
                        || (current.state.is_pending() == request.state.is_pending()
                            && request.payload.at > current.payload.at)
                    {
                        *current = request;
                    }
                })
                .or_insert(request);
        }
        let approvals = exact
            .into_iter()
            .map(|((run_id, tool_use_id), request)| {
                let approval = match request.state {
                    WorkflowPermissionState::Pending => ApprovalState::Pending {
                        level: format!("{:?}", request.payload.tier).to_lowercase(),
                        preview: None,
                    },
                    WorkflowPermissionState::Approved | WorkflowPermissionState::Unrestricted => {
                        ApprovalState::Approved
                    }
                    WorkflowPermissionState::Denied
                    | WorkflowPermissionState::Cancelled
                    | WorkflowPermissionState::Interrupted => ApprovalState::Denied {
                        reason: request
                            .payload
                            .reason
                            .clone()
                            .unwrap_or_else(|| "permission denied".into()),
                    },
                };
                (tool_node_id(&run_id, &tool_use_id), approval)
            })
            .collect();
        apply_permission_approvals(&mut self.root, &approvals);
    }

    pub fn apply_stream_frame(&mut self, frame: &crate::stream::StreamFrame) {
        self.apply_stream_frame_at(frame, None);
    }

    pub fn apply_stream_frame_at(
        &mut self,
        frame: &crate::stream::StreamFrame,
        override_ts: Option<chrono::DateTime<chrono::Utc>>,
    ) {
        use crate::stream::StreamFrame;
        let now = override_ts.unwrap_or_else(Utc::now);
        match frame {
            StreamFrame::FlowGraph { run_id, graph } => {
                if self.find_node(run_id).is_none() {
                    self.root.push(WorkflowNode {
                        id: run_id.clone(),
                        kind: WorkflowNodeKind::Flow {
                            run_id: run_id.clone(),
                            flow_name: graph.flow_name.clone(),
                        },
                        label: graph.flow_name.clone(),
                        status: NodeStatus::Running,
                        started_at: Some(now),
                        ended_at: None,
                        output_preview: None,
                        children: Vec::new(),
                        parallelism: Parallelism::Serial,
                        approval: None,
                        llm_stats: None,
                    });
                }
            }
            StreamFrame::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
            } => {
                if self.find_node(run_id).is_some() {
                    return;
                }
                let kind = if parent_run_id.is_some() {
                    WorkflowNodeKind::Subflow {
                        run_id: run_id.clone(),
                        flow_name: flow_name.clone(),
                    }
                } else {
                    WorkflowNodeKind::Flow {
                        run_id: run_id.clone(),
                        flow_name: flow_name.clone(),
                    }
                };
                let node = WorkflowNode {
                    id: run_id.clone(),
                    kind,
                    label: flow_name.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(now),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                match (parent_run_id.as_deref(), parent_node_id.as_deref()) {
                    (Some(prid), Some(pid)) => {
                        let scoped = scope_id(prid, pid);
                        if let Some(parent) = find_node_mut(&mut self.root, &scoped) {
                            parent.children.push(node);
                        } else {
                            self.root.push(node);
                        }
                    }
                    _ => self.root.push(node),
                }
            }
            StreamFrame::FlowNodeStart {
                run_id,
                node_id,
                kind: nk,
                label,
                parent_node_id,
            } => {
                let scoped_id = scope_id(run_id, node_id);
                let parent_id = parent_node_id
                    .as_deref()
                    .map(|p| scope_id(run_id, p))
                    .unwrap_or_else(|| run_id.clone());
                let kind = if let Some(idx) = parse_branch_index(node_id) {
                    WorkflowNodeKind::FanoutBranch { branch_index: idx }
                } else {
                    WorkflowNodeKind::Stmt {
                        node_kind: nk.clone(),
                    }
                };
                let node = WorkflowNode {
                    id: scoped_id,
                    kind,
                    label: label.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(now),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &parent_id) {
                    if matches!(node.kind, WorkflowNodeKind::FanoutBranch { .. }) {
                        parent.parallelism = Parallelism::Parallel;
                    }
                    parent.children.push(node);
                }
            }
            StreamFrame::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ..
            } => {
                let scoped = scope_id(run_id, node_id);
                if let Some(n) = find_node_mut(&mut self.root, &scoped) {
                    let new_status = match status {
                        FlowNodeStatus::Ok => NodeStatus::Ok,
                        FlowNodeStatus::Err => NodeStatus::Err,
                        FlowNodeStatus::Cancelled => NodeStatus::Cancelled,
                    };
                    n.status = new_status;
                    n.ended_at = Some(now);
                    if let Some(p) = output_preview {
                        n.output_preview = Some(p.clone());
                    }
                    for child in n.children.iter_mut() {
                        if matches!(child.status, NodeStatus::Running | NodeStatus::Pending) {
                            child.status = new_status;
                            child.ended_at = Some(now);
                        }
                    }
                }
            }
            StreamFrame::LlmCallStats {
                model,
                provider,
                context_call_purpose,
                context_call_scope,
                input_tokens,
                output_tokens,
                cache_read,
                cache_write,
                ttft_ms,
                tokens_per_second,
                wallclock_ms,
                run_id,
                node_id,
            } => {
                if let (Some(rid), Some(nid)) = (run_id.as_deref(), node_id.as_deref()) {
                    let scoped = scope_id(rid, nid);
                    if let Some(n) = find_node_mut(&mut self.root, &scoped) {
                        n.llm_stats = Some(LlmStats {
                            model: model.clone(),
                            provider: provider.clone(),
                            context_call_purpose: *context_call_purpose,
                            context_call_scope: *context_call_scope,
                            input_tokens: *input_tokens,
                            output_tokens: *output_tokens,
                            cache_read: *cache_read,
                            cache_write: *cache_write,
                            ttft_ms: *ttft_ms,
                            tokens_per_second: *tokens_per_second,
                            wallclock_ms: *wallclock_ms,
                        });
                    }
                }
            }
            StreamFrame::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool,
                args_preview,
                call_intent,
                ..
            } => {
                let scoped_parent = scope_id(run_id, parent_node_id);
                let id = tool_node_id(run_id, tool_use_id);
                if find_node(&self.root, &id).is_some() {
                    return;
                }
                let node = WorkflowNode {
                    id,
                    kind: WorkflowNodeKind::ToolCall {
                        tool_use_id: tool_use_id.clone(),
                        tool: tool.clone(),
                        args_preview: args_preview.clone(),
                        call_intent: call_intent.clone(),
                        result_preview: None,
                    },
                    label: tool.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(now),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                    llm_stats: None,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &scoped_parent) {
                    parent.children.push(node);
                    self.refresh_permission_tool_approvals();
                }
            }
            StreamFrame::ToolUseDone {
                id, ok, preview, ..
            } => {
                if let Some(n) = find_tool_node_by_tool_use_id(&mut self.root, id) {
                    n.status = if *ok { NodeStatus::Ok } else { NodeStatus::Err };
                    n.ended_at = Some(now);
                    n.output_preview = Some(preview.clone());
                }
            }
            StreamFrame::FlowDone {
                run_id,
                ok,
                cancelled,
                ..
            } => {
                if let Some(n) = find_node_mut(&mut self.root, run_id) {
                    let status = if *cancelled {
                        NodeStatus::Cancelled
                    } else if *ok {
                        NodeStatus::Ok
                    } else {
                        NodeStatus::Err
                    };
                    cascade_terminate(n, status, now);
                }
            }
            StreamFrame::AssistantMsg {
                flow_run_id,
                message,
            } => {
                let Some(rid_str) = flow_run_id else { return };
                let Ok(uuid) = uuid::Uuid::parse_str(rid_str) else {
                    return;
                };
                self.apply_event(&Event::AssistantMsg {
                    turn_id: crate::event::TurnId::now(),
                    flow_run_id: Some(crate::event::FlowRunId(uuid)),
                    message: message.clone(),
                });
            }
            StreamFrame::ToolResultMsg {
                flow_run_id,
                message,
            } => {
                let scoped_run_id = flow_run_id
                    .as_deref()
                    .and_then(|rid| uuid::Uuid::parse_str(rid).ok())
                    .map(crate::event::FlowRunId);
                self.apply_event(&Event::ToolResultMsg {
                    turn_id: crate::event::TurnId::now(),
                    flow_run_id: scoped_run_id,
                    message: message.clone(),
                });
            }
            StreamFrame::ToolPendingApproval {
                run_id,
                tool_use_id,
                level,
                preview,
                ..
            } => {
                let id = tool_node_id(run_id, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Pending {
                        level: level.clone(),
                        preview: preview.clone(),
                    });
                }
            }
            StreamFrame::ToolApproved {
                run_id,
                tool_use_id,
                ..
            } => {
                let id = tool_node_id(run_id, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Approved);
                }
            }
            StreamFrame::ToolDenied {
                run_id,
                tool_use_id,
                reason,
            } => {
                let id = tool_node_id(run_id, tool_use_id);
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.approval = Some(ApprovalState::Denied {
                        reason: reason.clone(),
                    });
                }
            }
            StreamFrame::PermissionRequestCreated { payload, .. }
            | StreamFrame::PermissionRequestTargeted { payload, .. }
            | StreamFrame::PermissionRequestDeferred { payload, .. } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Pending);
            }
            StreamFrame::PermissionRequestApproved { payload, .. } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Approved);
            }
            StreamFrame::PermissionRequestDenied { payload, .. } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Denied);
            }
            StreamFrame::PermissionRequestCancelled { payload, .. } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Cancelled);
            }
            StreamFrame::UnrestrictedExecution { payload, .. } => {
                self.apply_permission_request(payload, WorkflowPermissionState::Unrestricted);
            }
            StreamFrame::PermissionGroupCreated { payload, .. }
            | StreamFrame::PermissionGroupUpdated { payload, .. } => {
                self.apply_permission_group(payload, false);
            }
            StreamFrame::PermissionGroupResolved { payload, .. } => {
                self.apply_permission_group(payload, true);
            }
            _ => {}
        }
    }
}

fn cascade_terminate(n: &mut WorkflowNode, status: NodeStatus, now: DateTime<Utc>) {
    if matches!(n.status, NodeStatus::Running | NodeStatus::Pending) {
        n.status = status;
        n.ended_at = Some(now);
    }
    for child in n.children.iter_mut() {
        cascade_terminate(child, status, now);
    }
}

fn apply_permission_approvals(
    nodes: &mut [WorkflowNode],
    approvals: &BTreeMap<String, ApprovalState>,
) {
    for node in nodes {
        if let Some(approval) = approvals.get(&node.id) {
            node.approval = Some(approval.clone());
        }
        apply_permission_approvals(&mut node.children, approvals);
    }
}

fn find_node<'a>(nodes: &'a [WorkflowNode], id: &str) -> Option<&'a WorkflowNode> {
    for n in nodes {
        if n.id == id {
            return Some(n);
        }
        if let Some(hit) = find_node(&n.children, id) {
            return Some(hit);
        }
    }
    None
}

fn find_node_mut<'a>(nodes: &'a mut [WorkflowNode], id: &str) -> Option<&'a mut WorkflowNode> {
    for n in nodes.iter_mut() {
        if n.id == id {
            return Some(n);
        }
        if let Some(hit) = find_node_mut(&mut n.children, id) {
            return Some(hit);
        }
    }
    None
}

fn scope_id(run_id: &str, node_id: &str) -> String {
    format!("{run_id}::{node_id}")
}

fn tool_node_id(run_id: &str, tool_use_id: &str) -> String {
    format!("tool:{run_id}:{tool_use_id}")
}

fn collect_flow_run_ids(node: &WorkflowNode, out: &mut Vec<String>) {
    match &node.kind {
        WorkflowNodeKind::Flow { run_id, .. } | WorkflowNodeKind::Subflow { run_id, .. } => {
            out.push(run_id.clone());
        }
        _ => {}
    }
    for child in &node.children {
        collect_flow_run_ids(child, out);
    }
}

pub fn permission_preview(payload: &PermissionRequestAudit) -> Option<String> {
    let mut lines = vec![format!("intent: {} ({:?})", payload.tool, payload.tier)];
    if let Some(call_intent) = &payload.call_intent {
        lines.push(format!("purpose: {}", call_intent.as_str()));
    }
    let provenance = &payload.provenance;
    if let Some(path) = &provenance.path {
        lines.push(format!("path: {path}"));
    }
    if let Some(cwd) = &provenance.cwd {
        lines.push(format!("cwd: {cwd}"));
    }
    if let Some(workspace) = &provenance.workspace_root {
        lines.push(format!("workspace: {workspace}"));
    }
    if provenance.network {
        lines.push("network: true".into());
    }
    if !provenance.risks.is_empty() {
        lines.push(format!(
            "risks: {}",
            provenance
                .risks
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !provenance.targets.is_empty() {
        lines.push(format!("targets: {}", provenance.targets.join(", ")));
    }
    lines.push(format!("policy: {}", payload.policy.snapshot_id));
    lines.push(format!("rule: {}", payload.policy.rule_id));
    Some(lines.join("\n"))
}

fn find_tool_node_by_tool_use_id<'a>(
    nodes: &'a mut [WorkflowNode],
    tool_use_id: &str,
) -> Option<&'a mut WorkflowNode> {
    for n in nodes.iter_mut() {
        if let WorkflowNodeKind::ToolCall {
            tool_use_id: tid, ..
        } = &n.kind
            && tid == tool_use_id
        {
            return Some(n);
        }
        if let Some(hit) = find_tool_node_by_tool_use_id(&mut n.children, tool_use_id) {
            return Some(hit);
        }
    }
    None
}

fn parse_branch_index(node_id: &str) -> Option<usize> {
    let start = node_id.rfind(".branch[")?;
    let rest = &node_id[start + ".branch[".len()..];
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

/// Rebuild a full workflow tree from a session's event log. Replays every
/// FlowStart / FlowNodeStart / FlowNodeEnd / FlowEnd / FlowGraph event through
/// a fresh WorkflowGraph so the complete executor tree (root + subflows) is
/// restored on session reopen.
pub fn rebuild_workflow_tree(events: &[crate::event::Event]) -> WorkflowGraph {
    let mut g = WorkflowGraph::new(crate::event::TurnId::now());
    for ev in events {
        g.apply_event(ev);
    }
    g
}

/// Rebuild a single FlowRun's message segment from the event log, filtered by
/// run_id. Returns AssistantMsg + ToolResultMsg messages tagged with the given
/// flow_run_id, in event order.
pub fn rebuild_messages_for_run(
    events: &[crate::event::Event],
    run_id: &crate::event::FlowRunId,
) -> Vec<crate::message::Message> {
    events
        .iter()
        .filter_map(|ev| match ev {
            crate::event::Event::AssistantMsg {
                flow_run_id: Some(rid),
                message,
                ..
            } if rid == run_id => Some(message.clone()),
            crate::event::Event::ToolResultMsg {
                flow_run_id: Some(rid),
                message,
                ..
            } if rid == run_id => Some(message.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FlowRunId, FlowStatus};
    use crate::nodegraph::NodeKind;
    use std::collections::BTreeSet;

    fn flow_start(run_id: FlowRunId, name: &str) -> Event {
        Event::FlowStart {
            run_id,
            flow_name: name.into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        }
    }

    fn subflow_start(child: FlowRunId, parent: FlowRunId, parent_node: &str, name: &str) -> Event {
        Event::FlowStart {
            run_id: child,
            flow_name: name.into(),
            parent_run_id: Some(parent),
            parent_node_id: Some(parent_node.into()),
            spawned: false,
        }
    }

    fn stmt_start(run_id: FlowRunId, node_id: &str, parent: Option<&str>) -> Event {
        Event::FlowNodeStart {
            run_id,
            node_id: node_id.into(),
            kind: NodeKind::UserConfirm,
            label: node_id.into(),
            parent_node_id: parent.map(String::from),
        }
    }

    fn stmt_end(run_id: FlowRunId, node_id: &str, status: FlowNodeStatus) -> Event {
        Event::FlowNodeEnd {
            run_id,
            node_id: node_id.into(),
            status,
            output_preview: None,
        }
    }

    fn request_id() -> PermissionRequestId {
        PermissionRequestId(uuid::Uuid::now_v7())
    }

    fn permission_payload(
        request_id: PermissionRequestId,
        requesting_run_id: FlowRunId,
        root_run_id: FlowRunId,
        tool_use_id: &str,
        at: chrono::DateTime<chrono::Utc>,
    ) -> PermissionRequestAudit {
        PermissionRequestAudit {
            request_id: Some(request_id),
            revision: 1,
            session_id: "session".into(),
            requesting_run_id,
            parent_run_id: None,
            root_run_id,
            tool_use_id: tool_use_id.into(),
            tool: "fs.read".into(),
            call_intent: None,
            tier: crate::tool::Tier::Two,
            execution_boundary: Default::default(),
            provenance: crate::permission_audit::PermissionProvenanceSummary {
                cwd: None,
                path: None,
                path_origin: None,
                workspace_id: None,
                workspace_root: None,
                repository_root: None,
                network: false,
                risks: BTreeSet::new(),
                targets: Vec::new(),
            },
            target: crate::permission_audit::PermissionAuditTarget::User,
            group_ids: Vec::new(),
            policy: crate::permission_audit::PermissionPolicyReference {
                snapshot_id: "snapshot".into(),
                rule_id: "rule".into(),
            },
            escalation_path: Vec::new(),
            decision_id: None,
            actor: None,
            scope: None,
            reason: None,
            at,
        }
    }

    fn add_tool(graph: &mut WorkflowGraph, run_id: &FlowRunId, tool_use_id: &str) {
        graph.apply_event(&flow_start(run_id.clone(), "agent_loop"));
        graph.apply_event(&stmt_start(run_id.clone(), "dispatch_all", None));
        graph.apply_event(&Event::ToolNode {
            run_id: run_id.clone(),
            parent_node_id: "dispatch_all".into(),
            tool_use_id: tool_use_id.into(),
            tool_name: "fs.read".into(),
            args_preview: String::new(),
            call_intent: None,
        });
    }

    #[test]
    fn top_level_flow_becomes_root_child() {
        let mut g = WorkflowGraph::new(TurnId::now());
        let rid = FlowRunId::now();
        g.apply_event(&flow_start(rid.clone(), "main"));
        assert_eq!(g.root.len(), 1);
        let flow = &g.root[0];
        assert!(matches!(flow.kind, WorkflowNodeKind::Flow { .. }));
        assert_eq!(flow.status, NodeStatus::Running);
        assert_eq!(flow.id, rid.0.to_string());
    }

    #[test]
    fn subflow_attaches_under_parent_node() {
        let mut g = WorkflowGraph::new(TurnId::now());
        let parent_flow = FlowRunId::now();
        let child_flow = FlowRunId::now();
        g.apply_event(&flow_start(parent_flow.clone(), "outer"));
        g.apply_event(&stmt_start(parent_flow.clone(), "stmt_0", None));
        g.apply_event(&subflow_start(
            child_flow.clone(),
            parent_flow.clone(),
            "stmt_0",
            "inner",
        ));
        let scoped = scope_id(&parent_flow.0.to_string(), "stmt_0");
        let stmt = g.find_node(&scoped).unwrap();
        assert_eq!(stmt.children.len(), 1);
        assert!(matches!(
            stmt.children[0].kind,
            WorkflowNodeKind::Subflow { .. }
        ));
        assert_eq!(stmt.children[0].id, child_flow.0.to_string());
    }

    #[test]
    fn tool_node_attaches_and_flow_end_marks_status() {
        let mut g = WorkflowGraph::new(TurnId::now());
        let rid = FlowRunId::now();
        g.apply_event(&flow_start(rid.clone(), "main"));
        g.apply_event(&stmt_start(rid.clone(), "stmt_0", None));
        g.apply_event(&Event::ToolNode {
            run_id: rid.clone(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_1".into(),
            tool_name: "fs.read".into(),
            args_preview: "{\"path\":\"a\"}".into(),
            call_intent: crate::message::ToolCallIntent::new("Inspect source"),
        });
        g.apply_event(&stmt_end(rid.clone(), "stmt_0", FlowNodeStatus::Ok));
        let tool = g
            .find_node(&tool_node_id(&rid.0.to_string(), "tu_1"))
            .expect("tool node");
        assert!(matches!(
            &tool.kind,
            WorkflowNodeKind::ToolCall { call_intent: Some(intent), .. }
                if intent.as_str() == "Inspect source"
        ));
        g.apply_event(&Event::FlowEnd {
            run_id: rid.clone(),
            flow_name: "main".into(),
            status: FlowStatus::Ok,
        });
        let scoped = scope_id(&rid.0.to_string(), "stmt_0");
        let stmt = g.find_node(&scoped).unwrap();
        assert_eq!(stmt.status, NodeStatus::Ok);
        assert_eq!(stmt.children.len(), 1);
        let tool = &stmt.children[0];
        assert_eq!(tool.id, tool_node_id(&rid.0.to_string(), "tu_1"));
        assert!(matches!(tool.kind, WorkflowNodeKind::ToolCall { .. }));
        assert_eq!(g.root[0].status, NodeStatus::Ok);
    }

    #[test]
    fn assistant_tool_use_waits_for_scoped_tool_node_and_result() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
        use crate::stream::StreamFrame;

        let mut graph = WorkflowGraph::new(TurnId::now());
        let run_id = FlowRunId::now();
        let run = run_id.0.to_string();
        graph.apply_event(&flow_start(run_id.clone(), "agent_loop"));
        graph.apply_event(&stmt_start(run_id.clone(), "llm", None));
        graph.apply_stream_frame(&StreamFrame::AssistantMsg {
            flow_run_id: Some(run.clone()),
            message: Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "tu_1".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                    intent: None,
                }],
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
        });
        assert!(graph.find_node(&tool_node_id(&run, "tu_1")).is_none());

        graph.apply_event(&stmt_start(run_id.clone(), "dispatch_all", None));
        graph.apply_stream_frame(&StreamFrame::ToolNode {
            run_id: run.clone(),
            parent_node_id: "dispatch_all".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: "{path: a.rs}".into(),
            call_intent: None,
        });
        graph.apply_stream_frame(&StreamFrame::ToolResultMsg {
            flow_run_id: Some(run.clone()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "contents".into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let dispatch = graph
            .find_node(&scope_id(&run, "dispatch_all"))
            .expect("dispatch statement");
        assert_eq!(dispatch.children.len(), 1);
        assert_eq!(dispatch.children[0].status, NodeStatus::Ok);
        assert_eq!(
            dispatch.children[0].output_preview.as_deref(),
            Some("contents")
        );
        assert!(
            graph.root[0]
                .children
                .iter()
                .all(|node| !matches!(node.kind, WorkflowNodeKind::ToolCall { .. }))
        );
    }

    #[test]
    fn llm_stream_stats_preserve_route_metadata() {
        use crate::stream::StreamFrame;

        let mut graph = WorkflowGraph::new(TurnId::now());
        let run_id = FlowRunId::now();
        let run = run_id.0.to_string();
        graph.apply_event(&flow_start(run_id.clone(), "agent_loop"));
        graph.apply_event(&stmt_start(run_id, "llm", None));
        graph.apply_stream_frame(&StreamFrame::LlmCallStats {
            model: "helper-model".into(),
            provider: "helper-provider".into(),
            context_call_purpose: crate::context_plan::ContextCallPurpose::Extraction,
            context_call_scope: crate::context_plan::ContextCallScope::Child,
            input_tokens: 100,
            output_tokens: 10,
            cache_read: 20,
            cache_write: 30,
            ttft_ms: 40,
            tokens_per_second: 50.0,
            wallclock_ms: 60,
            run_id: Some(run.clone()),
            node_id: Some("llm".into()),
        });

        let stats = graph
            .find_node(&scope_id(&run, "llm"))
            .and_then(|node| node.llm_stats.as_ref())
            .expect("llm stats");
        assert_eq!(stats.provider, "helper-provider");
        assert_eq!(
            stats.context_call_purpose,
            crate::context_plan::ContextCallPurpose::Extraction
        );
        assert_eq!(
            stats.context_call_scope,
            crate::context_plan::ContextCallScope::Child
        );
    }

    #[test]
    fn scoped_tool_result_updates_only_matching_run() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
        use crate::stream::StreamFrame;

        let mut graph = WorkflowGraph::new(TurnId::now());
        let run_a = FlowRunId::now();
        let run_b = FlowRunId::now();
        for run_id in [&run_a, &run_b] {
            graph.apply_event(&flow_start(run_id.clone(), "agent_loop"));
            graph.apply_event(&stmt_start(run_id.clone(), "dispatch_all", None));
            graph.apply_stream_frame(&StreamFrame::ToolNode {
                run_id: run_id.0.to_string(),
                parent_node_id: "dispatch_all".into(),
                tool_use_id: "same_id".into(),
                tool: "fs.read".into(),
                args_preview: String::new(),
                call_intent: None,
            });
        }
        graph.apply_stream_frame(&StreamFrame::ToolResultMsg {
            flow_run_id: Some(run_a.0.to_string()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "same_id".into(),
                    content: "done".into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        assert_eq!(
            graph
                .find_node(&tool_node_id(&run_a.0.to_string(), "same_id"))
                .unwrap()
                .status,
            NodeStatus::Ok
        );
        assert_eq!(
            graph
                .find_node(&tool_node_id(&run_b.0.to_string(), "same_id"))
                .unwrap()
                .status,
            NodeStatus::Running
        );
    }

    #[test]
    fn permission_tool_approval_correlates_by_exact_run_and_tool() {
        let mut graph = WorkflowGraph::new(TurnId::now());
        let run_a = FlowRunId::now();
        let run_b = FlowRunId::now();
        add_tool(&mut graph, &run_a, "shared");
        add_tool(&mut graph, &run_b, "shared");
        let payload = permission_payload(
            request_id(),
            run_a.clone(),
            run_a.clone(),
            "shared",
            Utc::now(),
        );
        graph.apply_event(&Event::PermissionRequestCreated { payload });

        assert!(matches!(
            graph
                .find_node(&tool_node_id(&run_a.0.to_string(), "shared"))
                .unwrap()
                .approval,
            Some(ApprovalState::Pending { .. })
        ));
        assert_eq!(
            graph
                .find_node(&tool_node_id(&run_b.0.to_string(), "shared"))
                .unwrap()
                .approval,
            None
        );
    }

    #[test]
    fn permission_arriving_before_tool_refreshes_when_tool_is_added() {
        let mut graph = WorkflowGraph::new(TurnId::now());
        let run_id = FlowRunId::now();
        let payload = permission_payload(
            request_id(),
            run_id.clone(),
            run_id.clone(),
            "late-tool",
            Utc::now(),
        );
        graph.apply_event(&Event::PermissionRequestCreated { payload });
        add_tool(&mut graph, &run_id, "late-tool");
        assert!(matches!(
            graph
                .find_node(&tool_node_id(&run_id.0.to_string(), "late-tool"))
                .unwrap()
                .approval,
            Some(ApprovalState::Pending { .. })
        ));
    }

    #[test]
    fn batched_permission_projection_matches_incremental_updates() {
        let run_id = FlowRunId::now();
        let request_id = request_id();
        let pending = permission_payload(
            request_id.clone(),
            run_id.clone(),
            run_id.clone(),
            "batched-tool",
            Utc::now(),
        );
        let mut approved = pending.clone();
        approved.at += chrono::Duration::seconds(1);
        approved.reason = Some("approved".into());

        let mut incremental = WorkflowGraph::new(TurnId::now());
        add_tool(&mut incremental, &run_id, "batched-tool");
        incremental.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Canonical {
                request_id: request_id.clone(),
            },
            &pending,
            WorkflowPermissionState::Pending,
        );
        incremental.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Canonical {
                request_id: request_id.clone(),
            },
            &approved,
            WorkflowPermissionState::Approved,
        );

        let mut batched = WorkflowGraph::new(TurnId::now());
        add_tool(&mut batched, &run_id, "batched-tool");
        batched.apply_permission_requests([
            (
                WorkflowPermissionIdentity::Canonical {
                    request_id: request_id.clone(),
                },
                pending,
                WorkflowPermissionState::Pending,
            ),
            (
                WorkflowPermissionIdentity::Canonical { request_id },
                approved,
                WorkflowPermissionState::Approved,
            ),
        ]);

        assert_eq!(batched.permission_requests, incremental.permission_requests);
        assert_eq!(
            batched
                .find_node(&tool_node_id(&run_id.0.to_string(), "batched-tool"))
                .unwrap()
                .approval,
            incremental
                .find_node(&tool_node_id(&run_id.0.to_string(), "batched-tool"))
                .unwrap()
                .approval
        );
    }

    #[test]
    fn canonical_and_legacy_permission_lifecycles_count_and_interrupt_independently() {
        let mut graph = WorkflowGraph::new(TurnId::now());
        let root = FlowRunId::now();
        let child = FlowRunId::now();
        graph.apply_event(&flow_start(root.clone(), "root"));
        graph.apply_event(&stmt_start(root.clone(), "spawn", None));
        graph.apply_event(&subflow_start(
            child.clone(),
            root.clone(),
            "spawn",
            "child",
        ));
        let now = Utc::now();
        let canonical_id = request_id();
        let canonical = permission_payload(
            canonical_id.clone(),
            child.clone(),
            root.clone(),
            "canonical",
            now,
        );
        let legacy = permission_payload(
            request_id(),
            child.clone(),
            root.clone(),
            "legacy",
            now + chrono::Duration::seconds(1),
        );
        graph.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Canonical {
                request_id: canonical_id.clone(),
            },
            &canonical,
            WorkflowPermissionState::Pending,
        );
        graph.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Legacy {
                seq: 7,
                run_id: child.0.to_string(),
                tool_use_id: "legacy".into(),
            },
            &legacy,
            WorkflowPermissionState::Pending,
        );
        assert_eq!(graph.descendant_pending_permissions(&root.0.to_string()), 2);

        let mut approved = canonical.clone();
        approved.actor = Some(crate::permission_audit::PermissionProjectionActor::Flow {
            session_id: "session".into(),
            run_id: root.clone(),
        });
        approved.reason = Some("approved by parent".into());
        graph.apply_event(&Event::PermissionRequestApproved { payload: approved });
        assert_eq!(graph.descendant_pending_permissions(&root.0.to_string()), 1);
        assert_eq!(
            graph
                .permission_requests
                .get(&WorkflowPermissionIdentity::Canonical {
                    request_id: canonical_id,
                })
                .unwrap()
                .state,
            WorkflowPermissionState::Approved
        );

        graph.interrupt_pending_permissions();
        assert_eq!(graph.descendant_pending_permissions(&root.0.to_string()), 0);
        assert!(graph.permission_requests.values().any(|request| {
            request.state == WorkflowPermissionState::Interrupted
                && request.payload.tool_use_id == "legacy"
        }));
    }

    #[test]
    fn permission_event_and_stream_reducers_converge_on_canonical_state() {
        let run_id = FlowRunId::now();
        let request_id = request_id();
        let created = permission_payload(
            request_id,
            run_id.clone(),
            run_id.clone(),
            "tool",
            Utc::now(),
        );
        let mut approved = created.clone();
        approved.actor = Some(crate::permission_audit::PermissionProjectionActor::User {
            session_id: "session".into(),
            principal_id: Some("operator".into()),
        });
        approved.reason = Some("accepted".into());
        approved.at += chrono::Duration::seconds(1);

        let mut from_events = WorkflowGraph::new(TurnId::now());
        from_events.apply_event(&Event::PermissionRequestCreated {
            payload: created.clone(),
        });
        from_events.apply_event(&Event::PermissionRequestApproved {
            payload: approved.clone(),
        });
        let mut from_stream = WorkflowGraph::new(TurnId::now());
        from_stream.apply_stream_frame(&crate::stream::StreamFrame::PermissionRequestCreated {
            run_id: run_id.0.to_string(),
            payload: created,
        });
        from_stream.apply_stream_frame(&crate::stream::StreamFrame::PermissionRequestApproved {
            run_id: run_id.0.to_string(),
            payload: approved,
        });

        assert_eq!(
            from_events.permission_requests,
            from_stream.permission_requests
        );
        let request = from_events.permission_requests.values().next().unwrap();
        assert_eq!(request.state, WorkflowPermissionState::Approved);
        assert_eq!(request.payload.reason.as_deref(), Some("accepted"));
        assert!(matches!(
            request.payload.actor,
            Some(crate::permission_audit::PermissionProjectionActor::User { .. })
        ));
    }

    #[test]
    fn s8_canonical_jsonl_replay_converges_with_event_and_stream_reducers() {
        use crate::permission::PermissionGroupId;
        use crate::permission_audit::{PermissionGroupAudit, PermissionProjectionActor};
        use crate::projection::message_window::{TranscriptEntry, replay_transcript_from};
        use crate::stream::StreamFrame;

        let root = FlowRunId::now();
        let child = FlowRunId::now();
        let now = Utc::now();
        let approved_id = request_id();
        let unrestricted_id = request_id();
        let interrupted_id = request_id();
        let mut approved = permission_payload(
            approved_id.clone(),
            child.clone(),
            root.clone(),
            "approved",
            now,
        );
        approved.parent_run_id = Some(root.clone());
        let mut approved_final = approved.clone();
        approved_final.actor = Some(PermissionProjectionActor::User {
            session_id: "session".into(),
            principal_id: Some("operator".into()),
        });
        approved_final.reason = Some("accepted".into());
        approved_final.at += chrono::Duration::seconds(2);
        let mut unrestricted = permission_payload(
            unrestricted_id,
            child.clone(),
            root.clone(),
            "unrestricted",
            now,
        );
        unrestricted.parent_run_id = Some(root.clone());
        unrestricted.actor = Some(PermissionProjectionActor::Policy {
            policy_version: "snapshot".into(),
            rule_id: "unrestricted".into(),
        });
        let mut interrupted = permission_payload(
            interrupted_id,
            child.clone(),
            root.clone(),
            "interrupted",
            now,
        );
        interrupted.parent_run_id = Some(root.clone());
        let group = PermissionGroupAudit {
            group_id: PermissionGroupId(uuid::Uuid::now_v7()),
            owner: crate::permission_audit::PermissionGroupAuditOwner::Flow {
                run_id: root.clone(),
            },
            label: "acceptance".into(),
            request_ids: vec![approved_id.clone()],
            revision: 1,
            at: now,
        };

        let mut created_json = serde_json::to_value(&approved).unwrap();
        created_json.as_object_mut().unwrap().remove("provenance");
        let lines = [
            serde_json::json!({"type":"permission_request_created","seq":1,"payload":created_json}).to_string(),
            "{\"type\":\"permission_request_targeted\",\"payload\":".into(),
            serde_json::json!({"type":"permission_request_targeted","seq":2,"payload":approved}).to_string(),
            serde_json::json!({"type":"tool_pending_approval","seq":3,"run_id":child.0.to_string(),"tool_use_id":"approved","tool_name":"fs.read","args_preview":"{}","level":"approve"}).to_string(),
            serde_json::json!({"type":"permission_group_created","seq":4,"payload":group}).to_string(),
            serde_json::json!({"type":"permission_request_approved","seq":5,"payload":approved_final}).to_string(),
            serde_json::json!({"type":"tool_approved","seq":6,"run_id":child.0.to_string(),"tool_use_id":"approved","decided_by":"broker"}).to_string(),
            serde_json::json!({"type":"permission_group_resolved","seq":7,"payload":group}).to_string(),
            serde_json::json!({"type":"unrestricted_execution","seq":8,"payload":unrestricted}).to_string(),
            serde_json::json!({"type":"tool_approved","seq":9,"run_id":child.0.to_string(),"tool_use_id":"unrestricted","decided_by":"unrestricted"}).to_string(),
            serde_json::json!({"type":"permission_request_created","seq":10,"payload":interrupted}).to_string(),
            serde_json::json!({"type":"tool_pending_approval","seq":11,"run_id":child.0.to_string(),"tool_use_id":"interrupted","tool_name":"fs.read","args_preview":"{}","level":"approve"}).to_string(),
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let transcript = replay_transcript_from(&path).unwrap();

        let mut from_history = WorkflowGraph::new(TurnId::now());
        let mut from_events = WorkflowGraph::new(TurnId::now());
        let mut from_stream = WorkflowGraph::new(TurnId::now());
        for graph in [&mut from_history, &mut from_events, &mut from_stream] {
            graph.apply_event(&flow_start(root.clone(), "root"));
            graph.apply_event(&stmt_start(root.clone(), "spawn", None));
            graph.apply_event(&subflow_start(
                child.clone(),
                root.clone(),
                "spawn",
                "child",
            ));
            graph.apply_event(&stmt_start(child.clone(), "dispatch_all", None));
            for tool in ["approved", "unrestricted", "interrupted"] {
                graph.apply_event(&Event::ToolNode {
                    run_id: child.clone(),
                    parent_node_id: "dispatch_all".into(),
                    tool_use_id: tool.into(),
                    tool_name: "fs.read".into(),
                    args_preview: String::new(),
                    call_intent: None,
                });
            }
        }
        for entry in &transcript {
            match entry {
                TranscriptEntry::PermissionRequest {
                    identity,
                    payload,
                    state,
                } => {
                    from_history.apply_permission_request_with_identity(
                        identity.clone(),
                        payload,
                        *state,
                    );
                }
                TranscriptEntry::PermissionGroup { payload, resolved } => {
                    from_history.apply_permission_group(payload, *resolved);
                }
                _ => {}
            }
        }
        let events = [
            Event::PermissionRequestCreated {
                payload: approved.clone(),
            },
            Event::PermissionRequestTargeted {
                payload: approved.clone(),
            },
            Event::PermissionGroupCreated {
                payload: group.clone(),
            },
            Event::PermissionRequestApproved {
                payload: approved_final.clone(),
            },
            Event::PermissionGroupResolved {
                payload: group.clone(),
            },
            Event::UnrestrictedExecution {
                payload: unrestricted.clone(),
            },
            Event::PermissionRequestCreated {
                payload: interrupted.clone(),
            },
        ];
        for event in &events {
            from_events.apply_event(event);
        }
        from_events.interrupt_pending_permissions();
        let run_id = child.0.to_string();
        let frames = [
            StreamFrame::PermissionRequestCreated {
                run_id: run_id.clone(),
                payload: approved.clone(),
            },
            StreamFrame::PermissionRequestTargeted {
                run_id: run_id.clone(),
                payload: approved,
            },
            StreamFrame::PermissionGroupCreated {
                run_id: root.0.to_string(),
                payload: group.clone(),
            },
            StreamFrame::PermissionRequestApproved {
                run_id: run_id.clone(),
                payload: approved_final,
            },
            StreamFrame::PermissionGroupResolved {
                run_id: root.0.to_string(),
                payload: group,
            },
            StreamFrame::UnrestrictedExecution {
                run_id: run_id.clone(),
                payload: unrestricted,
            },
            StreamFrame::PermissionRequestCreated {
                run_id,
                payload: interrupted,
            },
        ];
        for frame in &frames {
            from_stream.apply_stream_frame(frame);
        }
        from_stream.interrupt_pending_permissions();

        assert_eq!(
            from_history.permission_requests,
            from_events.permission_requests
        );
        assert_eq!(
            from_history.permission_requests,
            from_stream.permission_requests
        );
        assert_eq!(
            from_history.permission_groups,
            from_events.permission_groups
        );
        assert_eq!(
            from_history.permission_groups,
            from_stream.permission_groups
        );
        fn without_timestamps(mut nodes: Vec<WorkflowNode>) -> Vec<WorkflowNode> {
            fn clear(nodes: &mut [WorkflowNode]) {
                for node in nodes {
                    node.started_at = None;
                    node.ended_at = None;
                    clear(&mut node.children);
                }
            }
            clear(&mut nodes);
            nodes
        }
        assert_eq!(
            without_timestamps(from_history.root.clone()),
            without_timestamps(from_events.root.clone())
        );
        assert_eq!(
            without_timestamps(from_history.root.clone()),
            without_timestamps(from_stream.root.clone())
        );
        assert_eq!(
            from_history.descendant_pending_permissions(&root.0.to_string()),
            0
        );
        assert_eq!(from_history.permission_requests.len(), 3);
        assert!(
            from_history
                .permission_requests
                .keys()
                .all(|identity| matches!(identity, WorkflowPermissionIdentity::Canonical { .. }))
        );
        assert!(from_history.permission_requests.contains_key(
            &WorkflowPermissionIdentity::Canonical {
                request_id: approved_id
            }
        ));
        assert!(from_history.permission_requests.values().any(|request| {
            request.state == WorkflowPermissionState::Interrupted
                && request.payload.reason.as_deref()
                    == Some("interrupted at end of persisted history")
        }));
    }

    #[test]
    fn s8_resolved_group_tombstones_prevent_live_stream_and_replay_resurrection() {
        use crate::permission::PermissionGroupId;
        use crate::permission_audit::PermissionGroupAudit;
        use crate::projection::message_window::{TranscriptEntry, replay_transcript_from};
        use crate::stream::StreamFrame;

        let owner = FlowRunId::now();
        let groups = [
            PermissionGroupAudit {
                group_id: PermissionGroupId(uuid::Uuid::now_v7()),
                owner: crate::permission_audit::PermissionGroupAuditOwner::Flow {
                    run_id: owner.clone(),
                },
                label: "resolved before ungroup".into(),
                request_ids: vec![request_id()],
                revision: 1,
                at: Utc::now(),
            },
            PermissionGroupAudit {
                group_id: PermissionGroupId(uuid::Uuid::now_v7()),
                owner: crate::permission_audit::PermissionGroupAuditOwner::Flow {
                    run_id: owner.clone(),
                },
                label: "terminal-only".into(),
                request_ids: Vec::new(),
                revision: 1,
                at: Utc::now(),
            },
        ];
        let mut from_events = WorkflowGraph::new(TurnId::now());
        let mut from_stream = WorkflowGraph::new(TurnId::now());
        let mut lines = Vec::new();
        let mut seq = 1_u64;
        for group in &groups {
            let mut updated = group.clone();
            updated.request_ids.clear();
            updated.revision += 1;

            for event in [
                Event::PermissionGroupCreated {
                    payload: group.clone(),
                },
                Event::PermissionGroupResolved {
                    payload: group.clone(),
                },
                Event::PermissionGroupUpdated {
                    payload: updated.clone(),
                },
            ] {
                from_events.apply_event(&event);
            }
            for frame in [
                StreamFrame::PermissionGroupCreated {
                    run_id: owner.0.to_string(),
                    payload: group.clone(),
                },
                StreamFrame::PermissionGroupResolved {
                    run_id: owner.0.to_string(),
                    payload: group.clone(),
                },
                StreamFrame::PermissionGroupUpdated {
                    run_id: owner.0.to_string(),
                    payload: updated.clone(),
                },
            ] {
                from_stream.apply_stream_frame(&frame);
            }
            for (kind, payload) in [
                ("permission_group_created", group),
                ("permission_group_resolved", group),
                ("permission_group_updated", &updated),
            ] {
                lines
                    .push(serde_json::json!({"type":kind,"seq":seq,"payload":payload}).to_string());
                seq += 1;
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let mut from_history = WorkflowGraph::new(TurnId::now());
        for entry in replay_transcript_from(&path).unwrap() {
            if let TranscriptEntry::PermissionGroup { payload, resolved } = entry {
                from_history.apply_permission_group(&payload, resolved);
            }
        }

        for graph in [&from_events, &from_stream, &from_history] {
            assert!(graph.permission_groups.is_empty());
            assert_eq!(graph.resolved_permission_groups.len(), groups.len());
            assert!(
                groups
                    .iter()
                    .all(|group| graph.resolved_permission_groups.contains(&group.group_id))
            );
        }
        let restored: WorkflowGraph =
            serde_json::from_value(serde_json::to_value(&from_events).unwrap()).unwrap();
        assert_eq!(
            restored.resolved_permission_groups,
            from_events.resolved_permission_groups
        );
    }

    #[test]
    fn fanout_branch_marks_parent_parallel() {
        let mut g = WorkflowGraph::new(TurnId::now());
        let rid = FlowRunId::now();
        g.apply_event(&flow_start(rid.clone(), "main"));
        g.apply_event(&stmt_start(rid.clone(), "stmt_1", None));
        g.apply_event(&stmt_start(rid.clone(), "stmt_1.branch[0]", Some("stmt_1")));
        g.apply_event(&stmt_start(rid.clone(), "stmt_1.branch[1]", Some("stmt_1")));
        let scoped = scope_id(&rid.0.to_string(), "stmt_1");
        let parent = g.find_node(&scoped).unwrap();
        assert_eq!(parent.parallelism, Parallelism::Parallel);
        assert_eq!(parent.children.len(), 2);
        assert!(matches!(
            parent.children[0].kind,
            WorkflowNodeKind::FanoutBranch { branch_index: 0 }
        ));
        assert!(matches!(
            parent.children[1].kind,
            WorkflowNodeKind::FanoutBranch { branch_index: 1 }
        ));
    }

    #[test]
    fn out_of_order_events_silently_dropped() {
        let mut g = WorkflowGraph::new(TurnId::now());
        g.apply_event(&stmt_start(FlowRunId::now(), "stmt_0", Some("missing")));
        g.apply_event(&Event::ToolNode {
            run_id: FlowRunId::now(),
            parent_node_id: "missing".into(),
            tool_use_id: "tu".into(),
            tool_name: "t".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        assert!(g.root.is_empty());
    }
}

#[test]
fn rebuild_workflow_tree_restores_root_and_subflow() {
    use crate::event::{Event, FlowRunId};
    let root = FlowRunId::now();
    let child = FlowRunId::now();
    let events = vec![
        Event::FlowStart {
            run_id: root.clone(),
            flow_name: "agent".into(),
            parent_run_id: None,
            parent_node_id: None,
            spawned: false,
        },
        Event::FlowNodeStart {
            run_id: root.clone(),
            node_id: "stmt_0".into(),
            kind: crate::nodegraph::NodeKind::Llm { model: None },
            label: "llm".into(),
            parent_node_id: None,
        },
        Event::FlowStart {
            run_id: child.clone(),
            flow_name: "subagent".into(),
            parent_run_id: Some(root.clone()),
            parent_node_id: Some("stmt_0".into()),
            spawned: false,
        },
        Event::FlowEnd {
            run_id: child.clone(),
            flow_name: "subagent".into(),
            status: crate::event::FlowStatus::Ok,
        },
        Event::FlowEnd {
            run_id: root.clone(),
            flow_name: "agent".into(),
            status: crate::event::FlowStatus::Ok,
        },
    ];
    let g = rebuild_workflow_tree(&events);
    assert_eq!(g.root.len(), 1, "one root flow");
    assert_eq!(g.root[0].id, root.0.to_string());
    // subflow attaches under stmt_0 (or root if parent node not found)
    assert!(!g.root[0].children.is_empty(), "subflow nested under root");
}

#[test]
fn rebuild_messages_for_run_filters_by_run_id() {
    use crate::event::{Event, FlowRunId, TurnId};
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
    let root = FlowRunId::now();
    let child = FlowRunId::now();
    let tid = TurnId::now();
    let mk = |role, text, rid: Option<FlowRunId>| Event::AssistantMsg {
        turn_id: tid.clone(),
        flow_run_id: rid,
        message: Message {
            role,
            parts: vec![MessagePart::Text { text }],
            turn_id: tid.clone(),
            origin: MessageOrigin::User,
        },
    };
    let events = vec![
        mk(MessageRole::Assistant, "root reply".into(), None),
        mk(
            MessageRole::Assistant,
            "child reply".into(),
            Some(child.clone()),
        ),
        mk(MessageRole::Assistant, "root again".into(), None),
    ];
    // Root agent messages are tagged None (restored via envelope, not per-run rebuild);
    // only sub-agent (child) messages carry Some(run_id) and are picked up here.
    let root_msgs = rebuild_messages_for_run(&events, &root);
    assert_eq!(root_msgs.len(), 0);
    let child_msgs = rebuild_messages_for_run(&events, &child);
    assert_eq!(child_msgs.len(), 1);
    assert_eq!(child_msgs[0].text_concat(), "child reply");
}
