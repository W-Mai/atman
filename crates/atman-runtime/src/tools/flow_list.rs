use crate::error::RuntimeError;
use crate::storage;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;
use std::path::Path;

pub struct FlowList;

impl Tool for FlowList {
    fn name(&self) -> &str {
        "flow.list"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "List all available .at flow files in ~/.config/atman/commands/. \
             Each entry includes the flow name, file, description (from the \
             describe() flow if present), and parameter list with types and \
             default values. Use this to discover flows before calling \
             flow.spawn.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let config_dir = storage::config_dir()
                .map_err(|e| RuntimeError::ToolFailed(format!("flow.list: config_dir: {e}")))?;
            let commands_dir = config_dir.join("commands");
            let mut entries: Vec<Value> = Vec::new();

            if commands_dir.is_dir() {
                let read = std::fs::read_dir(&commands_dir)
                    .map_err(|e| RuntimeError::ToolFailed(format!("flow.list: read dir: {e}")))?;
                for entry in read.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) != Some("at") {
                        continue;
                    }
                    if let Ok(flow_info) = scan_flow_file(&path) {
                        entries.push(flow_info);
                    }
                }
            }

            entries.sort_by(|a, b| {
                let name_a = match a {
                    Value::Struct(fields) => fields
                        .iter()
                        .find(|(k, _)| k == "name")
                        .and_then(|(_, v)| {
                            if let Value::Str(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                let name_b = match b {
                    Value::Struct(fields) => fields
                        .iter()
                        .find(|(k, _)| k == "name")
                        .and_then(|(_, v)| {
                            if let Value::Str(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                name_a.cmp(&name_b)
            });

            Ok(Value::List(entries))
        })
    }
}

fn scan_flow_file(path: &Path) -> Result<Value, RuntimeError> {
    let src = std::fs::read_to_string(path).map_err(|e| {
        RuntimeError::ToolFailed(format!("flow.list: read {}: {e}", path.display()))
    })?;
    let file = atman_dsl::parse::parse_file(&src).map_err(|e| {
        RuntimeError::ToolFailed(format!("flow.list: parse {}: {e}", path.display()))
    })?;

    let describe_flow = file.flows.iter().find(|f| f.name.name == "describe");
    let description = describe_flow
        .and_then(extract_return_string_literal)
        .unwrap_or_default();

    let entry_flow = file.flows.iter().find(|f| f.name.name != "describe");
    let (name, params) = if let Some(ef) = entry_flow {
        let params: Vec<Value> = ef
            .params
            .iter()
            .map(|p| {
                Value::Struct(vec![
                    ("name".into(), Value::Str(p.name.name.clone())),
                    ("ty".into(), Value::Str(format!("{:?}", p.ty))),
                    ("has_default".into(), Value::Bool(p.default.is_some())),
                ])
            })
            .collect();
        (ef.name.name.clone(), params)
    } else {
        (String::new(), Vec::new())
    };

    Ok(Value::Struct(vec![
        ("name".into(), Value::Str(name)),
        (
            "file".into(),
            Value::Str(
                path.file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string(),
            ),
        ),
        ("description".into(), Value::Str(description)),
        ("params".into(), Value::List(params)),
    ]))
}

fn extract_return_string_literal(flow: &atman_dsl::ast::FlowDecl) -> Option<String> {
    use atman_dsl::ast::{Expr, Literal, Stmt};
    for stmt in &flow.body {
        if let Stmt::Return { value } = stmt
            && let Expr::Literal(Literal::Str(s)) = value
        {
            return Some(s.clone());
        }
    }
    None
}
