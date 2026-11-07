//! Local workflow disclosure. Source nodes and permission authority remain immutable.

use std::collections::HashSet;

use atman_runtime::{
    nodegraph::NodeKind,
    workflow::{NodeStatus, WorkflowNode, WorkflowNodeKind},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presentation {
    pub hidden: bool,
    pub structure: bool,
    pub lanes: bool,
    pub metrics: bool,
    pub selected: Option<String>,
    pub section: Section,
    pub collapsed: HashSet<String>,
    pub revealed: HashSet<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Section {
    #[default]
    Source,
    Arguments,
    Result,
    Permission,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Content,
    Structure,
    Layout,
    Metrics,
    Review(String),
    Close,
    Select(String),
    Inspect(String, Section),
    Branch(String),
    Reveal(String),
    Section(Section),
}

impl Action {
    pub fn key(&self) -> String {
        let tail = match self {
            Self::Content => "content".into(),
            Self::Structure => "structure".into(),
            Self::Layout => "layout".into(),
            Self::Metrics => "metrics".into(),
            Self::Review(id) => format!("review:{id}"),
            Self::Close => "close".into(),
            Self::Select(id) => format!("select:{id}"),
            Self::Inspect(id, section) => format!("inspect:{}:{id}", section.key()),
            Self::Branch(id) => format!("branch:{id}"),
            Self::Reveal(id) => format!("reveal:{id}"),
            Self::Section(section) => format!(
                "section:{}",
                match section {
                    Section::Source => "source",
                    Section::Arguments => "args",
                    Section::Result => "result",
                    Section::Permission => "permission",
                }
            ),
        };
        format!("workflow:{tail}")
    }

    pub fn parse(key: &str) -> Option<Self> {
        Some(match key.strip_prefix("workflow:")? {
            "content" => Self::Content,
            "structure" => Self::Structure,
            "layout" => Self::Layout,
            "metrics" => Self::Metrics,
            "close" => Self::Close,
            "section:source" => Self::Section(Section::Source),
            "section:args" => Self::Section(Section::Arguments),
            "section:result" => Self::Section(Section::Result),
            "section:permission" => Self::Section(Section::Permission),
            tail => {
                let (action, id) = tail.split_once(':')?;
                match action {
                    "select" => Self::Select(id.into()),
                    "review" => Self::Review(id.into()),
                    "inspect" => {
                        let (section, id) = id.split_once(':')?;
                        Self::Inspect(id.into(), Section::parse(section)?)
                    }
                    "branch" => Self::Branch(id.into()),
                    "reveal" => Self::Reveal(id.into()),
                    _ => return None,
                }
            }
        })
    }
}

impl Presentation {
    pub(crate) fn retains_height(&self) -> bool {
        !self.hidden
            && self.selected.is_none()
            && !self.metrics
            && !self.structure
            && self.collapsed.is_empty()
            && self.revealed.is_empty()
    }

    pub fn apply(&mut self, action: Action, node: Option<&WorkflowNode>) {
        match action {
            Action::Content => self.hidden = !self.hidden,
            Action::Structure => self.structure = !self.structure,
            Action::Layout => self.lanes = !self.lanes,
            Action::Metrics => self.metrics = !self.metrics,
            Action::Review(_) => {}
            Action::Close => self.selected = None,
            Action::Select(id) => {
                self.section = match node {
                    Some(node) if node.approval.is_some() => Section::Permission,
                    Some(node) if node.status == NodeStatus::Err => Section::Result,
                    Some(WorkflowNode {
                        kind: WorkflowNodeKind::ToolCall { .. },
                        ..
                    }) => Section::Arguments,
                    _ => Section::Source,
                };
                self.selected = Some(id);
            }
            Action::Inspect(id, section) => {
                self.selected = Some(id);
                self.section = section;
            }
            Action::Branch(id) => toggle(&mut self.collapsed, id),
            Action::Reveal(id) => toggle(&mut self.revealed, id),
            Action::Section(section) => self.section = section,
        }
    }
}

impl Section {
    fn key(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Arguments => "args",
            Self::Result => "result",
            Self::Permission => "permission",
        }
    }

    fn parse(key: &str) -> Option<Self> {
        Some(match key {
            "source" => Self::Source,
            "args" => Self::Arguments,
            "result" => Self::Result,
            "permission" => Self::Permission,
            _ => return None,
        })
    }

    pub(crate) fn for_node(node: &WorkflowNode) -> Self {
        if node.approval.is_some() {
            Self::Permission
        } else if node.status == NodeStatus::Err {
            Self::Result
        } else if matches!(node.kind, WorkflowNodeKind::ToolCall { .. }) {
            Self::Arguments
        } else {
            Self::Source
        }
    }
}

fn toggle(set: &mut HashSet<String>, id: String) {
    if !set.remove(&id) {
        set.insert(id);
    }
}

pub(crate) struct Row<'a> {
    pub node: &'a WorkflowNode,
    pub wrapper: Option<&'a WorkflowNode>,
    pub depth: usize,
    pub completed: Vec<&'a WorkflowNode>,
}

