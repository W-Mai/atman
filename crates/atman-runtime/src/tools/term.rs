#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::error::RuntimeError;

pub const DEFAULT_ROWS: u16 = 24;
pub const DEFAULT_COLS: u16 = 80;
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
    #[serde(default)]
    pub wide_continuation: bool,
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
            wide_continuation: false,
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
                    wide_continuation: c.is_wide_continuation(),
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

/// Parse a string containing ANSI escape sequences into a [TerminalScreen].
///
/// Used by the TUI to render ` ```ansi ` code blocks with real colors.
/// cols is sized to the widest line (clamped 80..=256) so nothing wraps.
///
/// `\n` is upgraded to `\r\n` because vt100 treats bare LF as "line feed"
/// (move down, keep column) — most ANSI output expects carriage return too.
/// Trailing newlines are stripped so they don't trigger a scroll that
/// pushes the first line off the visible screen.
pub fn parse_ansi_to_screen(body: &str) -> TerminalScreen {
    let body = body.strip_suffix('\n').unwrap_or(body);
    let lines: Vec<&str> = body.split('\n').collect();
    let rows = lines.len().max(1) as u16;
    let cols = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(80, 256) as u16;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            parser.process(b"\r\n");
        }
        parser.process(line.as_bytes());
    }
    snapshot_screen(&parser)
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
    pub task_id: Mutex<Option<crate::task_registry::TaskId>>,
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
    entries: Mutex<HashMap<String, Arc<TermEntry>>>,
    task_registry: Option<crate::task_registry::TaskRegistry>,
}

impl TermRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_task_registry(mut self, tr: crate::task_registry::TaskRegistry) -> Self {
        self.task_registry = Some(tr);
        self
    }

    pub fn next_handle(&self, session_id: &str) -> TermHandle {
        let local_id = uuid::Uuid::now_v7().as_u64_pair().0;
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

    pub fn kill_all(&self) {
        let entries = self.entries.lock().expect("entries poisoned");
        for (_, entry) in entries.iter() {
            let mut child = entry.child.lock().expect("child poisoned");
            if let Some(child) = child.as_mut() {
                let _ = child.kill();
            }
        }
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
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_entry(
        self: &Arc<Self>,
        rows: u16,
        cols: u16,
        session_id: String,
        session_dir: PathBuf,
        pty_result: crate::sandbox::PtySpawnResult,
        tui_stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
        label: String,
        cancel: tokio_util::sync::CancellationToken,
        events: Option<crate::event::EventSink>,
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
            task_id: Mutex::new(None),
        });

        let kill_entry = entry.clone();
        let task_id = self.task_registry.as_ref().map(|tr| {
            let hook: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
                let mut child = kill_entry.child.lock().expect("child poisoned");
                if let Some(child) = child.as_mut() {
                    let _ = child.kill();
                }
            });
            tr.register_with_kill_hook(
                crate::task_registry::TaskKind::Terminal,
                label,
                handle_str.clone(),
                session_id.clone(),
                cancel,
                Some(hook),
            )
        });
        *entry.task_id.lock().unwrap() = task_id.clone();

        let reader = pty_result.reader;
        let handle_for_loop = handle_str.clone();
        let task_registry = self.task_registry.clone();
        let join = tokio::task::spawn_blocking(move || {
            run_reader_loop(
                reader,
                parser,
                state,
                stream_tx,
                log_file,
                tui_stream_tx,
                handle_for_loop,
                task_registry,
                task_id,
                events,
            );
        });
        *entry.reader_task.lock().expect("reader_task poisoned") = Some(join);

        self.insert(entry.clone());
        Ok((handle, entry))
    }
}

impl Drop for TermRegistry {
    fn drop(&mut self) {
        self.kill_all();
    }
}

