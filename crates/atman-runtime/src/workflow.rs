use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{Event, FlowNodeStatus, FlowStatus, TurnId};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowGraph {
    pub turn_id: TurnId,
    pub root: Vec<WorkflowNode>,
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
        }
    }

    pub fn apply_event(&mut self, event: &Event) {
        match event {
            Event::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ts,
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
                    started_at: Some(*ts),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
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
            Event::FlowEnd {
                run_id, status, ts, ..
            } => {
                let id = run_id.0.to_string();
                if let Some(n) = find_node_mut(&mut self.root, &id) {
                    n.status = match status {
                        FlowStatus::Ok => NodeStatus::Ok,
                        FlowStatus::Errored { .. } => NodeStatus::Err,
                    };
                    n.ended_at = Some(*ts);
                }
            }
            Event::FlowNodeStart {
                run_id,
                node_id,
                kind: nk,
                label,
                parent_node_id,
                ts,
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
                    started_at: Some(*ts),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
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
                ts,
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
                    n.ended_at = Some(*ts);
                    if let Some(p) = output_preview {
                        n.output_preview = Some(p.clone());
                    }
                    for child in n.children.iter_mut() {
                        if matches!(child.status, NodeStatus::Running | NodeStatus::Pending) {
                            child.status = new_status;
                            child.ended_at = Some(*ts);
                        }
                    }
                }
            }
            Event::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                ts,
                ..
            } => {
                let rid = run_id.0.to_string();
                let scoped_parent = scope_id(&rid, parent_node_id);
                let id = format!("tool:{tool_use_id}");
                let node = WorkflowNode {
                    id,
                    kind: WorkflowNodeKind::ToolCall {
                        tool_use_id: tool_use_id.clone(),
                        tool: tool_name.clone(),
                        args_preview: args_preview.clone(),
                        result_preview: None,
                    },
                    label: tool_name.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(*ts),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &scoped_parent) {
                    parent.children.push(node);
                }
            }
            Event::AssistantMsg {
                flow_run_id,
                message,
                ts,
                ..
            } => {
                let Some(flow_id) = flow_run_id.as_ref().map(|r| r.0.to_string()) else {
                    return;
                };
                for part in &message.parts {
                    if let crate::message::MessagePart::ToolUse { id, name, input } = part {
                        let node_id = format!("tool:{id}");
                        if find_node(&self.root, &node_id).is_some() {
                            continue;
                        }
                        let args_preview = serde_json::to_string(input).unwrap_or_default();
                        let args_preview: String = args_preview.chars().take(200).collect();
                        let node = WorkflowNode {
                            id: node_id,
                            kind: WorkflowNodeKind::ToolCall {
                                tool_use_id: id.clone(),
                                tool: name.clone(),
                                args_preview: args_preview.clone(),
                                result_preview: None,
                            },
                            label: name.clone(),
                            status: NodeStatus::Running,
                            started_at: Some(*ts),
                            ended_at: None,
                            output_preview: None,
                            children: Vec::new(),
                            parallelism: Parallelism::Serial,
                        };
                        if let Some(parent) = find_node_mut(&mut self.root, &flow_id) {
                            parent.children.push(node);
                        }
                    }
                }
            }
            Event::ToolResultMsg { message, ts, .. } => {
                for part in &message.parts {
                    if let crate::message::MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = part
                    {
                        let node_id = format!("tool:{tool_use_id}");
                        if let Some(n) = find_node_mut(&mut self.root, &node_id) {
                            n.status = if *is_error {
                                NodeStatus::Err
                            } else {
                                NodeStatus::Ok
                            };
                            n.ended_at = Some(*ts);
                            let preview: String = content.chars().take(300).collect();
                            n.output_preview = Some(preview.clone());
                            if let WorkflowNodeKind::ToolCall { result_preview, .. } = &mut n.kind {
                                *result_preview = Some(preview);
                            }
                        }
                    }
                }
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

    pub fn apply_stream_frame(&mut self, frame: &crate::stream::StreamFrame) {
        use crate::stream::StreamFrame;
        let now = Utc::now();
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
            StreamFrame::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool,
                args_preview,
                ..
            } => {
                let scoped_parent = scope_id(run_id, parent_node_id);
                let node = WorkflowNode {
                    id: format!("tool:{tool_use_id}"),
                    kind: WorkflowNodeKind::ToolCall {
                        tool_use_id: tool_use_id.clone(),
                        tool: tool.clone(),
                        args_preview: args_preview.clone(),
                        result_preview: None,
                    },
                    label: tool.clone(),
                    status: NodeStatus::Running,
                    started_at: Some(now),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                };
                if let Some(parent) = find_node_mut(&mut self.root, &scoped_parent) {
                    parent.children.push(node);
                }
            }
            StreamFrame::ToolUseDone {
                id, ok, preview, ..
            } => {
                let tool_id = format!("tool:{id}");
                if let Some(n) = find_node_mut(&mut self.root, &tool_id) {
                    n.status = if *ok { NodeStatus::Ok } else { NodeStatus::Err };
                    n.ended_at = Some(now);
                    n.output_preview = Some(preview.clone());
                }
            }
            StreamFrame::FlowDone { run_id, ok, .. } => {
                if let Some(n) = find_node_mut(&mut self.root, run_id) {
                    n.status = if *ok { NodeStatus::Ok } else { NodeStatus::Err };
                    n.ended_at = Some(now);
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
                    seq: 0,
                    turn_id: crate::event::TurnId::now(),
                    flow_run_id: Some(crate::event::FlowRunId(uuid)),
                    message: message.clone(),
                    ts: chrono::Utc::now(),
                });
            }
            StreamFrame::ToolResultMsg { message, .. } => {
                self.apply_event(&Event::ToolResultMsg {
                    seq: 0,
                    turn_id: crate::event::TurnId::now(),
                    flow_run_id: None,
                    message: message.clone(),
                    ts: chrono::Utc::now(),
                });
            }
            _ => {}
        }
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