/// A direct child is source parentage; labels alone never establish pairing.
fn paired(node: &WorkflowNode) -> Option<&WorkflowNode> {
    let WorkflowNodeKind::Stmt {
        node_kind: NodeKind::ToolCall { .. },
    } = &node.kind
    else {
        return None;
    };
    let [child] = node.children.as_slice() else {
        return None;
    };
    (matches!(child.kind, WorkflowNodeKind::ToolCall { .. })
        && child.status == node.status
        && node.approval.is_none()
        && node.output_preview.is_none()
        && node.llm_stats.is_none())
    .then_some(child)
}

fn safe_completed(node: &WorkflowNode) -> bool {
    node.status == NodeStatus::Ok
        && node.approval.is_none()
        && node.children.is_empty()
        && matches!(node.kind, WorkflowNodeKind::ToolCall { .. })
}

pub(crate) fn rows<'a>(nodes: &'a [WorkflowNode], state: &Presentation) -> Vec<Row<'a>> {
    fn visit<'a>(
        nodes: &'a [WorkflowNode],
        state: &Presentation,
        depth: usize,
        out: &mut Vec<Row<'a>>,
    ) {
        let mut index = 0;
        while let Some(original) = nodes.get(index) {
            if !state.structure && safe_completed(original) {
                let count = nodes[index..]
                    .iter()
                    .take_while(|node| safe_completed(node))
                    .count();
                if count >= 3 {
                    out.push(Row {
                        node: original,
                        wrapper: None,
                        depth,
                        completed: nodes[index..index + count].iter().collect(),
                    });
                    if state.revealed.contains(&original.id) {
                        for node in &nodes[index..index + count] {
                            out.push(Row {
                                node,
                                wrapper: None,
                                depth: depth + 1,
                                completed: Vec::new(),
                            });
                        }
                    }
                    index += count;
                    continue;
                }
            }
            let pair = (!state.structure).then(|| paired(original)).flatten();
            let node = pair.unwrap_or(original);
            out.push(Row {
                node,
                wrapper: pair.map(|_| original),
                depth,
                completed: Vec::new(),
            });
            if !state.collapsed.contains(&node.id) {
                visit(&node.children, state, depth + 1, out);
            }
            index += 1;
        }
    }
    let mut out = Vec::new();
    visit(nodes, state, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(id: &str) -> WorkflowNode {
        WorkflowNode {
            id: id.into(),
            label: "read".into(),
            kind: WorkflowNodeKind::ToolCall {
                tool_use_id: id.into(),
                tool: "fs.read".into(),
                args_preview: "path".into(),
                call_intent: None,
                result_preview: None,
            },
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: atman_runtime::workflow::Parallelism::Serial,
            approval: None,
            llm_stats: None,
        }
    }

    #[test]
    fn overview_preserves_original_identities_and_exceptions() {
        let mut nodes = vec![tool("a"), tool("b"), tool("c"), tool("failed")];
        nodes[3].status = NodeStatus::Err;
        let original = nodes.clone();
        let mut state = Presentation::default();
        let compact = rows(&nodes, &state);
        assert_eq!(compact.len(), 2);
        assert_eq!(
            compact[0]
                .completed
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(compact[1].node.id, "failed");
        state.apply(Action::Reveal("a".into()), None);
        let revealed = rows(&nodes, &state);
        assert_eq!(revealed.len(), 5);
        state.apply(Action::Reveal("a".into()), None);
        assert_eq!(rows(&nodes, &state).len(), 2);
        state.structure = true;
        assert_eq!(
            rows(&nodes, &state)
                .iter()
                .map(|row| row.node.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "failed"]
        );
        assert_eq!(nodes, original);
        nodes[1].approval = Some(atman_runtime::workflow::ApprovalState::Approved);
        state.structure = false;
        assert_eq!(rows(&nodes, &state).len(), 4);
    }

    #[test]
    fn pairing_requires_source_parentage_and_no_independent_wrapper_meaning() {
        let child = tool("tool:run:call");
        let mut wrapper = tool("run:statement");
        wrapper.kind = WorkflowNodeKind::Stmt {
            node_kind: NodeKind::ToolCall {
                path: "fs.read".into(),
            },
        };
        wrapper.children.push(child);
        assert_eq!(paired(&wrapper).unwrap().id, "tool:run:call");
        wrapper.output_preview = Some("wrapper output".into());
        assert!(paired(&wrapper).is_none());
        wrapper.output_preview = None;
        wrapper.status = NodeStatus::Running;
        assert!(paired(&wrapper).is_none());
    }

    #[test]
    fn action_identity_and_disclosure_are_independent() {
        let id = "tool:run:call/with:separators".to_owned();
        let action = Action::Branch(id.clone());
        assert_eq!(Action::parse(&action.key()), Some(action.clone()));
        let mut state = Presentation::default();
        state.apply(Action::Select(id.clone()), None);
        state.apply(action, None);
        state.apply(Action::Structure, None);
        assert_eq!(state.selected.as_ref(), Some(&id));
        assert!(state.collapsed.contains(&id));
        state.apply(Action::Section(Section::Arguments), None);
        assert!(state.collapsed.contains(&id));
        assert_eq!(state.section, Section::Arguments);
    }
}
