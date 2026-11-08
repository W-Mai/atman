//! Local workflow disclosure. Source nodes and permission authority remain immutable.

use std::collections::{HashMap, HashSet};

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
    pub completed: HashMap<String, bool>,
    pub source_details: bool,
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
    Reveal(String, bool),
    SourceDetails,
    Section(Section),
}

impl Action {
    pub(crate) fn hint(&self) -> &'static str {
        match self {
            Self::Content => "Show / hide content",
            Self::Structure => "Overview / complete structure",
            Self::Layout => "Outline / parallel lanes",
            Self::Metrics => "Tokens ↑/↓ · cache reuse ↻ · applied lines +/− · files ▧",
            Self::Review(_) => "Review current permission",
            Self::Close => "Close inspector",
            Self::Select(_) | Self::Inspect(_, _) => "Inspect node",
            Self::Branch(_) => "Show / hide descendants",
            Self::Reveal(_, _) => "Show / hide completed operations",
            Self::SourceDetails => "Recorded identities and timestamps",
            Self::Section(Section::Source) => "Source",
            Self::Section(Section::Arguments) => "Arguments · recorded preview",
            Self::Section(Section::Result) => "Result · recorded preview",
            Self::Section(Section::Permission) => "Permission audit",
        }
    }
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
            Self::Reveal(id, open) => format!("reveal:{}:{id}", u8::from(*open)),
            Self::SourceDetails => "source-details".into(),
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
            "source-details" => Self::SourceDetails,
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
                    "reveal" => {
                        let (open, id) = id.split_once(':')?;
                        Self::Reveal(
                            id.into(),
                            match open {
                                "0" => false,
                                "1" => true,
                                _ => return None,
                            },
                        )
                    }
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
            && self.completed.is_empty()
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
                self.section = node.map(Section::for_node).unwrap_or_default();
                self.selected = Some(id);
            }
            Action::Inspect(id, section) => {
                self.selected = Some(id);
                self.section = section;
            }
            Action::Branch(id) => toggle(&mut self.collapsed, id),
            Action::Reveal(id, open) => {
                self.completed.insert(id, open);
            }
            Action::SourceDetails => self.source_details = !self.source_details,
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
        if matches!(
            node.approval,
            Some(
                atman_runtime::workflow::ApprovalState::Pending { .. }
                    | atman_runtime::workflow::ApprovalState::Denied { .. }
            )
        ) {
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
    pub ancestors: Vec<bool>,
    pub last: bool,
    pub completed: Vec<&'a WorkflowNode>,
    pub completed_open: bool,
}

/// Direct source parentage and the recorded callee establish a transparent call.
fn paired(node: &WorkflowNode) -> Option<&WorkflowNode> {
    let WorkflowNodeKind::Stmt {
        node_kind: NodeKind::ToolCall { path },
    } = &node.kind
    else {
        return None;
    };
    let [child] = node.children.as_slice() else {
        return None;
    };
    let WorkflowNodeKind::ToolCall {
        tool,
        result_preview,
        ..
    } = &child.kind
    else {
        return None;
    };
    let result = result_preview.as_ref().or(child.output_preview.as_ref());
    (path == tool
        && node.status == child.status
        && node.approval.is_none()
        && node.parallelism == atman_runtime::workflow::Parallelism::Serial
        && node
            .output_preview
            .as_ref()
            .zip(result)
            .is_none_or(|(a, b)| a == b)
        && node
            .llm_stats
            .as_ref()
            .zip(child.llm_stats.as_ref())
            .is_none_or(|(a, b)| a == b))
    .then_some(child)
}

pub(crate) fn wrapper_for<'a>(nodes: &'a [WorkflowNode], id: &str) -> Option<&'a WorkflowNode> {
    for node in nodes {
        if paired(node).is_some_and(|child| child.id == id) {
            return Some(node);
        }
        if let Some(wrapper) = wrapper_for(&node.children, id) {
            return Some(wrapper);
        }
    }
    None
}

pub(crate) fn result_preview<'a>(
    node: &'a WorkflowNode,
    wrapper: Option<&'a WorkflowNode>,
) -> Option<&'a str> {
    let result = match &node.kind {
        WorkflowNodeKind::ToolCall { result_preview, .. } => result_preview.as_deref(),
        _ => None,
    };
    result
        .or(node.output_preview.as_deref())
        .or_else(|| wrapper.and_then(|node| node.output_preview.as_deref()))
}

#[derive(Clone, Copy)]
struct Facts {
    completed: bool,
    selected: bool,
}

