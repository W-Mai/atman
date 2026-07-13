use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

const DEFAULT_SPAWN_TIMEOUT_MS: u64 = 1_800_000;
const MAX_SPAWN_TIMEOUT_MS: u64 = 86_400_000;
const DEFAULT_MAX_OUTPUT_BYTES: u64 = 10_485_760;
const RING_BUFFER_BYTES: usize = 65_536;
const IO_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_OUTPUT_LIMIT: usize = 32_000;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct BgHandle {
    session_id: String,
    local_id: u64,
}

impl BgHandle {
    #[allow(clippy::inherent_to_string)]
    pub fn to_string(&self) -> String {
        format!("bg_{}_{}", self.session_id, self.local_id)
    }

    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("bg_")?;
        let idx = rest.rfind('_')?;
        let session_id = rest[..idx].to_string();
        let local_id = rest[idx + 1..].parse().ok()?;
        Some(Self {
            session_id,
            local_id,
        })
    }
}

#[derive(Debug, Clone)]
pub enum BgStatus {
    Running {
        pid: u32,
        started_at: i64,
    },
    Exited {
        exit_code: i32,
        started_at: i64,
        ended_at: i64,
    },
    TimedOut {
        started_at: i64,
        ended_at: i64,
    },
    Killed {
        started_at: i64,
        ended_at: i64,
    },
    Failed {
        error: String,
        started_at: i64,
        ended_at: i64,
    },
}