fn parse_branch_index(node_id: &str) -> Option<usize> {
    let start = node_id.rfind(".branch[")?;
    let rest = &node_id[start + ".branch[".len()..];
    let end = rest.find(']')?;
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{FlowRunId, FlowStatus};
    use crate::nodegraph::NodeKind;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn flow_start(run_id: FlowRunId, name: &str) -> Event {
        Event::FlowStart {
            seq: 0,
            run_id,
            flow_name: name.into(),
            parent_run_id: None,
            parent_node_id: None,
            ts: now(),
        }
    }

    fn subflow_start(child: FlowRunId, parent: FlowRunId, parent_node: &str, name: &str) -> Event {
        Event::FlowStart {
            seq: 0,
            run_id: child,
            flow_name: name.into(),
            parent_run_id: Some(parent),
            parent_node_id: Some(parent_node.into()),
            ts: now(),
        }
    }

    fn stmt_start(run_id: FlowRunId, node_id: &str, parent: Option<&str>) -> Event {
        Event::FlowNodeStart {
            seq: 0,
            run_id,
            node_id: node_id.into(),
            kind: NodeKind::UserConfirm,
            label: node_id.into(),
            parent_node_id: parent.map(String::from),
            ts: now(),
        }
    }

    fn stmt_end(run_id: FlowRunId, node_id: &str, status: FlowNodeStatus) -> Event {
        Event::FlowNodeEnd {
            seq: 0,
            run_id,
            node_id: node_id.into(),
            status,
            output_preview: None,
            ts: now(),
        }
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
            seq: 0,
            run_id: rid.clone(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_1".into(),
            tool_name: "fs.read".into(),
            args_preview: "{\"path\":\"a\"}".into(),
            ts: now(),
        });
        g.apply_event(&stmt_end(rid.clone(), "stmt_0", FlowNodeStatus::Ok));
        g.apply_event(&Event::FlowEnd {
            seq: 0,
            run_id: rid.clone(),
            flow_name: "main".into(),
            status: FlowStatus::Ok,
            ts: now(),
        });
        let scoped = scope_id(&rid.0.to_string(), "stmt_0");
        let stmt = g.find_node(&scoped).unwrap();
        assert_eq!(stmt.status, NodeStatus::Ok);
        assert_eq!(stmt.children.len(), 1);
        let tool = &stmt.children[0];
        assert_eq!(tool.id, "tool:tu_1");
        assert!(matches!(tool.kind, WorkflowNodeKind::ToolCall { .. }));
        assert_eq!(g.root[0].status, NodeStatus::Ok);
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
            seq: 0,
            run_id: FlowRunId::now(),
            parent_node_id: "missing".into(),
            tool_use_id: "tu".into(),
            tool_name: "t".into(),
            args_preview: "{}".into(),
            ts: now(),
        });
        assert!(g.root.is_empty());
    }
}
