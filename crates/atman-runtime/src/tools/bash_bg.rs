use std::collections::HashMap;
use std::fs::File;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use command_group::{AsyncCommandGroup, AsyncGroupChild};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::task_registry::{TaskDisplay, TaskKind, TaskRegistry, TaskStatus};
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

    fn is_finished(&self) -> bool {
        !matches!(self, Self::Running { .. })
    }

    fn error(&self) -> Option<&str> {
        match self {
            Self::Failed { error, .. } => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct BgOutput {
    pub combined: Vec<u8>,
    pub total_bytes: u64,
    pub truncated: bool,
    buffer_start: usize,
}

fn framed_output(kind: StreamKind, data: &[u8]) -> Vec<u8> {
    let prefix: &[u8] = match kind {
        StreamKind::Stdout => b"[out] ",
        StreamKind::Stderr => b"[err] ",
    };
    let mut frame = Vec::with_capacity(prefix.len() + data.len() + 1);
    frame.extend_from_slice(prefix);
    frame.extend_from_slice(data);
    if !data.ends_with(b"\n") {
        frame.push(b'\n');
    }
    frame
}

impl BgOutput {
    fn push(&mut self, kind: StreamKind, data: &[u8], max: u64) -> Vec<u8> {
        let mut new_total = self.total_bytes + data.len() as u64;
        let mut to_write = data;
        if new_total > max {
            let allowed = max.saturating_sub(self.total_bytes) as usize;
            to_write = &data[..allowed.min(data.len())];
            new_total = max;
            self.truncated = true;
        }
        let frame = if to_write.is_empty() {
            Vec::new()
        } else {
            framed_output(kind, to_write)
        };
        self.combined.extend_from_slice(&frame);
        self.total_bytes = new_total;
        let max_ring = RING_BUFFER_BYTES;
        if self.combined.len() > max_ring {
            let drop = self.combined.len() - max_ring;
            self.combined.drain(..drop);
            self.buffer_start += drop;
        }
        frame
    }

    fn read_from(&self, cursor: usize, limit: usize) -> (Vec<u8>, usize, usize, bool, bool) {
        let end = self.buffer_start + self.combined.len();
        let start = cursor.max(self.buffer_start).min(end);
        let local_cursor = start - self.buffer_start;
        let (chunk, local_next, eof) = page_bytes(&self.combined, local_cursor, limit);
        (
            chunk,
            start,
            self.buffer_start + local_next,
            eof,
            cursor < self.buffer_start,
        )
    }
}

fn page_bytes(data: &[u8], cursor: usize, limit: usize) -> (Vec<u8>, usize, bool) {
    if cursor >= data.len() {
        return (Vec::new(), data.len(), true);
    }
    let remaining = &data[cursor..];
    let mut take = remaining.len().min(limit);
    if take < remaining.len()
        && let Err(error) = std::str::from_utf8(&remaining[..take])
        && error.error_len().is_none()
        && error.valid_up_to() > 0
    {
        take = error.valid_up_to();
    }
    let chunk = remaining[..take].to_vec();
    let next = cursor + take;
    let eof = next >= data.len();
    (chunk, next, eof)
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
    pub task_id: Option<crate::task_registry::TaskId>,
}

impl crate::watch::Watchable for BgEntry {
    fn watch_output(
        self: std::sync::Arc<Self>,
        pattern: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::watch::WatchResult> + Send>>
    {
        let output = self.output.clone();
        let status = self.status.clone();
        Box::pin(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return crate::watch::WatchResult::Cancelled,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                        let text = {
                            let out = output.lock().unwrap();
                            String::from_utf8_lossy(&out.combined).into_owned()
                        };
                        if let Some(pos) = text.find(&pattern) {
                            return crate::watch::WatchResult::Matched {
                                row: None,
                                col: Some(pos as u16),
                                text: pattern,
                            };
                        }
                        let st = status.lock().unwrap().clone();
                        if !matches!(st, BgStatus::Running { .. }) {
                            return crate::watch::WatchResult::SourceExited;
                        }
                    }
                }
            }
        })
    }
}

struct DirectBackgroundLauncher {
    command: tokio::process::Command,
}

impl crate::sandbox::BackgroundLauncher for DirectBackgroundLauncher {
    fn launch(
        mut self: Box<Self>,
    ) -> Result<crate::sandbox::BackgroundSpawnResult, crate::sandbox::SandboxLaunchError> {
        self.command
            .group()
            .kill_on_drop(true)
            .spawn()
            .map(crate::sandbox::BackgroundSpawnResult::direct)
            .map_err(|error| {
                crate::sandbox::SandboxLaunchError::Runtime(RuntimeError::ToolFailed(format!(
                    "spawn: {error}"
                )))
            })
    }
}

#[derive(Default)]
pub struct BgRegistry {
    entries: Mutex<HashMap<String, Arc<BgEntry>>>,
    task_registry: Option<TaskRegistry>,
}

