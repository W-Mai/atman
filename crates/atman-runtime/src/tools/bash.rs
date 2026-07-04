use std::time::Instant;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct BashExec;

impl Tool for BashExec {
    fn name(&self) -> &str {
        "bash.exec"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Run a shell command via `sh -c`. Returns a struct with exit, stdout, stderr, \
             duration_ms. Flow contract must declare capabilities.shell = true.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command line, passed to `sh -c`."}
            },
            "required": ["cmd"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cmd = extract_string(&args, "cmd", 0)?;
            let start = Instant::now();
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec spawn: {e}")))?;
            let duration_ms = start.elapsed().as_millis() as i64;
            let exit = output.status.code().unwrap_or(-1) as i64;
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            Ok(Value::Struct(vec![
                ("exit".into(), Value::Int(exit)),
                ("stdout".into(), Value::Str(stdout)),
                ("stderr".into(), Value::Str(stderr)),
                ("duration_ms".into(), Value::Int(duration_ms)),
            ]))
        })
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn echo_returns_stdout() {
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("echo hello atman".into())],
            named: vec![],
        };
        let v = BashExec.call(args, &ctx).await.unwrap();
        if let Value::Struct(fields) = v {
            let stdout = fields.iter().find(|(k, _)| k == "stdout").unwrap();
            assert!(matches!(&stdout.1, Value::Str(s) if s.trim() == "hello atman"));
            let exit = fields.iter().find(|(k, _)| k == "exit").unwrap();
            assert!(matches!(exit.1, Value::Int(0)));
        } else {
            panic!("expected struct");
        }
    }

    #[tokio::test]
    async fn nonzero_exit_still_returns_struct() {
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("exit 7".into())],
            named: vec![],
        };
        let v = BashExec.call(args, &ctx).await.unwrap();
        if let Value::Struct(fields) = v {
            let exit = fields.iter().find(|(k, _)| k == "exit").unwrap();
            assert!(matches!(exit.1, Value::Int(7)));
        } else {
            panic!("expected struct");
        }
    }
}
