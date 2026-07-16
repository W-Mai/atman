use std::time::Duration;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct Sleep;

impl Tool for Sleep {
    fn name(&self) -> &str {
        "sleep"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Pause the workflow for a specified duration. Does NOT sleep the system — only the workflow waits. Use after spawning a background command to give it time to start, then check output. Max 60000ms (60s).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ms": {"type": "integer", "description": "Milliseconds to wait. Max 60000."}
            },
            "required": ["ms"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let ms = extract_int(&args, "ms", 0)?.clamp(0, 60_000) as u64;
            tokio::time::sleep(Duration::from_millis(ms)).await;
            Ok(Value::Unit)
        })
    }
}

fn extract_int(args: &ToolArgs, name: &str, pos: usize) -> Result<i64, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Int(n) => Ok(*n),
        other => Err(RuntimeError::TypeMismatch {
            expected: "integer".into(),
            actual: other.kind_name().into(),
        }),
    }
}