impl BgRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_task_registry(mut self, tr: TaskRegistry) -> Self {
        self.task_registry = Some(tr);
        self
    }

    pub fn kill_all(&self) {
        let entries = self.entries.lock().unwrap();
        for (_, entry) in entries.iter() {
            let _ = entry.control_tx.try_send(BgControl::Kill);
        }
    }

    pub fn spawn(
        self: &Arc<Self>,
        launcher: Box<dyn crate::sandbox::BackgroundLauncher>,
        cmd: String,
        timeout_ms: Option<u64>,
        max_output_bytes: u64,
        ctx: &ToolCtx,
    ) -> Result<Value, RuntimeError> {
        let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".to_string());
        let local_id = uuid::Uuid::now_v7().as_u64_pair().0;
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
        let log_file = open_log_file(&log_path)
            .map_err(|error| RuntimeError::ToolFailed(format!("bash.spawn: {error}")))?;

        let timeout = match timeout_ms {
            Some(0) => None,
            Some(ms) => Some(Duration::from_millis(ms.min(MAX_SPAWN_TIMEOUT_MS))),
            None => Some(Duration::from_millis(DEFAULT_SPAWN_TIMEOUT_MS)),
        };

        let (control_tx, control_rx) = mpsc::channel::<BgControl>(8);
        let spawn_result = match launcher.launch() {
            Ok(result) => result,
            Err(error) => {
                drop(log_file);
                let _ = std::fs::remove_file(&log_path);
                return Err(error.into_runtime("bash.spawn"));
            }
        };
        let crate::sandbox::BackgroundSpawnResult { child, profile } = spawn_result;
        let pid = child.id().unwrap_or(0);
        let status = Arc::new(Mutex::new(BgStatus::Running {
            pid,
            started_at: now_ms(),
        }));
        let output = Arc::new(Mutex::new(BgOutput::default()));
        let cancel = ctx.cancel.clone();
        let task_cancel = cancel.child_token();

        let task_id = self.task_registry.as_ref().map(|tr| {
            tr.register(
                TaskKind::Bash,
                TaskDisplay {
                    label: ctx
                        .call_intent
                        .as_ref()
                        .map(|intent| intent.as_str().to_owned())
                        .unwrap_or_else(|| cmd.clone()),
                    command: Some(cmd.clone()),
                },
                handle_str.clone(),
                session_id.clone(),
                task_cancel.clone(),
            )
        });

        let entry = Arc::new(BgEntry {
            session_id: session_id.clone(),
            control_tx,
            status: status.clone(),
            output: output.clone(),
            log_path: log_path.clone(),
            task_id: task_id.clone(),
        });
        {
            let mut entries = self.entries.lock().unwrap();
            entries.insert(handle_str.clone(), entry.clone());
        }

        let status_for_task = status.clone();
        let log_path_for_return = log_path.clone();
        let stream_tx = ctx.stream_tx.clone();
        let handle_for_task = handle_str.clone();
        let task_registry = self.task_registry.clone();
        let task_id_for_spawn = task_id.clone();
        let flow_run_id = ctx.flow_run_id.as_ref().map(|r| r.0.to_string());
        let call_intent = ctx.call_intent.clone();
        tokio::spawn(async move {
            run_bg_process(
                child,
                profile,
                timeout,
                max_output_bytes,
                log_file,
                status_for_task,
                output,
                control_rx,
                task_cancel,
                stream_tx,
                handle_for_task,
                task_registry,
                task_id_for_spawn,
                flow_run_id,
                call_intent,
            )
            .await;
        });

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

    pub fn lookup(&self, handle_str: &str, session_id: &str) -> Result<Arc<BgEntry>, RuntimeError> {
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
            (
                "log_path".into(),
                Value::Str(entry.log_path.to_string_lossy().into_owned()),
            ),
        ];
        if let Some(ec) = st.exit_code() {
            fields.push(("exit_code".into(), Value::Int(ec as i64)));
        }
        if let Some(ended) = st.ended_at() {
            fields.push(("ended_at".into(), Value::Int(ended)));
        }
        if let Some(error) = st.error() {
            fields.push(("error".into(), Value::Str(error.into())));
        }
        fields.push(("bytes_total".into(), Value::Int(out.total_bytes as i64)));
        fields.push(("output_truncated".into(), Value::Bool(out.truncated)));
        Ok(Value::Struct(fields))
    }

    pub fn output(
        &self,
        handle_str: &str,
        session_id: &str,
        session_dir: Option<&std::path::Path>,
        cursor: usize,
        limit: usize,
    ) -> Result<Value, RuntimeError> {
        if let Ok(entry) = self.lookup(handle_str, session_id) {
            let st = entry.status.lock().unwrap().clone();
            let out = entry.output.lock().unwrap();
            let (chunk, actual_cursor, next, eof, fell_behind) = out.read_from(cursor, limit);
            return Ok(Value::Struct(vec![
                ("handle".into(), Value::Str(handle_str.into())),
                ("status".into(), Value::Str(st.kind().into())),
                (
                    "chunk".into(),
                    Value::Str(String::from_utf8_lossy(&chunk).into_owned()),
                ),
                ("cursor".into(), Value::Int(actual_cursor as i64)),
                ("next_cursor".into(), Value::Int(next as i64)),
                (
                    "continuation".into(),
                    Value::Struct(vec![
                        ("type".into(), Value::Str("ByteCursor".into())),
                        ("next_byte".into(), Value::Int(next as i64)),
                        ("has_more".into(), Value::Bool(!eof)),
                    ]),
                ),
                ("eof".into(), Value::Bool(eof)),
                (
                    "truncated".into(),
                    Value::Bool(out.truncated || fell_behind),
                ),
                ("live".into(), Value::Bool(true)),
            ]));
        }

        let Some(dir) = session_dir else {
            return Err(RuntimeError::ToolFailed(format!(
                "bash: handle `{handle_str}` not found"
            )));
        };
        let log_path = dir.join(format!("bg_{handle_str}.log"));
        let data = std::fs::read(&log_path).map_err(|_| {
            RuntimeError::ToolFailed(format!("bash: handle `{handle_str}` not found"))
        })?;
        if cursor >= data.len() {
            return Ok(Value::Struct(vec![
                ("handle".into(), Value::Str(handle_str.into())),
                ("status".into(), Value::Str("exited".into())),
                ("chunk".into(), Value::Str(String::new())),
                ("cursor".into(), Value::Int(data.len() as i64)),
                ("next_cursor".into(), Value::Int(data.len() as i64)),
                (
                    "continuation".into(),
                    Value::Struct(vec![
                        ("type".into(), Value::Str("ByteCursor".into())),
                        ("next_byte".into(), Value::Int(data.len() as i64)),
                        ("has_more".into(), Value::Bool(false)),
                    ]),
                ),
                ("eof".into(), Value::Bool(true)),
                ("truncated".into(), Value::Bool(false)),
                ("live".into(), Value::Bool(false)),
            ]));
        }
        let (chunk, next, eof) = page_bytes(&data, cursor, limit);
        Ok(Value::Struct(vec![
            ("handle".into(), Value::Str(handle_str.into())),
            ("status".into(), Value::Str("exited".into())),
            (
                "chunk".into(),
                Value::Str(String::from_utf8_lossy(&chunk).into_owned()),
            ),
            ("cursor".into(), Value::Int(cursor as i64)),
            ("next_cursor".into(), Value::Int(next as i64)),
            (
                "continuation".into(),
                Value::Struct(vec![
                    ("type".into(), Value::Str("ByteCursor".into())),
                    ("next_byte".into(), Value::Int(next as i64)),
                    ("has_more".into(), Value::Bool(!eof)),
                ]),
            ),
            ("eof".into(), Value::Bool(eof)),
            ("truncated".into(), Value::Bool(false)),
            ("live".into(), Value::Bool(false)),
        ]))
    }

    pub fn output_for_llm(
        &self,
        handle_str: &str,
        session_id: &str,
        session_dir: Option<&std::path::Path>,
        cursor: usize,
        limit: usize,
        output_store: Option<&crate::tools::tool_output::OutputStore>,
    ) -> Result<Value, RuntimeError> {
        let persisted = || {
            let dir = session_dir.ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "bash.output: complete persisted output is unavailable; output is truncated and cannot be continued".into(),
                )
            })?;
            let log_path = dir.join(format!("bg_{handle_str}.log"));
            std::fs::read_to_string(log_path).map_err(|_| {
                RuntimeError::ToolFailed(
                    "bash.output: complete persisted output is unavailable; output is truncated and cannot be continued".into(),
                )
            })
        };
        let full = if let Ok(entry) = self.lookup(handle_str, session_id) {
            let out = entry.output.lock().unwrap();
            if out.buffer_start == 0 && !out.truncated {
                String::from_utf8(out.combined.clone()).map_err(|_| {
                    RuntimeError::ToolFailed(
                        "bash.output: current output is not valid UTF-8; output is truncated and cannot be continued".into(),
                    )
                })?
            } else {
                drop(out);
                persisted()?
            }
        } else {
            persisted()?
        };
        if full.len() <= limit {
            return self.output(handle_str, session_id, session_dir, cursor, limit);
        }
        let store = output_store.ok_or_else(|| {
            RuntimeError::ToolFailed(
                "bash.output: session output store unavailable; oversized output cannot be continued".into(),
            )
        })?;
        let output_id = store.register(handle_str, &full).ok_or_else(|| {
            RuntimeError::ToolFailed(
                "bash.output: complete output could not be persisted; output is truncated and cannot be continued".into(),
            )
        })?;
        let offset = cursor.min(full.len());
        if !full.is_char_boundary(offset) {
            return Err(RuntimeError::ToolFailed(
                "bash.output: cursor is not a valid UTF-8 byte boundary".into(),
            ));
        }
        let (chunk, next, eof) = page_bytes(full.as_bytes(), offset, limit);
        Ok(Value::Struct(vec![
            (
                "content".into(),
                Value::Str(String::from_utf8(chunk).map_err(|_| {
                    RuntimeError::ToolFailed(
                        "bash.output: persisted output is not valid UTF-8".into(),
                    )
                })?),
            ),
            ("output_id".into(), Value::Str(output_id)),
            ("total_bytes".into(), Value::Int(full.len() as i64)),
            (
                "next".into(),
                Value::Struct(vec![
                    ("mode".into(), Value::Str("bytes".into())),
                    ("offset".into(), Value::Int(next as i64)),
                    ("has_more".into(), Value::Bool(!eof)),
                ]),
            ),
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

    #[doc(hidden)]
    pub fn clear_for_test(&self) {
        self.entries.lock().unwrap().clear();
    }

    pub fn list(
        &self,
        session_id: &str,
        session_dir: Option<&std::path::Path>,
        all: bool,
    ) -> Value {
        let entries = self.entries.lock().unwrap();
        let mut live_handles: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut items: Vec<Value> = entries
            .iter()
            .filter(|(_, e)| e.session_id == session_id)
            .map(|(handle, entry)| {
                live_handles.insert(handle.clone());
                let st = entry.status.lock().unwrap().clone();
                let out = entry.output.lock().unwrap();
                let mut fields = vec![
                    ("handle".into(), Value::Str(handle.clone())),
                    ("status".into(), Value::Str(st.kind().into())),
                    ("started_at".into(), Value::Int(st.started_at())),
                    ("live".into(), Value::Bool(true)),
                ];
                if let Some(ec) = st.exit_code() {
                    fields.push(("exit_code".into(), Value::Int(ec as i64)));
                }
                fields.push(("bytes_total".into(), Value::Int(out.total_bytes as i64)));
                Value::Struct(fields)
            })
            .collect();

        if all {
            if let Some(dir) = session_dir {
                if let Ok(rd) = std::fs::read_dir(dir) {
                    for entry in rd.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        let Some(rest) = name
                            .strip_prefix("bg_")
                            .and_then(|s| s.strip_suffix(".log"))
                        else {
                            continue;
                        };
                        let handle: String = rest.to_string();
                        if live_handles.contains(&handle) {
                            continue;
                        }
                        let Ok(meta) = entry.metadata() else {
                            continue;
                        };
                        let modified = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                            .map(|d| d.as_millis() as i64)
                            .unwrap_or(0);
                        items.push(Value::Struct(vec![
                            ("handle".into(), Value::Str(handle)),
                            ("status".into(), Value::Str("exited".into())),
                            ("started_at".into(), Value::Int(modified)),
                            ("live".into(), Value::Bool(false)),
                            ("bytes_total".into(), Value::Int(meta.len() as i64)),
                        ]));
                    }
                }
            }
        }

        Value::List(items)
    }
}

impl Drop for BgRegistry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_bg_process(
    mut child: AsyncGroupChild,
    _profile: Option<crate::sandbox::TempProfile>,
    timeout: Option<Duration>,
    max_output_bytes: u64,
    log_file: File,
    status: Arc<Mutex<BgStatus>>,
    output: Arc<Mutex<BgOutput>>,
    mut control_rx: mpsc::Receiver<BgControl>,
    cancel: CancellationToken,
    stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    handle_for_stream: String,
    task_registry: Option<TaskRegistry>,
    task_id: Option<crate::task_registry::TaskId>,
    flow_run_id: Option<String>,
    call_intent: Option<crate::message::ToolCallIntent>,
) {
    let stdout = child.inner().stdout.take();
    let stderr = child.inner().stderr.take();
    let (log_tx, log_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let log_writer = tokio::spawn(write_log(log_file, log_rx));

    let stdout_reader = stdout.map(|s| {
        let ctx = ReadStreamCtx {
            output: output.clone(),
            log_tx: log_tx.clone(),
            kind: StreamKind::Stdout,
            max_output_bytes,
            stream_tx: stream_tx.clone(),
            handle: handle_for_stream.clone(),
            flow_run_id: flow_run_id.clone(),
            call_intent: call_intent.clone(),
        };
        tokio::spawn(read_stream(BufReader::new(s), ctx))
    });
    let stderr_reader = stderr.map(|s| {
        let ctx = ReadStreamCtx {
            output: output.clone(),
            log_tx: log_tx.clone(),
            kind: StreamKind::Stderr,
            max_output_bytes,
            stream_tx: stream_tx.clone(),
            handle: handle_for_stream.clone(),
            flow_run_id: flow_run_id.clone(),
            call_intent: call_intent.clone(),
        };
        tokio::spawn(read_stream(BufReader::new(s), ctx))
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

    let started_at = status.lock().unwrap().started_at();
    let ended_at = now_ms();
    let mut final_status = match &exit_reason {
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

    if let Some(mut r) = stdout_reader {
        if tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut r)
            .await
            .is_err()
        {
            r.abort();
            let _ = r.await;
        }
    }
    if let Some(mut r) = stderr_reader {
        if tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut r)
            .await
            .is_err()
        {
            r.abort();
            let _ = r.await;
        }
    }
    drop(log_tx);
    let mut log_writer = log_writer;
    let log_result = match tokio::time::timeout(IO_DRAIN_TIMEOUT, &mut log_writer).await {
        Ok(Ok(result)) => result,
        Ok(Err(join_error)) => Err(format!("log writer task failed: {join_error}")),
        Err(_) => {
            log_writer.abort();
            let _ = log_writer.await;
            Err("log writer timed out".into())
        }
    };
    if let Err(error) = log_result {
        final_status = BgStatus::Failed {
            error,
            started_at,
            ended_at: now_ms(),
        };
    }

    let exit_code = match &final_status {
        BgStatus::Exited { exit_code, .. } => Some(*exit_code),
        _ => None,
    };
    *status.lock().unwrap() = final_status.clone();

    if let Some(tx) = &stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::BashExited {
            handle: handle_for_stream,
            exit_code,
            error: final_status.error().map(str::to_owned),
            call_intent,
            run_id: flow_run_id,
        });
    }

    if let (Some(tr), Some(tid)) = (task_registry, task_id) {
        let ts = match &final_status {
            BgStatus::Exited { exit_code, .. } if *exit_code == 0 => TaskStatus::Ok,
            BgStatus::Killed { .. } | BgStatus::TimedOut { .. } => TaskStatus::Killed,
            _ => TaskStatus::Err,
        };
        tr.finish(&tid, ts);
    }
}

fn open_log_file(log_path: &std::path::Path) -> Result<File, String> {
    File::options()
        .write(true)
        .create_new(true)
        .open(log_path)
        .map_err(|e| format!("open log: {e}"))
}

async fn write_log(file: File, mut log_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Result<(), String> {
    let mut file = tokio::fs::File::from_std(file);
    while let Some(frame) = log_rx.recv().await {
        file.write_all(&frame)
            .await
            .map_err(|e| format!("write log: {e}"))?;
    }
    file.flush().await.map_err(|e| format!("flush log: {e}"))?;
    Ok(())
}

struct ReadStreamCtx {
    output: Arc<Mutex<BgOutput>>,
    log_tx: mpsc::UnboundedSender<Vec<u8>>,
    kind: StreamKind,
    max_output_bytes: u64,
    stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    handle: String,
    flow_run_id: Option<String>,
    call_intent: Option<crate::message::ToolCallIntent>,
}

async fn read_stream<R: tokio::io::AsyncBufRead + Unpin>(mut reader: R, ctx: ReadStreamCtx) {
    let kind_str = match ctx.kind {
        StreamKind::Stdout => "stdout",
        StreamKind::Stderr => "stderr",
    };
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => break,
            Ok(_) => {
                let data = buf.as_bytes();
                {
                    let mut out = ctx.output.lock().unwrap();
                    let _ = ctx.log_tx.send(framed_output(ctx.kind, data));
                    let _ = out.push(ctx.kind, data, ctx.max_output_bytes);
                }
                if let Some(tx) = &ctx.stream_tx {
                    let _ = tx.send(crate::stream::StreamFrame::BashChunk {
                        handle: ctx.handle.clone(),
                        kind: kind_str.to_string(),
                        line: buf.clone(),
                        call_intent: ctx.call_intent.clone(),
                        run_id: ctx.flow_run_id.clone(),
                    });
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
            "Run a shell command via `sh -c`.\n\n\
block=false (default): command runs in background, returns immediately with a\n\
handle. The command keeps running — use bash.output to read its output later,\n\
bash.status to check if it finished, bash.kill to stop it. Use this for:\n\
- long-running commands (servers, watchers)\n\
- commands where you need to check output incrementally\n\
- when you want to do other things while the command runs\n\n\
block=true: waits for the command to finish, then returns stdout/stderr/exit_code.\n\
Use block_timeout_ms to set a max wait (default 30s). Use this for:\n\
- short commands where you need the result immediately (ls, git status, echo)\n\
- commands that finish quickly\n\n\
Set cwd to the narrowest directory the command needs to access. In controlled\n\
modes, cwd is the filesystem scope shown for approval and opened by the process\n\
sandbox; paths embedded only in cmd do not expand sandbox access.\n\n\
Do NOT use `sleep` in your command to wait — use block=true with block_timeout_ms\n\
instead, or use the sleep tool to pause the workflow.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string", "description": "Shell command line."},
                "cwd": {"type": "string", "description": "Working directory and sandbox filesystem scope. Set this to the narrowest directory needed for paths outside the current workspace."},
                "block": {"type": "boolean", "default": false, "description": "If true, wait for process to exit before returning."},
                "block_timeout_ms": {"type": "integer", "description": "Only with block=true. Max wait. 0 = no timeout. Default 30000."},
                "timeout_ms": {"type": "integer", "description": "Process kill timeout in ms. Default 1800000 (30min). 0 = no timeout."},
                "max_output_bytes": {"type": "integer", "description": "Max combined output bytes. Default 10485760 (10MB)."}
            },
            "required": ["cmd"]
        })
    }

    fn invocation_provenance(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<crate::permission::ResourceProvenance, RuntimeError> {
        let explicit_cwd = extract_optional_string(args, "cwd").map(std::path::PathBuf::from);
        Ok(crate::permission::ResourceProvenance::for_ctx(ctx)
            .with_cwd(ctx, explicit_cwd.as_deref())?
            .with_risk(crate::trust::RiskKind::ProcessSpawn))
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let cmd = extract_string(&args, "cmd", 0)?;
            let block = args
                .named("block")
                .and_then(|v| {
                    if let Value::Bool(b) = v {
                        Some(*b)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            let block_timeout_ms = extract_optional_int(&args, "block_timeout_ms")
                .map(|v| v as u64)
                .unwrap_or(30_000);
            let timeout_ms = extract_optional_int(&args, "timeout_ms").map(|v| v as u64);
            let max_output = extract_optional_int(&args, "max_output_bytes")
                .map(|v| v as u64)
                .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES);
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.spawn: registry not available".into())
            })?;
            let explicit_cwd = extract_optional_string(&args, "cwd").map(std::path::PathBuf::from);
            let cwd = ctx.resolve_cwd(explicit_cwd.as_deref())?;
            let authorization = ctx.invocation_authorization_for("bash.spawn")?;
            let launcher: Box<dyn crate::sandbox::BackgroundLauncher> = match authorization
                .execution_boundary()
            {
                crate::permission::ExecutionBoundary::Sandboxed => {
                    let sandbox = ctx.sandbox.as_ref().ok_or_else(|| {
                        RuntimeError::ToolFailed(
                            "bash.spawn: sandbox unavailable for controlled execution".into(),
                        )
                    })?;
                    sandbox
                        .prepare_background(&["sh", "-c", cmd.as_str()], &[], &cwd, authorization)
                        .map_err(|error| error.into_runtime("bash.spawn"))?
                }
                crate::permission::ExecutionBoundary::Direct => {
                    let mut command = tokio::process::Command::new("sh");
                    command
                        .arg("-c")
                        .arg(&cmd)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .current_dir(&cwd);
                    Box::new(DirectBackgroundLauncher { command })
                }
            };
            let handle_str = registry.spawn(launcher, cmd, timeout_ms, max_output, ctx)?;

            if !block {
                return Ok(handle_str);
            }

            let handle_s = handle_str
                .field("handle")
                .and_then(|v| {
                    if let Value::Str(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    RuntimeError::ToolFailed("bash.spawn: missing handle field".into())
                })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());

            let deadline = if block_timeout_ms == 0 {
                None
            } else {
                Some(tokio::time::Instant::now() + Duration::from_millis(block_timeout_ms))
            };
            loop {
                let entry = registry.lookup(&handle_s, &session_id)?;
                let finished = {
                    let st = entry.status.lock().unwrap();
                    st.is_finished()
                };
                if finished {
                    break;
                }
                if let Some(d) = deadline {
                    if tokio::time::Instant::now() >= d {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            let entry = registry.lookup(&handle_s, &session_id)?;
            let st = entry.status.lock().unwrap().clone();
            let out = entry.output.lock().unwrap();
            let combined = String::from_utf8_lossy(&out.combined).into_owned();
            let log_path = entry.log_path.to_string_lossy().into_owned();
            Ok(Value::Struct(vec![
                ("handle".into(), Value::Str(handle_s)),
                ("status".into(), Value::Str(st.kind().into())),
                (
                    "exit_code".into(),
                    st.exit_code()
                        .map(|c| Value::Int(c as i64))
                        .unwrap_or(Value::Unit),
                ),
                (
                    "error".into(),
                    st.error()
                        .map(|e| Value::Str(e.into()))
                        .unwrap_or(Value::Unit),
                ),
                ("output".into(), Value::Str(combined)),
                ("bytes_total".into(), Value::Int(out.total_bytes as i64)),
                ("log_path".into(), Value::Str(log_path)),
            ]))
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
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".to_string());
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
            let limit = (extract_optional_int(&args, "limit_bytes")
                .unwrap_or(DEFAULT_OUTPUT_LIMIT as i64)
                .max(1) as usize)
                .min(ctx.tool_output_budget.max_bytes);
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.output: registry not available".into())
            })?;
            registry.output_for_llm(
                &handle,
                &session_id,
                ctx.session_dir.as_deref(),
                cursor,
                limit,
                ctx.output_store.as_deref(),
            )
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
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.kill: registry not available".into())
            })?;
            registry.kill(&handle, &session_id)
        })
    }
}

