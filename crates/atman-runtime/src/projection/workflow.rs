use std::collections::HashMap;
use std::ops::Deref;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::event::{Event, FlowNodeStatus, FlowStatus, TurnId};
use crate::permission_audit::{PermissionGroupAudit, PermissionRequestAudit};
use crate::stream::StreamFrame;
use crate::workflow::{
    ApprovalState, LlmStats, NodeStatus, Parallelism, WorkflowGraph, WorkflowNode,
    WorkflowNodeKind, WorkflowPermissionIdentity, WorkflowPermissionRequest,
    WorkflowPermissionState,
};

type NodePath = Vec<usize>;
type ToolKey = (String, String);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PerfCounters {
    indexed_lookups: u64,
    path_steps: u64,
}

#[cfg(test)]
thread_local! {
    static PERF_COUNTERS: std::cell::Cell<PerfCounters> = const {
        std::cell::Cell::new(PerfCounters {
            indexed_lookups: 0,
            path_steps: 0,
        })
    };
}

#[cfg(test)]
fn reset_perf_counters() {
    PERF_COUNTERS.with(|counters| counters.set(PerfCounters::default()));
}

#[cfg(test)]
fn perf_counters() -> PerfCounters {
    PERF_COUNTERS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn count_indexed_lookup(path: &[usize]) {
    PERF_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.indexed_lookups = value.indexed_lookups.saturating_add(1);
        value.path_steps = value.path_steps.saturating_add(path.len() as u64);
        counters.set(value);
    });
}

#[derive(Clone, Debug, Default)]
struct WorkflowIndex {
    node_paths: HashMap<String, NodePath>,
    tool_paths: HashMap<ToolKey, NodePath>,
    first_tool_paths: HashMap<String, NodePath>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectionDelta {
    pub revision: u64,
    pub dirty_nodes: Vec<String>,
    pub structural_changed: bool,
    pub layout_changed: bool,
}

impl ProjectionDelta {
    pub fn changed(&self) -> bool {
        self.structural_changed || self.layout_changed || !self.dirty_nodes.is_empty()
    }
}

#[derive(Default)]
struct PendingDelta {
    dirty_nodes: Vec<String>,
    structural_changed: bool,
    layout_changed: bool,
}

impl PendingDelta {
    fn mark(&mut self, node_id: impl Into<String>) {
        let node_id = node_id.into();
        if !self.dirty_nodes.contains(&node_id) {
            self.dirty_nodes.push(node_id);
        }
        self.layout_changed = true;
    }

    fn mark_structure(&mut self, parent_id: Option<&str>, node_id: &str) {
        if let Some(parent_id) = parent_id {
            self.mark(parent_id);
        }
        self.mark(node_id);
        self.structural_changed = true;
    }

    fn merge(&mut self, other: Self) {
        for node_id in other.dirty_nodes {
            self.mark(node_id);
        }
        self.structural_changed |= other.structural_changed;
        self.layout_changed |= other.layout_changed;
    }

    fn changed(&self) -> bool {
        self.structural_changed || self.layout_changed || !self.dirty_nodes.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowProjection {
    graph: WorkflowGraph,
    index: WorkflowIndex,
    revision: u64,
}

impl PartialEq for WorkflowProjection {
    fn eq(&self, other: &Self) -> bool {
        self.graph == other.graph
    }
}

impl Serialize for WorkflowProjection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.graph.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkflowProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        WorkflowGraph::deserialize(deserializer).map(Self::from)
    }
}

impl Deref for WorkflowProjection {
    type Target = WorkflowGraph;

    fn deref(&self) -> &Self::Target {
        &self.graph
    }
}

impl From<WorkflowGraph> for WorkflowProjection {
    fn from(graph: WorkflowGraph) -> Self {
        let mut projection = Self {
            graph,
            index: WorkflowIndex::default(),
            revision: 0,
        };
        projection.rebuild_index();
        projection
    }
}

impl From<WorkflowProjection> for WorkflowGraph {
    fn from(projection: WorkflowProjection) -> Self {
        projection.graph
    }
}

impl WorkflowProjection {
    pub fn new(turn_id: TurnId) -> Self {
        WorkflowGraph::new(turn_id).into()
    }

    pub fn graph(&self) -> &WorkflowGraph {
        &self.graph
    }

    pub fn into_graph(self) -> WorkflowGraph {
        self.graph
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn find_node(&self, id: &str) -> Option<&WorkflowNode> {
        let path = self.index.node_paths.get(id)?;
        node_at_path(&self.graph.root, path)
    }

    pub fn descendant_pending_permissions(&self, flow_node_id: &str) -> usize {
        let Some(node) = self.find_node(flow_node_id) else {
            return 0;
        };
        let mut run_ids = Vec::new();
        collect_flow_run_ids(node, &mut run_ids);
        self.graph
            .permission_requests
            .values()
            .filter(|request| {
                request.state.is_pending()
                    && run_ids.contains(&request.payload.requesting_run_id.0.to_string())
            })
            .count()
    }

    pub fn apply_batch<'a>(
        &mut self,
        events: impl IntoIterator<Item = &'a Event>,
    ) -> ProjectionDelta {
        let mut pending = PendingDelta::default();
        for event in events {
            pending.merge(self.apply_event_inner(event));
        }
        self.commit(pending)
    }

