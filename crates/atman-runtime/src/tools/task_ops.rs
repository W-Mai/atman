use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct TaskList;

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    match args.named(name)? {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn extract_optional_bool(args: &ToolArgs, name: &str) -> Option<bool> {
    match args.named(name)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

impl Tool for TaskList {
    fn name(&self) -> &str {
        "task.list"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "List all background tasks (bash, terminal, flow). Returns array of {id, kind, label, status, elapsed_ms, source_handle}. Filter by kind/status. Default: running only; pass all=true to include completed.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "description": "Filter by kind: bash|terminal|flow"},
                "status": {"type": "string", "description": "Filter by status: running|ok|err|killed"},
                "all": {"type": "boolean", "description": "Include completed tasks (default false)"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let kind_filter = extract_optional_string(&args, "kind");
            let status_filter = extract_optional_string(&args, "status");
            let all = extract_optional_bool(&args, "all").unwrap_or(false);

            let registry = ctx.task_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("task.list: registry not available".into())
            })?;

            let mut filter = crate::task_registry::TaskFilter::all();
            if !all {
                filter.status = Some(crate::task_registry::TaskStatus::Running);
            }
            if let Some(k) = &kind_filter {
                filter.kind = parse_kind(k);
            }
            if let Some(s) = &status_filter {
                filter.status = parse_status(s);
            }

            let snapshots = registry.list(&filter);
            let arr: Vec<Value> = snapshots
                .iter()
                .map(|s| {
                    Value::from_json(serde_json::json!({
                        "id": s.id.0.to_string(),
                        "kind": kind_to_str(s.kind),
                        "label": s.label,
                        "status": status_to_str(s.status),
                        "elapsed_ms": s.elapsed_ms(),
                        "source_handle": s.source_handle,
                    }))
                })
                .collect();
            Ok(Value::List(arr))
        })
    }
}

pub struct TaskKill;

impl Tool for TaskKill {
    fn name(&self) -> &str {
        "task.kill"
    }

    fn tier(&self) -> Tier {
        Tier::Three
    }

    fn description(&self) -> Option<&str> {
        Some("Kill a background task by id. Returns {killed: true/false}.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "Task id to kill"}
            },
            "required": ["id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id_str = extract_optional_string(&args, "id").ok_or_else(|| {
                RuntimeError::ToolFailed("task.kill: missing 'id' argument".into())
            })?;
            let id =
                crate::task_registry::TaskId(uuid::Uuid::parse_str(&id_str).map_err(|e| {
                    RuntimeError::ToolFailed(format!("task.kill: invalid id: {e}"))
                })?);
            let registry = ctx.task_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("task.kill: registry not available".into())
            })?;
            let killed = registry.kill(&id);
            Ok(Value::from_json(serde_json::json!({"killed": killed})))
        })
    }
}

fn parse_kind(s: &str) -> Option<crate::task_registry::TaskKind> {
    use crate::task_registry::TaskKind;
    match s {
        "bash" => Some(TaskKind::Bash),
        "terminal" => Some(TaskKind::Terminal),
        "flow" => Some(TaskKind::Flow),
        _ => None,
    }
}

fn parse_status(s: &str) -> Option<crate::task_registry::TaskStatus> {
    use crate::task_registry::TaskStatus;
    match s {
        "running" => Some(TaskStatus::Running),
        "ok" => Some(TaskStatus::Ok),
        "err" => Some(TaskStatus::Err),
        "killed" => Some(TaskStatus::Killed),
        _ => None,
    }
}

fn kind_to_str(k: crate::task_registry::TaskKind) -> &'static str {
    use crate::task_registry::TaskKind;
    match k {
        TaskKind::Bash => "bash",
        TaskKind::Terminal => "terminal",
        TaskKind::Flow => "flow",
    }
}

fn status_to_str(s: crate::task_registry::TaskStatus) -> &'static str {
    use crate::task_registry::TaskStatus;
    match s {
        TaskStatus::Running => "running",
        TaskStatus::Killing => "killing",
        TaskStatus::Ok => "ok",
        TaskStatus::Err => "err",
        TaskStatus::Killed => "killed",
    }
}
