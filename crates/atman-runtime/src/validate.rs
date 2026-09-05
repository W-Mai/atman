use std::collections::{HashMap, HashSet};

use atman_dsl::ast::{Arg, Expr, FlowDecl, Node, Stmt, WatchEvent};

use crate::tool::ToolRegistry;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("undefined variable `{0}`")]
    UndefinedVar(String),

    #[error("undefined tool `{0}`")]
    UndefinedTool(String),

    #[error("invocation user_message must reference a declared string parameter")]
    InvalidInvocationUserMessage,

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
    validate_with_tool_lookup(flow, &|name| tools.has(name))
}

pub fn validate_with_tool_lookup(
    flow: &FlowDecl,
    has_tool: &dyn Fn(&str) -> bool,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    validate_invocation_contract(flow, &mut errors);
    let mut scope: HashSet<String> = flow.params.iter().map(|p| p.name.name.clone()).collect();
    for name in BUILTIN_VARS {
        scope.insert(name.to_string());
    }
    let mut kinds: HashMap<String, &'static str> = HashMap::new();
    walk_stmts(&flow.body, &mut scope, &mut kinds, has_tool, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn validate_invocation_contract(flow: &FlowDecl, errors: &mut Vec<ValidationError>) {
    let Some((_, value)) = flow.contract.as_ref().and_then(|contract| {
        contract
            .blocks
            .iter()
            .find(|block| block.name.name == "invocation")
            .and_then(|block| {
                block
                    .kwargs
                    .iter()
                    .find(|(name, _)| name.name == "user_message")
            })
    }) else {
        return;
    };
    let atman_dsl::ast::Expr::Ident(parameter_name) = value else {
        errors.push(ValidationError::InvalidInvocationUserMessage);
        return;
    };
    let Some(parameter) = flow
        .params
        .iter()
        .find(|parameter| parameter.name.name == parameter_name.name)
    else {
        errors.push(ValidationError::InvalidInvocationUserMessage);
        return;
    };
    let is_string = matches!(
        &parameter.ty,
        atman_dsl::ast::TypeExpr::Named(name) if name.name == "string"
    );
    let has_supported_default = parameter.default.as_ref().is_none_or(|default| {
        matches!(
            default,
            atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Str(_))
        )
    });
    if !is_string || !has_supported_default {
        errors.push(ValidationError::InvalidInvocationUserMessage);
    }
}

const BUILTIN_VARS: &[&str] = &[
    "session",
    "fs",
    "bash",
    "term",
    "task",
    "web",
    "hunk",
    "git",
    "test",
    "memory",
    "plan",
    "form",
    "help",
    "preview",
    "session_tool",
    "sleep",
    "watch",
    "watcher",
];

fn infer_node_kind(value: &Expr) -> Option<&'static str> {
    match value {
        Expr::Node(Node::ToolCall { path, .. })
            if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
        {
            Some("llm")
        }
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
    has_tool: &dyn Fn(&str) -> bool,
    errors: &mut Vec<ValidationError>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { name, value } => {
                walk_expr(value, scope, has_tool, errors);
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
                walk_expr(cond, scope, has_tool, errors);
                walk_stmts(body, scope, kinds, has_tool, errors);
            }
            Stmt::Return { value } => walk_expr(value, scope, has_tool, errors),
            Stmt::Expr(e) => walk_expr(e, scope, has_tool, errors),
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
            Stmt::Loop { body } => {
                walk_stmts(body, scope, kinds, has_tool, errors);
            }
            Stmt::Break => {}
            Stmt::Continue => {}
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
    has_tool: &dyn Fn(&str) -> bool,
    errors: &mut Vec<ValidationError>,
) {
    match expr {
        Expr::Literal(_) | Expr::FileRef(_) => {}
        Expr::Ident(id) => {
            if !scope.contains(&id.name) {
                errors.push(ValidationError::UndefinedVar(id.name.clone()));
            }
        }
        Expr::Member { base, .. } => walk_expr(base, scope, has_tool, errors),
        Expr::Binary { left, right, .. } => {
            walk_expr(left, scope, has_tool, errors);
            walk_expr(right, scope, has_tool, errors);
        }
        Expr::Unary { operand, .. } => walk_expr(operand, scope, has_tool, errors),
        Expr::List(items) => {
            for item in items {
                walk_expr(item, scope, has_tool, errors);
            }
        }
        Expr::Struct(fields) => {
            for (_, v) in fields {
                walk_expr(v, scope, has_tool, errors);
            }
        }
        Expr::Node(node) => walk_node(node, scope, has_tool, errors),
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr(a, scope, has_tool, errors);
            }
        }
        Expr::Pipe { lhs, rhs } => {
            walk_expr(lhs, scope, has_tool, errors);
            walk_expr(rhs, scope, has_tool, errors);
        }
        Expr::Lambda { params, body } => {
            let mut child_scope = scope.clone();
            for p in params {
                child_scope.insert(p.name.clone());
            }
            walk_expr(body, &child_scope, has_tool, errors);
        }
        Expr::Annotated { expr, .. } => {
            // Type names and type list expressions in annotation position
            // are not variable references
            match expr.as_ref() {
                Expr::Ident(id) if crate::eval::is_type_name(&id.name) => {}
                Expr::List(inner) if inner.len() == 1 => {
                    if let Expr::Ident(id) = &inner[0] {
                        if crate::eval::is_type_name(&id.name) {
                            return;
                        }
                    }
                    walk_expr(expr, scope, has_tool, errors);
                }
                _ => walk_expr(expr, scope, has_tool, errors),
            }
        }
    }
}

