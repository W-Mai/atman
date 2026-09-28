use alloc::{
    collections::BTreeSet,
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::ast::{Arg, Expr, File, FlowDecl, Node, Stmt};

const MANY_POSITIONAL_THRESHOLD: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintHit {
    pub flow: String,
    pub rule: LintRule,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintRule {
    UnusedFlowParam,
    ManyPositional,
    UnusedFlowFuture,
}

impl LintRule {
    pub fn slug(&self) -> &'static str {
        match self {
            LintRule::UnusedFlowParam => "unused-flow-param",
            LintRule::ManyPositional => "many-positional",
            LintRule::UnusedFlowFuture => "unused-flow-future",
        }
    }
}

pub fn lint_file(file: &File) -> Vec<LintHit> {
    let mut hits = Vec::new();
    for flow in &file.flows {
        lint_flow(flow, &mut hits);
    }
    hits
}

fn lint_flow(flow: &FlowDecl, hits: &mut Vec<LintHit>) {
    let mut refs = BTreeSet::new();
    collect_ident_refs_stmts(&flow.body, &mut refs);
    for p in &flow.params {
        if !refs.contains(&p.name.name) {
            hits.push(LintHit {
                flow: flow.name.name.clone(),
                rule: LintRule::UnusedFlowParam,
                message: format!(
                    "parameter `{}` is declared but never referenced",
                    p.name.name
                ),
            });
        }
    }
    walk_stmts_for_nodes(&flow.body, &flow.name.name, hits);
}

fn collect_ident_refs_stmts(stmts: &[Stmt], refs: &mut BTreeSet<String>) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { value, .. } => collect_ident_refs_expr(value, refs),
            Stmt::When { cond, body } => {
                collect_ident_refs_expr(cond, refs);
                collect_ident_refs_stmts(body, refs);
            }
            Stmt::Return { value } => collect_ident_refs_expr(value, refs),
            Stmt::Expr(e) => collect_ident_refs_expr(e, refs),
            Stmt::Watch(w) => {
                refs.insert(w.target.name.clone());
            }
            Stmt::Loop { body } => collect_ident_refs_stmts(body, refs),
            Stmt::Break => {}
            Stmt::Continue => {}
        }
    }
}

fn collect_ident_refs_expr(expr: &Expr, refs: &mut BTreeSet<String>) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) => {}
        Expr::Ident(id) => {
            refs.insert(id.name.clone());
        }
        Expr::Member { base, .. } | Expr::Await { value: base } => {
            collect_ident_refs_expr(base, refs)
        }
        Expr::Binary { left, right, .. } => {
            collect_ident_refs_expr(left, refs);
            collect_ident_refs_expr(right, refs);
        }
        Expr::Index { base, index } => {
            collect_ident_refs_expr(base, refs);
            collect_ident_refs_expr(index, refs);
        }
        Expr::Unary { operand, .. } => collect_ident_refs_expr(operand, refs),
        Expr::List(items) => {
            for it in items {
                collect_ident_refs_expr(it, refs);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                collect_ident_refs_expr(v, refs);
            }
        }
        Expr::Node(node) => collect_ident_refs_node(node, refs),
        Expr::Call { args, .. } => {
            for a in args {
                collect_ident_refs_expr(a, refs);
            }
        }
        Expr::Annotated { expr, .. } => collect_ident_refs_expr(expr, refs),
        Expr::Lambda { params, body } => {
            for p in params {
                refs.insert(p.name.clone());
            }
            collect_ident_refs_expr(body, refs);
        }
    }
}

fn collect_ident_refs_node(node: &Node, refs: &mut BTreeSet<String>) {
    match node {
        Node::ToolCall { args, .. } | Node::FlowCall { args, .. } | Node::Message { args, .. } => {
            for a in args {
                match a {
                    Arg::Positional(e) => collect_ident_refs_expr(e, refs),
                    Arg::Named { value, .. } => collect_ident_refs_expr(value, refs),
                }
            }
        }
        Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                collect_ident_refs_expr(v, refs);
            }
        }
        Node::DynamicFanout { source, lambda, .. } => {
            collect_ident_refs_expr(source, refs);
            collect_ident_refs_expr(lambda, refs);
        }
        Node::Fanout { source } => collect_ident_refs_expr(source, refs),
        Node::UserConfirm { msg } => collect_ident_refs_expr(msg, refs),
    }
}

