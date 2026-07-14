use std::collections::{HashMap, HashSet};

use atman_dsl::ast::{Arg, Expr, FlowDecl, Node, Stmt, WatchEvent};

use crate::tool::ToolRegistry;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("undefined variable `{0}`")]
    UndefinedVar(String),

    #[error("undefined tool `{0}`")]
    UndefinedTool(String),

    #[error(
        "watch on `{target}` uses event `{event}`, but bind is a {target_kind} node — expected one of {expected}"
    )]
    WatchEventMismatch {
        target: String,
        event: String,
        target_kind: String,
        expected: String,
    },
}

pub fn validate(flow: &FlowDecl, tools: &ToolRegistry) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut scope: HashSet<String> = flow.params.iter().map(|(id, _)| id.name.clone()).collect();
    let mut kinds: HashMap<String, &'static str> = HashMap::new();
    walk_stmts(&flow.body, &mut scope, &mut kinds, tools, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn infer_node_kind(value: &Expr) -> Option<&'static str> {
    match value {
        Expr::Node(Node::Llm { .. }) => Some("llm"),
        Expr::Node(Node::ToolCall { path, .. }) => {
            let _ = path;
            Some("tool_call")
        }
        Expr::Node(Node::Fanout { .. }) => Some("fanout"),
        Expr::Node(Node::UserConfirm { .. }) => Some("user_confirm"),
        Expr::Node(Node::Subflow { .. }) => Some("subflow"),
        Expr::Node(Node::FixUntilTestPasses { .. }) => Some("fix_until"),
        Expr::Node(Node::Message { .. }) => Some("message"),
        _ => None,
    }
}

fn watch_event_expected_kinds(event: &WatchEvent) -> &'static [&'static str] {
    match event {
        WatchEvent::Token { .. } => &["llm"],
        WatchEvent::TokensConsumed { .. } => &["llm"],
        WatchEvent::Elapsed { .. } => &["llm", "tool_call", "subflow", "fix_until"],
    }
}

fn walk_stmts(
    stmts: &[Stmt],
    scope: &mut HashSet<String>,
    kinds: &mut HashMap<String, &'static str>,
    tools: &ToolRegistry,
    errors: &mut Vec<ValidationError>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { name, value } => {
                walk_expr(value, scope, tools, errors);
                let bound = name.bound_names();
                if let Some(k) = infer_node_kind(value)
                    && let Some(single) = name.as_single_ident()
                {
                    kinds.insert(single.name.clone(), k);
                }
                for n in bound {
                    scope.insert(n);
                }
            }
            Stmt::When { cond, body } => {
                walk_expr(cond, scope, tools, errors);
                walk_stmts(body, scope, kinds, tools, errors);
            }
            Stmt::Return { value } => walk_expr(value, scope, tools, errors),
            Stmt::Expr(e) => walk_expr(e, scope, tools, errors),
            Stmt::Watch(w) => {
                if !scope.contains(&w.target.name) {
                    errors.push(ValidationError::UndefinedVar(w.target.name.clone()));
                    continue;
                }
                let Some(target_kind) = kinds.get(&w.target.name).copied() else {
                    continue;
                };
                for on in &w.on_blocks {
                    let expected = watch_event_expected_kinds(&on.event);
                    if !expected.contains(&target_kind) {
                        errors.push(ValidationError::WatchEventMismatch {
                            target: w.target.name.clone(),
                            event: watch_event_label(&on.event).into(),
                            target_kind: target_kind.into(),
                            expected: expected.join(", "),
                        });
                    }
                }
            }
        }
    }
}

fn watch_event_label(event: &WatchEvent) -> &'static str {
    match event {
        WatchEvent::Token { .. } => "token",
        WatchEvent::TokensConsumed { .. } => "tokens_consumed",
        WatchEvent::Elapsed { .. } => "elapsed",
    }
}

fn walk_expr(
    expr: &Expr,
    scope: &HashSet<String>,
    tools: &ToolRegistry,
    errors: &mut Vec<ValidationError>,
) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) => {}
        Expr::Ident(id) => {
            if !scope.contains(&id.name) {
                errors.push(ValidationError::UndefinedVar(id.name.clone()));
            }
        }
        Expr::Member { base, .. } => walk_expr(base, scope, tools, errors),
        Expr::Binary { left, right, .. } => {
            walk_expr(left, scope, tools, errors);
            walk_expr(right, scope, tools, errors);
        }
        Expr::Unary { operand, .. } => walk_expr(operand, scope, tools, errors),
        Expr::List(items) => {
            for item in items {
                walk_expr(item, scope, tools, errors);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                walk_expr(v, scope, tools, errors);
            }
        }
        Expr::Node(node) => walk_node(node, scope, tools, errors),
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr(a, scope, tools, errors);
            }
        }
        Expr::Pipe { lhs, rhs } => {
            walk_expr(lhs, scope, tools, errors);
            walk_expr(rhs, scope, tools, errors);
        }
    }
}

