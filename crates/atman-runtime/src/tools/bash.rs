use std::process::Stdio;
use std::time::{Duration, Instant};

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_BYTES: usize = 1_048_576;
const OUTPUT_TAIL_BYTES: usize = 32_768;
const IO_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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
             duration_ms, timed_out, output_path. Flow contract must declare \
             capabilities.shell = true. Default timeout 120s, max 600s.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command line, passed to `sh -c`."},
                "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds. Default 120000, max 600000."}
            },
            "required": ["cmd"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cmd = extract_string(&args, "cmd", 0)?;
            let timeout_ms = extract_optional_int(&args, "timeout_ms")
                .unwrap_or(DEFAULT_TIMEOUT_MS as i64)
                .clamp(0, MAX_TIMEOUT_MS as i64) as u64;
            let timeout = if timeout_ms == 0 {
                None
            } else {
                Some(Duration::from_millis(timeout_ms))
            };
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
                    let deny = ctx
                        .trust
                        .as_ref()
                        .map(|t| {
                            t.mode != crate::trust::TrustMode::Reckless
                                && t.outside == crate::trust::OutsideBehavior::Deny
                        })
                        .unwrap_or(false);
                    if deny {
                        return Err(RuntimeError::ToolFailed(format!(
                            "bash.exec: sandbox blocked (cmd: {cmd}): {stderr}"
                        )));
                    }
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
            run_streaming(&cmd, ctx, start, timeout).await
        })
    }
}
async fn run_streaming(
    cmd: &str,
    ctx: &ToolCtx,
    start: Instant,
    timeout: Option<Duration>,
) -> ToolResult {
    let mut child: AsyncGroupChild = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
        .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec spawn: {e}")))?;
    let stdout = child
        .inner()
        .stdout
        .take()
        .ok_or_else(|| RuntimeError::ToolFailed("bash.exec: missing stdout".into()))?;
    let stderr = child
        .inner()
        .stderr
        .take()
        .ok_or_else(|| RuntimeError::ToolFailed("bash.exec: missing stderr".into()))?;

    let stdout_tap = ctx.stdout_broadcast.clone();
    let max_out = MAX_OUTPUT_BYTES;
    let stdout_reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut collected = String::new();
        let mut truncated = false;
        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(tx) = &stdout_tap {
                let _ = tx.send(line.clone());
            }
            if !truncated {
                if collected.len() + line.len() + 1 > max_out {
                    let remaining = max_out.saturating_sub(collected.len());
                    collected.push_str(&line[..remaining.min(line.len())]);
                    truncated = true;
                } else {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
        }
        (collected, truncated)
    });
    let stderr_reader = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut collected = String::new();
        let mut truncated = false;
        while let Ok(Some(line)) = reader.next_line().await {
            if !truncated {
                if collected.len() + line.len() + 1 > max_out {
                    let remaining = max_out.saturating_sub(collected.len());
                    collected.push_str(&line[..remaining.min(line.len())]);
                    truncated = true;
                } else {
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
        }
        (collected, truncated)
    });

    let status = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            drain_readers(stdout_reader, stderr_reader).await;
            return Err(RuntimeError::Cancelled("bash.exec cancelled".into()));
        }
        _ = async {
            if let Some(t) = timeout {
                tokio::time::sleep(t).await;
            } else {
                std::future::pending::<()>().await;
            }
        }         => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            return Ok(Value::Struct(vec![
                ("exit".into(), Value::Int(-1)),
                ("stdout".into(), Value::Str(String::new())),
                ("stderr".into(), Value::Str(String::new())),
                ("duration_ms".into(), Value::Int(start.elapsed().as_millis() as i64)),
                ("approval".into(), Value::Str("auto".into())),
                ("timed_out".into(), Value::Bool(true)),
            ]));
        }
        status = child.wait() => status
            .map_err(|e| RuntimeError::ToolFailed(format!("bash.exec wait: {e}")))?,
    };

    let (stdout_collected, stdout_truncated, stderr_collected, stderr_truncated) =
        drain_readers(stdout_reader, stderr_reader).await;

    let duration_ms = start.elapsed().as_millis() as i64;
    let exit = status.code().unwrap_or(-1) as i64;

    let (stdout_final, stdout_path) =
        maybe_persist_output(stdout_collected, stdout_truncated, ctx, "stdout").await;
    let (stderr_final, stderr_path) =
        maybe_persist_output(stderr_collected, stderr_truncated, ctx, "stderr").await;

    let mut fields = vec![
        ("exit".into(), Value::Int(exit)),
        ("stdout".into(), Value::Str(stdout_final)),
        ("stderr".into(), Value::Str(stderr_final)),
        ("duration_ms".into(), Value::Int(duration_ms)),
        ("approval".into(), Value::Str("auto".into())),
        ("timed_out".into(), Value::Bool(false)),
    ];
    if let Some(p) = stdout_path.or(stderr_path) {
        fields.push((
            "output_path".into(),
            Value::Str(p.to_string_lossy().into_owned()),
        ));
    }
    Ok(Value::Struct(fields))
}

