#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::error::RuntimeError;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const STREAM_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct TermHandle {
    pub session_id: String,
    pub local_id: u64,
}

impl TermHandle {
    pub fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("term_")?;
        let idx = rest.rfind('_')?;
        let session_id = rest[..idx].to_string();
        let local_id = rest[idx + 1..].parse().ok()?;
        Some(Self {
            session_id,
            local_id,
        })
    }
}

impl std::fmt::Display for TermHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "term_{}_{}", self.session_id, self.local_id)
    }
}

#[derive(Debug, Clone)]
pub enum TermState {
    Running {
        pid: u32,
        started_at: u64,
    },
    Exited {
        exit_code: Option<i32>,
        ended_at: u64,
    },
    Failed {
        error: String,
        ended_at: u64,
    },
    Killed {
        ended_at: u64,
    },
}

impl TermState {
    pub fn is_running(&self) -> bool {
        matches!(self, TermState::Running { .. })
    }

    pub fn to_snapshot(&self) -> TermStateSnapshot {
        match self {
            TermState::Running { .. } => TermStateSnapshot::Running,
            TermState::Exited { exit_code, .. } => TermStateSnapshot::Exited {
                exit_code: *exit_code,
            },
            TermState::Failed { error, .. } => TermStateSnapshot::Failed {
                error: error.clone(),
            },
            TermState::Killed { .. } => TermStateSnapshot::Killed,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum TermStateSnapshot {
    Running,
    Exited { exit_code: Option<i32> },
    Failed { error: String },
    Killed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalScreen {
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<TerminalCell>,
    pub cursor: Option<(u16, u16)>,
    pub alt_screen: bool,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TerminalCell {
    pub chars: String,
    pub fg: TerminalColor,
    pub bg: TerminalColor,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub dim: bool,
    pub wide: bool,
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            chars: String::new(),
            fg: TerminalColor::Default,
            bg: TerminalColor::Default,
            bold: false,
            italic: false,
            underline: false,
            inverse: false,
            dim: false,
            wide: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TerminalColor {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

impl From<vt100::Color> for TerminalColor {
    fn from(c: vt100::Color) -> Self {
        match c {
            vt100::Color::Default => TerminalColor::Default,
            vt100::Color::Idx(i) => TerminalColor::Idx(i),
            vt100::Color::Rgb(r, g, b) => TerminalColor::Rgb(r, g, b),
        }
    }
}

pub fn snapshot_screen(parser: &vt100::Parser) -> TerminalScreen {
    let screen = parser.screen();
    let (rows, cols) = screen.size();
    let mut cells = Vec::with_capacity(rows as usize * cols as usize);
    for row in 0..rows {
        for col in 0..cols {
            if let Some(c) = screen.cell(row, col) {
                cells.push(TerminalCell {
                    chars: c.contents().to_string(),
                    fg: c.fgcolor().into(),
                    bg: c.bgcolor().into(),
                    bold: c.bold(),
                    italic: c.italic(),
                    underline: c.underline(),
                    inverse: c.inverse(),
                    dim: c.dim(),
                    wide: c.is_wide(),
                });
            } else {
                cells.push(TerminalCell::default());
            }
        }
    }
    let cursor = if screen.hide_cursor() {
        None
    } else {
        Some(screen.cursor_position())
    };
    TerminalScreen {
        rows,
        cols,
        cells,
        cursor,
        alt_screen: screen.alternate_screen(),
    }
}

#[derive(Debug, Clone)]
pub enum TermStreamEvent {
    Chunk {
        bytes: Vec<u8>,
        screen: TerminalScreen,
        state: TermState,
    },
    Exited {
        exit_code: Option<i32>,
    },
}

pub struct TermEntry {
    pub handle: TermHandle,
    pub session_id: String,
    pub pty_size: portable_pty::PtySize,
    pub parser: Arc<Mutex<vt100::Parser>>,
    pub writer: Mutex<Box<dyn std::io::Write + Send>>,
    pub state: Arc<Mutex<TermState>>,
    pub stream_tx: broadcast::Sender<TermStreamEvent>,
    pub log_path: PathBuf,
    pub reader_task: Mutex<Option<JoinHandle<()>>>,
    pub child: Mutex<Option<Box<dyn portable_pty::Child + Send + Sync>>>,
    pub master: Mutex<Option<Box<dyn portable_pty::MasterPty + Send>>>,
    pub started_at: Instant,
}

impl TermEntry {
    pub fn snapshot(&self) -> TerminalScreen {
        let parser = self.parser.lock().expect("parser poisoned");
        snapshot_screen(&parser)
    }

    pub fn current_state(&self) -> TermState {
        self.state.lock().expect("state poisoned").clone()
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), RuntimeError> {
        {
            let mut parser = self.parser.lock().expect("parser poisoned");
            parser.screen_mut().set_size(rows, cols);
        }
        let master = self.master.lock().expect("master poisoned");
        if let Some(master) = master.as_ref() {
            master
                .resize(portable_pty::PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| RuntimeError::ToolFailed(format!("term resize: pty resize: {e}")))?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct TermRegistry {
    next_id: AtomicU64,
    entries: Mutex<HashMap<String, Arc<TermEntry>>>,
}

impl TermRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_handle(&self, session_id: &str) -> TermHandle {
        let local_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        TermHandle {
            session_id: session_id.to_string(),
            local_id,
        }
    }

    pub fn insert(&self, entry: Arc<TermEntry>) {
        let key = entry.handle.to_string();
        self.entries
            .lock()
            .expect("entries poisoned")
            .insert(key, entry);
    }

    pub fn get(&self, handle_str: &str) -> Option<Arc<TermEntry>> {
        self.entries
            .lock()
            .expect("entries poisoned")
            .get(handle_str)
            .cloned()
    }

    pub fn lookup(
        &self,
        handle_str: &str,
        session_id: &str,
    ) -> Result<Arc<TermEntry>, RuntimeError> {
        let handle = TermHandle::parse(handle_str).ok_or_else(|| {
            RuntimeError::ToolFailed(format!("term: invalid handle: {handle_str}"))
        })?;
        if handle.session_id != session_id {
            return Err(RuntimeError::ToolFailed(format!(
                "term: handle {handle_str} does not belong to session {session_id}"
            )));
        }
        self.get(handle_str).ok_or_else(|| {
            RuntimeError::ToolFailed(format!("term: handle not found: {handle_str}"))
        })
    }

    pub fn list(&self, session_id: &str) -> Vec<(String, TermState)> {
        self.entries
            .lock()
            .expect("entries poisoned")
            .iter()
            .filter(|(_, e)| e.session_id == session_id)
            .map(|(k, e)| (k.clone(), e.current_state()))
            .collect()
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

const READ_BUF_SIZE: usize = 4096;

impl TermRegistry {
    pub fn spawn_entry(
        self: &Arc<Self>,
        rows: u16,
        cols: u16,
        session_id: String,
        session_dir: PathBuf,
        pty_result: crate::sandbox::PtySpawnResult,
        tui_stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    ) -> Result<(TermHandle, Arc<TermEntry>), RuntimeError> {
        let handle = self.next_handle(&session_id);
        let handle_str = handle.to_string();

        std::fs::create_dir_all(&session_dir).map_err(|e| {
            RuntimeError::ToolFailed(format!("term.spawn: create session_dir: {e}"))
        })?;
        let log_path = session_dir.join(format!("term_{}.log", handle_str));

        let parser = vt100::Parser::new(rows, cols, 0);
        let parser = Arc::new(Mutex::new(parser));
        let state = Arc::new(Mutex::new(TermState::Running {
            pid: 0,
            started_at: now_ms(),
        }));
        let (stream_tx, _stream_rx) = broadcast::channel(STREAM_CHANNEL_CAPACITY);

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| RuntimeError::ToolFailed(format!("term.spawn: open log: {e}")))?;

        let entry = Arc::new(TermEntry {
            handle: handle.clone(),
            session_id: session_id.clone(),
            pty_size: portable_pty::PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            },
            parser: parser.clone(),
            writer: Mutex::new(pty_result.writer),
            state: state.clone(),
            stream_tx: stream_tx.clone(),
            log_path: log_path.clone(),
            reader_task: Mutex::new(None),
            child: Mutex::new(Some(pty_result.child)),
            master: Mutex::new(Some(pty_result.master)),
            started_at: Instant::now(),
        });

        let reader = pty_result.reader;
        let handle_for_loop = handle_str.clone();
        let join = tokio::task::spawn_blocking(move || {
            run_reader_loop(
                reader,
                parser,
                state,
                stream_tx,
                log_file,
                tui_stream_tx,
                handle_for_loop,
            );
        });
        *entry.reader_task.lock().expect("reader_task poisoned") = Some(join);

        self.insert(entry.clone());
        Ok((handle, entry))
    }
}

fn run_reader_loop(
    mut reader: Box<dyn std::io::Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    state: Arc<Mutex<TermState>>,
    stream_tx: broadcast::Sender<TermStreamEvent>,
    mut log_file: std::fs::File,
    tui_stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    handle: String,
) {
    let mut buf = [0u8; READ_BUF_SIZE];
    let mut last_screen: Option<TerminalScreen> = None;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                let _ = log_file.write_all(chunk);
                let screen = {
                    let mut p = parser.lock().expect("parser poisoned");
                    p.process(chunk);
                    snapshot_screen(&p)
                };
                let screen_changed = last_screen.as_ref() != Some(&screen);
                let st = state.lock().expect("state poisoned").clone();
                let _ = stream_tx.send(TermStreamEvent::Chunk {
                    bytes: chunk.to_vec(),
                    screen: screen.clone(),
                    state: st.clone(),
                });
                if let Some(tx) = &tui_stream_tx {
                    let tui_screen = if screen_changed {
                        last_screen = Some(screen.clone());
                        Some(screen)
                    } else {
                        None
                    };
                    let _ = tx.send(crate::stream::StreamFrame::TerminalChunk {
                        handle: handle.clone(),
                        bytes: chunk.to_vec(),
                        screen: tui_screen,
                        state: st.to_snapshot(),
                    });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    let exit_code = None;
    {
        let mut s = state.lock().expect("state poisoned");
        *s = TermState::Exited {
            exit_code,
            ended_at: now_ms(),
        };
    }
    let _ = stream_tx.send(TermStreamEvent::Exited { exit_code });
    if let Some(tx) = &tui_stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::TerminalExited { handle, exit_code });
    }
}

use std::io::Write;

use std::path::Path;

use crate::sandbox::PtySpawnResult;
use crate::tool::{ApprovalLevel, Tier, Tool};
use crate::value::Value;

fn extract_string(
    args: &crate::tool::ToolArgs,
    name: &str,
    pos: usize,
) -> Result<String, RuntimeError> {
    if let Some(v) = args.named(name) {
        if let Value::Str(s) = v {
            return Ok(s.clone());
        }
        return Err(RuntimeError::ToolFailed(format!(
            "term: arg {name} must be string"
        )));
    }
    if let Ok(Value::Str(s)) = args.positional(pos) {
        return Ok(s.clone());
    }
    Err(RuntimeError::MissingArg(format!("term: {name}")))
}

fn extract_optional_string(args: &crate::tool::ToolArgs, name: &str) -> Option<String> {
    args.named(name).and_then(|v| {
        if let Value::Str(s) = v {
            Some(s.clone())
        } else {
            None
        }
    })
}

fn extract_optional_int(args: &crate::tool::ToolArgs, name: &str) -> Option<i64> {
    args.named(name).and_then(|v| {
        if let Value::Int(i) = v {
            Some(*i)
        } else {
            None
        }
    })
}

pub struct TermSpawn;

impl Tool for TermSpawn {
    fn name(&self) -> &str {
        "term.spawn"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Spawn a PTY-backed interactive terminal. Supports TUI apps (vim, top, ssh, codex).\nReturns handle + state + dimensions. Does NOT return screen content — use\nterm.capture to read the screen.\n\nTypical flow:\n1. term.spawn(cmd: \"your command\", rows: 24, cols: 80)\n2. term.input(handle: \"...\", text: \"ls -la\") or key: \"enter\"\n3. term.capture(handle: \"...\") — returns screen as text by default\n4. Repeat 2-3 as needed\n5. term.kill(handle: \"...\")",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": "string"},
                "rows": {"type": "integer", "default": 24},
                "cols": {"type": "integer", "default": 80},
                "cwd": {"type": "string"},
                "env": {"type": "object"}
            }
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move { spawn_impl(args, ctx).await })
    }
}

async fn spawn_impl(
    args: crate::tool::ToolArgs,
    ctx: &crate::tool::ToolCtx,
) -> crate::tool::ToolResult {
    let cmd_str = extract_optional_string(&args, "cmd");
    let rows = extract_optional_int(&args, "rows")
        .map(|v| v as u16)
        .unwrap_or(DEFAULT_ROWS)
        .max(1);
    let cols = extract_optional_int(&args, "cols")
        .map(|v| v as u16)
        .unwrap_or(DEFAULT_COLS)
        .max(2);
    let cwd = extract_optional_string(&args, "cwd")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let env: Vec<(String, String)> = if let Some(Value::Struct(fields)) = args.named("env") {
        fields
            .iter()
            .filter_map(|(k, v)| {
                if let Value::Str(s) = v {
                    Some((k.clone(), s.clone()))
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let registry = ctx
        .term_registry
        .clone()
        .ok_or_else(|| RuntimeError::ToolFailed("term.spawn: registry not available".into()))?;
    let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
    let session_dir = ctx
        .session_dir
        .clone()
        .ok_or_else(|| RuntimeError::ToolFailed("term.spawn: session_dir not available".into()))?;

    let pty_size = portable_pty::PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let default_shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".into());
    let cmd_args: Vec<&str> = if let Some(ref c) = cmd_str {
        vec!["sh", "-c", c.as_str()]
    } else {
        vec![default_shell.as_str()]
    };
    let env_refs: Vec<(String, String)> = env.clone();

    let pty_result = if let Some(sandbox) = &ctx.sandbox {
        match sandbox
            .spawn_pty(&cmd_args, &env_refs, &cwd, pty_size)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("Operation not permitted") || msg.contains("denied") {
                    let outcome = crate::approval::request_approval(
                        ctx,
                        "term.spawn",
                        "term.spawn",
                        &args,
                        ApprovalLevel::Dangerous,
                        None,
                    )
                    .await;
                    match outcome {
                        crate::approval::ApprovalOutcome::Approve => {
                            sandbox
                                .spawn_pty_relaxed(&cmd_args, &env_refs, &cwd, pty_size)
                                .await?
                        }
                        crate::approval::ApprovalOutcome::Deny { reason } => {
                            return Err(RuntimeError::ToolFailed(format!(
                                "term.spawn denied: {reason}"
                            )));
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    } else {
        spawn_pty_direct(&cmd_args, &env_refs, &cwd, pty_size)?
    };

    let (handle, entry) = registry.spawn_entry(
        rows,
        cols,
        session_id,
        session_dir,
        pty_result,
        ctx.stream_tx.clone(),
    )?;

    let state = entry.current_state();
    Ok(Value::Struct(vec![
        ("handle".into(), Value::Str(handle.to_string())),
        ("state".into(), state_to_value(&state)),
        ("rows".into(), Value::Int(rows as i64)),
        ("cols".into(), Value::Int(cols as i64)),
    ]))
}

fn spawn_pty_direct(
    cmd: &[&str],
    env: &[(String, String)],
    cwd: &Path,
    pty_size: portable_pty::PtySize,
) -> Result<PtySpawnResult, RuntimeError> {
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(pty_size)
        .map_err(|e| RuntimeError::ToolFailed(format!("openpty: {e}")))?;
    let mut builder = portable_pty::CommandBuilder::new(cmd[0]);
    for arg in &cmd[1..] {
        builder.arg(arg);
    }
    builder.cwd(cwd);
    for (k, v) in env {
        builder.env(k, v);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| RuntimeError::ToolFailed(format!("pty spawn: {e}")))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| RuntimeError::ToolFailed(format!("pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| RuntimeError::ToolFailed(format!("pty writer: {e}")))?;
    Ok(PtySpawnResult {
        child,
        reader,
        writer,
        master: pair.master,
    })
}

fn screen_to_value(screen: &TerminalScreen) -> Value {
    let cells: Vec<Value> = screen
        .cells
        .iter()
        .map(|c| {
            Value::Struct(vec![
                ("chars".into(), Value::Str(c.chars.clone())),
                ("fg".into(), color_to_value(c.fg)),
                ("bg".into(), color_to_value(c.bg)),
                ("bold".into(), Value::Bool(c.bold)),
                ("italic".into(), Value::Bool(c.italic)),
                ("underline".into(), Value::Bool(c.underline)),
                ("inverse".into(), Value::Bool(c.inverse)),
                ("dim".into(), Value::Bool(c.dim)),
                ("wide".into(), Value::Bool(c.wide)),
            ])
        })
        .collect();
    Value::Struct(vec![
        ("rows".into(), Value::Int(screen.rows as i64)),
        ("cols".into(), Value::Int(screen.cols as i64)),
        ("cells".into(), Value::List(cells)),
        (
            "cursor".into(),
            match screen.cursor {
                Some((r, c)) => Value::Struct(vec![
                    ("row".into(), Value::Int(r as i64)),
                    ("col".into(), Value::Int(c as i64)),
                ]),
                None => Value::Unit,
            },
        ),
        ("alt_screen".into(), Value::Bool(screen.alt_screen)),
    ])
}

fn color_to_value(c: TerminalColor) -> Value {
    match c {
        TerminalColor::Default => Value::Str("default".into()),
        TerminalColor::Idx(i) => Value::Int(i as i64),
        TerminalColor::Rgb(r, g, b) => Value::Struct(vec![
            ("r".into(), Value::Int(r as i64)),
            ("g".into(), Value::Int(g as i64)),
            ("b".into(), Value::Int(b as i64)),
        ]),
    }
}

fn state_to_value(state: &TermState) -> Value {
    match state {
        TermState::Running { pid, started_at } => Value::Struct(vec![
            ("kind".into(), Value::Str("running".into())),
            ("pid".into(), Value::Int(*pid as i64)),
            ("started_at".into(), Value::Int(*started_at as i64)),
        ]),
        TermState::Exited {
            exit_code,
            ended_at,
        } => Value::Struct(vec![
            ("kind".into(), Value::Str("exited".into())),
            (
                "exit_code".into(),
                exit_code
                    .map(|c| Value::Int(c as i64))
                    .unwrap_or(Value::Unit),
            ),
            ("ended_at".into(), Value::Int(*ended_at as i64)),
        ]),
        TermState::Failed { error, ended_at } => Value::Struct(vec![
            ("kind".into(), Value::Str("failed".into())),
            ("error".into(), Value::Str(error.clone())),
            ("ended_at".into(), Value::Int(*ended_at as i64)),
        ]),
        TermState::Killed { ended_at } => Value::Struct(vec![
            ("kind".into(), Value::Str("killed".into())),
            ("ended_at".into(), Value::Int(*ended_at as i64)),
        ]),
    }
}

pub struct TermInput;
impl Tool for TermInput {
    fn name(&self) -> &str {
        "term.input"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Send input to a terminal's PTY. Use `text` for literal text, or `key` for\nspecial keys (enter, tab, esc, backspace, up, down, left, right, ctrl+c,\nctrl+d, ctrl+z). Use key: \"enter\" to submit a command, not text: \"\\r\".",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "text": {"type": "string", "description": "Literal text to write. Do NOT use \\r or \\n here — use key:\"enter\" instead."},
                "key": {"type": "string", "enum": ["enter", "tab", "esc", "backspace", "up", "down", "left", "right", "ctrl+c", "ctrl+d", "ctrl+z"]}
            },
            "required": ["handle"]
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let text = extract_optional_string(&args, "text").unwrap_or_default();
            let key = extract_optional_string(&args, "key");
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.input: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let entry = registry.lookup(&handle, &session_id)?;

            let mut payload = text.into_bytes();
            if let Some(k) = &key {
                payload.extend_from_slice(&key_to_bytes(k));
            }
            if payload.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "term.input: provide at least one of `text` or `key`".into(),
                ));
            }

            let n = {
                let mut w = entry.writer.lock().expect("writer poisoned");
                w.write_all(&payload)
                    .map_err(|e| RuntimeError::ToolFailed(format!("term.input write: {e}")))?;
                payload.len()
            };
            Ok(Value::Struct(vec![
                ("ok".into(), Value::Bool(true)),
                ("bytes_written".into(), Value::Int(n as i64)),
            ]))
        })
    }
}

fn key_to_bytes(key: &str) -> Vec<u8> {
    match key {
        "enter" => vec![b'\r'],
        "tab" => vec![b'\t'],
        "esc" => vec![0x1b],
        "backspace" => vec![0x7f],
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        "ctrl+c" => vec![0x03],
        "ctrl+d" => vec![0x04],
        "ctrl+z" => vec![0x1a],
        _ => Vec::new(),
    }
}

pub struct TermCapture;
impl Tool for TermCapture {
    fn name(&self) -> &str {
        "term.capture"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Read the terminal screen. Default returns plain text (format: \"text\").\nUse start_row/end_row to read only part of the screen — e.g. last 5 rows\nto check the prompt or command output without wasting context.\n\nBest practices:\n- After sending a command, capture to see the result.\n- Use start_row/end_row to read only the relevant part (e.g. last 10 rows).\n- format: \"screen\" returns full cell data with colors/styles — rarely needed,\n  only use when you need to inspect TUI layout or colors.\n- Default format: \"text\" is sufficient for most cases (reading command output,\n  checking prompts, seeing error messages).",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "format": {"type": "string", "enum": ["text", "screen"], "default": "text"},
                "start_row": {"type": "integer", "default": 0, "description": "Start row (0-based). Default 0."},
                "end_row": {"type": "integer", "description": "End row (exclusive). Default: full height."}
            },
            "required": ["handle"]
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let format = extract_optional_string(&args, "format").unwrap_or_else(|| "text".into());
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.capture: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let entry = registry.lookup(&handle, &session_id)?;
            let screen = entry.snapshot();
            let start_row = extract_optional_int(&args, "start_row")
                .unwrap_or(0)
                .clamp(0, screen.rows as i64) as u16;
            let end_row = extract_optional_int(&args, "end_row")
                .unwrap_or(screen.rows as i64)
                .clamp(start_row as i64, screen.rows as i64) as u16;
            let state = entry.current_state();
            let mut fields = vec![
                ("handle".into(), Value::Str(handle.clone())),
                ("state".into(), state_to_value(&state)),
                ("rows".into(), Value::Int(screen.rows as i64)),
                ("cols".into(), Value::Int(screen.cols as i64)),
            ];
            if format == "screen" {
                let cols = screen.cols as usize;
                let start = start_row as usize * cols;
                let end = end_row as usize * cols;
                let cursor = screen.cursor.and_then(|(row, col)| {
                    (start_row..end_row)
                        .contains(&row)
                        .then_some((row - start_row, col))
                });
                let partial_screen = TerminalScreen {
                    rows: end_row - start_row,
                    cols: screen.cols,
                    cells: screen.cells[start..end].to_vec(),
                    cursor,
                    alt_screen: screen.alt_screen,
                };
                fields[2] = ("rows".into(), Value::Int(partial_screen.rows as i64));
                fields.push(("screen".into(), screen_to_value(&partial_screen)));
            } else {
                let parser = entry.parser.lock().expect("parser poisoned");
                let text = parser
                    .screen()
                    .rows(0, screen.cols)
                    .skip(start_row as usize)
                    .take((end_row - start_row) as usize)
                    .collect::<Vec<_>>()
                    .join("\n");
                fields.push(("text".into(), Value::Str(text)));
            }
            Ok(Value::Struct(fields))
        })
    }
}

pub struct TermResize;
impl Tool for TermResize {
    fn name(&self) -> &str {
        "term.resize"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "rows": {"type": "integer"},
                "cols": {"type": "integer"}
            },
            "required": ["handle", "rows", "cols"]
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let rows = extract_optional_int(&args, "rows")
                .ok_or_else(|| RuntimeError::MissingArg("rows".into()))?
                as u16;
            let cols = extract_optional_int(&args, "cols")
                .ok_or_else(|| RuntimeError::MissingArg("cols".into()))?
                as u16;
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.resize: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let entry = registry.lookup(&handle, &session_id)?;
            entry.resize(rows, cols)?;
            Ok(Value::Struct(vec![
                ("ok".into(), Value::Bool(true)),
                ("rows".into(), Value::Int(rows as i64)),
                ("cols".into(), Value::Int(cols as i64)),
            ]))
        })
    }
}

pub struct TermKill;
impl Tool for TermKill {
    fn name(&self) -> &str {
        "term.kill"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"handle": {"type": "string"}},
            "required": ["handle"]
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let handle = extract_string(&args, "handle", 0)?;
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.kill: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let entry = registry.lookup(&handle, &session_id)?;
            {
                let mut child = entry.child.lock().expect("child poisoned");
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                }
            }
            {
                let mut state = entry.state.lock().expect("state poisoned");
                *state = TermState::Killed { ended_at: now_ms() };
            }
            Ok(Value::Struct(vec![
                ("ok".into(), Value::Bool(true)),
                ("state".into(), Value::Str("killed".into())),
            ]))
        })
    }
}

pub struct TermList;
impl Tool for TermList {
    fn name(&self) -> &str {
        "term.list"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"all": {"type": "boolean", "default": false}}
        })
    }
    fn call<'a>(
        &'a self,
        args: crate::tool::ToolArgs,
        ctx: &'a crate::tool::ToolCtx,
    ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
        Box::pin(async move {
            let all = args
                .named("all")
                .and_then(|v| {
                    if let Value::Bool(b) = v {
                        Some(*b)
                    } else {
                        None
                    }
                })
                .unwrap_or(false);
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.list: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let _ = all;
            let list = registry.list(&session_id);
            let entries: Vec<Value> = list
                .iter()
                .map(|(h, st)| {
                    Value::Struct(vec![
                        ("handle".into(), Value::Str(h.clone())),
                        ("state".into(), state_to_value(st)),
                    ])
                })
                .collect();
            Ok(Value::Struct(vec![(
                "terminals".into(),
                Value::List(entries),
            )]))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handle_parse_roundtrip() {
        let h = TermHandle {
            session_id: "abc".into(),
            local_id: 7,
        };
        assert_eq!(h.to_string(), "term_abc_7");
        let back = TermHandle::parse("term_abc_7").unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn handle_parse_rejects_bad_format() {
        assert!(TermHandle::parse("not_term").is_none());
        assert!(TermHandle::parse("term_nosuffix").is_none());
        assert!(TermHandle::parse("term_x_notnum").is_none());
    }

    #[test]
    fn snapshot_screen_captures_text() {
        let mut parser = vt100::Parser::new(3, 5, 0);
        parser.process(b"hello");
        let screen = snapshot_screen(&parser);
        assert_eq!(screen.rows, 3);
        assert_eq!(screen.cols, 5);
        assert_eq!(screen.cells.len(), 15);
        assert_eq!(screen.cells[0].chars, "h");
        assert_eq!(screen.cells[4].chars, "o");
    }

    #[test]
    fn registry_lookup_rejects_cross_session() {
        let registry = Arc::new(TermRegistry::new());
        let h = registry.next_handle("session_a");
        let entry = Arc::new(TermEntry {
            handle: h.clone(),
            session_id: "session_a".into(),
            pty_size: portable_pty::PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
            writer: Mutex::new(Box::new(std::io::sink())),
            state: Arc::new(Mutex::new(TermState::Running {
                pid: 0,
                started_at: 0,
            })),
            stream_tx: broadcast::channel(STREAM_CHANNEL_CAPACITY).0,
            log_path: std::env::temp_dir().join("term_test_dummy.log"),
            reader_task: Mutex::new(None),
            child: Mutex::new(None),
            master: Mutex::new(None),
            started_at: Instant::now(),
        });
        registry.insert(entry);
        assert!(registry.lookup(&h.to_string(), "session_a").is_ok());
        assert!(registry.lookup(&h.to_string(), "session_b").is_err());
    }
}