fn walk_node(
    node: &Node,
    scope: &HashSet<String>,
    has_tool: &dyn Fn(&str) -> bool,
    errors: &mut Vec<ValidationError>,
) {
    match node {
        Node::ToolCall { path, args } => {
            let name = path
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(".");
            // Evaluator intrinsics are not registered or exposed as provider tools.
            let is_intrinsic = crate::eval::is_evaluator_intrinsic(&name);
            if !is_intrinsic && !has_tool(&name) {
                errors.push(ValidationError::UndefinedTool(name));
            }
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, has_tool, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, has_tool, errors),
                }
            }
        }
        Node::DynamicFanout { source, lambda, .. } => {
            walk_expr(source, scope, has_tool, errors);
            walk_expr(lambda, scope, has_tool, errors);
        }
        Node::Fanout { items, .. } => {
            for item in items {
                walk_expr(item, scope, has_tool, errors);
            }
        }
        Node::UserConfirm { msg } => walk_expr(msg, scope, has_tool, errors),
        Node::Subflow { args, .. } => {
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, has_tool, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, has_tool, errors),
                }
            }
        }
        Node::FixUntilTestPasses { kwargs } => {
            for (_, v) in kwargs {
                walk_expr(v, scope, has_tool, errors);
            }
        }
        Node::Message { args, .. } => {
            for arg in args {
                match arg {
                    Arg::Positional(e) => walk_expr(e, scope, has_tool, errors),
                    Arg::Named { value, .. } => walk_expr(value, scope, has_tool, errors),
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
        let reg = ToolRegistry::new();
        tools::register_tier_zero(&reg);
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
    fn invocation_user_message_requires_a_declared_string_parameter() {
        for source in [
            r#"flow t(count: int) -> int {
    contract { invocation { user_message: count } }
    return count
}"#,
            r#"flow t(prompt: string) -> string {
    contract { invocation { user_message: "prompt" } }
    return prompt
}"#,
            r#"flow t(prefix: string, prompt: string = prefix) -> string {
    contract { invocation { user_message: prompt } }
    return prompt
}"#,
        ] {
            let file = parse_file(source).unwrap();
            let errors = validate(&file.flows[0], &registry_with_fs()).unwrap_err();
            assert!(
                errors
                    .iter()
                    .any(|error| matches!(error, ValidationError::InvalidInvocationUserMessage))
            );
        }
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
    fn tool_lookup_accepts_dynamic_tool_names_without_a_registry_clone() {
        let file = parse_file(
            r#"flow t() -> string {
    return remote.search(query: "atman")
}"#,
        )
        .unwrap();

        validate_with_tool_lookup(&file.flows[0], &|name| name == "remote.search")
            .expect("dynamic tool name is available");
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
    x = llm.call(model: "m", prompt: "hi")
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

    #[test]
    fn invocation_env_is_a_valid_intrinsic_without_a_registered_tool() {
        let file = parse_file(
            r#"flow t() -> string {
    return env("effort")
}"#,
        )
        .unwrap();

        validate(&file.flows[0], &ToolRegistry::new()).expect("env is evaluator-owned");
    }
}