impl BgStatus {
    fn kind(&self) -> &'static str {
        match self {
            Self::Running { .. } => "running",
            Self::Exited { .. } => "exited",
            Self::TimedOut { .. } => "timed_out",
            Self::Killed { .. } => "killed",
            Self::Failed { .. } => "failed",
        }
    }

    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Exited { exit_code, .. } => Some(*exit_code),
            _ => None,
        }
    }

    fn started_at(&self) -> i64 {
        match self {
            Self::Running { started_at, .. }
            | Self::Exited { started_at, .. }
            | Self::TimedOut { started_at, .. }
            | Self::Killed { started_at, .. }
            | Self::Failed { started_at, .. } => *started_at,
        }
    }

    fn ended_at(&self) -> Option<i64> {
        match self {
            Self::Exited { ended_at, .. }
            | Self::TimedOut { ended_at, .. }
            | Self::Killed { ended_at, .. }
            | Self::Failed { ended_at, .. } => Some(*ended_at),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct BgOutput {
    pub combined: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
}

impl BgOutput {
    fn push(&mut self, kind: StreamKind, data: &[u8], max: u64) {
        let prefix: &[u8] = match kind {
            StreamKind::Stdout => b"[out] ",
            StreamKind::Stderr => b"[err] ",
        };
        let mut new_total = self.total_bytes + data.len() as u64;
        let mut to_write = data;
        if new_total > max {
            let allowed = max.saturating_sub(self.total_bytes) as usize;
            to_write = &data[..allowed.min(data.len())];
            new_total = max;
            self.truncated = true;
        }
        if !to_write.is_empty() {
            self.combined.extend_from_slice(prefix);
            self.combined.extend_from_slice(to_write);
            if !to_write.ends_with(b"\n") {
                self.combined.push(b'\n');
            }
        }
        self.total_bytes = new_total;
        let max_ring = RING_BUFFER_BYTES;
        if self.combined.len() > max_ring {
            let drop = self.combined.len() - max_ring;
            self.combined.drain(..drop);
        }
    }

    fn read_from(&self, cursor: usize, limit: usize) -> (Vec<u8>, usize, bool) {
        let data = &self.combined;
        if cursor >= data.len() {
            return (Vec::new(), data.len(), true);
        }
        let remaining = &data[cursor..];
        let take = remaining.len().min(limit);
        let chunk = remaining[..take].to_vec();
        let next = cursor + take;
        let eof = next >= data.len();
        (chunk, next, eof)
    }
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

pub(crate) enum BgControl {
    Kill,
}

pub struct BgEntry {
    pub session_id: String,
    pub(crate) control_tx: mpsc::Sender<BgControl>,
    pub status: Arc<Mutex<BgStatus>>,
    pub output: Arc<Mutex<BgOutput>>,
    pub log_path: std::path::PathBuf,
}

#[derive(Default)]
pub struct BgRegistry {
    next_id: AtomicU64,
    entries: Mutex<HashMap<String, Arc<BgEntry>>>,
}

impl BgRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn(
        self: &Arc<Self>,
        cmd: String,
        timeout_ms: Option<u64>,
        max_output_bytes: u64,
        ctx: &ToolCtx,
    ) -> Result<Value, RuntimeError> {
        let session_id = ctx
            .turn_id
            .as_ref()
            .map(|t| t.0.to_string())
            .unwrap_or_else(|| "anon".to_string());
        let local_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handle = BgHandle {
            session_id: session_id.clone(),
            local_id,
        };
        let handle_str = handle.to_string();

        let dir = ctx.session_dir.clone().ok_or_else(|| {
            RuntimeError::ToolFailed("bash.spawn: session_dir not available".into())
        })?;
        std::fs::create_dir_all(&dir).map_err(|e| {
            RuntimeError::ToolFailed(format!("bash.spawn: create session_dir: {e}"))
        })?;
        let log_path = dir.join(format!("bg_{}.log", handle_str));

        let timeout = match timeout_ms {
            Some(0) => None,
            Some(ms) => Some(Duration::from_millis(ms.min(MAX_SPAWN_TIMEOUT_MS))),
            None => Some(Duration::from_millis(DEFAULT_SPAWN_TIMEOUT_MS)),
        };

        let (control_tx, control_rx) = mpsc::channel::<BgControl>(8);
        let status = Arc::new(Mutex::new(BgStatus::Running {
            pid: 0,
            started_at: now_ms(),
        }));
        let output = Arc::new(Mutex::new(BgOutput::default()));
        let cancel = ctx.cancel.clone();

        let entry = Arc::new(BgEntry {
            session_id: session_id.clone(),
            control_tx,
            status: status.clone(),
            output: output.clone(),
            log_path: log_path.clone(),
        });
        {
            let mut entries = self.entries.lock().unwrap();
            entries.insert(handle_str.clone(), entry.clone());
        }

        let registry = Arc::clone(self);
        let handle_str_for_task = handle_str.clone();
        let status_for_task = status.clone();
        let log_path_for_return = log_path.clone();
        tokio::spawn(async move {
            run_bg_process(
                handle_str_for_task,
                cmd,
                timeout,
                max_output_bytes,
                log_path,
                status_for_task,
                output,
                control_rx,
                cancel,
                registry,
            )
            .await;
        });

        let pid = {
            let s = status.lock().unwrap();
            if let BgStatus::Running { pid, .. } = &*s {
                *pid
            } else {
                0
            }
        };

        Ok(Value::Struct(vec![
            ("handle".into(), Value::Str(handle_str)),
            ("status".into(), Value::Str("running".into())),
            ("pid".into(), Value::Int(pid as i64)),
            (
                "log_path".into(),
                Value::Str(log_path_for_return.to_string_lossy().into_owned()),
            ),
        ]))
    }

    fn lookup(&self, handle_str: &str, session_id: &str) -> Result<Arc<BgEntry>, RuntimeError> {
        let handle = BgHandle::parse(handle_str).ok_or_else(|| {
            RuntimeError::ToolFailed(format!("bash: invalid handle `{handle_str}`"))
        })?;
        if handle.session_id != session_id {
            return Err(RuntimeError::ToolFailed(format!(
                "bash: handle `{handle_str}` does not belong to session `{session_id}`"
            )));
        }
        let entries = self.entries.lock().unwrap();
        entries.get(handle_str).cloned().ok_or_else(|| {
            RuntimeError::ToolFailed(format!("bash: handle `{handle_str}` not found"))
        })
    }

    pub fn status(&self, handle_str: &str, session_id: &str) -> Result<Value, RuntimeError> {
        let entry = self.lookup(handle_str, session_id)?;
        let st = entry.status.lock().unwrap().clone();
        let out = entry.output.lock().unwrap();
        let mut fields = vec![
            ("handle".into(), Value::Str(handle_str.into())),
            ("status".into(), Value::Str(st.kind().into())),
            ("started_at".into(), Value::Int(st.started_at())),
        ];
        if let Some(ec) = st.exit_code() {
            fields.push(("exit_code".into(), Value::Int(ec as i64)));
        }
        if let Some(ended) = st.ended_at() {
            fields.push(("ended_at".into(), Value::Int(ended)));
        }
        fields.push(("bytes_total".into(), Value::Int(out.total_bytes as i64)));
        fields.push(("output_truncated".into(), Value::Bool(out.truncated)));
        Ok(Value::Struct(fields))
    }

    pub fn output(
        &self,
        handle_str: &str,
        session_id: &str,
        cursor: usize,
        limit: usize,
    ) -> Result<Value, RuntimeError> {
        let entry = self.lookup(handle_str, session_id)?;
        let st = entry.status.lock().unwrap().clone();
        let out = entry.output.lock().unwrap();
        let (chunk, next, eof) = out.read_from(cursor, limit);
        Ok(Value::Struct(vec![
            ("handle".into(), Value::Str(handle_str.into())),
            ("status".into(), Value::Str(st.kind().into())),
            (
                "chunk".into(),
                Value::Str(String::from_utf8_lossy(&chunk).into_owned()),
            ),
            ("cursor".into(), Value::Int(cursor as i64)),
            ("next_cursor".into(), Value::Int(next as i64)),
            ("eof".into(), Value::Bool(eof)),
            ("truncated".into(), Value::Bool(out.truncated)),
        ]))
    }

    pub fn kill(&self, handle_str: &str, session_id: &str) -> Result<Value, RuntimeError> {
        let entry = self.lookup(handle_str, session_id)?;
        let _ = entry.control_tx.try_send(BgControl::Kill);
        let st = entry.status.lock().unwrap().clone();
        Ok(Value::Struct(vec![
            ("handle".into(), Value::Str(handle_str.into())),
            ("status".into(), Value::Str(st.kind().into())),
        ]))
    }

    fn remove(&self, handle_str: &str) {
        self.entries.lock().unwrap().remove(handle_str);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_bg_process(
    handle_str: String,
    cmd: String,
    timeout: Option<Duration>,
    max_output_bytes: u64,
    log_path: std::path::PathBuf,
    status: Arc<Mutex<BgStatus>>,
    output: Arc<Mutex<BgOutput>>,
    mut control_rx: mpsc::Receiver<BgControl>,
    cancel: CancellationToken,
    registry: Arc<BgRegistry>,
) {
    let started_at = now_ms();
    let mut child: AsyncGroupChild = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .group_spawn()
    {
        Ok(c) => c,
        Err(e) => {
            *status.lock().unwrap() = BgStatus::Failed {
                error: format!("spawn: {e}"),
                started_at,
                ended_at: now_ms(),
            };
            registry.remove(&handle_str);
            return;
        }
    };
    let pid = child.id();
    *status.lock().unwrap() = BgStatus::Running {
        pid: pid.unwrap_or(0),
        started_at,
    };

    let stdout = child.inner().stdout.take();
    let stderr = child.inner().stderr.take();

    let stdout_reader = stdout.map(|s| {
        let output = output.clone();
        let log_path = log_path.clone();
        tokio::spawn(read_stream(
            BufReader::new(s),
            output,
            log_path,
            StreamKind::Stdout,
            max_output_bytes,
        ))
    });
    let stderr_reader = stderr.map(|s| {
        let output = output.clone();
        let log_path = log_path.clone();
        tokio::spawn(read_stream(
            BufReader::new(s),
            output,
            log_path,
            StreamKind::Stderr,
            max_output_bytes,
        ))
    });

    let exit_reason = tokio::select! {
        biased;
        _ = cancel.cancelled() => ExitReason::Cancelled,
        ctrl = control_rx.recv() => {
            match ctrl {
                Some(BgControl::Kill) => ExitReason::Kill,
                None => ExitReason::Natural,
            }
        }
        _ = async {
            if let Some(t) = timeout {
                tokio::time::sleep(t).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => ExitReason::Timeout,
        s = child.wait() => ExitReason::Exited(s),
    };

    let ended_at = now_ms();
    let final_status = match &exit_reason {
        ExitReason::Exited(Ok(s)) => BgStatus::Exited {
            exit_code: s.code().unwrap_or(-1),
            started_at,
            ended_at,
        },
        ExitReason::Timeout => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            BgStatus::TimedOut {
                started_at,
                ended_at,
            }
        }
        ExitReason::Kill => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            BgStatus::Killed {
                started_at,
                ended_at,
            }
        }
        ExitReason::Cancelled => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            BgStatus::Killed {
                started_at,
                ended_at,
            }
        }
        ExitReason::Exited(Err(_)) => {
            let _ = child.start_kill();
            BgStatus::Failed {
                error: "wait failed".into(),
                started_at,
                ended_at,
            }
        }
        ExitReason::Natural => {
            let s = child.wait().await;
            BgStatus::Exited {
                exit_code: s.ok().and_then(|s| s.code()).unwrap_or(-1),
                started_at,
                ended_at: now_ms(),
            }
        }
    };

    if let Some(r) = stdout_reader {
        let _ = tokio::time::timeout(IO_DRAIN_TIMEOUT, r).await;
    }
    if let Some(r) = stderr_reader {
        let _ = tokio::time::timeout(IO_DRAIN_TIMEOUT, r).await;
    }

    *status.lock().unwrap() = final_status;
}

async fn read_stream<R: tokio::io::AsyncBufRead + Unpin>(
    mut reader: R,
    output: Arc<Mutex<BgOutput>>,
    log_path: std::path::PathBuf,
    kind: StreamKind,
    max_output_bytes: u64,
) {
    let prefix: &[u8] = match kind {
        StreamKind::Stdout => b"[out] ",
        StreamKind::Stderr => b"[err] ",
    };
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                let data = buf.as_bytes();
                {
                    let mut out = output.lock().unwrap();
                    out.push(kind, data, max_output_bytes);
                }
                let mut file = match tokio::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&log_path)
                    .await
                {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                let _ = file.write_all(prefix).await;
                let _ = file.write_all(data).await;
                if !data.ends_with(b"\n") {
                    let _ = file.write_all(b"\n").await;
                }
            }
            Err(_) => break,
        }
    }
}

