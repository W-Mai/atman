use std::collections::HashSet;

use atman_dsl::ast::{Arg, Expr, File, FlowDecl, Node, Stmt};

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
}

impl LintRule {
    pub fn slug(&self) -> &'static str {
        match self {
            LintRule::UnusedFlowParam => "unused-flow-param",
            LintRule::ManyPositional => "many-positional",
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
    let mut refs = HashSet::new();
    collect_ident_refs_stmts(&flow.body, &mut refs);
    for (param, _) in &flow.params {
        if !refs.contains(&param.name) {
            hits.push(LintHit {
                flow: flow.name.name.clone(),
                rule: LintRule::UnusedFlowParam,
                message: format!(
                    "parameter `{}` is declared but never referenced",
                    param.name
                ),
            });
        }
    }
    walk_stmts_for_nodes(&flow.body, &flow.name.name, hits);
}

fn collect_ident_refs_stmts(stmts: &[Stmt], refs: &mut HashSet<String>) {
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
        }
    }
}

fn collect_ident_refs_expr(expr: &Expr, refs: &mut HashSet<String>) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) => {}
        Expr::Ident(id) => {
            refs.insert(id.name.clone());
        }
        Expr::Member { base, .. } => collect_ident_refs_expr(base, refs),
        Expr::Binary { left, right, .. } => {
            collect_ident_refs_expr(left, refs);
            collect_ident_refs_expr(right, refs);
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
        Expr::Pipe { lhs, rhs } => {
            collect_ident_refs_expr(lhs, refs);
            collect_ident_refs_expr(rhs, refs);
        }
    }
}

fn collect_ident_refs_node(node: &Node, refs: &mut HashSet<String>) {
    match node {
        Node::ToolCall { args, .. } | Node::Subflow { args, .. } | Node::Message { args, .. } => {
            for a in args {
                match a {
                    Arg::Positional(e) => collect_ident_refs_expr(e, refs),
                    Arg::Named { value, .. } => collect_ident_refs_expr(value, refs),
                }
            }
        }
        Node::Llm { kwargs } | Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                collect_ident_refs_expr(v, refs);
            }
        }
        Node::Fanout { items, .. } => {
            for it in items {
                collect_ident_refs_expr(it, refs);
            }
        }
        Node::UserConfirm { msg } => collect_ident_refs_expr(msg, refs),
    }
}

fn walk_stmts_for_nodes(stmts: &[Stmt], flow_name: &str, hits: &mut Vec<LintHit>) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { value, .. } | Stmt::Return { value } | Stmt::Expr(value) => {
                walk_expr_for_nodes(value, flow_name, hits);
            }
            Stmt::When { cond, body } => {
                walk_expr_for_nodes(cond, flow_name, hits);
                walk_stmts_for_nodes(body, flow_name, hits);
            }
            Stmt::Watch(_) => {}
        }
    }
}

fn walk_expr_for_nodes(expr: &Expr, flow_name: &str, hits: &mut Vec<LintHit>) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) | Expr::Ident(_) => {}
        Expr::Member { base, .. } => walk_expr_for_nodes(base, flow_name, hits),
        Expr::Binary { left, right, .. } => {
            walk_expr_for_nodes(left, flow_name, hits);
            walk_expr_for_nodes(right, flow_name, hits);
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
        Expr::Pipe { lhs, rhs } => {
            walk_expr_for_nodes(lhs, flow_name, hits);
            walk_expr_for_nodes(rhs, flow_name, hits);
        }
        Expr::Node(node) => {
            check_node(node, flow_name, hits);
            for e in child_exprs(node) {
                walk_expr_for_nodes(e, flow_name, hits);
            }
        }
    }
}

fn check_node(node: &Node, flow_name: &str, hits: &mut Vec<LintHit>) {
    if let Node::ToolCall { path, args } = node {
        let positional = args
            .iter()
            .filter(|a| matches!(a, Arg::Positional(_)))
            .count();
        let named = args
            .iter()
            .filter(|a| matches!(a, Arg::Named { .. }))
            .count();
        if positional >= MANY_POSITIONAL_THRESHOLD && named == 0 {
            let name = path
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            hits.push(LintHit {
                flow: flow_name.to_string(),
                rule: LintRule::ManyPositional,
                message: format!(
                    "{name} takes {positional} positional args with no names — prefer named args for readability"
                ),
            });
        }
    }
}

fn child_exprs(node: &Node) -> Vec<&Expr> {
    let mut out: Vec<&Expr> = Vec::new();
    match node {
        Node::ToolCall { args, .. } | Node::Subflow { args, .. } | Node::Message { args, .. } => {
            for a in args {
                match a {
                    Arg::Positional(e) => out.push(e),
                    Arg::Named { value, .. } => out.push(value),
                }
            }
        }
        Node::Llm { kwargs } | Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                out.push(v);
            }
        }
        Node::Fanout { items, .. } => {
            for i in items {
                out.push(i);
            }
        }
        Node::UserConfirm { msg } => out.push(msg),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    fn lint(src: &str) -> Vec<LintHit> {
        let file = parse_file(src).unwrap_or_else(|e| panic!("parse: {e}"));
        lint_file(&file)
    }

    #[test]
    fn llm_without_fallback_is_intentional_and_clean() {
        let src = r#"flow t() -> string {
    return llm { model: "mock", prompt: "hi" }
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
    fn used_params_are_clean() {
        let src = r#"flow t(x: int, y: int) -> int {
    z = x
    return z + y
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
    x = llm { model: "m", prompt: "p" }
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