    pub fn apply_event(&mut self, event: &Event) -> ProjectionDelta {
        let pending = self.apply_event_inner(event);
        self.commit(pending)
    }

    pub fn apply_stream_frame(&mut self, frame: &StreamFrame) -> ProjectionDelta {
        self.apply_stream_frame_at(frame, None)
    }

    pub fn apply_stream_frame_at(
        &mut self,
        frame: &StreamFrame,
        override_ts: Option<DateTime<Utc>>,
    ) -> ProjectionDelta {
        let pending = self.apply_stream_frame_inner(frame, override_ts);
        self.commit(pending)
    }

    pub fn interrupt_pending_permissions(&mut self) -> ProjectionDelta {
        if !self
            .graph
            .permission_requests
            .values()
            .any(|request| request.state.is_pending())
        {
            return ProjectionDelta {
                revision: self.revision,
                ..ProjectionDelta::default()
            };
        }
        let dirty_nodes = self
            .graph
            .permission_requests
            .values()
            .filter(|request| request.state.is_pending())
            .map(|request| {
                tool_node_id(
                    &request.payload.requesting_run_id.0.to_string(),
                    &request.payload.tool_use_id,
                )
            })
            .collect::<Vec<_>>();
        self.graph.interrupt_pending_permissions();
        let mut pending = PendingDelta::default();
        for node_id in dirty_nodes {
            pending.mark(node_id);
        }
        self.commit(pending)
    }

    pub fn apply_permission_request(
        &mut self,
        payload: &PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) -> ProjectionDelta {
        let Some(request_id) = payload.request_id.clone() else {
            return ProjectionDelta {
                revision: self.revision,
                ..ProjectionDelta::default()
            };
        };
        self.apply_permission_request_with_identity(
            WorkflowPermissionIdentity::Canonical { request_id },
            payload,
            state,
        )
    }

    pub fn apply_permission_request_with_identity(
        &mut self,
        identity: WorkflowPermissionIdentity,
        payload: &PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) -> ProjectionDelta {
        self.apply_permission_requests([(identity, payload.clone(), state)])
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
    ) -> ProjectionDelta {
        let requests = requests.into_iter().collect::<Vec<_>>();
        let changed = requests.iter().any(|(identity, payload, state)| {
            self.graph.permission_requests.get(identity)
                != Some(&WorkflowPermissionRequest {
                    payload: payload.clone(),
                    state: *state,
                })
        });
        if !changed {
            return ProjectionDelta {
                revision: self.revision,
                ..ProjectionDelta::default()
            };
        }
        let mut pending = PendingDelta::default();
        for (_, payload, _) in &requests {
            pending.mark(tool_node_id(
                &payload.requesting_run_id.0.to_string(),
                &payload.tool_use_id,
            ));
        }
        self.graph.apply_permission_requests(requests);
        self.commit(pending)
    }

    pub fn apply_permission_group(
        &mut self,
        payload: &PermissionGroupAudit,
        resolved: bool,
    ) -> ProjectionDelta {
        let before_group = self.graph.permission_groups.get(&payload.group_id).cloned();
        let before_resolved = self
            .graph
            .resolved_permission_groups
            .contains(&payload.group_id);
        self.graph.apply_permission_group(payload, resolved);
        if before_group == self.graph.permission_groups.get(&payload.group_id).cloned()
            && before_resolved
                == self
                    .graph
                    .resolved_permission_groups
                    .contains(&payload.group_id)
        {
            return ProjectionDelta {
                revision: self.revision,
                ..ProjectionDelta::default()
            };
        }
        let pending = PendingDelta {
            layout_changed: true,
            ..PendingDelta::default()
        };
        self.commit(pending)
    }