pub(crate) fn rows<'a>(nodes: &'a [WorkflowNode], state: &Presentation) -> Vec<Row<'a>> {
    fn classify<'a>(
        nodes: &'a [WorkflowNode],
        selected: Option<&str>,
        facts: &mut HashMap<&'a str, Facts>,
    ) {
        for node in nodes {
            classify(&node.children, selected, facts);
            let ordinary = matches!(
                node.kind,
                WorkflowNodeKind::ToolCall { .. }
                    | WorkflowNodeKind::Stmt {
                        node_kind: NodeKind::ToolCall { .. }
                    }
            );
            facts.insert(
                &node.id,
                Facts {
                    completed: ordinary
                        && node.status == NodeStatus::Ok
                        && !matches!(
                            node.approval,
                            Some(
                                atman_runtime::workflow::ApprovalState::Pending { .. }
                                    | atman_runtime::workflow::ApprovalState::Denied { .. }
                            )
                        )
                        && node
                            .children
                            .iter()
                            .all(|child| facts[child.id.as_str()].completed),
                    selected: selected == Some(node.id.as_str())
                        || node
                            .children
                            .iter()
                            .any(|child| facts[child.id.as_str()].selected),
                },
            );
        }
    }
    fn append<'a>(
        original: &'a WorkflowNode,
        state: &Presentation,
        ancestors: &[bool],
        last: bool,
        facts: &HashMap<&str, Facts>,
        out: &mut Vec<Row<'a>>,
    ) {
        let pair = (!state.structure).then(|| paired(original)).flatten();
        let node = pair.unwrap_or(original);
        out.push(Row {
            node,
            wrapper: pair.map(|_| original),
            depth: ancestors.len(),
            ancestors: ancestors.to_vec(),
            last,
            completed: Vec::new(),
            completed_open: false,
        });
        if !state.collapsed.contains(&node.id) {
            let mut next = ancestors.to_vec();
            next.push(last);
            visit(&node.children, state, &next, facts, out);
        }
    }
    fn visit<'a>(
        nodes: &'a [WorkflowNode],
        state: &Presentation,
        ancestors: &[bool],
        facts: &HashMap<&str, Facts>,
        out: &mut Vec<Row<'a>>,
    ) {
        let mut index = 0;
        while let Some(original) = nodes.get(index) {
            if !state.structure && facts[original.id.as_str()].completed {
                let count = nodes[index..]
                    .iter()
                    .take_while(|node| facts[node.id.as_str()].completed)
                    .count();
                if count >= 3 {
                    let members = &nodes[index..index + count];
                    let last = index + count == nodes.len();
                    let open = state
                        .completed
                        .get(&original.id)
                        .copied()
                        .unwrap_or_else(|| {
                            members.iter().any(|node| facts[node.id.as_str()].selected)
                        });
                    out.push(Row {
                        node: original,
                        wrapper: None,
                        depth: ancestors.len(),
                        ancestors: ancestors.to_vec(),
                        last,
                        completed: members.iter().collect(),
                        completed_open: open,
                    });
                    if open {
                        let mut next = ancestors.to_vec();
                        next.push(last);
                        for (member, node) in members.iter().enumerate() {
                            append(node, state, &next, member + 1 == count, facts, out);
                        }
                    }
                    index += count;
                    continue;
                }
            }
            append(
                original,
                state,
                ancestors,
                index + 1 == nodes.len(),
                facts,
                out,
            );
            index += 1;
        }
    }
    let mut facts = HashMap::new();
    classify(nodes, state.selected.as_deref(), &mut facts);
    let mut out = Vec::new();
    visit(nodes, state, &[], &facts, &mut out);
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
        state.apply(Action::Reveal("a".into(), true), None);
        let revealed = rows(&nodes, &state);
        assert_eq!(revealed.len(), 5);
        state.apply(Action::Reveal("a".into(), false), None);
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
        assert_eq!(rows(&nodes, &state).len(), 2);
        nodes[1].approval = Some(atman_runtime::workflow::ApprovalState::Pending {
            level: "user".into(),
            preview: None,
        });
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
        assert!(paired(&wrapper).is_some());
        if let WorkflowNodeKind::ToolCall { result_preview, .. } = &mut wrapper.children[0].kind {
            *result_preview = Some("different result".into());
        }
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

    #[test]
    fn completed_projection_keeps_selected_calls_results_and_exception_subtrees() {
        let mut nodes = (0..4)
            .map(|index| {
                let mut call = tool(&format!("call-{index}"));
                call.approval = Some(atman_runtime::workflow::ApprovalState::Approved);
                let mut statement = tool(&format!("stmt-{index}"));
                statement.kind = WorkflowNodeKind::Stmt {
                    node_kind: NodeKind::ToolCall {
                        path: "fs.read".into(),
                    },
                };
                statement.output_preview = Some(format!("returned-{index}"));
                statement.children = vec![call];
                statement
            })
            .collect::<Vec<_>>();
        let original = nodes.clone();
        let mut state = Presentation {
            selected: Some("call-1".into()),
            ..Default::default()
        };
        let visible = rows(&nodes, &state);
        assert_eq!(visible[0].completed.len(), 4);
        assert!(visible[0].completed_open);
        let selected = visible.iter().find(|row| row.node.id == "call-1").unwrap();
        assert_eq!(
            result_preview(selected.node, selected.wrapper),
            Some("returned-1")
        );
        let close = Action::Reveal("stmt-0".into(), false);
        assert_eq!(Action::parse(&close.key()), Some(close.clone()));
        state.apply(close, None);
        assert_eq!(rows(&nodes, &state).len(), 1);
        assert_eq!(state.selected.as_deref(), Some("call-1"));
        assert_eq!(nodes, original);
        nodes[2].children[0].status = NodeStatus::Err;
        assert!(
            rows(&nodes, &state)
                .iter()
                .any(|row| row.node.id == "call-2")
        );
        state.structure = true;
        let original_ids = nodes
            .iter()
            .flat_map(|node| {
                std::iter::once(node.id.as_str())
                    .chain(node.children.iter().map(|child| child.id.as_str()))
            })
            .collect::<HashSet<_>>();
        assert_eq!(
            rows(&nodes, &state)
                .iter()
                .map(|row| row.node.id.as_str())
                .collect::<HashSet<_>>(),
            original_ids
        );
    }
}