fn walk_node(
    node: &Node,
    scope: &HashSet<String>,
    tools: &ToolRegistry,
    errors: &mut Vec<ValidationError>,
) {
    match node {
        Node::ToolCall { path, args } => {
            let name = path
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            if !tools.has(&name) {
                errors.push(ValidationError::UndefinedTool(name));
            }
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, tools, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, tools, errors),
                }
            }
        }
        Node::Llm { kwargs } => {
            for (_, v) in kwargs {
                walk_expr(v, scope, tools, errors);
            }
        }
        Node::Fanout { items, .. } => {
            for item in items {
                walk_expr(item, scope, tools, errors);
            }
        }
        Node::UserConfirm { msg } => walk_expr(msg, scope, tools, errors),
        Node::Subflow { args, .. } => {
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, tools, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, tools, errors),
                }
            }
        }
        Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                walk_expr(v, scope, tools, errors);
            }
        }
        Node::Message { args, .. } => {
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, tools, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, tools, errors),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;
    use atman_dsl::parse::parse_file;

    fn registry_with_fs() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        tools::register_tier_zero(&mut reg);
        reg
    }

    #[test]
    fn valid_flow_using_declared_var_and_registered_tool() {
        let src = r#"flow t(p: path) -> string {
    body = fs.read(p)
    return body
}
"#;
        let file = parse_file(src).unwrap();
        validate(&file.flows[0], &registry_with_fs()).expect("valid flow");
    }

    #[test]
    fn undefined_var_is_reported() {
        let src = r#"flow t() -> Int {
    return missing
}
"#;
        let file = parse_file(src).unwrap();
        let errs = validate(&file.flows[0], &registry_with_fs()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::UndefinedVar(name) if name == "missing"))
        );
    }

    #[test]
    fn undefined_tool_is_reported() {
        let src = r#"flow t(p: path) -> Int {
    return fs.nope(p)
}
"#;
        let file = parse_file(src).unwrap();
        let errs = validate(&file.flows[0], &registry_with_fs()).unwrap_err();
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::UndefinedTool(name) if name == "fs.nope"))
        );
    }

    #[test]
    fn errors_accumulate_not_fail_fast() {
        let src = r#"flow t() -> Int {
    x = nope1
    y = nope2.tool()
    return x
}
"#;
        let file = parse_file(src).unwrap();
        let errs = validate(&file.flows[0], &registry_with_fs()).unwrap_err();
        assert!(errs.len() >= 2);
    }

    #[test]
    fn watch_on_llm_bind_with_token_event_is_ok() {
        let src = r#"flow r() -> string {
    x = llm { model: "m", prompt: "hi" }
    watch x { on token(match: "bad") { abort("no") } }
    return x
}
"#;
        let file = parse_file(src).unwrap();
        validate(&file.flows[0], &registry_with_fs()).expect("token on llm is fine");
    }

    #[test]
    fn watch_token_on_non_llm_bind_is_rejected() {
        let src = r#"flow r(p: path) -> string {
    body = fs.read(p)
    watch body { on token(match: "bad") { warn() } }
    return body
}
"#;
        let file = parse_file(src).unwrap();
        let errs = validate(&file.flows[0], &registry_with_fs()).unwrap_err();
        let mismatch = errs
            .iter()
            .find(|e| matches!(e, ValidationError::WatchEventMismatch { .. }))
            .expect("expected WatchEventMismatch");
        let msg = mismatch.to_string();
        assert!(msg.contains("body"), "msg: {msg}");
        assert!(msg.contains("token"), "msg: {msg}");
        assert!(msg.contains("llm"), "msg: {msg}");
    }

    #[test]
    fn bind_introduces_variable_for_later_stmts() {
        let src = r#"flow t() -> Int {
    x = 1
    return x
}
"#;
        let file = parse_file(src).unwrap();
        validate(&file.flows[0], &registry_with_fs()).expect("valid flow");
    }
}