    fn apply_event_inner(&mut self, event: &Event) -> PendingDelta {
        match event {
            Event::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ..
            } => self.insert_flow(
                run_id.0.to_string(),
                flow_name.clone(),
                parent_run_id.as_ref().map(|id| id.0.to_string()),
                parent_node_id.clone(),
                Utc::now(),
                false,
            ),
            Event::FlowEnd { run_id, status, .. } => {
                let status = match status {
                    FlowStatus::Ok => NodeStatus::Ok,
                    FlowStatus::Errored { .. } => NodeStatus::Err,
                    FlowStatus::Cancelled => NodeStatus::Cancelled,
                };
                self.finish_node(&run_id.0.to_string(), status, None, Utc::now(), false)
            }
            Event::FlowNodeStart {
                run_id,
                node_id,
                kind,
                label,
                parent_node_id,
                ..
            } => self.insert_flow_node(
                &run_id.0.to_string(),
                node_id,
                kind,
                label,
                parent_node_id.as_deref(),
                Utc::now(),
            ),
            Event::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ..
            } => {
                let status = match status {
                    FlowNodeStatus::Ok => NodeStatus::Ok,
                    FlowNodeStatus::Err => NodeStatus::Err,
                    FlowNodeStatus::Cancelled => NodeStatus::Cancelled,
                };
                self.finish_node(
                    &scope_id(&run_id.0.to_string(), node_id),
                    status,
                    output_preview.as_deref(),
                    Utc::now(),
                    false,
                )
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
                let Some((run_id, node_id)) = run_id.as_ref().zip(node_id.as_deref()) else {
                    return PendingDelta::default();
                };
                self.set_llm_stats(
                    &scope_id(&run_id.0.to_string(), node_id),
                    LlmStats {
                        model: model.clone(),
                        provider: provider.clone(),
                        context_call_purpose: context_call_purpose.unwrap_or_default(),
                        context_call_scope: context_call_identity
                            .as_ref()
                            .map(|identity| identity.scope)
                            .unwrap_or(crate::context_plan::ContextCallScope::Root),
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        cache_read: usage.cached_input,
                        cache_write: usage.cache_write,
                        ttft_ms: ttft_ms.unwrap_or(0),
                        tokens_per_second: tokens_per_second.unwrap_or(0.0),
                        wallclock_ms: *wallclock_ms,
                    },
                )
            }
            Event::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                call_intent,
                ..
            } => self.insert_tool(
                &run_id.0.to_string(),
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                call_intent.clone(),
                Utc::now(),
            ),
            Event::ToolResultMsg {
                flow_run_id,
                message,
                ..
            } => self.apply_tool_results(
                flow_run_id.as_ref().map(|id| id.0.to_string()).as_deref(),
                message,
                Utc::now(),
            ),
            Event::ToolPendingApproval {
                run_id,
                tool_use_id,
                level,
                preview,
                ..
            } => self.set_tool_approval(
                &run_id.0.to_string(),
                tool_use_id,
                ApprovalState::Pending {
                    level: level.clone(),
                    preview: preview.clone(),
                },
            ),
            Event::ToolApproved {
                run_id,
                tool_use_id,
                ..
            } => {
                self.set_tool_approval(&run_id.0.to_string(), tool_use_id, ApprovalState::Approved)
            }
            Event::ToolDenied {
                run_id,
                tool_use_id,
                reason,
                ..
            } => self.set_tool_approval(
                &run_id.0.to_string(),
                tool_use_id,
                ApprovalState::Denied {
                    reason: reason.clone(),
                },
            ),
            Event::PermissionRequestCreated { payload }
            | Event::PermissionRequestTargeted { payload }
            | Event::PermissionRequestDeferred { payload } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Pending)
            }
            Event::PermissionRequestApproved { payload } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Approved)
            }
            Event::PermissionRequestDenied { payload } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Denied)
            }
            Event::PermissionRequestCancelled { payload } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Cancelled)
            }
            Event::UnrestrictedExecution { payload } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Unrestricted)
            }
            Event::PermissionGroupCreated { payload }
            | Event::PermissionGroupUpdated { payload } => {
                self.apply_permission_group_inner(payload, false)
            }
            Event::PermissionGroupResolved { payload } => {
                self.apply_permission_group_inner(payload, true)
            }
            _ => PendingDelta::default(),
        }
    }

    fn apply_stream_frame_inner(
        &mut self,
        frame: &StreamFrame,
        override_ts: Option<DateTime<Utc>>,
    ) -> PendingDelta {
        let now = override_ts.unwrap_or_else(Utc::now);
        match frame {
            StreamFrame::FlowGraph { run_id, graph } => self.insert_flow(
                run_id.clone(),
                graph.flow_name.clone(),
                None,
                None,
                now,
                true,
            ),
            StreamFrame::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
            } => self.insert_flow(
                run_id.clone(),
                flow_name.clone(),
                parent_run_id.clone(),
                parent_node_id.clone(),
                now,
                true,
            ),
            StreamFrame::FlowNodeStart {
                run_id,
                node_id,
                kind,
                label,
                parent_node_id,
            } => {
                self.insert_flow_node(run_id, node_id, kind, label, parent_node_id.as_deref(), now)
            }
            StreamFrame::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ..
            } => {
                let status = match status {
                    FlowNodeStatus::Ok => NodeStatus::Ok,
                    FlowNodeStatus::Err => NodeStatus::Err,
                    FlowNodeStatus::Cancelled => NodeStatus::Cancelled,
                };
                self.finish_node(
                    &scope_id(run_id, node_id),
                    status,
                    output_preview.as_deref(),
                    now,
                    false,
                )
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
                let Some((run_id, node_id)) = run_id.as_deref().zip(node_id.as_deref()) else {
                    return PendingDelta::default();
                };
                self.set_llm_stats(
                    &scope_id(run_id, node_id),
                    LlmStats {
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
                    },
                )
            }
            StreamFrame::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool,
                args_preview,
                call_intent,
                ..
            } => self.insert_tool(
                run_id,
                parent_node_id,
                tool_use_id,
                tool,
                args_preview,
                call_intent.clone(),
                now,
            ),
            StreamFrame::ToolUseDone {
                id, ok, preview, ..
            } => self.finish_tool(None, id, *ok, preview, now, false),
            StreamFrame::FlowDone {
                run_id,
                ok,
                cancelled,
                ..
            } => {
                let status = if *cancelled {
                    NodeStatus::Cancelled
                } else if *ok {
                    NodeStatus::Ok
                } else {
                    NodeStatus::Err
                };
                self.finish_node(run_id, status, None, now, true)
            }
            StreamFrame::ToolResultMsg {
                flow_run_id,
                message,
            } => self.apply_tool_results(flow_run_id.as_deref(), message, now),
            StreamFrame::ToolPendingApproval {
                run_id,
                tool_use_id,
                level,
                preview,
                ..
            } => self.set_tool_approval(
                run_id,
                tool_use_id,
                ApprovalState::Pending {
                    level: level.clone(),
                    preview: preview.clone(),
                },
            ),
            StreamFrame::ToolApproved {
                run_id,
                tool_use_id,
                ..
            } => self.set_tool_approval(run_id, tool_use_id, ApprovalState::Approved),
            StreamFrame::ToolDenied {
                run_id,
                tool_use_id,
                reason,
            } => self.set_tool_approval(
                run_id,
                tool_use_id,
                ApprovalState::Denied {
                    reason: reason.clone(),
                },
            ),
            StreamFrame::PermissionRequestCreated { payload, .. }
            | StreamFrame::PermissionRequestTargeted { payload, .. }
            | StreamFrame::PermissionRequestDeferred { payload, .. } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Pending)
            }
            StreamFrame::PermissionRequestApproved { payload, .. } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Approved)
            }
            StreamFrame::PermissionRequestDenied { payload, .. } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Denied)
            }
            StreamFrame::PermissionRequestCancelled { payload, .. } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Cancelled)
            }
            StreamFrame::UnrestrictedExecution { payload, .. } => {
                self.apply_permission_inner(payload, WorkflowPermissionState::Unrestricted)
            }
            StreamFrame::PermissionGroupCreated { payload, .. }
            | StreamFrame::PermissionGroupUpdated { payload, .. } => {
                self.apply_permission_group_inner(payload, false)
            }
            StreamFrame::PermissionGroupResolved { payload, .. } => {
                self.apply_permission_group_inner(payload, true)
            }
            _ => PendingDelta::default(),
        }
    }

    fn insert_flow(
        &mut self,
        run_id: String,
        flow_name: String,
        parent_run_id: Option<String>,
        parent_node_id: Option<String>,
        now: DateTime<Utc>,
        fallback_to_root: bool,
    ) -> PendingDelta {
        if self.index.node_paths.contains_key(&run_id) {
            return PendingDelta::default();
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
            label: flow_name,
            status: NodeStatus::Running,
            started_at: Some(now),
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        };
        let parent_id = parent_run_id
            .as_deref()
            .zip(parent_node_id.as_deref())
            .map(|(parent_run_id, parent_node_id)| scope_id(parent_run_id, parent_node_id));
        if let Some(parent_id) = parent_id.as_deref()
            && self.append_child(parent_id, node.clone(), None)
        {
            let mut delta = PendingDelta::default();
            delta.mark_structure(Some(parent_id), &run_id);
            return delta;
        }
        if parent_id.is_none() || fallback_to_root {
            self.append_root(node, None);
            let mut delta = PendingDelta::default();
            delta.mark_structure(None, &run_id);
            return delta;
        }
        PendingDelta::default()
    }

    fn insert_flow_node(
        &mut self,
        run_id: &str,
        node_id: &str,
        node_kind: &crate::nodegraph::NodeKind,
        label: &str,
        parent_node_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> PendingDelta {
        let id = scope_id(run_id, node_id);
        if self.index.node_paths.contains_key(&id) {
            return PendingDelta::default();
        }
        let parent_id = parent_node_id
            .map(|parent| scope_id(run_id, parent))
            .unwrap_or_else(|| run_id.to_string());
        let kind = parse_branch_index(node_id).map_or_else(
            || WorkflowNodeKind::Stmt {
                node_kind: node_kind.clone(),
            },
            |branch_index| WorkflowNodeKind::FanoutBranch { branch_index },
        );
        let parallel = matches!(kind, WorkflowNodeKind::FanoutBranch { .. });
        let node = WorkflowNode {
            id: id.clone(),
            kind,
            label: label.to_string(),
            status: NodeStatus::Running,
            started_at: Some(now),
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        };
        if !self.append_child(&parent_id, node, None) {
            return PendingDelta::default();
        }
        if parallel {
            self.mutate_node(&parent_id, |parent| {
                if parent.parallelism == Parallelism::Parallel {
                    false
                } else {
                    parent.parallelism = Parallelism::Parallel;
                    true
                }
            });
        }
        let mut delta = PendingDelta::default();
        delta.mark_structure(Some(&parent_id), &id);
        delta
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_tool(
        &mut self,
        run_id: &str,
        parent_node_id: &str,
        tool_use_id: &str,
        tool: &str,
        args_preview: &str,
        call_intent: Option<crate::message::ToolCallIntent>,
        now: DateTime<Utc>,
    ) -> PendingDelta {
        let id = tool_node_id(run_id, tool_use_id);
        if self.index.node_paths.contains_key(&id) {
            return PendingDelta::default();
        }
        let parent_id = scope_id(run_id, parent_node_id);
        let node = WorkflowNode {
            id: id.clone(),
            kind: WorkflowNodeKind::ToolCall {
                tool_use_id: tool_use_id.to_string(),
                tool: tool.to_string(),
                args_preview: args_preview.to_string(),
                call_intent,
                result_preview: None,
            },
            label: tool.to_string(),
            status: NodeStatus::Running,
            started_at: Some(now),
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        };
        if !self.append_child(&parent_id, node, Some((run_id, tool_use_id))) {
            return PendingDelta::default();
        }
        if self.graph.permission_requests.values().any(|request| {
            request.payload.requesting_run_id.0.to_string() == run_id
                && request.payload.tool_use_id == tool_use_id
        }) {
            self.graph.refresh_permission_tool_approvals();
        }
        let mut delta = PendingDelta::default();
        delta.mark_structure(Some(&parent_id), &id);
        delta
    }

    fn finish_node(
        &mut self,
        id: &str,
        status: NodeStatus,
        output_preview: Option<&str>,
        now: DateTime<Utc>,
        recursive: bool,
    ) -> PendingDelta {
        let changed = self.mutate_node(id, |node| {
            let before = (node.status, node.ended_at, node.output_preview.clone());
            if recursive {
                cascade_terminate(node, status, now);
            } else {
                node.status = status;
                node.ended_at = Some(now);
                for child in &mut node.children {
                    if matches!(child.status, NodeStatus::Running | NodeStatus::Pending) {
                        child.status = status;
                        child.ended_at = Some(now);
                    }
                }
            }
            if let Some(output_preview) = output_preview {
                node.output_preview = Some(output_preview.to_string());
            }
            before != (node.status, node.ended_at, node.output_preview.clone())
        });
        let mut delta = PendingDelta::default();
        if changed {
            delta.mark(id);
        }
        delta
    }

    fn set_llm_stats(&mut self, id: &str, stats: LlmStats) -> PendingDelta {
        let changed = self.mutate_node(id, |node| {
            if node.llm_stats.as_ref() == Some(&stats) {
                false
            } else {
                node.llm_stats = Some(stats);
                true
            }
        });
        let mut delta = PendingDelta::default();
        if changed {
            delta.mark(id);
        }
        delta
    }

    fn apply_tool_results(
        &mut self,
        run_id: Option<&str>,
        message: &crate::message::Message,
        now: DateTime<Utc>,
    ) -> PendingDelta {
        let mut delta = PendingDelta::default();
        for part in &message.parts {
            let crate::message::MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = part
            else {
                continue;
            };
            delta.merge(self.finish_tool(run_id, tool_use_id, !*is_error, content, now, true));
        }
        delta
    }

    fn finish_tool(
        &mut self,
        run_id: Option<&str>,
        tool_use_id: &str,
        ok: bool,
        preview: &str,
        now: DateTime<Utc>,
        set_kind_preview: bool,
    ) -> PendingDelta {
        let path = match run_id {
            Some(run_id) => self
                .index
                .tool_paths
                .get(&(run_id.to_string(), tool_use_id.to_string())),
            None => self.index.first_tool_paths.get(tool_use_id),
        }
        .cloned();
        let Some(path) = path else {
            return PendingDelta::default();
        };
        let Some(node) = node_at_path_mut(&mut self.graph.root, &path) else {
            return PendingDelta::default();
        };
        let content = if set_kind_preview {
            preview.chars().take(300).collect::<String>()
        } else {
            preview.to_string()
        };
        let status = if ok { NodeStatus::Ok } else { NodeStatus::Err };
        let changed = node.status != status
            || node.ended_at != Some(now)
            || node.output_preview.as_deref() != Some(content.as_str());
        node.status = status;
        node.ended_at = Some(now);
        node.output_preview = Some(content.clone());
        if set_kind_preview
            && let WorkflowNodeKind::ToolCall { result_preview, .. } = &mut node.kind
        {
            *result_preview = Some(content);
        }
        let mut delta = PendingDelta::default();
        if changed {
            delta.mark(node.id.clone());
        }
        delta
    }

    fn set_tool_approval(
        &mut self,
        run_id: &str,
        tool_use_id: &str,
        approval: ApprovalState,
    ) -> PendingDelta {
        let id = tool_node_id(run_id, tool_use_id);
        let changed = self.mutate_node(&id, |node| {
            if node.approval.as_ref() == Some(&approval) {
                false
            } else {
                node.approval = Some(approval);
                true
            }
        });
        let mut delta = PendingDelta::default();
        if changed {
            delta.mark(id);
        }
        delta
    }

    fn apply_permission_inner(
        &mut self,
        payload: &PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) -> PendingDelta {
        let Some(request_id) = payload.request_id.clone() else {
            return PendingDelta::default();
        };
        let identity = WorkflowPermissionIdentity::Canonical { request_id };
        let request = WorkflowPermissionRequest {
            payload: payload.clone(),
            state,
        };
        if self.graph.permission_requests.get(&identity) == Some(&request) {
            return PendingDelta::default();
        }
        self.graph
            .apply_permission_requests([(identity, payload.clone(), state)]);
        let mut delta = PendingDelta::default();
        delta.mark(tool_node_id(
            &payload.requesting_run_id.0.to_string(),
            &payload.tool_use_id,
        ));
        delta
    }

    fn apply_permission_group_inner(
        &mut self,
        payload: &PermissionGroupAudit,
        resolved: bool,
    ) -> PendingDelta {
        let before_group = self.graph.permission_groups.get(&payload.group_id).cloned();
        let before_resolved = self
            .graph
            .resolved_permission_groups
            .contains(&payload.group_id);
        self.graph.apply_permission_group(payload, resolved);
        let mut delta = PendingDelta::default();
        if before_group != self.graph.permission_groups.get(&payload.group_id).cloned()
            || before_resolved
                != self
                    .graph
                    .resolved_permission_groups
                    .contains(&payload.group_id)
        {
            delta.layout_changed = true;
        }
        delta
    }

    fn append_root(&mut self, node: WorkflowNode, tool: Option<(&str, &str)>) {
        let path = vec![self.graph.root.len()];
        let id = node.id.clone();
        self.graph.root.push(node);
        self.index.node_paths.insert(id, path.clone());
        if let Some((run_id, tool_use_id)) = tool {
            self.index_tool(run_id, tool_use_id, path);
        }
    }

    fn append_child(
        &mut self,
        parent_id: &str,
        node: WorkflowNode,
        tool: Option<(&str, &str)>,
    ) -> bool {
        let Some(mut path) = self.index.node_paths.get(parent_id).cloned() else {
            return false;
        };
        let Some(parent) = node_at_path_mut(&mut self.graph.root, &path) else {
            return false;
        };
        path.push(parent.children.len());
        let id = node.id.clone();
        parent.children.push(node);
        self.index.node_paths.insert(id, path.clone());
        if let Some((run_id, tool_use_id)) = tool {
            self.index_tool(run_id, tool_use_id, path);
        }
        true
    }

    fn index_tool(&mut self, run_id: &str, tool_use_id: &str, path: NodePath) {
        self.index
            .tool_paths
            .entry((run_id.to_string(), tool_use_id.to_string()))
            .and_modify(|current| {
                if path < *current {
                    *current = path.clone();
                }
            })
            .or_insert_with(|| path.clone());
        self.index
            .first_tool_paths
            .entry(tool_use_id.to_string())
            .and_modify(|current| {
                if path < *current {
                    *current = path.clone();
                }
            })
            .or_insert(path);
    }

    fn mutate_node(&mut self, id: &str, mutation: impl FnOnce(&mut WorkflowNode) -> bool) -> bool {
        let Some(path) = self.index.node_paths.get(id) else {
            return false;
        };
        node_at_path_mut(&mut self.graph.root, path).is_some_and(mutation)
    }

    fn rebuild_index(&mut self) {
        self.index = WorkflowIndex::default();
        let mut path = Vec::new();
        index_nodes(&self.graph.root, &mut path, None, &mut self.index);
    }

    fn commit(&mut self, pending: PendingDelta) -> ProjectionDelta {
        if pending.changed() {
            self.revision = self.revision.wrapping_add(1);
        }
        ProjectionDelta {
            revision: self.revision,
            dirty_nodes: pending.dirty_nodes,
            structural_changed: pending.structural_changed,
            layout_changed: pending.layout_changed,
        }
    }
}

fn index_nodes(
    nodes: &[WorkflowNode],
    path: &mut NodePath,
    inherited_run_id: Option<&str>,
    index: &mut WorkflowIndex,
) {
    for (child_index, node) in nodes.iter().enumerate() {
        path.push(child_index);
        index
            .node_paths
            .entry(node.id.clone())
            .or_insert_with(|| path.clone());
        let run_id = match &node.kind {
            WorkflowNodeKind::Flow { run_id, .. } | WorkflowNodeKind::Subflow { run_id, .. } => {
                Some(run_id.as_str())
            }
            _ => inherited_run_id,
        };
        if let WorkflowNodeKind::ToolCall { tool_use_id, .. } = &node.kind
            && let Some(run_id) = run_id
        {
            index
                .tool_paths
                .entry((run_id.to_string(), tool_use_id.clone()))
                .and_modify(|current| {
                    if path < current {
                        *current = path.clone();
                    }
                })
                .or_insert_with(|| path.clone());
            index
                .first_tool_paths
                .entry(tool_use_id.clone())
                .and_modify(|current| {
                    if path < current {
                        *current = path.clone();
                    }
                })
                .or_insert_with(|| path.clone());
        }
        index_nodes(&node.children, path, run_id, index);
        path.pop();
    }
}

fn node_at_path<'a>(nodes: &'a [WorkflowNode], path: &[usize]) -> Option<&'a WorkflowNode> {
    #[cfg(test)]
    count_indexed_lookup(path);
    let (first, tail) = path.split_first()?;
    let mut node = nodes.get(*first)?;
    for child in tail {
        node = node.children.get(*child)?;
    }
    Some(node)
}