fn walk_stmts_for_nodes(stmts: &[Stmt], flow_name: &str, hits: &mut Vec<LintHit>) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { value, .. } | Stmt::Return { value } => {
                walk_expr_for_nodes(value, flow_name, hits);
            }
            Stmt::Expr(value) => {
                if let Expr::Node(Node::FlowCall { name, .. }) = value {
                    hits.push(LintHit {
                        flow: flow_name.to_string(),
                        rule: LintRule::UnusedFlowFuture,
                        message: format!(
                            "flow call `{}` creates an unused future; add `.await` or bind it",
                            name.display_name()
                        ),
                    });
                }
                walk_expr_for_nodes(value, flow_name, hits);
            }
            Stmt::When { cond, body } => {
                walk_expr_for_nodes(cond, flow_name, hits);
                walk_stmts_for_nodes(body, flow_name, hits);
            }
            Stmt::Watch(_) => {}
            Stmt::Loop { body } => {
                walk_stmts_for_nodes(body, flow_name, hits);
            }
            Stmt::Break => {}
            Stmt::Continue => {}
        }
    }
}

fn walk_expr_for_nodes(expr: &Expr, flow_name: &str, hits: &mut Vec<LintHit>) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) | Expr::Ident(_) => {}
        Expr::Member { base, .. } | Expr::Await { value: base } => {
            walk_expr_for_nodes(base, flow_name, hits)
        }
        Expr::Binary { left, right, .. } => {
            walk_expr_for_nodes(left, flow_name, hits);
            walk_expr_for_nodes(right, flow_name, hits);
        }
        Expr::Index { base, index } => {
            walk_expr_for_nodes(base, flow_name, hits);
            walk_expr_for_nodes(index, flow_name, hits);
        }
        Expr::Unary { operand, .. } => walk_expr_for_nodes(operand, flow_name, hits),
        Expr::List(items) => {
            for it in items {
                walk_expr_for_nodes(it, flow_name, hits);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                walk_expr_for_nodes(v, flow_name, hits);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr_for_nodes(a, flow_name, hits);
            }
        }
        Expr::Node(node) => {
            check_node(node, flow_name, hits);
            for e in child_exprs(node) {
                walk_expr_for_nodes(e, flow_name, hits);
            }
        }
        Expr::Annotated { expr, .. } => walk_expr_for_nodes(expr, flow_name, hits),
        Expr::Lambda { body, .. } => walk_expr_for_nodes(body, flow_name, hits),
    }
}

fn check_node(node: &Node, flow_name: &str, hits: &mut Vec<LintHit>) {
    let (name, args) = match node {
        Node::ToolCall { path, args } => (
            path.iter()
                .map(|part| part.name.as_str())
                .collect::<Vec<_>>()
                .join("."),
            args,
        ),
        Node::FlowCall { name, args } => (name.display_name(), args),
        _ => return,
    };
    let positional = args
        .iter()
        .filter(|a| matches!(a, Arg::Positional(_)))
        .count();
    let named = args
        .iter()
        .filter(|a| matches!(a, Arg::Named { .. }))
        .count();
    if positional >= MANY_POSITIONAL_THRESHOLD && named == 0 {
        hits.push(LintHit {
            flow: flow_name.to_string(),
            rule: LintRule::ManyPositional,
            message: format!(
                "{name} takes {positional} positional args with no names — prefer named args for readability"
            ),
        });
    }
}

