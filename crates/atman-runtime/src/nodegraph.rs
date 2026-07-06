use atman_dsl::ast::{Arg, Expr, FanoutCollect, FlowDecl, Ident, Node, Stmt};
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
    Fanout { collect: FanoutMode },
    UserConfirm,
    Subflow { name: String },
    Message { role: String },
    FixUntilTest,
    When { condition_preview: String },
    Return,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FanoutMode {
    All,
    First,
}

impl From<FanoutCollect> for FanoutMode {
    fn from(c: FanoutCollect) -> Self {
        match c {
            FanoutCollect::All => FanoutMode::All,
            FanoutCollect::First => FanoutMode::First,
        }
    }
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
    }
}

fn extract_expr(expr: &Expr, prefix: &str, out: &mut Vec<StaticNode>) {
    match expr {
        Expr::Node(node) => extract_node(node, prefix, out),
        Expr::Pipe { lhs, rhs } => {
            extract_expr(lhs, &format!("{prefix}.l"), out);
            extract_expr(rhs, &format!("{prefix}.r"), out);
        }
        _ => {}
    }
}

fn extract_node(node: &Node, prefix: &str, out: &mut Vec<StaticNode>) {
    let (kind, label, children) = match node {
        Node::Llm { kwargs } => {
            let model = kwargs
                .iter()
                .find(|(k, _)| k.name == "model")
                .and_then(|(_, v)| match v {
                    Expr::Literal(atman_dsl::ast::Literal::Str(s)) => Some(s.clone()),
                    _ => None,
                });
            let label = match &model {
                Some(m) => format!("llm({m})"),
                None => "llm".to_string(),
            };
            (NodeKind::Llm { model }, label, Vec::new())
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
        Node::Fanout { items, collect } => {
            let mut branch_children = Vec::new();
            for (i, item) in items.iter().enumerate() {
                extract_expr(item, &format!("{prefix}.branch[{i}]"), &mut branch_children);
            }
            let label = format!("fanout ×{}", items.len());
            (
                NodeKind::Fanout {
                    collect: (*collect).into(),
                },
                label,
                branch_children,
            )
        }
        Node::UserConfirm { .. } => (NodeKind::UserConfirm, "user_confirm".into(), Vec::new()),
        Node::Subflow { name, args } => {
            let label = format!(
                "subflow({}{})",
                name.name,
                if args.is_empty() { "" } else { ", …" }
            );
            (
                NodeKind::Subflow {
                    name: name.name.clone(),
                },
                label,
                Vec::new(),
            )
        }
        Node::Message { role, args } => {
            let role_str = match role {
                atman_dsl::ast::MessageRole::User => "user",
                atman_dsl::ast::MessageRole::Assistant => "assistant",
                atman_dsl::ast::MessageRole::System => "system",
                atman_dsl::ast::MessageRole::Tool => "tool",
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

fn format_expr_short(expr: &Expr) -> String {
    match expr {
        Expr::Literal(atman_dsl::ast::Literal::Bool(b)) => b.to_string(),
        Expr::Literal(atman_dsl::ast::Literal::Str(s)) => format!("\"{s}\""),
        Expr::Literal(atman_dsl::ast::Literal::Int(i)) => i.to_string(),
        Expr::Literal(atman_dsl::ast::Literal::Float(f)) => f.to_string(),
        Expr::Ident(id) => id.name.clone(),
        Expr::Binary { op, left, right } => {
            let sym = match op {
                atman_dsl::ast::BinOp::Eq => "==",
                atman_dsl::ast::BinOp::Ne => "!=",
                atman_dsl::ast::BinOp::Lt => "<",
                atman_dsl::ast::BinOp::Le => "<=",
                atman_dsl::ast::BinOp::Gt => ">",
                atman_dsl::ast::BinOp::Ge => ">=",
                atman_dsl::ast::BinOp::And => "and",
                atman_dsl::ast::BinOp::Or => "or",
                atman_dsl::ast::BinOp::Add => "+",
                atman_dsl::ast::BinOp::Sub => "-",
                atman_dsl::ast::BinOp::Mul => "*",
                atman_dsl::ast::BinOp::Div => "/",
                atman_dsl::ast::BinOp::Mod => "%",
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
    use atman_dsl::parse::parse_file;

    fn parse_first_flow(src: &str) -> FlowDecl {
        let file = parse_file(src).expect("parse ok");
        file.flows.into_iter().next().expect("has flow")
    }

    #[test]
    fn extracts_llm_only_flow() {
        let src = r#"flow smoke() -> string {
            x = llm {
                model: "glm"
                messages: []
            }
            return x
        }"#;
        let flow = parse_first_flow(src);
        let g = extract_graph(&flow);
        assert_eq!(g.flow_name, "smoke");
        let kinds: Vec<_> = g.root.iter().map(|n| n.kind.clone()).collect();
        assert!(matches!(kinds[0], NodeKind::Llm { .. }));
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
            .find(|n| matches!(n.kind, NodeKind::Fanout { .. }))
            .expect("has fanout");
        assert!(fanout.children.len() >= 2);
    }

    #[test]
    fn extracts_when_body() {
        let src = r#"flow t() -> string {
            when true {
                a = llm { model: "m" messages: [] }
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