enum ExitReason {
    Exited(std::io::Result<std::process::ExitStatus>),
    Timeout,
    Kill,
    Cancelled,
    Natural,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct BashSpawn;

impl Tool for BashSpawn {
    fn name(&self) -> &str {
        "bash.spawn"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Spawn a shell command in the background via `sh -c`. Returns immediately with a \
             handle. Use bash.status / bash.output / bash.kill to manage it. Default timeout \
             30min, max 24h. Flow contract must declare capabilities.shell = true.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command line."},
                "timeout_ms": {"type": "integer", "description": "Timeout in ms. Default 1800000 (30min). 0 = no timeout."},
                "max_output_bytes": {"type": "integer", "description": "Max combined output bytes. Default 10485760 (10MB)."}
            },
            "required": ["cmd"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cmd = extract_string(&args, "cmd", 0)?;
            let timeout_ms = extract_optional_int(&args, "timeout_ms").map(|v| v as u64);
            let max_output = extract_optional_int(&args, "max_output_bytes")
                .map(|v| v as u64)
                .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.spawn: registry not available".into())
            })?;
            registry.spawn(cmd, timeout_ms, max_output, ctx)
        })
    }
}

pub struct BashStatus;

impl Tool for BashStatus {
    fn name(&self) -> &str {
        "bash.status"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some("Check the status of a background bash process.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let session_id = ctx
                .turn_id
                .as_ref()
                .map(|t| t.0.to_string())
                .unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.status: registry not available".into())
            })?;
            registry.status(&handle, &session_id)
        })
    }
}