async fn drain_readers(
    stdout_reader: tokio::task::JoinHandle<(String, bool)>,
    stderr_reader: tokio::task::JoinHandle<(String, bool)>,
) -> (String, bool, String, bool) {
    let (stdout, stdout_trunc) = match tokio::time::timeout(IO_DRAIN_TIMEOUT, stdout_reader).await {
        Ok(Ok((s, t))) => (s, t),
        _ => (String::new(), false),
    };
    let (stderr, stderr_trunc) = match tokio::time::timeout(IO_DRAIN_TIMEOUT, stderr_reader).await {
        Ok(Ok((s, t))) => (s, t),
        _ => (String::new(), false),
    };
    (stdout, stdout_trunc, stderr, stderr_trunc)
}

async fn maybe_persist_output(
    collected: String,
    truncated: bool,
    ctx: &ToolCtx,
    stream: &str,
) -> (String, Option<std::path::PathBuf>) {
    if !truncated {
        return (collected, None);
    }
    let Some(dir) = ctx.session_dir.as_ref() else {
        let tail = tail_bytes(&collected, OUTPUT_TAIL_BYTES);
        return (
            format!("{tail}\n[atman: truncated at {} bytes]", MAX_OUTPUT_BYTES),
            None,
        );
    };
    let id = uuid::Uuid::now_v7().simple();
    let path = dir.join(format!("bash_{stream}_{id}.log"));
    if tokio::fs::write(&path, &collected).await.is_err() {
        let tail = tail_bytes(&collected, OUTPUT_TAIL_BYTES);
        return (
            format!("{tail}\n[atman: truncated at {} bytes]", MAX_OUTPUT_BYTES),
            None,
        );
    }
    let tail = tail_bytes(&collected, OUTPUT_TAIL_BYTES);
    let body = format!(
        "{tail}\n[atman: truncated at {} bytes; full output at {}]",
        MAX_OUTPUT_BYTES,
        path.display()
    );
    (body, Some(path))
}

fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let start = s.len() - max;
    let boundary = s[start..]
        .char_indices()
        .next()
        .map(|(i, _)| start + i)
        .unwrap_or(start);
    &s[boundary..]
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
        ("timed_out".into(), Value::Bool(false)),
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
        bypass_auto_ceiling: false,
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

fn extract_optional_int(args: &ToolArgs, name: &str) -> Option<i64> {
    match args.named(name)? {
        Value::Int(n) => Some(*n),
        _ => None,
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

    #[tokio::test]
    async fn timeout_kills_long_running_command() {
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("sleep 30".into())],
            named: vec![("timeout_ms".into(), Value::Int(200))],
        };
        let v = BashExec.call(args, &ctx).await.unwrap();
        if let Value::Struct(fields) = v {
            let timed_out = fields.iter().find(|(k, _)| k == "timed_out").unwrap();
            assert!(
                matches!(timed_out.1, Value::Bool(true)),
                "should be timed out"
            );
        } else {
            panic!("expected struct");
        }
    }

    #[tokio::test]
    async fn timeout_zero_means_no_timeout() {
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str("echo fast".into())],
            named: vec![("timeout_ms".into(), Value::Int(0))],
        };
        let v = BashExec.call(args, &ctx).await.unwrap();
        if let Value::Struct(fields) = v {
            let timed_out = fields.iter().find(|(k, _)| k == "timed_out").unwrap();
            assert!(matches!(timed_out.1, Value::Bool(false)));
        }
    }
}
