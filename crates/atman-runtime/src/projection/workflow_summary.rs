use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};

use crate::context_plan::{ContextCallPurpose, ContextCallScope};
use crate::workflow::{LlmStats, NodeStatus, WorkflowGraph, WorkflowNode, WorkflowNodeKind};

const RECENT_COMPLETED_LEAF_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkflowCounts {
    pub nodes: usize,
    pub agents: usize,
    pub tools: usize,
    pub edits: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowAggregateStatus {
    Empty,
    Running,
    Error,
    Ok,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WorkflowLlmRoute {
    pub provider: String,
    pub model: String,
    pub purpose: ContextCallPurpose,
    pub scope: ContextCallScope,
}

impl WorkflowLlmRoute {
    pub fn is_primary(&self) -> bool {
        self.purpose == ContextCallPurpose::General && self.scope == ContextCallScope::Root
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WorkflowLlmAggregate {
    pub calls: usize,
    pub total_in: u64,
    pub total_out: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_ttft_ms: u64,
    pub speed_sum: f64,
    pub speed_count: usize,
}

impl WorkflowLlmAggregate {
    pub fn record(&mut self, stats: &LlmStats) {
        self.calls = self.calls.saturating_add(1);
        self.total_in = self
            .total_in
            .saturating_add(stats.input_tokens)
            .saturating_add(stats.cache_read)
            .saturating_add(stats.cache_write);
        self.total_out = self.total_out.saturating_add(stats.output_tokens);
        self.cache_read = self.cache_read.saturating_add(stats.cache_read);
        self.cache_write = self.cache_write.saturating_add(stats.cache_write);
        self.total_ttft_ms = self.total_ttft_ms.saturating_add(stats.ttft_ms);
        if stats.tokens_per_second > 0.0 {
            self.speed_sum += stats.tokens_per_second;
            self.speed_count = self.speed_count.saturating_add(1);
        }
    }

    pub fn merge(&mut self, other: Self) {
        self.calls = self.calls.saturating_add(other.calls);
        self.total_in = self.total_in.saturating_add(other.total_in);
        self.total_out = self.total_out.saturating_add(other.total_out);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.total_ttft_ms = self.total_ttft_ms.saturating_add(other.total_ttft_ms);
        self.speed_sum += other.speed_sum;
        self.speed_count = self.speed_count.saturating_add(other.speed_count);
    }

    pub fn average_speed(self) -> f64 {
        if self.speed_count == 0 {
            0.0
        } else {
            self.speed_sum / self.speed_count as f64
        }
    }

    fn remove(&mut self, stats: &LlmStats) {
        self.calls = self.calls.saturating_sub(1);
        self.total_in = self
            .total_in
            .saturating_sub(stats.input_tokens)
            .saturating_sub(stats.cache_read)
            .saturating_sub(stats.cache_write);
        self.total_out = self.total_out.saturating_sub(stats.output_tokens);
        self.cache_read = self.cache_read.saturating_sub(stats.cache_read);
        self.cache_write = self.cache_write.saturating_sub(stats.cache_write);
        self.total_ttft_ms = self.total_ttft_ms.saturating_sub(stats.ttft_ms);
        if stats.tokens_per_second > 0.0 {
            self.speed_sum -= stats.tokens_per_second;
            self.speed_count = self.speed_count.saturating_sub(1);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct NodeSummaryState {
    status: NodeStatus,
    started_at: Option<DateTime<Utc>>,
    ended_at: Option<DateTime<Utc>>,
    llm_stats: Option<LlmStats>,
}

impl From<&WorkflowNode> for NodeSummaryState {
    fn from(node: &WorkflowNode) -> Self {
        Self {
            status: node.status,
            started_at: node.started_at,
            ended_at: node.ended_at,
            llm_stats: node.llm_stats.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LeafOrder {
    started_at: DateTime<Utc>,
    ordinal: u64,
    node_id: String,
    path: Vec<usize>,
}

impl Ord for LeafOrder {
    fn cmp(&self, other: &Self) -> Ordering {
        self.started_at
            .cmp(&other.started_at)
            .then_with(|| other.ordinal.cmp(&self.ordinal))
            .then_with(|| self.node_id.cmp(&other.node_id))
    }
}

impl PartialOrd for LeafOrder {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeafBucket {
    Running,
    Completed,
}

#[derive(Clone, Debug)]
struct LeafRecord {
    order: LeafOrder,
    bucket: LeafBucket,
}

#[derive(Clone, Debug, Default)]
pub struct WorkflowSummary {
    counts: WorkflowCounts,
    running_nodes: usize,
    error_nodes: usize,
    root_started_at: BTreeMap<DateTime<Utc>, usize>,
    root_ended_at: BTreeMap<DateTime<Utc>, usize>,
    llm_routes: HashMap<WorkflowLlmRoute, WorkflowLlmAggregate>,
    node_states: HashMap<String, NodeSummaryState>,
    leaves: HashMap<String, LeafRecord>,
    running_leaves: BTreeSet<LeafOrder>,
    recent_completed_leaves: BTreeSet<LeafOrder>,
    next_leaf_ordinal: u64,
}

impl WorkflowSummary {
    pub(super) fn rebuild(graph: &WorkflowGraph) -> Self {
        fn visit(summary: &mut WorkflowSummary, nodes: &[WorkflowNode], path: &mut Vec<usize>) {
            for (index, node) in nodes.iter().enumerate() {
                path.push(index);
                summary.insert_node(node, path, path.len() == 1);
                visit(summary, &node.children, path);
                path.pop();
            }
        }

        let mut summary = Self::default();
        visit(&mut summary, &graph.root, &mut Vec::new());
        summary
    }

    pub fn counts(&self) -> WorkflowCounts {
        self.counts
    }

    pub fn status(&self) -> WorkflowAggregateStatus {
        if self.running_nodes > 0 {
            WorkflowAggregateStatus::Running
        } else if self.error_nodes > 0 {
            WorkflowAggregateStatus::Error
        } else if self.counts.nodes == 0 {
            WorkflowAggregateStatus::Empty
        } else {
            WorkflowAggregateStatus::Ok
        }
    }

    pub fn started_at(&self) -> Option<DateTime<Utc>> {
        self.root_started_at.first_key_value().map(|(at, _)| *at)
    }

    pub fn ended_at(&self) -> Option<DateTime<Utc>> {
        self.root_ended_at.last_key_value().map(|(at, _)| *at)
    }

    pub fn elapsed_secs(&self, now: DateTime<Utc>) -> i64 {
        let Some(started_at) = self.started_at() else {
            return 0;
        };
        let ended_at = if self.status() == WorkflowAggregateStatus::Running {
            now
        } else {
            self.ended_at().unwrap_or(started_at)
        };
        (ended_at - started_at).num_seconds().max(0)
    }

    pub fn llm_routes(&self) -> &HashMap<WorkflowLlmRoute, WorkflowLlmAggregate> {
        &self.llm_routes
    }

    pub fn collapsed_leaf_paths(&self, limit: usize) -> Vec<Vec<usize>> {
        let leaves = if self.running_leaves.is_empty() {
            &self.recent_completed_leaves
        } else {
            &self.running_leaves
        };
        leaves
            .iter()
            .rev()
            .take(limit)
            .map(|leaf| leaf.path.clone())
            .collect()
    }

    pub(super) fn insert_node(&mut self, node: &WorkflowNode, path: &[usize], is_root: bool) {
        self.counts.nodes = self.counts.nodes.saturating_add(1);
        if let WorkflowNodeKind::ToolCall { tool, .. } = &node.kind {
            self.counts.tools = self.counts.tools.saturating_add(1);
            if tool == "flow.spawn" {
                self.counts.agents = self.counts.agents.saturating_add(1);
            }
            if matches!(
                tool.as_str(),
                "fs.edit" | "fs.write" | "hunk.apply" | "hunk.plan_edit"
            ) {
                self.counts.edits = self.counts.edits.saturating_add(1);
            }
        }
        self.add_status(node.status);
        if is_root {
            insert_time(&mut self.root_started_at, node.started_at);
            insert_time(&mut self.root_ended_at, node.ended_at);
        }
        if let Some(stats) = &node.llm_stats {
            self.add_llm_stats(stats);
        }
        self.node_states
            .insert(node.id.clone(), NodeSummaryState::from(node));
        if node.children.is_empty() && is_render_leaf(node) {
            let order = LeafOrder {
                started_at: node.started_at.unwrap_or_else(Utc::now),
                ordinal: self.next_leaf_ordinal,
                node_id: node.id.clone(),
                path: path.to_vec(),
            };
            self.next_leaf_ordinal = self.next_leaf_ordinal.wrapping_add(1);
            self.insert_leaf(order, leaf_bucket(node.status));
        }
    }

    pub(super) fn remove_leaf(&mut self, node_id: &str) {
        let Some(record) = self.leaves.remove(node_id) else {
            return;
        };
        self.remove_leaf_order(&record);
    }

    pub(super) fn sync_node(&mut self, node: &WorkflowNode, is_root: bool) -> bool {
        let next = NodeSummaryState::from(node);
        let Some(previous) = self.node_states.insert(node.id.clone(), next.clone()) else {
            return false;
        };
        if previous == next {
            return false;
        }
        if previous.status != next.status {
            self.remove_status(previous.status);
            self.add_status(next.status);
            self.update_leaf_bucket(&node.id, next.status);
        }
        if is_root {
            if previous.started_at != next.started_at {
                remove_time(&mut self.root_started_at, previous.started_at);
                insert_time(&mut self.root_started_at, next.started_at);
            }
            if previous.ended_at != next.ended_at {
                remove_time(&mut self.root_ended_at, previous.ended_at);
                insert_time(&mut self.root_ended_at, next.ended_at);
            }
        }
        if previous.llm_stats != next.llm_stats {
            if let Some(stats) = &previous.llm_stats {
                self.remove_llm_stats(stats);
            }
            if let Some(stats) = &next.llm_stats {
                self.add_llm_stats(stats);
            }
        }
        true
    }

    pub(super) fn sync_subtree(&mut self, node: &WorkflowNode, is_root: bool) -> bool {
        let mut changed = self.sync_node(node, is_root);
        for child in &node.children {
            changed |= self.sync_subtree(child, false);
        }
        changed
    }

    fn add_status(&mut self, status: NodeStatus) {
        match status {
            NodeStatus::Running | NodeStatus::Pending => {
                self.running_nodes = self.running_nodes.saturating_add(1);
            }
            NodeStatus::Err => {
                self.error_nodes = self.error_nodes.saturating_add(1);
            }
            NodeStatus::Ok | NodeStatus::Cancelled => {}
        }
    }

    fn remove_status(&mut self, status: NodeStatus) {
        match status {
            NodeStatus::Running | NodeStatus::Pending => {
                self.running_nodes = self.running_nodes.saturating_sub(1);
            }
            NodeStatus::Err => {
                self.error_nodes = self.error_nodes.saturating_sub(1);
            }
            NodeStatus::Ok | NodeStatus::Cancelled => {}
        }
    }

    fn add_llm_stats(&mut self, stats: &LlmStats) {
        self.llm_routes
            .entry(llm_route(stats))
            .or_default()
            .record(stats);
    }

    fn remove_llm_stats(&mut self, stats: &LlmStats) {
        let route = llm_route(stats);
        let remove_route = self.llm_routes.get_mut(&route).is_some_and(|aggregate| {
            aggregate.remove(stats);
            aggregate.calls == 0
        });
        if remove_route {
            self.llm_routes.remove(&route);
        }
    }

    fn insert_leaf(&mut self, order: LeafOrder, bucket: LeafBucket) {
        self.leaves.insert(
            order.node_id.clone(),
            LeafRecord {
                order: order.clone(),
                bucket,
            },
        );
        match bucket {
            LeafBucket::Running => {
                self.running_leaves.insert(order);
            }
            LeafBucket::Completed => {
                self.recent_completed_leaves.insert(order);
                while self.recent_completed_leaves.len() > RECENT_COMPLETED_LEAF_LIMIT {
                    if let Some(evicted) = self.recent_completed_leaves.pop_first() {
                        self.leaves.remove(&evicted.node_id);
                    }
                }
            }
        }
    }

    fn remove_leaf_order(&mut self, record: &LeafRecord) {
        match record.bucket {
            LeafBucket::Running => {
                self.running_leaves.remove(&record.order);
            }
            LeafBucket::Completed => {
                self.recent_completed_leaves.remove(&record.order);
            }
        }
    }

    fn update_leaf_bucket(&mut self, node_id: &str, status: NodeStatus) {
        let Some(mut record) = self.leaves.get(node_id).cloned() else {
            return;
        };
        let next_bucket = leaf_bucket(status);
        if record.bucket == next_bucket {
            return;
        }
        self.remove_leaf_order(&record);
        record.bucket = next_bucket;
        self.leaves.insert(node_id.to_string(), record.clone());
        match next_bucket {
            LeafBucket::Running => {
                self.running_leaves.insert(record.order);
            }
            LeafBucket::Completed => {
                self.recent_completed_leaves.insert(record.order);
                while self.recent_completed_leaves.len() > RECENT_COMPLETED_LEAF_LIMIT {
                    if let Some(evicted) = self.recent_completed_leaves.pop_first() {
                        self.leaves.remove(&evicted.node_id);
                    }
                }
            }
        }
    }
}

fn is_render_leaf(node: &WorkflowNode) -> bool {
    matches!(
        node.kind,
        WorkflowNodeKind::ToolCall { .. }
            | WorkflowNodeKind::Stmt { .. }
            | WorkflowNodeKind::FanoutBranch { .. }
    )
}

fn leaf_bucket(status: NodeStatus) -> LeafBucket {
    if matches!(status, NodeStatus::Running | NodeStatus::Pending) {
        LeafBucket::Running
    } else {
        LeafBucket::Completed
    }
}

fn llm_route(stats: &LlmStats) -> WorkflowLlmRoute {
    WorkflowLlmRoute {
        provider: stats.provider.clone(),
        model: stats.model.clone(),
        purpose: stats.context_call_purpose,
        scope: stats.context_call_scope,
    }
}

fn insert_time(times: &mut BTreeMap<DateTime<Utc>, usize>, value: Option<DateTime<Utc>>) {
    if let Some(value) = value {
        *times.entry(value).or_default() += 1;
    }
}

fn remove_time(times: &mut BTreeMap<DateTime<Utc>, usize>, value: Option<DateTime<Utc>>) {
    let Some(value) = value else {
        return;
    };
    if let Some(count) = times.get_mut(&value) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            times.remove(&value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::workflow::{Parallelism, WorkflowNodeKind};

    fn completed_tool(index: usize, started_at: DateTime<Utc>) -> WorkflowNode {
        WorkflowNode {
            id: format!("tool-{index}"),
            kind: WorkflowNodeKind::ToolCall {
                tool_use_id: format!("call-{index}"),
                tool: "fs.read".into(),
                args_preview: "{}".into(),
                call_intent: None,
                result_preview: Some("done".into()),
            },
            label: format!("tool-{index}"),
            status: NodeStatus::Ok,
            started_at: Some(started_at),
            ended_at: Some(started_at),
            output_preview: Some("done".into()),
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        }
    }

    #[test]
    fn recent_completed_leaf_selection_is_bounded_and_newest_first() {
        let now = Utc::now();
        let graph = WorkflowGraph {
            turn_id: TurnId::now(),
            root: (0..10_000)
                .map(|index| {
                    completed_tool(index, now + chrono::Duration::milliseconds(index as i64))
                })
                .collect(),
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };

        let summary = WorkflowSummary::rebuild(&graph);
        let paths = summary.collapsed_leaf_paths(128);

        assert_eq!(summary.counts().nodes, 10_000);
        assert_eq!(summary.counts().tools, 10_000);
        assert_eq!(summary.status(), WorkflowAggregateStatus::Ok);
        assert_eq!(paths.len(), 128);
        assert_eq!(paths.first(), Some(&vec![9_999]));
        assert_eq!(paths.last(), Some(&vec![9_872]));
        assert_eq!(
            summary.recent_completed_leaves.len(),
            RECENT_COMPLETED_LEAF_LIMIT
        );
        assert_eq!(summary.leaves.len(), RECENT_COMPLETED_LEAF_LIMIT);
    }
}