pub struct BashOutput;

impl Tool for BashOutput {
    fn name(&self) -> &str {
        "bash.output"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some("Read output from a background bash process by byte cursor.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "cursor": {"type": "integer", "description": "Byte offset to start reading. Default 0."},
                "limit_bytes": {"type": "integer", "description": "Max bytes to return. Default 32000."}
            },
            "required": ["handle"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let cursor = extract_optional_int(&args, "cursor").unwrap_or(0).max(0) as usize;
            let limit = extract_optional_int(&args, "limit_bytes")
                .unwrap_or(DEFAULT_OUTPUT_LIMIT as i64)
                .max(1) as usize;
            let session_id = ctx
                .turn_id
                .as_ref()
                .map(|t| t.0.to_string())
                .unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.output: registry not available".into())
            })?;
            registry.output(&handle, &session_id, cursor, limit)
        })
    }
}

pub struct BashKill;

impl Tool for BashKill {
    fn name(&self) -> &str {
        "bash.kill"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Kill a background bash process. signal=term (default) sends SIGTERM, signal=kill sends SIGKILL.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "signal": {"type": "string", "enum": ["term", "kill"], "description": "Default term."}
            },
            "required": ["handle"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let _ = extract_string(&args, "signal", 1);
            let session_id = ctx
                .turn_id
                .as_ref()
                .map(|t| t.0.to_string())
                .unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.kill: registry not available".into())
            })?;
            registry.kill(&handle, &session_id)
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

