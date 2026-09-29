use atman_rt::ast::{Arg, Expr, FlowDecl, FlowRef, Ident, Node, Stmt};
use atman_rt::{VmNode, VmNodeKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FlowGraph {
    pub flow_name: String,
    pub root: Vec<StaticNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StaticNode {
    pub node_id: String,
    pub kind: NodeKind,
    pub label: String,
    pub children: Vec<StaticNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeKind {
    Llm { model: Option<String> },
    ToolCall { path: String },
    Fanout,
    UserConfirm,
    FlowFuture { name: String },
    FlowAwait { target: String },
    Subflow { name: String },
    Message { role: String },
    FixUntilTest,
    When { condition_preview: String },
    Loop,
    Yield,
    Return,
}

impl From<&VmNodeKind> for NodeKind {
    fn from(kind: &VmNodeKind) -> Self {
        match kind {
            VmNodeKind::Llm { model } => Self::Llm {
                model: model.clone(),
            },
            VmNodeKind::ToolCall { path } => Self::ToolCall { path: path.clone() },
            VmNodeKind::Fanout => Self::Fanout,
            VmNodeKind::UserConfirm => Self::UserConfirm,
            VmNodeKind::FlowFuture { name } => Self::FlowFuture { name: name.clone() },
            VmNodeKind::FlowAwait { target } => Self::FlowAwait {
                target: target.clone(),
            },
            VmNodeKind::Subflow { name } => Self::Subflow { name: name.clone() },
            VmNodeKind::Message { role } => Self::Message { role: role.clone() },
            VmNodeKind::FixUntilTest => Self::FixUntilTest,
            VmNodeKind::When { condition_preview } => Self::When {
                condition_preview: condition_preview.clone(),
            },
            VmNodeKind::Loop => Self::Loop,
            VmNodeKind::Yield => Self::Yield,
            VmNodeKind::Return => Self::Return,
        }
    }
}

impl From<VmNodeKind> for NodeKind {
    fn from(kind: VmNodeKind) -> Self {
        Self::from(&kind)
    }
}

impl From<&VmNode> for NodeKind {
    fn from(node: &VmNode) -> Self {
        Self::from(&node.kind)
    }
}

impl From<VmNode> for NodeKind {
    fn from(node: VmNode) -> Self {
        Self::from(node.kind)
    }
}

pub fn vm_node_kind_label(node: &VmNode) -> (NodeKind, String) {
    (NodeKind::from(node), node.label.clone())
}

pub fn extract_graph(flow: &FlowDecl) -> FlowGraph {
    let mut root = Vec::new();
    for (i, stmt) in flow.body.iter().enumerate() {
        extract_stmt(stmt, &format!("{i}"), &mut root);
    }
    FlowGraph {
        flow_name: flow.name.name.clone(),
        root,
    }
}

fn extract_stmt(stmt: &Stmt, prefix: &str, out: &mut Vec<StaticNode>) {
    match stmt {
        Stmt::Bind { value, .. } => extract_expr(value, prefix, out),
        Stmt::Expr(expr) => extract_expr(expr, prefix, out),
        Stmt::Return { value } => {
            extract_expr(value, &format!("{prefix}.v"), out);
            out.push(StaticNode {
                node_id: prefix.to_string(),
                kind: NodeKind::Return,
                label: "return".into(),
                children: Vec::new(),
            });
        }
        Stmt::When { cond, body } => {
            let mut inner = Vec::new();
            for (i, s) in body.iter().enumerate() {
                extract_stmt(s, &format!("{prefix}.{i}"), &mut inner);
            }
            out.push(StaticNode {
                node_id: prefix.to_string(),
                kind: NodeKind::When {
                    condition_preview: format_expr_short(cond),
                },
                label: format!("when {}", format_expr_short(cond)),
                children: inner,
            });
        }
        Stmt::Watch(_) => {}
        Stmt::Loop { body } => {
            let mut inner = Vec::new();
            for (i, s) in body.iter().enumerate() {
                extract_stmt(s, &format!("{prefix}.{i}"), &mut inner);
            }
            out.push(StaticNode {
                node_id: prefix.to_string(),
                kind: NodeKind::Loop,
                label: "loop".into(),
                children: inner,
            });
        }
        Stmt::Yield => out.push(StaticNode {
            node_id: prefix.to_string(),
            kind: NodeKind::Yield,
            label: "yield".into(),
            children: Vec::new(),
        }),
        Stmt::Break => {}
        Stmt::Continue => {}
    }
}

fn extract_expr(expr: &Expr, prefix: &str, out: &mut Vec<StaticNode>) {
    match expr {
        Expr::Node(node) => extract_node(node, prefix, out),
        Expr::Await { value } => {
            let (kind, children) = match value.as_ref() {
                Expr::Node(Node::FlowCall { name, .. }) => (
                    NodeKind::Subflow {
                        name: name.display_name(),
                    },
                    Vec::new(),
                ),
                _ => {
                    let mut children = Vec::new();
                    extract_expr(value, &format!("{prefix}.value"), &mut children);
                    (
                        NodeKind::FlowAwait {
                            target: format_expr_short(value),
                        },
                        children,
                    )
                }
            };
            out.push(StaticNode {
                node_id: prefix.to_string(),
                kind,
                label: format_expr_short(expr),
                children,
            });
        }
        _ => {
            if let Some(effect) = first_effect_expr(expr) {
                extract_expr(effect, prefix, out);
            }
        }
    }
}

pub(crate) fn first_effect_expr(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Node(_) | Expr::Await { .. } => Some(expr),
        Expr::Member { base, .. }
        | Expr::Unary { operand: base, .. }
        | Expr::Annotated { expr: base, .. } => first_effect_expr(base),
        Expr::Index { base, index }
        | Expr::Binary {
            left: base,
            right: index,
            ..
        } => first_effect_expr(base).or_else(|| first_effect_expr(index)),
        Expr::Call { args, .. } | Expr::List(args) => args.iter().find_map(first_effect_expr),
        Expr::Struct(fields) => fields
            .iter()
            .find_map(|(_, value)| first_effect_expr(value)),
        Expr::Literal(_) | Expr::Ident(_) | Expr::FileRef(_) | Expr::Lambda { .. } => None,
    }
}

fn flow_call_label(name: &FlowRef, args: &[Arg]) -> String {
    format!(
        "{}({})",
        name.display_name(),
        if args.is_empty() { "" } else { "…" }
    )
}

fn extract_node(node: &Node, prefix: &str, out: &mut Vec<StaticNode>) {
    let (kind, label, children) = match node {
        Node::ToolCall { path, args }
            if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
        {
            let model = args.iter().find_map(|a| match a {
                Arg::Named { name, value } if name.name == "model" => {
                    if let Expr::Literal(atman_rt::ast::Literal::Str(s)) = value {
                        Some(s.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            });
            (NodeKind::Llm { model }, "llm.call".into(), Vec::new())
        }
        Node::ToolCall { path, .. } => {
            let path_str = path
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(".");
            let label = format!("⟶ {path_str}");
            (NodeKind::ToolCall { path: path_str }, label, Vec::new())
        }
        Node::DynamicFanout { source, lambda } => {
            let mut branch_children = Vec::new();
            extract_expr(source, &format!("{prefix}.source"), &mut branch_children);
            extract_expr(lambda, &format!("{prefix}.lambda"), &mut branch_children);
            (NodeKind::Fanout, "fanout (dynamic)".into(), branch_children)
        }
        Node::Fanout { source } => {
            let mut branch_children = Vec::new();
            let label = if let Expr::List(items) = source.as_ref() {
                for (i, item) in items.iter().enumerate() {
                    extract_expr(item, &format!("{prefix}.branch[{i}]"), &mut branch_children);
                }
                format!("fanout ×{}", items.len())
            } else {
                extract_expr(source, &format!("{prefix}.source"), &mut branch_children);
                "fanout".into()
            };
            (NodeKind::Fanout, label, branch_children)
        }
        Node::UserConfirm { .. } => (NodeKind::UserConfirm, "user_confirm".into(), Vec::new()),
        Node::FlowCall { name, args } => (
            NodeKind::FlowFuture {
                name: name.display_name(),
            },
            flow_call_label(name, args),
            Vec::new(),
        ),
        Node::Message { role, args } => {
            let role_str = match role {
                atman_rt::ast::MessageRole::User => "user",
                atman_rt::ast::MessageRole::Assistant => "assistant",
                atman_rt::ast::MessageRole::System => "system",
                atman_rt::ast::MessageRole::Tool => "tool",
            };
            let _ = args;
            (
                NodeKind::Message {
                    role: role_str.into(),
                },
                format!("{role_str}_msg"),
                Vec::new(),
            )
        }
        Node::FixUntilTestPasses { .. } => {
            (NodeKind::FixUntilTest, "fix_until_test".into(), Vec::new())
        }
    };
    out.push(StaticNode {
        node_id: prefix.to_string(),
        kind,
        label,
        children,
    });
}

pub fn format_expr_short(expr: &Expr) -> String {
    match expr {
        Expr::Await { value } => format!("{}.await", format_expr_short(value)),
        Expr::Index { base, index } => {
            format!("{}[{}]", format_expr_short(base), format_expr_short(index))
        }
        Expr::Node(Node::FlowCall { name, args }) => flow_call_label(name, args),
        Expr::Literal(atman_rt::ast::Literal::Bool(b)) => b.to_string(),
        Expr::Literal(atman_rt::ast::Literal::Str(s)) => format!("\"{s}\""),
        Expr::Literal(atman_rt::ast::Literal::Int(i)) => i.to_string(),
        Expr::Literal(atman_rt::ast::Literal::Float(f)) => f.to_string(),
        Expr::Ident(id) => id.name.clone(),
        Expr::Member { base, field } => format!("{}.{}", format_expr_short(base), field.name),
        Expr::Binary { op, left, right } => {
            let sym = match op {
                atman_rt::ast::BinOp::Eq => "==",
                atman_rt::ast::BinOp::Ne => "!=",
                atman_rt::ast::BinOp::Lt => "<",
                atman_rt::ast::BinOp::Le => "<=",
                atman_rt::ast::BinOp::Gt => ">",
                atman_rt::ast::BinOp::Ge => ">=",
                atman_rt::ast::BinOp::And => "and",
                atman_rt::ast::BinOp::Or => "or",
                atman_rt::ast::BinOp::Add => "+",
                atman_rt::ast::BinOp::Sub => "-",
                atman_rt::ast::BinOp::Mul => "*",
                atman_rt::ast::BinOp::Div => "/",
                atman_rt::ast::BinOp::Mod => "%",
            };
            format!(
                "{} {} {}",
                format_expr_short(left),
                sym,
                format_expr_short(right)
            )
        }
        _ => "…".into(),
    }
}

#[allow(dead_code)]
fn _unused_ident(_: &Ident, _: &[Arg]) {}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::ast::Span;
    use atman_rt::parse_file;

    fn parse_first_flow(src: &str) -> FlowDecl {
        let file = parse_file(src).expect("parse ok");
        file.flows.into_iter().next().expect("has flow")
    }

    #[test]
    fn maps_every_vm_node_kind_without_losing_metadata() {
        let cases = [
            (
                VmNodeKind::Llm {
                    model: Some("reasoner".into()),
                },
                NodeKind::Llm {
                    model: Some("reasoner".into()),
                },
            ),
            (
                VmNodeKind::ToolCall {
                    path: "files.read".into(),
                },
                NodeKind::ToolCall {
                    path: "files.read".into(),
                },
            ),
            (VmNodeKind::Fanout, NodeKind::Fanout),
            (VmNodeKind::UserConfirm, NodeKind::UserConfirm),
            (
                VmNodeKind::FlowFuture {
                    name: "background".into(),
                },
                NodeKind::FlowFuture {
                    name: "background".into(),
                },
            ),
            (
                VmNodeKind::FlowAwait {
                    target: "pending".into(),
                },
                NodeKind::FlowAwait {
                    target: "pending".into(),
                },
            ),
            (
                VmNodeKind::Subflow {
                    name: "worker".into(),
                },
                NodeKind::Subflow {
                    name: "worker".into(),
                },
            ),
            (
                VmNodeKind::Message {
                    role: "assistant".into(),
                },
                NodeKind::Message {
                    role: "assistant".into(),
                },
            ),
            (VmNodeKind::FixUntilTest, NodeKind::FixUntilTest),
            (
                VmNodeKind::When {
                    condition_preview: "ready == true".into(),
                },
                NodeKind::When {
                    condition_preview: "ready == true".into(),
                },
            ),
            (VmNodeKind::Loop, NodeKind::Loop),
            (VmNodeKind::Yield, NodeKind::Yield),
            (VmNodeKind::Return, NodeKind::Return),
        ];

        for (vm_kind, expected) in cases {
            assert_eq!(NodeKind::from(vm_kind), expected);
        }
    }

    #[test]
    fn maps_vm_node_and_preserves_its_label() {
        let node = VmNode {
            kind: VmNodeKind::Subflow {
                name: "worker".into(),
            },
            label: "worker(input).await".into(),
        };

        assert_eq!(
            vm_node_kind_label(&node),
            (
                NodeKind::Subflow {
                    name: "worker".into(),
                },
                "worker(input).await".into(),
            )
        );
    }

    #[test]
    fn extracts_yield_as_a_distinct_static_node() {
        let flow = parse_first_flow("flow main() { yield }");
        let graph = extract_graph(&flow);

        assert_eq!(graph.root.len(), 1);
        assert_eq!(graph.root[0].kind, NodeKind::Yield);
        assert_eq!(graph.root[0].label, "yield");
    }

    #[test]
    fn extracts_llm_only_flow() {
        let src = r#"flow smoke() -> string {
            x = llm.call(
                model: "glm",
                messages: [],
            )
            return x
        }"#;
        let flow = parse_first_flow(src);
        let g = extract_graph(&flow);
        assert_eq!(g.flow_name, "smoke");
        let kinds: Vec<_> = g.root.iter().map(|n| n.kind.clone()).collect();
        assert_eq!(
            kinds[0],
            NodeKind::Llm {
                model: Some("glm".into())
            }
        );
        assert!(matches!(kinds.last(), Some(NodeKind::Return)));
    }

    #[test]
    fn extracts_fanout_branches_from_example() {
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/look_into.at"
        ))
        .expect("example loadable");
        let file = parse_file(&src).expect("parse ok");
        let flow = file
            .flows
            .iter()
            .find(|f| f.name.name == "look_into")
            .expect("has flow");
        let g = extract_graph(flow);
        let fanout = g
            .root
            .iter()
            .find(|n| matches!(n.kind, NodeKind::Fanout))
            .expect("has fanout");
        assert!(fanout.children.len() >= 2);
    }

    #[test]
    fn distinguishes_cold_flow_calls_from_awaited_flows() {
        let call = Expr::Node(Node::FlowCall {
            name: FlowRef::Local(Ident::new("worker", Span::default())),
            args: Vec::new(),
        });
        let mut nodes = Vec::new();
        extract_expr(&call, "0", &mut nodes);
        assert_eq!(
            nodes[0].kind,
            NodeKind::FlowFuture {
                name: "worker".into()
            }
        );
        assert_eq!(nodes[0].label, "worker()");

        nodes.clear();
        extract_expr(
            &Expr::Await {
                value: Box::new(call),
            },
            "1",
            &mut nodes,
        );
        assert_eq!(
            nodes[0].kind,
            NodeKind::Subflow {
                name: "worker".into()
            }
        );
        assert_eq!(nodes[0].label, "worker().await");

        nodes.clear();
        let handle = Expr::Ident(Ident::new("pending", Span::default()));
        extract_expr(
            &Expr::Await {
                value: Box::new(handle),
            },
            "2",
            &mut nodes,
        );
        assert_eq!(
            nodes[0].kind,
            NodeKind::FlowAwait {
                target: "pending".into()
            }
        );
        assert_eq!(nodes[0].label, "pending.await");
    }

    #[test]
    fn indexes_keep_labels_and_nested_nodes() {
        let expr = Expr::Index {
            base: Box::new(Expr::Ident(Ident::new("items", Span::default()))),
            index: Box::new(Expr::Node(Node::ToolCall {
                path: vec![
                    Ident::new("cursor", Span::default()),
                    Ident::new("next", Span::default()),
                ],
                args: Vec::new(),
            })),
        };
        assert_eq!(format_expr_short(&expr), "items[…]");
        let mut nodes = Vec::new();
        extract_expr(&expr, "0", &mut nodes);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, "0");
        assert!(matches!(
            &nodes[0].kind,
            NodeKind::ToolCall { path } if path == "cursor.next"
        ));

        let member = Expr::Member {
            base: Box::new(expr),
            field: Ident::new("value", Span::default()),
        };
        nodes.clear();
        extract_expr(&member, "1", &mut nodes);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].node_id, "1");
    }

    #[test]
    fn extracts_when_body() {
        let src = r#"flow t() -> string {
            when true {
                a = llm.call(model: "m", messages: [])
            }
            return "x"
        }"#;
        let flow = parse_first_flow(src);
        let g = extract_graph(&flow);
        let when = g
            .root
            .iter()
            .find(|n| matches!(n.kind, NodeKind::When { .. }));
        assert!(when.is_some());
        assert_eq!(when.unwrap().children.len(), 1);
    }

    #[test]
    fn simple_return_only_flow() {
        let src = r#"flow t() -> string { return "hi" }"#;
        let flow = parse_first_flow(src);
        let g = extract_graph(&flow);
        assert_eq!(g.root.len(), 1);
        assert!(matches!(g.root[0].kind, NodeKind::Return));
    }
}
