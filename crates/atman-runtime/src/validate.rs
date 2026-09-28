use atman_rt::ast::{Expr, FlowDecl, Literal, TypeExpr};
use atman_rt::{LanguageValidationError, validate_flow};

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
    let mut errors = Vec::new();
    validate_invocation_contract(flow, &mut errors);
    let report = validate_flow(flow, BUILTIN_VARS);
    errors.extend(report.errors.into_iter().map(|error| match error {
        LanguageValidationError::UndefinedVar(name) => ValidationError::UndefinedVar(name),
        LanguageValidationError::WatchEventMismatch {
            target,
            event,
            target_kind,
            expected,
        } => ValidationError::WatchEventMismatch {
            target,
            event,
            target_kind,
            expected,
        },
    }));
    for name in report.tool_calls {
        if !crate::eval::is_evaluator_intrinsic(&name) && !tools.has(&name) {
            errors.push(ValidationError::UndefinedTool(name));
        }
    }
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
    let Expr::Ident(parameter_name) = value else {
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
        TypeExpr::Named(name) if name.name == "string"
    );
    let has_supported_default = parameter
        .default
        .as_ref()
        .is_none_or(|default| matches!(default, Expr::Literal(Literal::Str(_))));
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools;
    use atman_rt::parse_file;

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
    fn annotation_type_names_are_not_variable_references() {
        let file = parse_file(
            r#"flow t() -> Int {
    fields = { count: int -- "number", items: [string] -- "names" }
    return 1
}"#,
        )
        .unwrap();
        validate(&file.flows[0], &registry_with_fs()).expect("type descriptors are valid");
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
