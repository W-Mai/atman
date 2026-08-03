use crate::error::RuntimeError;
use crate::storage;
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
            "Validate and lint a .at flow file. Checks all flows in the file \
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

            let (path, src) = read_flow_source(&flow_ref).await?;
            let file = atman_dsl::parse::parse_file(&src).map_err(|e| {
                RuntimeError::ToolFailed(format!("flow.check: parse {}: {e}", path.display()))
            })?;

            let registry = ctx
                .registry
                .as_ref()
                .ok_or_else(|| RuntimeError::ToolFailed("flow.check: no tool registry".into()))?;

            let mut errors: Vec<Value> = Vec::new();
            for flow in &file.flows {
                if let Err(errs) = crate::validate::validate(flow, registry) {
                    for e in errs {
                        errors.push(Value::Struct(vec![
                            ("kind".into(), Value::Str("validate".into())),
                            ("flow".into(), Value::Str(flow.name.name.clone())),
                            ("message".into(), Value::Str(format!("{e:?}"))),
                        ]));
                    }
                }
            }

            let mut warnings: Vec<Value> = Vec::new();
            for hit in crate::flow_lint::lint_file(&file) {
                warnings.push(Value::Struct(vec![
                    ("kind".into(), Value::Str(hit.rule.slug().into())),
                    ("flow".into(), Value::Str(hit.flow.clone())),
                    ("message".into(), Value::Str(hit.message.clone())),
                ]));
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

async fn read_flow_source(flow_ref: &str) -> Result<(std::path::PathBuf, String), RuntimeError> {
    for path in flow_candidates(flow_ref) {
        match tokio::fs::read_to_string(&path).await {
            Ok(src) => return Ok((path, src)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "flow.check: read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(RuntimeError::ToolFailed(format!(
        "flow.check: flow `{flow_ref}` not found"
    )))
}

fn flow_candidates(flow_ref: &str) -> Vec<std::path::PathBuf> {
    let path = std::path::PathBuf::from(flow_ref);
    if path.is_absolute() {
        return vec![path];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut out = Vec::new();
    if let Ok(config_dir) = storage::config_dir() {
        out.push(config_dir.join("commands").join(&file_name));
    }
    out.push(std::path::PathBuf::from(file_name));
    out
}
