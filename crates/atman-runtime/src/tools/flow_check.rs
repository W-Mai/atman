use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct FlowCheck;

impl Tool for FlowCheck {
    fn name(&self) -> &str {
        "flow.check"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Validate and lint a .at flow file and its used sources. Checks all flows \
             for undefined variables, undefined tools, and type mismatches \
             (errors), plus unused params and too many positional args \
             (warnings). Pass `flow` as a filename (e.g. 'subagent.at') or \
             path. Use after writing or editing a flow to catch mistakes \
             before spawning.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "flow": {
                    "type": "string",
                    "description": "Flow file name or path (e.g. 'subagent.at' or '/path/to/my.at')"
                }
            },
            "required": ["flow"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let flow_ref = match args.named("flow").or_else(|| args.positional.first()) {
                Some(Value::Str(s)) if !s.trim().is_empty() => s.clone(),
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "non-empty flow string".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => {
                    return Err(RuntimeError::MissingArg("flow.check.flow".into()));
                }
            };

            let path = find_flow_path(&flow_ref, ctx).await?;
            let program = crate::source_program::load_program(
                &path,
                &super::flow_source::source_roots_for_ctx(ctx),
            )
            .map_err(|error| RuntimeError::ToolFailed(format!("flow.check: {error:#}")))?;
            let registry = ctx
                .registry
                .as_ref()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.check: no tool registry".into()))?;

            let mut errors: Vec<Value> = Vec::new();
            for (id, flow) in program.iter_flows() {
                if let Err(errs) = crate::validate::validate(flow, registry) {
                    let source_path = program.source_path(&id).unwrap_or(&path);
                    for e in errs {
                        errors.push(Value::Struct(vec![
                            ("kind".into(), Value::Str("validate".into())),
                            ("flow".into(), Value::Str(flow.name.name.clone())),
                            (
                                "message".into(),
                                Value::Str(format!("{}: {e:?}", source_path.display())),
                            ),
                        ]));
                    }
                }
            }

            let mut warnings: Vec<Value> = Vec::new();
            for (source_path, file) in program.iter_modules() {
                let source_path = source_path.unwrap_or(&path);
                for hit in atman_rt::lint_file(file) {
                    warnings.push(Value::Struct(vec![
                        ("kind".into(), Value::Str(hit.rule.slug().into())),
                        ("flow".into(), Value::Str(hit.flow)),
                        (
                            "message".into(),
                            Value::Str(format!("{}: {}", source_path.display(), hit.message)),
                        ),
                    ]));
                }
            }

            let valid = errors.is_empty();

            Ok(Value::Struct(vec![
                ("valid".into(), Value::Bool(valid)),
                ("errors".into(), Value::List(errors)),
                ("warnings".into(), Value::List(warnings)),
            ]))
        })
    }
}

async fn find_flow_path(flow_ref: &str, ctx: &ToolCtx) -> Result<std::path::PathBuf, RuntimeError> {
    for path in super::flow_source::candidates(flow_ref, ctx) {
        match tokio::fs::metadata(&path).await {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "flow.check: stat {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(RuntimeError::ToolFailed(format!(
        "flow.check: flow `{flow_ref}` not found"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;
    use std::sync::Arc;

    #[tokio::test]
    async fn check_validates_flows_in_used_sources() {
        let dir = tempfile::tempdir().unwrap();
        let library = dir.path().join("lib");
        std::fs::create_dir_all(&library).unwrap();
        let entry = dir.path().join("review.at");
        std::fs::write(
            &entry,
            "use \"./lib/text.at\"::normalize\nflow review(input: string) -> string { return normalize(input).await }\n",
        )
        .unwrap();
        std::fs::write(
            library.join("text.at"),
            "pub flow normalize(input: string) -> string { return missing }\n",
        )
        .unwrap();
        let mut ctx = ToolCtx::new();
        ctx.registry = Some(Arc::new(ToolRegistry::new()));
        let args = ToolArgs {
            named: vec![("flow".into(), Value::Str(entry.display().to_string()))],
            ..ToolArgs::default()
        };

        let result = FlowCheck.call(args, &ctx).await.unwrap();
        assert!(matches!(result.field("valid"), Some(Value::Bool(false))));
        let Some(Value::List(errors)) = result.field("errors") else {
            panic!("flow.check must return validation errors");
        };
        assert!(errors.iter().any(|error| {
            matches!(error.field("message"), Some(Value::Str(message)) if message.contains("text.at") && message.contains("missing"))
        }));
    }
}