#[allow(clippy::too_many_arguments)]
fn run_reader_loop(
    mut reader: Box<dyn std::io::Read + Send>,
    parser: Arc<Mutex<vt100::Parser>>,
    state: Arc<Mutex<TermState>>,
    stream_tx: broadcast::Sender<TermStreamEvent>,
    mut log_file: std::fs::File,
    tui_stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    handle: String,
    task_registry: Option<crate::task_registry::TaskRegistry>,
    task_id: Option<crate::task_registry::TaskId>,
    events_sink: Option<crate::event::EventSink>,
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
        // Only transition to Exited if not already Killed — term.kill sets
        // Killed before the reader loop observes the closed PTY.
        if matches!(*s, TermState::Running { .. }) {
            *s = TermState::Exited {
                exit_code,
                ended_at: now_ms(),
            };
        }
    }

    // Persist the last screen state before notifying exit — restore reads this
    // to recover the full cell grid (with colors) that stream frames never wrote.
    if let Some(sink) = &events_sink {
        let (final_screen, final_state) = {
            let p = parser.lock().expect("parser poisoned");
            let screen = snapshot_screen(&p);
            let st = state.lock().expect("state poisoned").to_snapshot();
            (screen, st)
        };
        sink.emit(crate::event::Event::TerminalFinalState {
            handle: handle.clone(),
            screen: final_screen,
            state: final_state,
        });
    }

    let _ = stream_tx.send(TermStreamEvent::Exited { exit_code });
    if let Some(tx) = &tui_stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::TerminalExited { handle, exit_code });
    }

    if let (Some(tr), Some(tid)) = (task_registry, task_id) {
        tr.finish(&tid, crate::task_registry::TaskStatus::Ok);
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
        cmd_str.unwrap_or_else(|| "terminal".into()),
        {
            let tc = ctx.cancel.clone();
            tc.child_token()
        },
        ctx.events.clone(),
    )?;

    let state = entry.current_state();
    let text = {
        let parser = entry.parser.lock().expect("parser poisoned");
        parser.screen().contents()
    };
    Ok(Value::Struct(vec![
        ("handle".into(), Value::Str(handle.to_string())),
        ("state".into(), state_to_value(&state)),
        ("rows".into(), Value::Int(rows as i64)),
        ("cols".into(), Value::Int(cols as i64)),
        ("text".into(), Value::Str(text)),
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

fn cell_text(screen: &TerminalScreen, row: u16, col: u16) -> String {
    let idx = (row as usize) * (screen.cols as usize) + (col as usize);
    match screen.cells.get(idx) {
        Some(cell) if cell.wide_continuation => String::new(),
        Some(cell) if cell.chars.is_empty() => " ".to_string(),
        Some(cell) => cell.chars.clone(),
        None => " ".to_string(),
    }
}

fn find_pattern_on_screen(screen: &TerminalScreen, pattern: &str) -> Option<(u16, u16)> {
    for r in 0..screen.rows {
        let row_text: String = (0..screen.cols)
            .map(|c| cell_text(screen, r, c))
            .collect::<String>()
            .trim_end()
            .to_string();
        if let Some(pos) = row_text.find(pattern) {
            return Some((r, pos as u16));
        }
    }
    None
}

impl crate::watch::Watchable for TermEntry {
    fn watch_output(
        self: std::sync::Arc<Self>,
        pattern: String,
        cancel: tokio_util::sync::CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::watch::WatchResult> + Send>>
    {
        let stream_tx = self.stream_tx.clone();
        let state = self.state.clone();
        let parser = self.parser.clone();
        Box::pin(async move {
            // Subscribe BEFORE checking state — closes the TOCTOU window where
            // the terminal exits between the check and subscribe().
            let mut rx = stream_tx.subscribe();
            {
                let st = state.lock().unwrap().clone();
                if !st.is_running() {
                    let p = parser.lock().unwrap();
                    let screen = snapshot_screen(&p);
                    if let Some((r, c)) = find_pattern_on_screen(&screen, &pattern) {
                        return crate::watch::WatchResult::Matched {
                            row: Some(r),
                            col: Some(c),
                            text: pattern,
                        };
                    }
                    return crate::watch::WatchResult::SourceExited;
                }
            }
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return crate::watch::WatchResult::Cancelled,
                    result = rx.recv() => match result {
                        Ok(crate::tools::term::TermStreamEvent::Chunk { screen, .. }) => {
                            if let Some((r, c)) = find_pattern_on_screen(&screen, &pattern) {
                                return crate::watch::WatchResult::Matched {
                                    row: Some(r),
                                    col: Some(c),
                                    text: pattern,
                                };
                            }
                        }
                        _ => return crate::watch::WatchResult::SourceExited,
                    }
                }
            }
        })
    }
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
                ("wide_continuation".into(), Value::Bool(c.wide_continuation)),
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
            "Send input to a terminal's PTY. Use `text` for literal text, `key` for\nspecial keys (enter, tab, esc, backspace, up, down, left, right, ctrl+c,\nctrl+d, ctrl+z), or `mouse` for mouse events. Use key: \"enter\" to\nsubmit a command, not text: \"\\r\". Mouse actions: click, double_click,\nlong_press, drag, press, release, move, scroll_up, scroll_down.\nUse term.find to locate text on screen before clicking.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "text": {"type": "string", "description": "Literal text to write. Do NOT use \\r or \\n here — use key:\"enter\" instead."},
                "key": {"type": "string", "enum": ["enter", "tab", "esc", "backspace", "up", "down", "left", "right", "ctrl+c", "ctrl+d", "ctrl+z"]},
                "mouse": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["click", "double_click", "long_press", "drag", "press", "release", "move", "scroll_up", "scroll_down"],
                            "description": "click=press+release, double_click=two clicks, long_press=press+500ms+release, drag=press+move+release (needs x2,y2), move=hover, scroll_up/down=wheel"
                        },
                        "button": {"type": "string", "enum": ["left", "right", "middle"], "default": "left"},
                        "x": {"type": "integer", "description": "Column (0-indexed, same as term.find col)"},
                        "y": {"type": "integer", "description": "Row (0-indexed, same as term.find row)"},
                        "x2": {"type": "integer", "description": "End column for drag"},
                        "y2": {"type": "integer", "description": "End row for drag"}
                    },
                    "required": ["action", "x", "y"],
                    "description": "Mouse event. Coordinates are 0-indexed — pass term.find (col,row) directly."
                }
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
            let mouse = args.named("mouse");
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.input: registry not available".into())
            })?;
            let session_id = ctx.session_id.clone().unwrap_or_else(|| "anon".into());
            let entry = registry.lookup(&handle, &session_id)?;

            // Build steps: (bytes, delay_after).
            let mut steps: Vec<(Vec<u8>, std::time::Duration)> = Vec::new();
            let mut first = text.into_bytes();
            if let Some(k) = &key {
                first.extend_from_slice(&key_to_bytes(k));
            }
            if !first.is_empty() {
                steps.push((first, std::time::Duration::ZERO));
            }
            if let Some(m) = &mouse {
                steps.extend(mouse_to_steps(m)?);
            }
            if steps.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "term.input: provide at least one of `text`, `key`, or `mouse`".into(),
                ));
            }

            let mut total = 0usize;
            for (payload, delay) in steps {
                if !payload.is_empty() {
                    let mut w = entry.writer.lock().expect("writer poisoned");
                    w.write_all(&payload)
                        .map_err(|e| RuntimeError::ToolFailed(format!("term.input write: {e}")))?;
                    total += payload.len();
                    drop(w);
                }
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
            Ok(Value::Struct(vec![
                ("ok".into(), Value::Bool(true)),
                ("bytes_written".into(), Value::Int(total as i64)),
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

/// SGR mouse bytes. Input coords are 0-indexed (term.find convention);
/// SGR protocol is 1-indexed so we +1 internally.
fn sgr(btn: u32, action: &str, col0: i64, row0: i64) -> Vec<u8> {
    let x = col0 + 1;
    let y = row0 + 1;
    match action {
        "press" => format!("\x1b[<{btn};{x};{y}M").into_bytes(),
        "release" => format!("\x1b[<{btn};{x};{y}m").into_bytes(),
        "move" => format!("\x1b[<{btn};{x};{y}M", btn = btn + 32).into_bytes(),
        _ => Vec::new(),
    }
}

/// Build (bytes, delay) steps from a mouse parameter.
/// Coords are 0-indexed — pass term.find (col, row) directly as (x, y).
fn mouse_to_steps(val: &Value) -> Result<Vec<(Vec<u8>, std::time::Duration)>, RuntimeError> {
    let fields = match val {
        Value::Struct(f) => f,
        _ => {
            return Err(RuntimeError::ToolFailed(
                "term.input: mouse must be an object".into(),
            ));
        }
    };
    let get_str = |name: &str| -> Result<&str, RuntimeError> {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| {
                if let Value::Str(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .ok_or_else(|| RuntimeError::ToolFailed(format!("term.input: mouse.{name} missing")))
    };
    let get_int = |name: &str| -> Result<i64, RuntimeError> {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| {
                if let Value::Int(i) = v {
                    Some(*i)
                } else {
                    None
                }
            })
            .ok_or_else(|| RuntimeError::ToolFailed(format!("term.input: mouse.{name} missing")))
    };
    let get_opt_int = |name: &str| -> Option<i64> {
        fields.iter().find(|(k, _)| k == name).and_then(|(_, v)| {
            if let Value::Int(i) = v {
                Some(*i)
            } else {
                None
            }
        })
    };

    let action = get_str("action")?;
    let button_str = get_opt_str(fields, "button").unwrap_or("left");
    let x = get_int("x")?;
    let y = get_int("y")?;
    let btn: u32 = match button_str {
        "left" => 0,
        "middle" => 1,
        "right" => 2,
        _ => {
            return Err(RuntimeError::ToolFailed(format!(
                "term.input: mouse.button must be left/right/middle, got {button_str}"
            )));
        }
    };

    let z = std::time::Duration::ZERO;
    let gap = std::time::Duration::from_millis(50);
    let long = std::time::Duration::from_millis(500);

    match action {
        "press" => Ok(vec![(sgr(btn, "press", x, y), z)]),
        "release" => Ok(vec![(sgr(btn, "release", x, y), z)]),
        "click" => Ok(vec![
            (sgr(btn, "press", x, y), z),
            (sgr(btn, "release", x, y), z),
        ]),
        "double_click" => Ok(vec![
            (sgr(btn, "press", x, y), z),
            (sgr(btn, "release", x, y), gap),
            (sgr(btn, "press", x, y), z),
            (sgr(btn, "release", x, y), z),
        ]),
        "long_press" => Ok(vec![
            (sgr(btn, "press", x, y), long),
            (sgr(btn, "release", x, y), z),
        ]),
        "drag" => {
            let x2 = get_opt_int("x2").ok_or_else(|| {
                RuntimeError::ToolFailed("term.input: mouse.drag requires x2".into())
            })?;
            let y2 = get_opt_int("y2").ok_or_else(|| {
                RuntimeError::ToolFailed("term.input: mouse.drag requires y2".into())
            })?;
            let mut steps = vec![(sgr(btn, "press", x, y), z)];
            let dx = x2 - x;
            let dy = y2 - y;
            let n = dx.unsigned_abs().max(dy.unsigned_abs());
            for i in 1..=n {
                let cx = x + dx * i as i64 / n as i64;
                let cy = y + dy * i as i64 / n as i64;
                steps.push((sgr(btn, "move", cx, cy), z));
            }
            steps.push((sgr(btn, "release", x2, y2), z));
            Ok(steps)
        }
        "move" => Ok(vec![(sgr(0, "move", x, y), z)]),
        "scroll_up" => Ok(vec![(
            format!("\x1b[<64;{};{}M", x + 1, y + 1).into_bytes(),
            z,
        )]),
        "scroll_down" => Ok(vec![(
            format!("\x1b[<65;{};{}M", x + 1, y + 1).into_bytes(),
            z,
        )]),
        _ => Err(RuntimeError::ToolFailed(format!(
            "term.input: mouse.action must be click/double_click/long_press/drag/press/release/move/scroll_up/scroll_down, got {action}"
        ))),
    }
}

fn get_opt_str<'a>(fields: &'a [(String, Value)], name: &'a str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == name).and_then(|(_, v)| {
        if let Value::Str(s) = v {
            Some(s.as_str())
        } else {
            None
        }
    })
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
            "Read the terminal screen. Default returns plain text (format: \"text\").\n\
             Use start_row/end_row and start_col/end_col to read a rectangular region.\n\n\
             Best practices:\n\
             - After sending a command, capture to see the result.\n\
             - Use start_row/end_row to read only the relevant part (e.g. last 10 rows).\n\
             - Use start_col/end_col to read a column range (e.g. skip line numbers).\n\
             - format: \"screen\" returns full cell data with colors/styles.\n\
             - Default format: \"text\" is sufficient for most cases.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "format": {"type": "string", "enum": ["text", "screen"], "default": "text"},
                "start_row": {"type": "integer", "default": 0, "description": "Start row (0-based). Default 0."},
                "end_row": {"type": "integer", "description": "End row (exclusive). Default: full height."},
                "start_col": {"type": "integer", "default": 0, "description": "Start column (0-based). Default 0."},
                "end_col": {"type": "integer", "description": "End column (exclusive). Default: full width."}
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
            let start_col = extract_optional_int(&args, "start_col")
                .unwrap_or(0)
                .clamp(0, screen.cols as i64) as u16;
            let end_col = extract_optional_int(&args, "end_col")
                .unwrap_or(screen.cols as i64)
                .clamp(start_col as i64, screen.cols as i64) as u16;
            let state = entry.current_state();
            let mut fields = vec![
                ("handle".into(), Value::Str(handle.clone())),
                ("state".into(), state_to_value(&state)),
                ("rows".into(), Value::Int((end_row - start_row) as i64)),
                ("cols".into(), Value::Int((end_col - start_col) as i64)),
            ];
            if format == "screen" {
                let sub_rows = end_row - start_row;
                let sub_cols = end_col - start_col;
                let sub_cells: Vec<TerminalCell> = (start_row..end_row)
                    .flat_map(|r| {
                        (start_col..end_col)
                            .map(|c| {
                                screen
                                    .cells
                                    .get((r as usize) * (screen.cols as usize) + (c as usize))
                                    .cloned()
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect();
                let cursor = screen.cursor.and_then(|(row, col)| {
                    if (start_row..end_row).contains(&row) && (start_col..end_col).contains(&col) {
                        Some((row - start_row, col - start_col))
                    } else {
                        None
                    }
                });
                let partial_screen = TerminalScreen {
                    rows: sub_rows,
                    cols: sub_cols,
                    cells: sub_cells,
                    cursor,
                    alt_screen: screen.alt_screen,
                };
                fields.push(("screen".into(), screen_to_value(&partial_screen)));
            } else {
                let text = (start_row..end_row)
                    .map(|r| {
                        let row_text: String = (start_col..end_col)
                            .map(|c| cell_text(&screen, r, c))
                            .collect();
                        row_text.trim_end().to_string()
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                fields.push(("text".into(), Value::Str(text)));
            }
            Ok(Value::Struct(fields))
        })
    }
}

pub struct TermFind;
impl Tool for TermFind {
    fn name(&self) -> &str {
        "term.find"
    }
    fn tier(&self) -> Tier {
        Tier::Four
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Search terminal screen for text and/or style. Returns list of matches.\n\n\
             - pattern: substring to search for (case-sensitive). Omit for style-only search.\n\
             - style: optional filter. Only cells matching ALL specified style fields count.\n\
             - Combine both: find red 'error' text, bold prompts, etc.\n\n\
             Style fields: bold, italic, underline, inverse, dim, fg, bg.\n\
             Colors: name (\"red\") or {r,g,b} struct (discover via term.capture format=\"screen\").\n\n\
             Returns: { matches: [{row, col, text}], count: N }",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "handle": {"type": "string"},
                "pattern": {"type": "string", "description": "Substring to search for. Omit for style-only search."},
                "style": {
                    "type": "object",
                    "properties": {
                        "bold": {"type": "boolean"},
                        "italic": {"type": "boolean"},
                        "underline": {"type": "boolean"},
                        "inverse": {"type": "boolean"},
                        "dim": {"type": "boolean"},
                        "fg": {"type": "string", "description": "Color name or {r,g,b} struct"},
                        "bg": {"type": "string", "description": "Color name or {r,g,b} struct"}
                    },
                    "description": "Optional style filter. All specified fields must match."
                },
                "start_row": {"type": "integer", "default": 0},
                "end_row": {"type": "integer", "description": "Exclusive. Default: full height."}
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
            let pattern = extract_optional_string(&args, "pattern");
            let style_filter = parse_style_filter(&args)?;
            if pattern.is_none() && style_filter.is_none() {
                return Err(RuntimeError::ToolFailed(
                    "term.find: provide at least pattern or style".into(),
                ));
            }
            let registry = ctx.term_registry.clone().ok_or_else(|| {
                RuntimeError::ToolFailed("term.find: registry not available".into())
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

            let mut matches = Vec::new();
            for r in start_row..end_row {
                let row_cells: Vec<&TerminalCell> =
                    (0..screen.cols).map(|c| cell_ref(&screen, r, c)).collect();
                let row_text: String = row_cells
                    .iter()
                    .map(|c| {
                        if c.wide_continuation {
                            ""
                        } else {
                            c.chars.as_str()
                        }
                    })
                    .collect::<String>();
                let row_text_trimmed = row_text.trim_end();

                if let Some(ref pat) = pattern {
                    let mut start = 0;
                    while let Some(pos) = row_text_trimmed[start..].find(pat) {
                        let abs_col = start + pos;
                        let style_ok = style_filter
                            .as_ref()
                            .map(|sf| sf.matches(row_cells.get(abs_col).copied()))
                            .unwrap_or(true);
                        if style_ok {
                            let matched_text = pat.clone();
                            matches.push(Value::Struct(vec![
                                ("row".into(), Value::Int(r as i64)),
                                ("col".into(), Value::Int(abs_col as i64)),
                                ("text".into(), Value::Str(matched_text)),
                            ]));
                        }
                        start += pos + pat.len();
                        if start >= row_text_trimmed.len() {
                            break;
                        }
                    }
                } else if let Some(ref sf) = style_filter {
                    let mut col = 0usize;
                    while col < screen.cols as usize {
                        if sf.matches(row_cells.get(col).copied())
                            && !row_cells[col].chars.is_empty()
                        {
                            let start_col = col;
                            let mut text = String::new();
                            while col < screen.cols as usize
                                && sf.matches(row_cells.get(col).copied())
                                && !row_cells[col].wide_continuation
                            {
                                text.push_str(&row_cells[col].chars);
                                col += 1;
                            }
                            if !text.trim().is_empty() {
                                matches.push(Value::Struct(vec![
                                    ("row".into(), Value::Int(r as i64)),
                                    ("col".into(), Value::Int(start_col as i64)),
                                    ("text".into(), Value::Str(text)),
                                ]));
                            }
                        } else {
                            col += 1;
                        }
                    }
                }
            }
            let count = matches.len() as i64;
            Ok(Value::Struct(vec![
                ("matches".into(), Value::List(matches)),
                ("count".into(), Value::Int(count)),
            ]))
        })
    }
}

fn cell_ref(screen: &TerminalScreen, row: u16, col: u16) -> &TerminalCell {
    let idx = (row as usize) * (screen.cols as usize) + (col as usize);
    screen.cells.get(idx).unwrap_or(&DEFAULT_CELL)
}

static DEFAULT_CELL: TerminalCell = TerminalCell {
    chars: String::new(),
    fg: TerminalColor::Default,
    bg: TerminalColor::Default,
    bold: false,
    italic: false,
    underline: false,
    inverse: false,
    dim: false,
    wide: false,
    wide_continuation: false,
};

struct StyleFilter {
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    inverse: Option<bool>,
    dim: Option<bool>,
    fg: Option<TerminalColor>,
    bg: Option<TerminalColor>,
}

impl StyleFilter {
    fn matches(&self, cell: Option<&TerminalCell>) -> bool {
        let Some(cell) = cell else {
            return false;
        };
        if self.bold == Some(true) && !cell.bold {
            return false;
        }
        if self.bold == Some(false) && cell.bold {
            return false;
        }
        if self.italic == Some(true) && !cell.italic {
            return false;
        }
        if self.italic == Some(false) && cell.italic {
            return false;
        }
        if self.underline == Some(true) && !cell.underline {
            return false;
        }
        if self.underline == Some(false) && cell.underline {
            return false;
        }
        if self.inverse == Some(true) && !cell.inverse {
            return false;
        }
        if self.inverse == Some(false) && cell.inverse {
            return false;
        }
        if self.dim == Some(true) && !cell.dim {
            return false;
        }
        if self.dim == Some(false) && cell.dim {
            return false;
        }
        if let Some(fg) = &self.fg {
            if !color_matches(fg, &cell.fg) {
                return false;
            }
        }
        if let Some(bg) = &self.bg {
            if !color_matches(bg, &cell.bg) {
                return false;
            }
        }
        true
    }
}

fn parse_style_filter(args: &crate::tool::ToolArgs) -> Result<Option<StyleFilter>, RuntimeError> {
    let style = match args.named("style") {
        Some(v) => v,
        None => return Ok(None),
    };
    Ok(Some(StyleFilter {
        bold: get_optional_bool(style, "bold"),
        italic: get_optional_bool(style, "italic"),
        underline: get_optional_bool(style, "underline"),
        inverse: get_optional_bool(style, "inverse"),
        dim: get_optional_bool(style, "dim"),
        fg: get_optional_color(style, "fg")?,
        bg: get_optional_color(style, "bg")?,
    }))
}

fn get_optional_bool(style: &Value, field: &str) -> Option<bool> {
    match style.field(field) {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

fn get_optional_color(style: &Value, field: &str) -> Result<Option<TerminalColor>, RuntimeError> {
    match style.field(field) {
        Some(v) => parse_color(v).map(Some).ok_or_else(|| {
            RuntimeError::ToolFailed(format!(
                "term.find: invalid color for '{field}' — use name (\"red\") or {{r,g,b}} struct"
            ))
        }),
        None => Ok(None),
    }
}

fn parse_color(val: &Value) -> Option<TerminalColor> {
    match val {
        Value::Str(name) => color_name_to_terminal(name),
        Value::Struct(fields) => {
            let r = fields
                .iter()
                .find(|(k, _)| k == "r")
                .and_then(|(_, v)| match v {
                    Value::Int(n) => Some(*n),
                    _ => None,
                })?;
            let g = fields
                .iter()
                .find(|(k, _)| k == "g")
                .and_then(|(_, v)| match v {
                    Value::Int(n) => Some(*n),
                    _ => None,
                })?;
            let b = fields
                .iter()
                .find(|(k, _)| k == "b")
                .and_then(|(_, v)| match v {
                    Value::Int(n) => Some(*n),
                    _ => None,
                })?;
            Some(TerminalColor::Rgb(r as u8, g as u8, b as u8))
        }
        _ => None,
    }
}

fn color_name_to_terminal(name: &str) -> Option<TerminalColor> {
    let idx = match name {
        "default" => return Some(TerminalColor::Default),
        "black" => 0,
        "red" => 1,
        "green" => 2,
        "yellow" => 3,
        "blue" => 4,
        "magenta" => 5,
        "cyan" => 6,
        "white" => 7,
        _ => return None,
    };
    Some(TerminalColor::Idx(idx))
}

fn color_matches(desired: &TerminalColor, actual: &TerminalColor) -> bool {
    match (desired, actual) {
        (TerminalColor::Default, TerminalColor::Default) => true,
        (TerminalColor::Idx(d), TerminalColor::Idx(a)) => *d == *a || (*d < 8 && *a == *d + 8),
        (TerminalColor::Rgb(dr, dg, db), TerminalColor::Rgb(ar, ag, ab)) => {
            dr == ar && dg == ag && db == ab
        }
        _ => false,
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
    fn description(&self) -> Option<&str> {
        Some("Resize a terminal's PTY dimensions. Sends SIGWINCH to the child process.")
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
    fn description(&self) -> Option<&str> {
        Some("Kill a terminal process. The terminal handle remains in the registry for history.")
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
    fn description(&self) -> Option<&str> {
        Some("List all terminal handles in the current session.")
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
            task_id: Mutex::new(None),
        });
        registry.insert(entry);
        assert!(registry.lookup(&h.to_string(), "session_a").is_ok());
        assert!(registry.lookup(&h.to_string(), "session_b").is_err());
    }

    fn make_screen(rows: u16, cols: u16, text: &str) -> TerminalScreen {
        let mut parser = vt100::Parser::new(rows, cols, 0);
        parser.process(text.as_bytes());
        snapshot_screen(&parser)
    }

    #[test]
    fn cell_text_returns_chars_for_filled_cell() {
        let screen = make_screen(1, 5, "hello");
        assert_eq!(cell_text(&screen, 0, 0), "h");
        assert_eq!(cell_text(&screen, 0, 4), "o");
    }

    #[test]
    fn cell_text_returns_space_for_empty_cell() {
        let screen = make_screen(1, 5, "hi");
        assert_eq!(cell_text(&screen, 0, 2), " ");
    }

    #[test]
    fn find_pattern_returns_position() {
        let screen = make_screen(2, 10, "hello\r\nworld");
        let pos = find_pattern_on_screen(&screen, "world");
        assert_eq!(pos, Some((1, 0)));
    }

    #[test]
    fn find_pattern_returns_none_when_absent() {
        let screen = make_screen(1, 5, "hello");
        assert!(find_pattern_on_screen(&screen, "xyz").is_none());
    }

    #[test]
    fn color_name_maps_to_ansi_idx() {
        assert_eq!(color_name_to_terminal("red"), Some(TerminalColor::Idx(1)));
        assert_eq!(
            color_name_to_terminal("default"),
            Some(TerminalColor::Default)
        );
        assert_eq!(color_name_to_terminal("nonexistent"), None);
    }

    #[test]
    fn color_matches_bright_variant_to_base_name() {
        let desired = TerminalColor::Idx(1);
        let actual_bright = TerminalColor::Idx(9);
        assert!(color_matches(&desired, &actual_bright));
        let actual_normal = TerminalColor::Idx(1);
        assert!(color_matches(&desired, &actual_normal));
    }

    #[test]
    fn color_matches_rgb_exact() {
        let desired = TerminalColor::Rgb(255, 128, 0);
        let actual = TerminalColor::Rgb(255, 128, 0);
        assert!(color_matches(&desired, &actual));
        let wrong = TerminalColor::Rgb(255, 128, 1);
        assert!(!color_matches(&desired, &wrong));
    }

    #[test]
    fn parse_color_accepts_name_and_rgb() {
        assert_eq!(
            parse_color(&Value::Str("blue".into())),
            Some(TerminalColor::Idx(4))
        );
        let rgb = Value::Struct(vec![
            ("r".into(), Value::Int(10)),
            ("g".into(), Value::Int(20)),
            ("b".into(), Value::Int(30)),
        ]);
        assert_eq!(parse_color(&rgb), Some(TerminalColor::Rgb(10, 20, 30)));
    }
}