fn child_exprs(node: &Node) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    match node {
        Node::ToolCall { args, .. } | Node::FlowCall { args, .. } | Node::Message { args, .. } => {
            for a in args {
                match a {
                    Arg::Positional(e) => out.push(e),
                    Arg::Named { value, .. } => out.push(value),
                }
            }
        }
        Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                out.push(v);
            }
        }
        Node::DynamicFanout { source, lambda, .. } => {
            out.push(source);
            out.push(lambda);
        }
        Node::Fanout { source } => out.push(source),
        Node::UserConfirm { msg } => out.push(msg),
    }
    out
}

#[cfg(all(test, feature = "syntax"))]
mod tests {
    use super::*;
    use crate::parse_file;
    use alloc::vec;

    fn lint(src: &str) -> Vec<LintHit> {
        let file = parse_file(src).unwrap_or_else(|e| panic!("parse: {e}"));
        lint_file(&file)
    }

    #[test]
    fn llm_without_fallback_is_intentional_and_clean() {
        let src = r#"flow t() -> string {
    return llm.call(model: "mock", prompt: "hi")
}
"#;
        assert!(lint(src).is_empty());
    }

    #[test]
    fn unused_flow_param_fires() {
        let src = r#"flow t(x: int, y: int) -> int {
    return x
}
"#;
        let hits = lint(src);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule, LintRule::UnusedFlowParam);
        assert!(hits[0].message.contains("`y`"), "hit={:?}", hits[0]);
    }

    #[test]
    fn discarded_linked_flow_future_is_reported() {
        use crate::program::{LinkedProgram, ModuleId, ModuleInput};

        let source = "flow child() {}\nflow main() { child() child().await }";
        let file = parse_file(source).unwrap();
        let program = LinkedProgram::link(
            vec![ModuleInput {
                source_id: "main.at".into(),
                display_name: "main.at".into(),
                source: source.into(),
                file,
                dependencies: Default::default(),
            }],
            ModuleId(0),
        )
        .unwrap();
        let hits = lint_file(program.entry_file());
        assert_eq!(
            hits.iter()
                .filter(|hit| hit.rule == LintRule::UnusedFlowFuture)
                .count(),
            1
        );
    }

    #[test]
    fn used_params_are_clean() {
        let src = r#"flow t(x: int, y: int) -> int {
    z = x
    return z + y
}
"#;
        assert!(lint(src).is_empty());
    }

    #[test]
    fn index_operands_count_as_references() {
        let src = r#"flow t(items: [int], index: int) -> int {
    return items[index]
}
"#;
        assert!(lint(src).is_empty());
    }

    #[test]
    fn many_positional_fires_at_threshold() {
        let src = r#"flow t() -> string {
    return stdlib.compose_email_preview("s", "b", ["a"], "extra")
}
"#;
        let hits = lint(src);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule, LintRule::ManyPositional);
    }

    #[test]
    fn many_positional_with_any_named_arg_is_clean() {
        let src = r#"flow t() -> string {
    return stdlib.compose_email_preview("s", "b", to: ["a"])
}
"#;
        assert!(lint(src).is_empty());
    }

    #[test]
    fn three_positional_below_threshold_is_clean() {
        let src = r#"flow t() -> string {
    return stdlib.compose_email_preview("s", "b", ["a"])
}
"#;
        assert!(lint(src).is_empty());
    }

    #[test]
    fn multiple_hits_across_flows_reported_together() {
        let src = r#"flow a() -> string {
    return stdlib.compose_email_preview("s", "b", ["a"], "extra")
}

flow b(unused: int) -> int {
    return 1
}
"#;
        let hits = lint(src);
        assert_eq!(hits.len(), 2);
        assert!(
            hits.iter()
                .any(|h| h.flow == "a" && h.rule == LintRule::ManyPositional)
        );
        assert!(
            hits.iter()
                .any(|h| h.flow == "b" && h.rule == LintRule::UnusedFlowParam)
        );
    }

    #[test]
    fn watch_target_counts_as_reference() {
        let src = r#"flow t() -> string {
    x = llm.call(model: "m", prompt: "p")
    watch x {
        on token(match: "err") { }
    }
    return x
}
"#;
        let hits = lint(src);
        assert!(hits.is_empty(), "unexpected hits: {hits:?}");
    }
}