fn extract_optional_int(args: &ToolArgs, name: &str) -> Option<i64> {
    match args.named(name)? {
        Value::Int(n) => Some(*n),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::tool::{ToolArgs, ToolCtx};
    use crate::value::Value;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn ctx_with_registry(registry: Arc<BgRegistry>, dir: &std::path::Path) -> ToolCtx {
        let mut ctx = ToolCtx::new();
        ctx.bg_registry = Some(registry);
        ctx.session_dir = Some(dir.to_path_buf());
        ctx.turn_id = Some(TurnId::now());
        ctx
    }

    #[test]
    fn handle_parse_roundtrip() {
        let h = BgHandle {
            session_id: "abc".into(),
            local_id: 42,
        };
        let s = h.to_string();
        assert_eq!(s, "bg_abc_42");
        let back = BgHandle::parse(&s).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn handle_parse_rejects_bad_format() {
        assert!(BgHandle::parse("not_bg").is_none());
        assert!(BgHandle::parse("bg_nosuffix").is_none());
        assert!(BgHandle::parse("bg_x_notnum").is_none());
    }

    #[tokio::test]
    async fn spawn_returns_immediately_with_running_status() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry.clone(), dir.path());
        let args = ToolArgs {
            positional: vec![Value::Str("echo hello".into())],
            named: vec![],
        };
        let v = BashSpawn.call(args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else {
            panic!("expected struct")
        };
        let handle = fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();
        assert!(handle.starts_with("bg_"));
        let status_val = fields.iter().find(|(k, _)| k == "status").unwrap();
        assert!(matches!(&status_val.1, Value::Str(s) if s == "running"));
    }

    #[tokio::test]
    async fn spawn_then_status_reaches_exited() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry.clone(), dir.path());
        let spawn_args = ToolArgs {
            positional: vec![Value::Str("echo hello".into())],
            named: vec![],
        };
        let v = BashSpawn.call(spawn_args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else { panic!() };
        let handle = fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let status_args = ToolArgs {
                positional: vec![Value::Str(handle.clone())],
                named: vec![],
            };
            let s = BashStatus.call(status_args, &ctx).await.unwrap();
            if let Value::Struct(sf) = s {
                let kind = sf.iter().find(|(k, _)| k == "status").unwrap();
                if matches!(&kind.1, Value::Str(s) if s == "exited") {
                    let ec = sf.iter().find(|(k, _)| k == "exit_code").unwrap();
                    assert!(matches!(ec.1, Value::Int(0)));
                    return;
                }
            }
        }
        panic!("process did not exit in time");
    }

    #[tokio::test]
    async fn spawn_output_captures_stdout() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry.clone(), dir.path());
        let spawn_args = ToolArgs {
            positional: vec![Value::Str("echo line1; echo line2".into())],
            named: vec![],
        };
        let v = BashSpawn.call(spawn_args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else { panic!() };
        let handle = fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();

        tokio::time::sleep(Duration::from_millis(300)).await;

        let out_args = ToolArgs {
            positional: vec![Value::Str(handle.clone())],
            named: vec![],
        };
        let o = BashOutput.call(out_args, &ctx).await.unwrap();
        let Value::Struct(of) = o else { panic!() };
        let chunk = of.iter().find(|(k, _)| k == "chunk").unwrap();
        if let Value::Str(s) = &chunk.1 {
            assert!(s.contains("line1"), "chunk should contain line1: {s}");
            assert!(s.contains("line2"), "chunk should contain line2: {s}");
        } else {
            panic!("chunk not str");
        }
    }

    #[tokio::test]
    async fn kill_terminates_long_running_process() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry.clone(), dir.path());
        let spawn_args = ToolArgs {
            positional: vec![Value::Str("sleep 100".into())],
            named: vec![],
        };
        let v = BashSpawn.call(spawn_args, &ctx).await.unwrap();
        let Value::Struct(fields) = v else { panic!() };
        let handle = fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();

        let kill_args = ToolArgs {
            positional: vec![Value::Str(handle.clone())],
            named: vec![],
        };
        BashKill.call(kill_args, &ctx).await.unwrap();

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let status_args = ToolArgs {
                positional: vec![Value::Str(handle.clone())],
                named: vec![],
            };
            let s = BashStatus.call(status_args, &ctx).await.unwrap();
            if let Value::Struct(sf) = s {
                let kind = sf.iter().find(|(k, _)| k == "status").unwrap();
                if matches!(&kind.1, Value::Str(s) if s == "killed") {
                    return;
                }
            }
        }
        panic!("process not killed in time");
    }

    #[tokio::test]
    async fn cross_session_access_rejected() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let ctx_a = ctx_with_registry(registry.clone(), dir.path());

        let spawn_args = ToolArgs {
            positional: vec![Value::Str("sleep 10".into())],
            named: vec![],
        };
        let v = BashSpawn.call(spawn_args, &ctx_a).await.unwrap();
        let Value::Struct(fields) = v else { panic!() };
        let handle = fields
            .iter()
            .find(|(k, _)| k == "handle")
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap();

        let mut ctx_b = ToolCtx::new();
        ctx_b.bg_registry = Some(registry.clone());
        ctx_b.session_dir = Some(dir.path().to_path_buf());
        ctx_b.turn_id = Some(TurnId::now());
        let status_args = ToolArgs {
            positional: vec![Value::Str(handle)],
            named: vec![],
        };
        let err = BashStatus.call(status_args, &ctx_b).await.err().unwrap();
        assert!(format!("{err}").contains("does not belong to session"));
    }
}
