use std::collections::HashSet;

use atman_dsl::ast::{Arg, Expr, FlowDecl, Node, Stmt};

use crate::tool::ToolRegistry;

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("undefined variable `{0}`")]
    UndefinedVar(String),

    #[error("undefined tool `{0}`")]
    UndefinedTool(String),
}

pub fn validate(flow: &FlowDecl, tools: &ToolRegistry) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    let mut scope: HashSet<String> = flow.params.iter().map(|(id, _)| id.name.clone()).collect();
    walk_stmts(&flow.body, &mut scope, tools, &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn walk_stmts(
    stmts: &[Stmt],
    scope: &mut HashSet<String>,
    tools: &ToolRegistry,
    errors: &mut Vec<ValidationError>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Bind { name, value } => {
                walk_expr(value, scope, tools, errors);
                scope.insert(name.name.clone());
            }
            Stmt::When { cond, body } => {
                walk_expr(cond, scope, tools, errors);
                walk_stmts(body, scope, tools, errors);
            }
            Stmt::Return { value } => walk_expr(value, scope, tools, errors),
            Stmt::Expr(e) => walk_expr(e, scope, tools, errors),
            Stmt::Watch(w) => {
                if !scope.contains(&w.target.name) {
                    errors.push(ValidationError::UndefinedVar(w.target.name.clone()));
                }
            }
        }
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