pub struct BashList;

impl Tool for BashList {
    fn name(&self) -> &str {
        "bash.list"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some(
            "List background bash processes for the current session. Default: only live processes. Pass all=true to include historical processes whose log files persist in session_dir (status=exited, live=false).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "all": {"type": "boolean", "description": "Include historical processes (default false)."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let all = extract_optional_bool(&args, "all").unwrap_or(false);
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".to_string());
            let registry = ctx.bg_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("bash.list: registry not available".into())
            })?;
            Ok(registry.list(&session_id, ctx.session_dir.as_deref(), all))
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

fn extract_optional_bool(args: &ToolArgs, name: &str) -> Option<bool> {
    match args.named(name)? {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

fn extract_optional_string(args: &ToolArgs, name: &str) -> Option<String> {
    match args.named(name)? {
        Value::Str(value) => Some(value.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ToolArgs, ToolCtx};
    use crate::value::Value;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    #[derive(Clone, Copy)]
    enum StrictLaunch {
        Success,
        Denied,
        RuntimeError,
    }

    struct TestBackgroundLauncher;

    #[derive(Clone, Copy)]
    enum LauncherFailure {
        Denied,
        Runtime,
    }

    struct CountingFailingLauncher {
        launch_calls: Arc<AtomicUsize>,
        provisional_logs_seen: Arc<AtomicUsize>,
        session_dir: std::path::PathBuf,
        failure: LauncherFailure,
    }

    impl crate::sandbox::BackgroundLauncher for CountingFailingLauncher {
        fn launch(
            self: Box<Self>,
        ) -> Result<crate::sandbox::BackgroundSpawnResult, crate::sandbox::SandboxLaunchError>
        {
            self.launch_calls.fetch_add(1, Ordering::SeqCst);
            self.provisional_logs_seen
                .store(background_log_count(&self.session_dir), Ordering::SeqCst);
            match self.failure {
                LauncherFailure::Denied => Err(crate::sandbox::SandboxLaunchError::Denied(
                    Box::new(crate::sandbox::SandboxDenial {
                        operation: crate::sandbox::SandboxOperation::BackgroundSpawn,
                        reason: "launcher denied sentinel".into(),
                        provenance: crate::permission::ResourceProvenance::none(),
                    }),
                )),
                LauncherFailure::Runtime => Err(crate::sandbox::SandboxLaunchError::Runtime(
                    RuntimeError::ToolFailed("launcher runtime sentinel".into()),
                )),
            }
        }
    }

    impl crate::sandbox::BackgroundLauncher for TestBackgroundLauncher {
        fn launch(
            self: Box<Self>,
        ) -> Result<crate::sandbox::BackgroundSpawnResult, crate::sandbox::SandboxLaunchError>
        {
            let mut command = tokio::process::Command::new("sh");
            command
                .arg("-c")
                .arg("exit 0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let child = command
                .group()
                .kill_on_drop(true)
                .spawn()
                .map_err(|error| {
                    crate::sandbox::SandboxLaunchError::Runtime(RuntimeError::ToolFailed(format!(
                        "test spawn: {error}"
                    )))
                })?;
            Ok(crate::sandbox::BackgroundSpawnResult::direct(child))
        }
    }

    struct RecordingBackgroundSandbox {
        strict: StrictLaunch,
        strict_calls: AtomicUsize,
        cwd: Mutex<Option<std::path::PathBuf>>,
    }

    impl crate::sandbox::Sandbox for RecordingBackgroundSandbox {
        fn spawn<'a>(
            &'a self,
            _cmd: &'a [&'a str],
            _env: &'a [(String, String)],
            _cwd: &'a std::path::Path,
            _authorization: &'a crate::permission::InvocationAuthorization,
        ) -> crate::tool::BoxFut<'a, Result<std::process::Output, RuntimeError>> {
            Box::pin(async { Err(RuntimeError::ToolFailed("unsupported".into())) })
        }

        fn prepare_background(
            &self,
            _cmd: &[&str],
            _env: &[(String, String)],
            cwd: &std::path::Path,
            authorization: &crate::permission::InvocationAuthorization,
        ) -> Result<Box<dyn crate::sandbox::BackgroundLauncher>, crate::sandbox::SandboxLaunchError>
        {
            self.strict_calls.fetch_add(1, Ordering::SeqCst);
            *self.cwd.lock().unwrap() = Some(cwd.to_path_buf());
            match self.strict {
                StrictLaunch::Success => Ok(Box::new(TestBackgroundLauncher)),
                StrictLaunch::Denied => Err(crate::sandbox::SandboxLaunchError::Denied(Box::new(
                    crate::sandbox::SandboxDenial {
                        operation: crate::sandbox::SandboxOperation::BackgroundSpawn,
                        reason: "strict denied".into(),
                        provenance: authorization.provenance().clone(),
                    },
                ))),
                StrictLaunch::RuntimeError => Err(crate::sandbox::SandboxLaunchError::Runtime(
                    RuntimeError::ToolFailed("strict runtime sentinel".into()),
                )),
            }
        }

        fn spawn_pty<'a>(
            &'a self,
            _cmd: &'a [&'a str],
            _env: &'a [(String, String)],
            _cwd: &'a std::path::Path,
            _pty_size: portable_pty::PtySize,
            _authorization: &'a crate::permission::InvocationAuthorization,
        ) -> crate::tool::BoxFut<
            'a,
            Result<crate::sandbox::PtySpawnResult, crate::sandbox::SandboxLaunchError>,
        > {
            Box::pin(async {
                Err(crate::sandbox::SandboxLaunchError::Runtime(
                    RuntimeError::ToolFailed("unsupported".into()),
                ))
            })
        }

        fn is_available(&self) -> bool {
            true
        }

        fn kind(&self) -> &'static str {
            "test-background"
        }
    }

    fn ctx_with_registry(registry: Arc<BgRegistry>, dir: &std::path::Path) -> ToolCtx {
        let mut ctx = ToolCtx::new().with_trust(crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        });
        ctx.bg_registry = Some(registry);
        ctx.session_dir = Some(dir.to_path_buf());
        ctx.session_id = Some("test-session".to_string());
        ctx.for_tool_invocation(crate::tool::Tier::Four)
            .authorized_for(crate::permission::InvocationAuthorization::new(
                crate::permission::PermissionRequestId::now(),
                "test-call",
                "bash.spawn",
                crate::permission::ResourceProvenance::none(),
                crate::permission::ExecutionBoundary::Direct,
            ))
    }

    fn brokered_spawn_ctx(
        registry: Arc<BgRegistry>,
        dir: &std::path::Path,
        trust: crate::trust::TrustConfig,
    ) -> ToolCtx {
        let flows = Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let broker = crate::permission::PermissionBroker::shared(Arc::clone(&flows));
        let run_id = crate::event::FlowRunId::now();
        let identity = flows
            .register_root(
                "test-session".into(),
                run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let mut ctx = ctx_with_registry(registry, dir);
        ctx.approval = Some(Arc::new(crate::session::ApprovalRegistry::new()));
        ctx.permission_broker = Some(broker);
        ctx.flow_registry = Some(flows);
        ctx.flow_identity = Some(identity);
        ctx.flow_run_id = Some(run_id);
        ctx.with_trust(trust)
            .for_tool_invocation(crate::tool::Tier::Four)
    }

    async fn authorize_bash_spawn(ctx: ToolCtx, args: &ToolArgs) -> ToolCtx {
        match crate::approval::request_approval(
            &ctx,
            "bash.spawn",
            "bash.spawn",
            args,
            crate::tool::ApprovalLevel::Dangerous,
            Some(&BashSpawn),
        )
        .await
        {
            crate::approval::ApprovalOutcome::Approve { authorization } => {
                ctx.authorized_for(*authorization)
            }
            crate::approval::ApprovalOutcome::Deny { reason } => {
                panic!("strict bash spawn authorization denied: {reason}")
            }
        }
    }

    fn eager_sandbox_policy() -> crate::trust::TrustConfig {
        crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Eager,
            escalation: crate::trust::EscalationPolicy::Deny,
            ..crate::trust::TrustConfig::default()
        }
    }

    fn background_entries(registry: &BgRegistry, dir: &std::path::Path) -> usize {
        match registry.list("test-session", Some(dir), false) {
            Value::List(items) => items.len(),
            other => panic!("expected background list, got {other:?}"),
        }
    }

    fn background_log_count(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        name.starts_with("bg_") && name.ends_with(".log")
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn spawn_provenance_uses_explicit_cwd_not_cmd_as_path() {
        let dir = TempDir::new().unwrap();
        let ctx = ctx_with_registry(Arc::new(BgRegistry::new()), dir.path());
        let args = ToolArgs {
            named: vec![
                ("cmd".into(), Value::Str("/bin/echo hi".into())),
                ("cwd".into(), Value::Str(dir.path().display().to_string())),
            ],
            ..ToolArgs::default()
        };
        let provenance = BashSpawn.invocation_provenance(&args, &ctx).unwrap();
        assert_eq!(provenance.path, None);
        assert_eq!(
            provenance.cwd,
            Some(crate::fs_access::canonicalize_stable(dir.path()))
        );
        assert!(
            provenance
                .risks
                .contains(&crate::trust::RiskKind::ProcessSpawn)
        );
    }

    #[test]
    fn pushed_frame_matches_live_output_after_budget_truncation() {
        let mut output = BgOutput::default();
        let first = output.push(StreamKind::Stdout, b"first\n", 6);
        let second = output.push(StreamKind::Stderr, b"second\n", 6);

        assert_eq!(first, b"[out] first\n");
        assert!(second.is_empty());
        assert_eq!(output.combined, first);
        assert_eq!(output.total_bytes, 6);
        assert!(output.truncated);
    }

    #[test]
    fn ring_buffer_cursors_remain_absolute_after_eviction() {
        let first_data = vec![b'a'; 40_000];
        let second_data = vec![b'b'; 40_000];
        let mut full = Vec::new();
        full.extend_from_slice(b"[out] ");
        full.extend_from_slice(&first_data);
        full.push(b'\n');
        full.extend_from_slice(b"[out] ");
        full.extend_from_slice(&second_data);
        full.push(b'\n');

        let mut output = BgOutput::default();
        output.push(StreamKind::Stdout, &first_data, u64::MAX);
        let (_, _, cursor, _, fell_behind) = output.read_from(0, 32_000);
        assert_eq!(cursor, 32_000);
        assert!(!fell_behind);

        output.push(StreamKind::Stdout, &second_data, u64::MAX);
        assert!(output.buffer_start > 0);
        let (chunk, actual_cursor, next, _, fell_behind) = output.read_from(cursor, 32_000);
        assert_eq!(actual_cursor, cursor);
        assert_eq!(chunk, full[cursor..next]);
        assert!(!fell_behind);

        let (chunk, actual_cursor, next, _, fell_behind) = output.read_from(0, 32);
        assert_eq!(actual_cursor, output.buffer_start);
        assert_eq!(chunk, full[actual_cursor..next]);
        assert!(fell_behind);

        let (chunk, actual_cursor, next, eof, fell_behind) = output.read_from(usize::MAX, 32);
        assert!(chunk.is_empty());
        assert_eq!(actual_cursor, full.len());
        assert_eq!(next, full.len());
        assert!(eof);
        assert!(!fell_behind);
    }

    #[test]
    fn page_bytes_preserves_utf8_across_pages() {
        let data = "你好世界".as_bytes();
        let (first, next, eof) = page_bytes(data, 0, 4);
        assert_eq!(first, "你".as_bytes());
        assert_eq!(next, 3);
        assert!(!eof);
        let (second, next, eof) = page_bytes(data, next, 4);
        assert_eq!(second, "好".as_bytes());
        assert_eq!(next, 6);
        assert!(!eof);
    }

    #[test]
    fn persisted_output_pages_utf8_and_returns_continuation() {
        let registry = BgRegistry::new();
        let dir = TempDir::new().unwrap();
        let handle = "missing";
        std::fs::write(dir.path().join("bg_missing.log"), "你好").unwrap();

        let first = registry
            .output(handle, "test-session", Some(dir.path()), 0, 4)
            .unwrap();
        let Value::Struct(fields) = first else {
            panic!("expected output fields");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "chunk"),
            Some((_, Value::Str(chunk))) if chunk == "你"
        ));
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "next_cursor"),
            Some((_, Value::Int(3)))
        ));

        let second = registry
            .output(handle, "test-session", Some(dir.path()), 3, 4)
            .unwrap();
        let Value::Struct(fields) = second else {
            panic!("expected output fields");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "chunk"),
            Some((_, Value::Str(chunk))) if chunk == "好"
        ));
        let Value::Struct(cursor) = fields
            .iter()
            .find_map(|(name, value)| (name == "continuation").then_some(value))
            .unwrap()
        else {
            panic!("expected continuation");
        };
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_byte"),
            Some((_, Value::Int(6)))
        ));
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "has_more"),
            Some((_, Value::Bool(false)))
        ));

        let beyond_eof = registry
            .output(handle, "test-session", Some(dir.path()), 99, 4)
            .unwrap();
        let Value::Struct(fields) = beyond_eof else {
            panic!("expected output fields");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "cursor"),
            Some((_, Value::Int(6)))
        ));
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "next_cursor"),
            Some((_, Value::Int(6)))
        ));
        let Value::Struct(cursor) = fields
            .iter()
            .find_map(|(name, value)| (name == "continuation").then_some(value))
            .unwrap()
        else {
            panic!("expected continuation");
        };
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_byte"),
            Some((_, Value::Int(6)))
        ));
    }

    #[test]
    fn oversized_output_registers_and_reassembles_through_output_store() {
        let registry = BgRegistry::new();
        let dir = TempDir::new().unwrap();
        let full = format!("{}{}", "前缀🚀".repeat(104_857), "前缀");
        assert_eq!(full.len(), 1_048_576);
        std::fs::write(dir.path().join("bg_missing.log"), full.as_bytes()).unwrap();
        let store = crate::tools::tool_output::OutputStore::at(dir.path());

        let value = registry
            .output_for_llm(
                "missing",
                "test-session",
                Some(dir.path()),
                0,
                1024,
                Some(&store),
            )
            .unwrap();
        let Value::Struct(fields) = value else {
            panic!("expected output fields");
        };
        let output_id = fields
            .iter()
            .find_map(|(name, value)| (name == "output_id").then_some(value))
            .and_then(|value| match value {
                Value::Str(id) => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        let initial_content = fields
            .iter()
            .find_map(|(name, value)| (name == "content").then_some(value))
            .and_then(|value| match value {
                Value::Str(content) => Some(content.clone()),
                _ => None,
            })
            .unwrap();
        let total_bytes = fields
            .iter()
            .find_map(|(name, value)| (name == "total_bytes").then_some(value))
            .and_then(|value| match value {
                Value::Int(total_bytes) => Some(*total_bytes as usize),
                _ => None,
            })
            .unwrap();
        let Value::Struct(next_fields) = fields
            .iter()
            .find_map(|(name, value)| (name == "next").then_some(value))
            .unwrap()
        else {
            panic!("expected next fields");
        };
        let next_offset = next_fields
            .iter()
            .find_map(|(name, value)| (name == "offset").then_some(value))
            .and_then(|value| match value {
                Value::Int(offset) => Some(*offset as usize),
                _ => None,
            })
            .unwrap();
        let has_more = next_fields
            .iter()
            .find_map(|(name, value)| (name == "has_more").then_some(value))
            .and_then(|value| match value {
                Value::Bool(has_more) => Some(*has_more),
                _ => None,
            })
            .unwrap();
        assert!(output_id.starts_with("out_"));
        assert_eq!(total_bytes, 1_048_576);
        assert_eq!(next_offset, initial_content.len());
        assert!(next_offset <= 1_024);
        assert!(full.is_char_boundary(next_offset));
        assert!(has_more);
        assert!(!fields.iter().any(|(name, _)| name == "continuation"));

        let budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: 1024,
            max_line_bytes: usize::MAX,
        };
        let mut offset = next_offset;
        let mut assembled = initial_content;
        assert!(has_more);
        loop {
            let page = store.read_bytes(&output_id, offset, 1024, budget).unwrap();
            assembled.push_str(&page.content);
            if !page.has_more {
                break;
            }
            offset = page.next_offset;
        }
        assert_eq!(assembled.len(), total_bytes);
        assert_eq!(assembled, full);
    }

    #[test]
    fn page_bytes_always_advances_for_incomplete_utf8() {
        let data = [0xf0, 0x9f, 0x9a, 0x80];
        let (chunk, next, eof) = page_bytes(&data, 0, 1);
        assert_eq!(chunk, vec![0xf0]);
        assert_eq!(next, 1);
        assert!(!eof);
    }

    #[test]
    fn failed_status_preserves_error_reason() {
        let status = BgStatus::Failed {
            error: "open log: permission denied".into(),
            started_at: 1,
            ended_at: 2,
        };
        assert_eq!(status.kind(), "failed");
        assert_eq!(status.error(), Some("open log: permission denied"));
        assert_eq!(status.exit_code(), None);
        assert!(status.is_finished());
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

    #[test]
    fn log_file_reports_open_failure_before_spawn() {
        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("log");
        std::fs::create_dir(&log_path).unwrap();

        let error = open_log_file(&log_path).unwrap_err();
        assert!(error.contains("open log"));
    }

    #[test]
    fn bg_registry_pre_launch_session_dir_failure_does_not_call_launcher() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let session_path = dir.path().join("not-a-directory");
        std::fs::write(&session_path, b"occupied").unwrap();
        let launch_calls = Arc::new(AtomicUsize::new(0));
        let launcher = Box::new(CountingFailingLauncher {
            launch_calls: launch_calls.clone(),
            provisional_logs_seen: Arc::new(AtomicUsize::new(0)),
            session_dir: session_path.clone(),
            failure: LauncherFailure::Runtime,
        });
        let ctx = ctx_with_registry(registry.clone(), &session_path);

        let error = registry
            .spawn(launcher, "ignored".into(), None, 1024, &ctx)
            .unwrap_err();

        assert!(error.to_string().contains("create session_dir"));
        assert_eq!(launch_calls.load(Ordering::SeqCst), 0);
        assert_eq!(background_entries(&registry, &session_path), 0);
    }

    #[test]
    fn bg_registry_launcher_denied_removes_provisional_log_and_registry_entry() {
        let task_registry = crate::task_registry::TaskRegistry::new();
        let registry = Arc::new(BgRegistry::new().with_task_registry(task_registry.clone()));
        let dir = TempDir::new().unwrap();
        let launch_calls = Arc::new(AtomicUsize::new(0));
        let provisional_logs_seen = Arc::new(AtomicUsize::new(0));
        let launcher = Box::new(CountingFailingLauncher {
            launch_calls: launch_calls.clone(),
            provisional_logs_seen: provisional_logs_seen.clone(),
            session_dir: dir.path().to_path_buf(),
            failure: LauncherFailure::Denied,
        });
        let ctx = ctx_with_registry(registry.clone(), dir.path());

        let error = registry
            .spawn(launcher, "ignored".into(), None, 1024, &ctx)
            .unwrap_err();

        assert!(error.to_string().contains("launcher denied sentinel"));
        assert_eq!(launch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provisional_logs_seen.load(Ordering::SeqCst), 1);
        assert_eq!(background_log_count(dir.path()), 0);
        assert_eq!(background_entries(&registry, dir.path()), 0);
        assert_eq!(task_registry.running_count(), 0);
        assert!(
            task_registry
                .list(&crate::task_registry::TaskFilter::all())
                .is_empty()
        );
    }

    #[test]
    fn bg_registry_launcher_runtime_error_removes_provisional_log_and_registry_entry() {
        let task_registry = crate::task_registry::TaskRegistry::new();
        let registry = Arc::new(BgRegistry::new().with_task_registry(task_registry.clone()));
        let dir = TempDir::new().unwrap();
        let launch_calls = Arc::new(AtomicUsize::new(0));
        let provisional_logs_seen = Arc::new(AtomicUsize::new(0));
        let launcher = Box::new(CountingFailingLauncher {
            launch_calls: launch_calls.clone(),
            provisional_logs_seen: provisional_logs_seen.clone(),
            session_dir: dir.path().to_path_buf(),
            failure: LauncherFailure::Runtime,
        });
        let ctx = ctx_with_registry(registry.clone(), dir.path());

        let error = registry
            .spawn(launcher, "ignored".into(), None, 1024, &ctx)
            .unwrap_err();

        assert!(error.to_string().contains("launcher runtime sentinel"));
        assert_eq!(launch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provisional_logs_seen.load(Ordering::SeqCst), 1);
        assert_eq!(background_log_count(dir.path()), 0);
        assert_eq!(background_entries(&registry, dir.path()), 0);
        assert_eq!(task_registry.running_count(), 0);
        assert!(
            task_registry
                .list(&crate::task_registry::TaskFilter::all())
                .is_empty()
        );
    }

    #[tokio::test]
    async fn sandbox_background_strict_success_registers_process() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let sandbox = Arc::new(RecordingBackgroundSandbox {
            strict: StrictLaunch::Success,
            strict_calls: AtomicUsize::new(0),
            cwd: Mutex::new(None),
        });
        let args = ToolArgs {
            positional: vec![Value::Str("ignored".into())],
            named: vec![("cwd".into(), Value::Str(dir.path().display().to_string()))],
        };
        let ctx = brokered_spawn_ctx(registry.clone(), dir.path(), eager_sandbox_policy())
            .with_sandbox(sandbox.clone());
        let ctx = authorize_bash_spawn(ctx, &args).await;

        BashSpawn.call(args, &ctx).await.unwrap();

        assert_eq!(sandbox.strict_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            *sandbox.cwd.lock().unwrap(),
            Some(crate::fs_access::canonicalize_stable(dir.path()))
        );
        assert_eq!(background_entries(&registry, dir.path()), 1);
        registry.kill_all();
    }

    #[tokio::test]
    async fn direct_authorization_skips_available_sandbox() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let sandbox = Arc::new(RecordingBackgroundSandbox {
            strict: StrictLaunch::Denied,
            strict_calls: AtomicUsize::new(0),
            cwd: Mutex::new(None),
        });
        let args = ToolArgs {
            positional: vec![Value::Str("exit 0".into())],
            named: vec![("block".into(), Value::Bool(true))],
        };
        let ctx =
            ctx_with_registry(Arc::clone(&registry), dir.path()).with_sandbox(sandbox.clone());

        let result = BashSpawn.call(args, &ctx).await.unwrap();

        assert_eq!(sandbox.strict_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(result.field("exit_code"), Some(Value::Int(0))));
        registry.kill_all();
    }

    #[tokio::test]
    async fn sandbox_background_typed_denial_never_relaunches() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let sandbox = Arc::new(RecordingBackgroundSandbox {
            strict: StrictLaunch::Denied,
            strict_calls: AtomicUsize::new(0),
            cwd: Mutex::new(None),
        });
        let args = ToolArgs {
            positional: vec![Value::Str("ignored".into())],
            named: vec![],
        };
        let ctx = brokered_spawn_ctx(registry.clone(), dir.path(), eager_sandbox_policy())
            .with_sandbox(sandbox.clone());
        let ctx = authorize_bash_spawn(ctx, &args).await;

        let error = BashSpawn.call(args, &ctx).await.unwrap_err();

        assert!(error.to_string().contains("strict denied"));
        assert_eq!(sandbox.strict_calls.load(Ordering::SeqCst), 1);
        assert_eq!(background_entries(&registry, dir.path()), 0);
    }

    #[tokio::test]
    async fn sandbox_background_runtime_error_never_falls_back_or_registers() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let sandbox = Arc::new(RecordingBackgroundSandbox {
            strict: StrictLaunch::RuntimeError,
            strict_calls: AtomicUsize::new(0),
            cwd: Mutex::new(None),
        });
        let args = ToolArgs {
            positional: vec![Value::Str("ignored".into())],
            named: vec![],
        };
        let ctx = brokered_spawn_ctx(registry.clone(), dir.path(), eager_sandbox_policy())
            .with_sandbox(sandbox.clone());
        let ctx = authorize_bash_spawn(ctx, &args).await;

        let error = BashSpawn.call(args, &ctx).await.unwrap_err();

        assert!(error.to_string().contains("strict runtime sentinel"));
        assert_eq!(sandbox.strict_calls.load(Ordering::SeqCst), 1);
        assert_eq!(background_entries(&registry, dir.path()), 0);
    }

    #[tokio::test]
    async fn sandbox_background_denial_leaves_registry_unchanged() {
        let registry = Arc::new(BgRegistry::new());
        let dir = TempDir::new().unwrap();
        let sandbox = Arc::new(RecordingBackgroundSandbox {
            strict: StrictLaunch::Denied,
            strict_calls: AtomicUsize::new(0),
            cwd: Mutex::new(None),
        });
        let args = ToolArgs {
            positional: vec![Value::Str("ignored".into())],
            named: vec![],
        };
        let ctx = brokered_spawn_ctx(registry.clone(), dir.path(), eager_sandbox_policy())
            .with_sandbox(sandbox.clone());
        let ctx = authorize_bash_spawn(ctx, &args).await;

        let error = BashSpawn.call(args, &ctx).await.unwrap_err();

        assert!(error.to_string().contains("denied"));
        assert_eq!(sandbox.strict_calls.load(Ordering::SeqCst), 1);
        assert_eq!(background_entries(&registry, dir.path()), 0);
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
    async fn spawn_defaults_to_managed_workspace() {
        let registry = Arc::new(BgRegistry::new());
        let session_dir = TempDir::new().unwrap();
        let workspace = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry, session_dir.path()).with_workspace(
            crate::git_workspace::WorkspaceBinding {
                workspace_id: "test".into(),
                repository_root: workspace.path().to_path_buf(),
                path: workspace.path().to_path_buf(),
                branch: None,
            },
        );
        let value = BashSpawn
            .call(
                ToolArgs {
                    positional: vec![Value::Str("pwd".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let log_path = value
            .field("log_path")
            .and_then(|value| match value {
                Value::Str(path) => Some(path.clone()),
                _ => None,
            })
            .unwrap();

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let output = std::fs::read_to_string(&log_path).unwrap_or_default();
            if !output.is_empty() {
                let canonical_workspace = crate::fs_access::canonicalize_stable(workspace.path());
                assert!(output.contains(&canonical_workspace.display().to_string()));
                return;
            }
        }
        panic!("pwd output did not arrive in time");
    }

    #[tokio::test]
    async fn spawn_uses_explicit_cwd() {
        let registry = Arc::new(BgRegistry::new());
        let session_dir = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let ctx = ctx_with_registry(registry, session_dir.path());
        let value = BashSpawn
            .call(
                ToolArgs {
                    positional: vec![Value::Str("pwd".into())],
                    named: vec![("cwd".into(), Value::Str(cwd.path().display().to_string()))],
                },
                &ctx,
            )
            .await
            .unwrap();
        let log_path = value
            .field("log_path")
            .and_then(|value| match value {
                Value::Str(path) => Some(path.clone()),
                _ => None,
            })
            .unwrap();

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let output = std::fs::read_to_string(&log_path).unwrap_or_default();
            if !output.is_empty() {
                let canonical_cwd = crate::fs_access::canonicalize_stable(cwd.path());
                assert!(output.contains(&canonical_cwd.display().to_string()));
                return;
            }
        }
        panic!("pwd output did not arrive in time");
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
        let log_path = fields
            .iter()
            .find(|(k, _)| k == "log_path")
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
            assert_eq!(std::fs::read_to_string(log_path).unwrap(), *s);
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
        ctx_b.session_id = Some("other-session".to_string());
        let status_args = ToolArgs {
            positional: vec![Value::Str(handle)],
            named: vec![],
        };
        let err = BashStatus.call(status_args, &ctx_b).await.err().unwrap();
        assert!(format!("{err}").contains("does not belong to session"));
    }

    #[tokio::test]
    async fn read_stream_keeps_complete_log_after_memory_budget_is_exhausted() {
        let output = Arc::new(Mutex::new(BgOutput::default()));
        let (log_tx, mut log_rx) = mpsc::unbounded_channel();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(4);
        let (mut writer, reader) = tokio::io::duplex(1024);
        let input = b"first line\nsecond line\n";
        let write_task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(input).await.unwrap();
        });

        read_stream(
            BufReader::new(reader),
            ReadStreamCtx {
                output: Arc::clone(&output),
                log_tx,
                kind: StreamKind::Stdout,
                max_output_bytes: 5,
                stream_tx: Some(stream_tx),
                handle: "test".into(),
                flow_run_id: None,
                call_intent: crate::message::ToolCallIntent::new("检查命令输出"),
            },
        )
        .await;
        write_task.await.unwrap();

        let frames: Vec<Vec<u8>> = std::iter::from_fn(|| log_rx.try_recv().ok()).collect();
        assert_eq!(frames.concat(), b"[out] first line\n[out] second line\n");
        assert!(output.lock().unwrap().truncated);
        for _ in 0..2 {
            let frame = stream_rx.try_recv().expect("streamed bash line");
            assert!(matches!(
                frame,
                crate::stream::StreamFrame::BashChunk {
                    call_intent: Some(intent),
                    ..
                } if intent.as_str() == "检查命令输出"
            ));
        }
    }
}
