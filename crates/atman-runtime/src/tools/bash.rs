use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, BufReader};

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

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cmd = extract_string(&args, "cmd", 0)?;
            let start = Instant::now();
            let cwd = std::env::current_dir()
                .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec cwd: {e}")))?;
            if let Some(sandbox) = &ctx.sandbox {
                let output = sandbox.spawn(&["sh", "-c", &cmd], &[], &cwd).await?;
                if output.status.success() {
                    let duration_ms = start.elapsed().as_millis() as i64;
                    return Ok(struct_result(&output, duration_ms, false));
                }
                let stderr = String::from_utf8_lossy(&output.stderr);
                if stderr.contains("Operation not permitted") || stderr.contains("denied") {
                    match request_write_approval(ctx, &cmd, &stderr).await {
                        Some(true) => {
                            let output = sandbox
                                .spawn_relaxed(&["sh", "-c", &cmd], &[], &cwd)
                                .await?;
                            let duration_ms = start.elapsed().as_millis() as i64;
                            return Ok(struct_result(&output, duration_ms, true));
                        }
                        Some(false) => {
                            return Err(RuntimeError::ToolFailed(format!(
                                "bash.exec: user denied the write operation (cmd: {cmd})"
                            )));
                        }
                        None => {}
                    }
                }
                let duration_ms = start.elapsed().as_millis() as i64;
                return Ok(struct_result(&output, duration_ms, false));
            }
            run_streaming(&cmd, ctx, start).await
        })
    }
}

async fn run_streaming(cmd: &str, ctx: &ToolCtx, start: Instant) -> ToolResult {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec spawn: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::ToolFailed("bash.exec: missing stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| RuntimeError::ToolFailed("bash.exec: missing stderr".into()))?;

    let stdout_tap = ctx.stdout_broadcast.clone();
    let stdout_reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(tx) = &stdout_tap {
                let _ = tx.send(line.clone());
            }
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    });
    let stderr_reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = reader.next_line().await {
            collected.push_str(&line);
            collected.push('\n');
        }
        collected
    });

    let status = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            let _ = child.kill().await;
            let _ = stdout_reader.await;
            let _ = stderr_reader.await;
            return Err(RuntimeError::Cancelled("bash.exec cancelled".into()));
        }
        status = child.wait() => status
            .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec wait: {e}")))?,
    };
    let stdout = stdout_reader
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec stdout join: {e}")))?;
    let stderr = stderr_reader
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec stderr join: {e}")))?;
    let duration_ms = start.elapsed().as_millis() as i64;
    let exit = status.code().unwrap_or(-1) as i64;
    Ok(Value::Struct(vec![
        ("exit".into(), Value::Int(exit)),
        ("stdout".into(), Value::Str(stdout)),
        ("stderr".into(), Value::Str(stderr)),
        ("duration_ms".into(), Value::Int(duration_ms)),
        ("approval".into(), Value::Str("auto".into())),
    ]))
}

fn struct_result(output: &std::process::Output, duration_ms: i64, user_approved: bool) -> Value {
    let exit = output.status.code().unwrap_or(-1) as i64;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let approval = if user_approved { "approved" } else { "auto" };
    Value::Struct(vec![
        ("exit".into(), Value::Int(exit)),
        ("stdout".into(), Value::Str(stdout)),
        ("stderr".into(), Value::Str(stderr)),
        ("duration_ms".into(), Value::Int(duration_ms)),
        ("approval".into(), Value::Str(approval.into())),
    ])
}

async fn request_write_approval(ctx: &ToolCtx, cmd: &str, stderr: &str) -> Option<bool> {
    use crate::tool::ApprovalLevel;
    let Some(approval) = &ctx.approval else {
        return None;
    };
    let run_id = ctx.flow_run_id.clone()?;
    let id = format!("bash_write_{}", uuid::Uuid::now_v7());
    let pending = crate::session::PendingApproval {
        tool_use_id: id.clone(),
        tool_name: "bash.exec (sandboxed write)".to_string(),
        args_preview: format!("cmd={cmd}"),
        preview: Some(stderr.lines().take(5).collect::<Vec<_>>().join("\n")),
        level: ApprovalLevel::Dangerous,
        run_id,
        emitted_at: chrono::Utc::now(),
    };
    let rx = approval.request(pending);
    let run_id_for_emit = ctx.flow_run_id.clone();
    if let (Some(sink), Some(rid)) = (ctx.events.as_ref(), run_id_for_emit) {
        sink.emit(crate::event::Event::ToolPendingApproval {
            seq: 0,
            run_id: rid,
            tool_use_id: id,
            tool_name: "bash.exec".into(),
            args_preview: cmd.to_string(),
            level: "dangerous".into(),
            preview: Some(stderr.lines().take(5).collect::<Vec<_>>().join("\n")),
            ts: chrono::Utc::now(),
        });
    }
    match rx.await {
        Ok(crate::session::ApprovalDecision::Approve) => Some(true),
        _ => Some(false),
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