fn node_at_path_mut<'a>(
    nodes: &'a mut [WorkflowNode],
    path: &[usize],
) -> Option<&'a mut WorkflowNode> {
    #[cfg(test)]
    count_indexed_lookup(path);
    let (first, tail) = path.split_first()?;
    let mut node = nodes.get_mut(*first)?;
    for child in tail {
        node = node.children.get_mut(*child)?;
    }
    Some(node)
}

fn cascade_terminate(node: &mut WorkflowNode, status: NodeStatus, now: DateTime<Utc>) {
    if matches!(node.status, NodeStatus::Running | NodeStatus::Pending) {
        node.status = status;
        node.ended_at = Some(now);
    }
    for child in &mut node.children {
        cascade_terminate(child, status, now);
    }
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

fn scope_id(run_id: &str, node_id: &str) -> String {
    format!("{run_id}::{node_id}")
}

fn tool_node_id(run_id: &str, tool_use_id: &str) -> String {
    format!("tool:{run_id}:{tool_use_id}")
}

fn parse_branch_index(node_id: &str) -> Option<usize> {
    let start = node_id.rfind(".branch[")?;
    let rest = &node_id[start + ".branch[".len()..];
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_projection() -> WorkflowProjection {
        let mut projection = WorkflowProjection::new(TurnId::now());
        projection.apply_stream_frame(&StreamFrame::FlowStart {
            run_id: "root".into(),
            flow_name: "root".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        projection.apply_stream_frame(&StreamFrame::FlowNodeStart {
            run_id: "root".into(),
            node_id: "dispatch".into(),
            kind: crate::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch".into(),
            parent_node_id: None,
        });
        projection
    }

    fn without_timestamps(mut graph: WorkflowGraph) -> WorkflowGraph {
        fn clear(nodes: &mut [WorkflowNode]) {
            for node in nodes {
                node.started_at = None;
                node.ended_at = None;
                clear(&mut node.children);
            }
        }
        clear(&mut graph.root);
        graph
    }

    #[test]
    fn indexed_tool_updates_are_depth_bounded_after_large_append() {
        let mut projection = start_projection();
        for idx in 0..10_000 {
            let delta = projection.apply_stream_frame(&StreamFrame::ToolNode {
                run_id: "root".into(),
                parent_node_id: "dispatch".into(),
                tool_use_id: format!("tool-{idx}"),
                tool: "fs.read".into(),
                args_preview: "{}".into(),
                call_intent: None,
            });
            assert!(delta.structural_changed);
        }
        assert_eq!(projection.index.node_paths.len(), 10_002);
        assert_eq!(projection.index.node_paths["tool:root:tool-9999"].len(), 3);

        reset_perf_counters();
        let delta = projection.apply_stream_frame(&StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            id: "tool-9999".into(),
            ok: true,
            preview: "done".into(),
        });
        assert_eq!(
            perf_counters(),
            PerfCounters {
                indexed_lookups: 1,
                path_steps: 3,
            }
        );
        assert_eq!(delta.dirty_nodes, ["tool:root:tool-9999"]);
        assert_eq!(
            projection.find_node("tool:root:tool-9999").unwrap().status,
            NodeStatus::Ok
        );
    }

    #[test]
    fn duplicate_structural_frames_preserve_revision_and_paths() {
        let mut projection = start_projection();
        let frame = StreamFrame::ToolNode {
            run_id: "root".into(),
            parent_node_id: "dispatch".into(),
            tool_use_id: "same".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        };
        let first = projection.apply_stream_frame(&frame);
        let revision = first.revision;
        let duplicate = projection.apply_stream_frame(&frame);
        assert!(!duplicate.changed());
        assert_eq!(duplicate.revision, revision);
        assert_eq!(
            projection
                .find_node("root::dispatch")
                .unwrap()
                .children
                .len(),
            1
        );
    }

    #[test]
    fn append_only_mutations_preserve_existing_paths() {
        let mut projection = start_projection();
        projection.apply_stream_frame(&StreamFrame::ToolNode {
            run_id: "root".into(),
            parent_node_id: "dispatch".into(),
            tool_use_id: "first".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        let path = projection.index.node_paths["tool:root:first"].clone();

        projection.apply_stream_frame(&StreamFrame::ToolNode {
            run_id: "root".into(),
            parent_node_id: "dispatch".into(),
            tool_use_id: "second".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });

        assert_eq!(projection.index.node_paths["tool:root:first"], path);
        assert_eq!(
            projection.find_node("tool:root:first").unwrap().label,
            "fs.read"
        );
    }

    #[test]
    fn batch_commits_one_revision_and_reports_all_dirty_nodes() {
        let run_id = crate::event::FlowRunId::now();
        let events = vec![
            Event::FlowStart {
                run_id: run_id.clone(),
                flow_name: "root".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned: false,
            },
            Event::FlowNodeStart {
                run_id: run_id.clone(),
                node_id: "dispatch".into(),
                kind: crate::nodegraph::NodeKind::ToolCall {
                    path: "dispatch_all".into(),
                },
                label: "dispatch".into(),
                parent_node_id: None,
            },
        ];
        let mut projection = WorkflowProjection::new(TurnId::now());
        let delta = projection.apply_batch(&events);
        assert_eq!(delta.revision, 1);
        assert!(delta.structural_changed);
        assert_eq!(delta.dirty_nodes.len(), 2);
        assert!(
            projection
                .find_node(&scope_id(&run_id.0.to_string(), "dispatch"))
                .is_some()
        );
    }

    #[test]
    fn serialization_preserves_graph_shape_and_rebuilds_indices() {
        let mut projection = start_projection();
        projection.apply_stream_frame(&StreamFrame::ToolNode {
            run_id: "root".into(),
            parent_node_id: "dispatch".into(),
            tool_use_id: "tool".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        let graph_json = serde_json::to_value(projection.graph()).unwrap();
        let projection_json = serde_json::to_value(&projection).unwrap();
        assert_eq!(projection_json, graph_json);

        let mut restored: WorkflowProjection = serde_json::from_value(projection_json).unwrap();
        assert!(restored.find_node("tool:root:tool").is_some());
        let delta = restored.apply_stream_frame(&StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            id: "tool".into(),
            ok: true,
            preview: "done".into(),
        });
        assert!(delta.changed());
        assert_eq!(
            restored.find_node("tool:root:tool").unwrap().status,
            NodeStatus::Ok
        );
    }

    #[test]
    fn indexed_stream_application_matches_recursive_graph_semantics() {
        let run_id = crate::event::FlowRunId::now().0.to_string();
        let now = Utc::now();
        let tool_result = crate::message::Message {
            role: crate::message::MessageRole::Tool,
            parts: vec![crate::message::MessagePart::ToolResult {
                tool_use_id: "tool".into(),
                content: "contents".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        let frames = vec![
            StreamFrame::FlowStart {
                run_id: run_id.clone(),
                flow_name: "root".into(),
                parent_run_id: None,
                parent_node_id: None,
            },
            StreamFrame::FlowNodeStart {
                run_id: run_id.clone(),
                node_id: "dispatch".into(),
                kind: crate::nodegraph::NodeKind::ToolCall {
                    path: "dispatch_all".into(),
                },
                label: "dispatch".into(),
                parent_node_id: None,
            },
            StreamFrame::ToolNode {
                run_id: run_id.clone(),
                parent_node_id: "dispatch".into(),
                tool_use_id: "tool".into(),
                tool: "fs.read".into(),
                args_preview: "{}".into(),
                call_intent: None,
            },
            StreamFrame::ToolPendingApproval {
                run_id: run_id.clone(),
                tool_use_id: "tool".into(),
                tool_name: "fs.read".into(),
                args_preview: "{}".into(),
                level: "two".into(),
                preview: Some("read a file".into()),
            },
            StreamFrame::ToolResultMsg {
                flow_run_id: Some(run_id.clone()),
                message: tool_result,
            },
            StreamFrame::FlowNodeEnd {
                run_id: run_id.clone(),
                node_id: "dispatch".into(),
                status: FlowNodeStatus::Ok,
                output_preview: Some("done".into()),
                parent_node_id: None,
            },
            StreamFrame::FlowDone {
                run_id: run_id.clone(),
                flow_name: "root".into(),
                ok: true,
                cancelled: false,
                suicide: false,
            },
        ];
        let mut recursive = WorkflowGraph::new(TurnId::now());
        let mut indexed = WorkflowProjection::new(recursive.turn_id.clone());

        for frame in frames {
            recursive.apply_stream_frame_at(&frame, Some(now));
            indexed.apply_stream_frame_at(&frame, Some(now));
            assert_eq!(
                without_timestamps(recursive.clone()),
                without_timestamps(indexed.graph().clone())
            );
        }
    }

    #[test]
    fn scoped_tool_result_never_falls_back_to_a_different_run() {
        let mut projection = start_projection();
        projection.apply_stream_frame(&StreamFrame::ToolNode {
            run_id: "root".into(),
            parent_node_id: "dispatch".into(),
            tool_use_id: "shared".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
            call_intent: None,
        });
        let message = crate::message::Message {
            role: crate::message::MessageRole::Tool,
            parts: vec![crate::message::MessagePart::ToolResult {
                tool_use_id: "shared".into(),
                content: "wrong run".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };

        let delta = projection.apply_stream_frame(&StreamFrame::ToolResultMsg {
            flow_run_id: Some("other".into()),
            message,
        });

        assert!(!delta.changed());
        assert_eq!(
            projection.find_node("tool:root:shared").unwrap().status,
            NodeStatus::Running
        );
    }

    #[test]
    fn unscoped_tool_completion_preserves_depth_first_selection() {
        let mut graph = WorkflowGraph::new(TurnId::now());
        for run_id in ["first", "second"] {
            let mut flow = WorkflowNode {
                id: run_id.into(),
                kind: WorkflowNodeKind::Flow {
                    run_id: run_id.into(),
                    flow_name: run_id.into(),
                },
                label: run_id.into(),
                status: NodeStatus::Running,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            };
            flow.children.push(WorkflowNode {
                id: tool_node_id(run_id, "shared"),
                kind: WorkflowNodeKind::ToolCall {
                    tool_use_id: "shared".into(),
                    tool: "fs.read".into(),
                    args_preview: "{}".into(),
                    call_intent: None,
                    result_preview: None,
                },
                label: "fs.read".into(),
                status: NodeStatus::Running,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            });
            graph.root.push(flow);
        }
        let mut projection = WorkflowProjection::from(graph);

        projection.apply_stream_frame(&StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            id: "shared".into(),
            ok: true,
            preview: "done".into(),
        });

        assert_eq!(
            projection.find_node("tool:first:shared").unwrap().status,
            NodeStatus::Ok
        );
        assert_eq!(
            projection.find_node("tool:second:shared").unwrap().status,
            NodeStatus::Running
        );
    }
}
