use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::ast::{Arg, Expr, FlowDecl, Node, Stmt, WatchEvent};
use crate::expr::is_type_name;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageValidationError {
    UndefinedVar(String),
    WatchEventMismatch {
        target: String,
        event: String,
        target_kind: String,
        expected: String,
    },
}

impl fmt::Display for LanguageValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UndefinedVar(name) => write!(f, "undefined variable `{name}`"),
            Self::WatchEventMismatch {
                target,
                event,
                target_kind,
                expected,
            } => write!(
                f,
                "watch on `{target}` uses event `{event}`, but bind is a {target_kind} node — expected one of {expected}"
            ),
        }
    }
}

#[derive(Debug, Default)]
pub struct LanguageValidationReport {
    pub errors: Vec<LanguageValidationError>,
    pub tool_calls: Vec<String>,
}

pub fn validate_flow(flow: &FlowDecl, globals: &[&str]) -> LanguageValidationReport {
    let mut report = LanguageValidationReport::default();
    let mut scope: BTreeSet<String> = flow.params.iter().map(|p| p.name.name.clone()).collect();
    scope.extend(globals.iter().map(|name| (*name).to_string()));
    let mut kinds = BTreeMap::new();
    walk_stmts(&flow.body, &mut scope, &mut kinds, &mut report);
    report
}

fn infer_node_kind(value: &Expr) -> Option<&'static str> {
    match value {
        Expr::Node(Node::ToolCall { path, .. })
            if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
        {
            Some("llm")
        }
        Expr::Node(Node::ToolCall { .. }) => Some("tool_call"),
        Expr::Node(Node::Fanout { .. }) => Some("fanout"),
        Expr::Node(Node::UserConfirm { .. }) => Some("user_confirm"),
        Expr::Node(Node::FlowCall { .. }) => Some("flow_call"),
        Expr::Node(Node::FixUntilTestPasses { .. }) => Some("fix_until"),
        Expr::Node(Node::Message { .. }) => Some("message"),
        _ => None,
    }
}

fn watch_event_expected_kinds(event: &WatchEvent) -> &'static [&'static str] {
    match event {
        WatchEvent::Token { .. } | WatchEvent::TokensConsumed { .. } => &["llm"],
        WatchEvent::Elapsed { .. } => &["llm", "tool_call", "flow_call", "fix_until"],
    }
}

fn watch_event_label(event: &WatchEvent) -> &'static str {
    match event {
        WatchEvent::Token { .. } => "token",
        WatchEvent::TokensConsumed { .. } => "tokens_consumed",
        WatchEvent::Elapsed { .. } => "elapsed",
    }
}

fn walk_stmts(
    stmts: &[Stmt],
    scope: &mut BTreeSet<String>,
    kinds: &mut BTreeMap<String, &'static str>,
    report: &mut LanguageValidationReport,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { name, value } => {
                walk_expr(value, scope, report);
                if let Some(kind) = infer_node_kind(value)
                    && let Some(single) = name.as_single_ident()
                {
                    kinds.insert(single.name.clone(), kind);
                }
                scope.extend(name.bound_names());
            }
            Stmt::When { cond, body } => {
                walk_expr(cond, scope, report);
                walk_stmts(body, scope, kinds, report);
            }
            Stmt::Return { value } | Stmt::Expr(value) => walk_expr(value, scope, report),
            Stmt::Watch(watch) => {
                if !scope.contains(&watch.target.name) {
                    report.errors.push(LanguageValidationError::UndefinedVar(
                        watch.target.name.clone(),
                    ));
                    continue;
                }
                let Some(target_kind) = kinds.get(&watch.target.name).copied() else {
                    continue;
                };
                for on in &watch.on_blocks {
                    let expected = watch_event_expected_kinds(&on.event);
                    if !expected.contains(&target_kind) {
                        report
                            .errors
                            .push(LanguageValidationError::WatchEventMismatch {
                                target: watch.target.name.clone(),
                                event: watch_event_label(&on.event).into(),
                                target_kind: target_kind.into(),
                                expected: expected.join(", "),
                            });
                    }
                }
            }
            Stmt::Loop { body } => walk_stmts(body, scope, kinds, report),
            Stmt::Break | Stmt::Continue => {}
        }
    }
}

fn walk_expr(expr: &Expr, scope: &BTreeSet<String>, report: &mut LanguageValidationReport) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) => {}
        Expr::Ident(id) => {
            if !scope.contains(&id.name) {
                report
                    .errors
                    .push(LanguageValidationError::UndefinedVar(id.name.clone()));
            }
        }
        Expr::Member { base, .. } | Expr::Await { value: base } => walk_expr(base, scope, report),
        Expr::Binary { left, right, .. } => {
            walk_expr(left, scope, report);
            walk_expr(right, scope, report);
        }
        Expr::Unary { operand, .. } => walk_expr(operand, scope, report),
        Expr::List(items) => {
            for item in items {
                walk_expr(item, scope, report);
            }
        }
        Expr::Struct(fields) => {
            for (_, value) in fields {
                walk_expr(value, scope, report);
            }
        }
        Expr::Node(node) => walk_node(node, scope, report),
        Expr::Call { args, .. } => {
            for arg in args {
                walk_expr(arg, scope, report);
            }
        }
        Expr::Lambda { params, body } => {
            let mut child_scope = scope.clone();
            child_scope.extend(params.iter().map(|param| param.name.clone()));
            walk_expr(body, &child_scope, report);
        }
        Expr::Annotated { expr, .. } => match expr.as_ref() {
            Expr::Ident(id) if is_type_name(&id.name) => {}
            Expr::List(inner) if inner.len() == 1 => {
                if let Expr::Ident(id) = &inner[0]
                    && is_type_name(&id.name)
                {
                    return;
                }
                walk_expr(expr, scope, report);
            }
            _ => walk_expr(expr, scope, report),
        },
    }
}

fn walk_args(args: &[Arg], scope: &BTreeSet<String>, report: &mut LanguageValidationReport) {
    for arg in args {
        match arg {
            Arg::Positional(value) | Arg::Named { value, .. } => {
                walk_expr(value, scope, report);
            }
        }
    }
}

fn walk_node(node: &Node, scope: &BTreeSet<String>, report: &mut LanguageValidationReport) {
    match node {
        Node::ToolCall { path, args } => {
            report.tool_calls.push(
                path.iter()
                    .map(|part| part.name.as_str())
                    .collect::<Vec<_>>()
                    .join("."),
            );
            walk_args(args, scope, report);
        }
        Node::DynamicFanout { source, lambda, .. } => {
            walk_expr(source, scope, report);
            walk_expr(lambda, scope, report);
        }
        Node::Fanout { source } => walk_expr(source, scope, report),
        Node::UserConfirm { msg } => walk_expr(msg, scope, report),
        Node::FlowCall { args, .. } | Node::Message { args, .. } => {
            walk_args(args, scope, report);
        }
        Node::FixUntilTestPasses { kwargs } => {
            for (_, value) in kwargs {
                walk_expr(value, scope, report);
            }
        }
    }
}

#[cfg(all(test, feature = "syntax"))]
mod tests {
    use super::*;

    #[test]
    fn validates_host_globals_without_a_tool_registry() {
        let file = crate::parse_file(
            "flow main() -> string { value = foreign.echo(external) return value }",
        )
        .unwrap();
        let report = validate_flow(&file.flows[0], &["external"]);
        assert!(report.errors.is_empty());
        assert_eq!(report.tool_calls, ["foreign.echo"]);
    }
}
