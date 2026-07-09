use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::{broadcast, mpsc};

pub mod app;
pub mod approval_bar;
pub mod clipboard;
pub mod compact_review_modal;
pub mod completion;
pub mod highlight;
pub mod history;
pub mod history_search_modal;
pub mod humanize;
pub mod input;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod output;
pub mod palette;
pub mod session_switcher;
pub mod sidebar;
pub mod status;
pub mod terminal_guard;

use app::{AppState, NoteLevel};
use atman_runtime::stream::StreamFrame;
use input::{InputEditor, input_paragraph};
use keys::{KeyAction, map as map_key};
use terminal_guard::TerminalGuard;

pub enum TuiNote {
    Info(String),
    Warn(String),
    Error(String),
}

impl TuiNote {
    fn into_parts(self) -> (String, NoteLevel) {
        match self {
            Self::Info(t) => (t, NoteLevel::Info),
            Self::Warn(t) => (t, NoteLevel::Warn),
            Self::Error(t) => (t, NoteLevel::Error),
        }
    }
}

pub enum TuiControl {
    CancelFlow,
    ApproveTool(String),
    DenyTool {
        tool_use_id: String,
        reason: String,
    },
    ApproveAllPending,
    DenyAllPending {
        reason: String,
    },
    CompactNow,
    CompactReviewAccept {
        review_id: String,
        edited: Option<String>,
    },
    CompactReviewReject {
        review_id: String,
    },
    SwitchSession(String),
    DeleteSession(String),
    RenameSession {
        session_id: String,
        title: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct SessionPickerRow {
    pub id: String,
    pub project: Option<String>,
    pub message_count: usize,
    pub updated_at: String,
    pub goal: Option<String>,
}

pub enum TuiCommand {
    SetSidebar(sidebar::SidebarMode),
}

pub struct TuiHandle {
    pub session_id: String,
    pub session_dir: String,
    pub goal: Option<String>,
    pub stream_rx: broadcast::Receiver<StreamFrame>,
    pub submit_tx: Option<mpsc::UnboundedSender<String>>,
    pub note_rx: Option<mpsc::UnboundedReceiver<TuiNote>>,
    pub shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    pub control_tx: Option<mpsc::UnboundedSender<TuiControl>>,
    pub cmd_rx: Option<mpsc::UnboundedReceiver<TuiCommand>>,
    pub initial_items: Vec<app::OutputItem>,
    pub goal_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    pub context_rx: Option<tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
    pub attach_rx: Option<tokio::sync::watch::Receiver<usize>>,
    pub todos_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>>,
    pub plans_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>>,
    pub approvals_rx:
        Option<tokio::sync::watch::Receiver<Vec<atman_runtime::session::PendingApproval>>>,
    pub compact_review_rx:
        Option<tokio::sync::watch::Receiver<Option<atman_runtime::PendingCompactReview>>>,
    pub flow_names: Vec<(String, String)>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
}

impl TuiHandle {
    pub fn from_session(session: std::sync::Arc<atman_runtime::Session>) -> Self {
        Self {
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            goal: session.goal(),
            stream_rx: session.stream_subscribe(),
            submit_tx: None,
            note_rx: None,
            shutdown_rx: None,
            control_tx: None,
            cmd_rx: None,
            initial_items: Vec::new(),
            goal_rx: Some(session.subscribe_goal()),
            context_rx: Some(session.subscribe_context()),
            attach_rx: Some(session.subscribe_attach()),
            todos_rx: Some(session.subscribe_todos()),
            plans_rx: Some(session.subscribe_plans()),
            approvals_rx: Some(session.subscribe_pending_approvals()),
            compact_review_rx: Some(session.compact_reviews().subscribe()),
            flow_names: Vec::new(),
            session: Some(session),
        }
    }
}

pub async fn run_tui(handle: TuiHandle) -> Result<()> {
    let _guard = TerminalGuard::install()?;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    run_frames(&mut terminal, handle).await
}

async fn run_frames(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: TuiHandle,
) -> Result<()> {
    let mut app = AppState::new(handle.session_id.clone(), handle.goal.clone())
        .with_initial_items(std::mem::take(&mut handle.initial_items))
        .with_session_dir(handle.session_dir.clone())
        .with_flow_names(std::mem::take(&mut handle.flow_names))
        .with_session(handle.session.clone());
    if let Some(rx) = handle.context_rx.as_ref() {
        app.context = rx.borrow().clone();
    }
    if let Some(rx) = handle.goal_rx.as_ref() {
        app.goal = rx.borrow().clone();
    }
    if let Some(rx) = handle.attach_rx.as_ref() {
        app.attach_count = *rx.borrow();
    }
    if let Some(rx) = handle.todos_rx.as_ref() {
        app.todos = rx.borrow().clone();
    }
    if let Some(rx) = handle.plans_rx.as_ref() {
        app.plans = rx.borrow().clone();
    }
    if let Some(rx) = handle.approvals_rx.as_ref() {
        app.pending_approvals = rx.borrow().clone();
    }
    let mut editor = InputEditor::default();
    let (mut key_events, reader_shutdown) = spawn_event_reader();
    let mut interrupt_prompt = false;
    let mut shutdown = handle.shutdown_rx.take();
    let mut sigterm = build_sigterm_stream();
    let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let _reader_guard = ReaderGuard(reader_shutdown);
    loop {
        terminal.draw(|f| render_frame(f, &mut app, &editor))?;

        if app.should_quit {
            break;
        }

        tokio::select! {
            biased;
            _ = wait_shutdown(shutdown.as_mut()) => {
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            _ = wait_sigterm(sigterm.as_mut()) => {
                break;
            }
            _ = animation_tick.tick(), if app.has_running_workflow() => {
                app.animation_frame = app.animation_frame.wrapping_add(1);
            }
            key = key_events.recv() => {
                if std::env::var_os("ATMAN_TRACE_EVENTS").is_some() {
                    eprintln!("[atman] event: {key:?}");
                }
                let mut current = key;
                let mut scroll_delta: i32 = 0;
                let mut drained: u32 = 0;
                loop {
                    match current {
                        Some(Ok(CtEvent::Mouse(me)))
                            if matches!(me.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) =>
                        {
                            if matches!(me.kind, MouseEventKind::ScrollUp) {
                                scroll_delta = scroll_delta.saturating_sub(3);
                            } else {
                                scroll_delta = scroll_delta.saturating_add(3);
                            }
                            interrupt_prompt = false;
                        }
                        Some(Ok(CtEvent::Key(ke))) => {
                            handle_key(
                                map_key(ke),
                                &mut app,
                                &mut editor,
                                &mut interrupt_prompt,
                                handle.submit_tx.as_ref(),
                                handle.control_tx.as_ref(),
                            );
                        }
                        Some(Ok(CtEvent::Paste(s))) => {
                            editor.insert_str(&s);
                            interrupt_prompt = false;
                            app.refresh_popup(editor.buf());
                        }
                        Some(Ok(CtEvent::Mouse(me))) => {
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                if let Some((panel_idx, node_id)) =
                                    app.hit_test_node(me.column, me.row)
                                {
                                    app.toggle_workflow_node(panel_idx, &node_id);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::WorkflowPanel { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_workflow_panel_expansion(idx);
                                }
                            }
                            interrupt_prompt = false;
                        }
                        Some(Ok(CtEvent::Resize(_, _))) => {}
                        _ => {}
                    }
                    drained = drained.saturating_add(1);
                    if drained >= 100 {
                        break;
                    }
                    match key_events.try_recv() {
                        Ok(next) => current = Some(next),
                        Err(_) => break,
                    }
                }
                if scroll_delta < 0 {
                    app.scroll_up((-scroll_delta) as u16);
                } else if scroll_delta > 0 {
                    app.scroll_down(scroll_delta as u16);
                }
            }
            frame = handle.stream_rx.recv() => {
                match frame {
                    Ok(frame) => {
                        app.apply_stream_frame(frame);
                        interrupt_prompt = false;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        app.record_lag(n, std::time::Instant::now());
                        interrupt_prompt = false;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            note = recv_note(handle.note_rx.as_mut()) => {
                if let Some(n) = note {
                    let (text, level) = n.into_parts();
                    app.push_note(text, level);
                }
            }
            _ = wait_goal_change(handle.goal_rx.as_mut()) => {
                if let Some(rx) = handle.goal_rx.as_mut() {
                    app.goal = rx.borrow().clone();
                }
            }
            _ = wait_context_change(handle.context_rx.as_mut()) => {
                if let Some(rx) = handle.context_rx.as_mut() {
                    app.context = rx.borrow().clone();
                }
            }
            _ = wait_attach_change(handle.attach_rx.as_mut()) => {
                if let Some(rx) = handle.attach_rx.as_mut() {
                    app.attach_count = *rx.borrow();
                }
            }
            _ = wait_todos_change(handle.todos_rx.as_mut()) => {
                if let Some(rx) = handle.todos_rx.as_mut() {
                    app.todos = rx.borrow().clone();
                }
            }
            _ = wait_plans_change(handle.plans_rx.as_mut()) => {
                if let Some(rx) = handle.plans_rx.as_mut() {
                    app.plans = rx.borrow().clone();
                }
            }
            _ = wait_approvals_change(handle.approvals_rx.as_mut()) => {
                if let Some(rx) = handle.approvals_rx.as_mut() {
                    app.pending_approvals = rx.borrow().clone();
                }
            }
            _ = wait_compact_review_change(handle.compact_review_rx.as_mut()) => {
                if let Some(rx) = handle.compact_review_rx.as_mut() {
                    let latest = rx.borrow().clone();
                    match (latest, app.compact_review.is_some()) {
                        (Some(pending), false) => {
                            app.compact_review = Some(
                                crate::compact_review_modal::CompactReviewModal::new(pending),
                            );
                        }
                        (Some(pending), true) => {
                            if app
                                .compact_review
                                .as_ref()
                                .is_some_and(|m| m.pending.review_id != pending.review_id)
                            {
                                app.compact_review = Some(
                                    crate::compact_review_modal::CompactReviewModal::new(pending),
                                );
                            }
                        }
                        (None, _) => {
                            app.compact_review = None;
                        }
                    }
                }
            }
            cmd = recv_cmd(handle.cmd_rx.as_mut()) => {
                if let Some(cmd) = cmd {
                    match cmd {
                        TuiCommand::SetSidebar(mode) => {
                            app.sidebar_mode = mode;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

async fn recv_cmd(rx: Option<&mut mpsc::UnboundedReceiver<TuiCommand>>) -> Option<TuiCommand> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn wait_goal_change(rx: Option<&mut tokio::sync::watch::Receiver<Option<String>>>) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_context_change(
    rx: Option<&mut tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_attach_change(rx: Option<&mut tokio::sync::watch::Receiver<usize>>) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_todos_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_plans_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_approvals_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::session::PendingApproval>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

async fn wait_compact_review_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Option<atman_runtime::PendingCompactReview>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(unix)]
fn build_sigterm_stream() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
fn build_sigterm_stream() -> Option<()> {
    None
}

#[cfg(unix)]
async fn wait_sigterm(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            let _ = s.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn wait_sigterm(_sig: Option<&mut ()>) {
    std::future::pending().await
}

async fn recv_note(rx: Option<&mut mpsc::UnboundedReceiver<TuiNote>>) -> Option<TuiNote> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

struct ReaderGuard(Arc<AtomicBool>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

fn spawn_event_reader() -> (
    tokio::sync::mpsc::UnboundedReceiver<std::io::Result<CtEvent>>,
    Arc<AtomicBool>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = shutdown.clone();
    std::thread::Builder::new()
        .name("atman-tui-input".into())
        .spawn(move || {
            loop {
                if shutdown_for_thread.load(Ordering::SeqCst) {
                    break;
                }
                match crossterm::event::poll(std::time::Duration::from_millis(50)) {
                    Ok(true) => match crossterm::event::read() {
                        Ok(ev) => {
                            if tx.send(Ok(ev)).is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(_) => {}
                }
            }
        })
        .expect("spawn tui input thread");
    (rx, shutdown)
}

async fn wait_shutdown(rx: Option<&mut tokio::sync::oneshot::Receiver<()>>) {
    match rx {
        Some(r) => {
            let _ = r.await;
        }
        None => std::future::pending().await,
    }
}

fn yank_candidate_indices(app: &AppState) -> Vec<usize> {
    app.items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| match it {
            app::OutputItem::AssistantMd { .. } | app::OutputItem::UserTurn { .. } => Some(i),
            _ => None,
        })
        .collect()
}

fn emit_yank_selection_note(app: &mut AppState, cands: &[usize]) {
    let total = cands.len();
    let cur = app.yank_index.min(total.saturating_sub(1)) + 1;
    let kind = cands
        .get(app.yank_index)
        .and_then(|i| app.items.get(*i))
        .map(|it| match it {
            app::OutputItem::AssistantMd { .. } => "assistant",
            app::OutputItem::UserTurn { .. } => "user",
            _ => "other",
        })
        .unwrap_or("?");
    app.push_note(format!("yank {cur}/{total} — {kind}"), app::NoteLevel::Info);
}

fn yank_selected_text(app: &AppState) -> Option<String> {
    let cands = yank_candidate_indices(app);
    let item_idx = *cands.get(app.yank_index)?;
    match app.items.get(item_idx)? {
        app::OutputItem::AssistantMd { md, .. } => Some(md.clone()),
        app::OutputItem::UserTurn { text } => Some(text.clone()),
        _ => None,
    }
}

fn enumerate_session_rows(
    app: &AppState,
    scope: crate::session_switcher::SessionScope,
) -> Vec<crate::SessionPickerRow> {
    let Some(session) = &app.session else {
        return Vec::new();
    };
    let session_dir = session.dir();
    let Some(sessions_root) = session_dir.parent() else {
        return Vec::new();
    };
    let current_fp = session.meta().and_then(|m| m.project_fingerprint);
    let restrict_to_project = matches!(scope, crate::session_switcher::SessionScope::Project);
    let mut rows = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        if sid == session.id().to_string() {
            continue;
        }
        let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
        let peer_fp = meta.as_ref().and_then(|m| m.project_fingerprint.clone());
        let is_legacy = peer_fp.is_none();
        if restrict_to_project
            && let Some(current_fp) = current_fp.as_ref()
            && !is_legacy
            && peer_fp.as_deref() != Some(current_fp.as_str())
        {
            continue;
        }
        let project = if is_legacy {
            Some("(legacy)".into())
        } else {
            meta.as_ref()
                .and_then(|m| m.project_root.as_ref())
                .map(|p| p.display().to_string())
        };
        let events_path = entry.path().join("events.jsonl");
        let updated_at = std::fs::metadata(&events_path)
            .and_then(|m| m.modified())
            .or_else(|_| entry.metadata().and_then(|m| m.modified()))
            .ok()
            .map(|st| {
                let ts: chrono::DateTime<chrono::Local> = st.into();
                ts.to_rfc3339()
            })
            .unwrap_or_default();
        let (user_count, total_count) = count_message_events(&events_path);
        if user_count == 0 {
            continue;
        }
        rows.push(crate::SessionPickerRow {
            id: sid,
            project,
            message_count: total_count,
            updated_at,
            goal: meta.as_ref().and_then(|m| m.title.clone()),
        });
    }
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows.truncate(200);
    rows
}

fn count_message_events(path: &std::path::Path) -> (usize, usize) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut user = 0usize;
    let mut total = 0usize;
    for l in contents.lines() {
        let is_user = l.contains("\"type\":\"user_msg\"");
        let is_assistant = l.contains("\"type\":\"assistant_msg\"");
        let is_tool = l.contains("\"type\":\"tool_result_msg\"");
        if is_user {
            user += 1;
        }
        if is_user || is_assistant || is_tool {
            total += 1;
        }
    }
    (user, total)
}

fn handle_history_search_key(action: &KeyAction, app: &mut AppState) {
    use crate::history_search_modal::{HistoryHit, HistorySearchScope};
    match action {
        KeyAction::Escape => app.history_search.close(),
        KeyAction::HistoryUp | KeyAction::CursorLeft => {
            app.history_search.move_up();
            refresh_history_preview(app);
        }
        KeyAction::HistoryDown | KeyAction::CursorRight => {
            app.history_search.move_down();
            refresh_history_preview(app);
        }
        KeyAction::Tab => {
            app.history_search.scope = app.history_search.scope.toggle();
        }
        KeyAction::Submit => {
            let query = app.history_search.editor.buf().trim().to_string();
            if query.is_empty() {
                app.history_search.set_error("empty query".into());
                return;
            }
            let Some(session) = app.session.as_ref() else {
                app.history_search.set_error("no session in context".into());
                return;
            };
            let Some(idx) = session.project_index() else {
                app.history_search
                    .set_error("project index unavailable".into());
                return;
            };
            let session_filter = match app.history_search.scope {
                HistorySearchScope::Session => Some(session.id().to_string()),
                HistorySearchScope::Project => None,
            };
            let rows = match idx.fts_search_project_events(&query, session_filter.as_deref(), 50) {
                Ok(rows) => rows,
                Err(e) => {
                    app.history_search.set_error(format!("search failed: {e}"));
                    return;
                }
            };
            let hits: Vec<HistoryHit> = rows
                .into_iter()
                .map(|row| {
                    let snippet: String = row
                        .payload
                        .chars()
                        .take(200)
                        .collect::<String>()
                        .replace('\n', " ");
                    HistoryHit {
                        session_id: row.session_id,
                        seq: row.seq,
                        ts: row.ts,
                        kind: row.kind,
                        snippet,
                    }
                })
                .collect();
            app.history_search.set_results(hits, query);
            refresh_history_preview(app);
        }
        KeyAction::Char(c) => {
            app.history_search.editor.insert_char(*c);
        }
        KeyAction::Backspace => {
            app.history_search.editor.backspace();
        }
        _ => {}
    }
}

fn refresh_history_preview(app: &mut AppState) {
    let (session_id, seq) = match app.history_search.selected_hit() {
        Some(hit) => (hit.session_id.clone(), hit.seq),
        None => {
            app.history_search.set_preview(Vec::new());
            return;
        }
    };
    let Some(session) = app.session.as_ref() else {
        return;
    };
    let Some(idx) = session.project_index() else {
        return;
    };
    let rows = match idx.find_project_events_around(&session_id, seq, 3) {
        Ok(r) => r,
        Err(_) => {
            app.history_search.set_preview(Vec::new());
            return;
        }
    };
    let lines: Vec<String> = rows
        .into_iter()
        .filter_map(|row| {
            let is_hit = row.seq == seq;
            let text = extract_event_text(&row.payload);
            if text.is_none() && !is_hit {
                return None;
            }
            let marker = if is_hit { "▶" } else { " " };
            let snippet: String = text
                .unwrap_or_else(|| format!("<{}>", row.kind))
                .chars()
                .take(180)
                .collect::<String>()
                .replace('\n', " ");
            Some(format!("{marker} [{}] {}: {}", row.seq, row.kind, snippet))
        })
        .collect();
    app.history_search.set_preview(lines);
}

fn extract_event_text(payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let parts = v.get("message")?.get("parts")?.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

fn handle_session_switcher_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    if app.session_switcher.rename_mode {
        match action {
            KeyAction::Escape => {
                app.session_switcher.cancel_rename();
                app.push_note("rename cancelled", app::NoteLevel::Info);
            }
            KeyAction::Submit => {
                if let Some((sid, title)) = app.session_switcher.commit_rename() {
                    if let Some(tx) = control_tx {
                        let _ = tx.send(TuiControl::RenameSession {
                            session_id: sid.clone(),
                            title: title.clone(),
                        });
                    }
                    let msg = match &title {
                        Some(t) => format!("renamed {sid} → {t}"),
                        None => format!("cleared title on {sid}"),
                    };
                    app.push_note(msg, app::NoteLevel::Info);
                }
            }
            KeyAction::Backspace => app.session_switcher.rename_pop(),
            KeyAction::Char(c) => app.session_switcher.rename_push(*c),
            _ => {}
        }
        return;
    }
    if app.session_switcher.filter_mode {
        match action {
            KeyAction::Escape | KeyAction::Submit => {
                app.session_switcher.leave_filter_mode();
            }
            KeyAction::Backspace => app.session_switcher.filter_pop(),
            KeyAction::Char(c) => app.session_switcher.filter_push(*c),
            _ => {}
        }
        return;
    }
    if let KeyAction::Char('d') | KeyAction::Char('D') = action {
        if app.session_switcher.delete_armed_matches_selected() {
            if let Some(sid) = app.session_switcher.remove_selected() {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::DeleteSession(sid.clone()));
                }
                app.push_note(format!("deleted session {sid}"), app::NoteLevel::Info);
            }
        } else {
            let armed = app.session_switcher.arm_delete().map(str::to_owned);
            match armed {
                Some(sid) => app.push_note(
                    format!("press d again to confirm delete {sid}"),
                    app::NoteLevel::Warn,
                ),
                None => app.push_note("no session selected", app::NoteLevel::Warn),
            }
        }
        return;
    }
    if app.session_switcher.delete_armed.is_some() {
        app.session_switcher.clear_delete_arm();
        app.push_note("delete cancelled", app::NoteLevel::Info);
    }
    if let KeyAction::Char('s') | KeyAction::Char('S') = action {
        app.session_switcher.toggle_sort();
        return;
    }
    if let KeyAction::Char('f') | KeyAction::Char('F') = action {
        app.session_switcher.enter_filter_mode();
        return;
    }
    if let KeyAction::Char('r') | KeyAction::Char('R') = action {
        if app.session_switcher.begin_rename().is_none() {
            app.push_note("no session selected", app::NoteLevel::Warn);
        }
        return;
    }
    match action {
        KeyAction::Escape => app.session_switcher.close(),
        KeyAction::HistoryUp | KeyAction::CursorLeft => app.session_switcher.move_up(),
        KeyAction::HistoryDown | KeyAction::CursorRight => app.session_switcher.move_down(),
        KeyAction::Tab => {
            let new_scope = app.session_switcher.scope.toggle();
            let rows = enumerate_session_rows(app, new_scope);
            app.session_switcher.scope = new_scope;
            app.session_switcher.set_rows(rows);
        }
        KeyAction::Submit => {
            if let Some(sid) = app.session_switcher.selected_id() {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::SwitchSession(sid.clone()));
                    app.push_note(format!("switching to session {sid}…"), app::NoteLevel::Info);
                }
                app.session_switcher.close();
            }
        }
        _ => {}
    }
}

fn handle_palette_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    match action {
        KeyAction::Escape => app.palette.close(),
        KeyAction::HistoryUp | KeyAction::CursorLeft => app.palette.move_up(),
        KeyAction::HistoryDown | KeyAction::CursorRight => app.palette.move_down(),
        KeyAction::Backspace => app.palette.backspace(),
        KeyAction::Char(c) => app.palette.push_char(*c),
        KeyAction::Submit => {
            if let Some(id) = app.palette.selected() {
                app.palette.close();
                dispatch_palette_entry(id, app, control_tx);
            }
        }
        _ => {}
    }
}

fn dispatch_palette_entry(
    id: crate::palette::PaletteEntryId,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    use crate::palette::PaletteEntryId;
    match id {
        PaletteEntryId::YankMode => {
            let cands = yank_candidate_indices(app);
            if cands.is_empty() {
                app.push_note("nothing to yank yet", app::NoteLevel::Warn);
                return;
            }
            app.yank_mode = true;
            app.yank_index = cands.len().saturating_sub(1);
            app.push_note(
                "yank mode — j/k to move, Enter to copy, Esc to cancel",
                app::NoteLevel::Info,
            );
        }
        PaletteEntryId::CopyLastMessage => copy_last_message(app),
        PaletteEntryId::CopyLastTool => copy_last_tool(app),
        PaletteEntryId::CompactNow => {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::CompactNow);
                app.push_note("requested transcript compaction", app::NoteLevel::Info);
            }
        }
        PaletteEntryId::SwitchSession => {
            let scope = crate::session_switcher::SessionScope::Project;
            let rows = enumerate_session_rows(app, scope);
            app.session_switcher.open_with(rows, scope);
        }
        PaletteEntryId::SearchHistory => {
            app.history_search.open();
        }
        PaletteEntryId::ToggleSidebar => {
            app.sidebar_mode = app.sidebar_mode.toggle();
        }
        PaletteEntryId::ShowHelp => {
            app.cheatsheet_open = true;
        }
    }
}

fn copy_last_message(app: &mut AppState) {
    let text = app.items.iter().rev().find_map(|item| match item {
        app::OutputItem::AssistantMd { md, .. } => Some(md.clone()),
        _ => None,
    });
    match text {
        Some(t) if !t.is_empty() => {
            let n = t.chars().count();
            crate::clipboard::write_osc52(&t);
            app.push_note(
                format!("copied {n} chars from last message"),
                app::NoteLevel::Info,
            );
        }
        _ => app.push_note("no assistant message to copy", app::NoteLevel::Warn),
    }
}

fn copy_last_tool(app: &mut AppState) {
    let text = app.items.iter().rev().find_map(|item| match item {
        app::OutputItem::AssistantMd { md, .. } => Some(md.clone()),
        _ => None,
    });
    match text {
        Some(t) if !t.is_empty() => {
            crate::clipboard::write_osc52(&t);
            app.push_note("copied last tool output", app::NoteLevel::Info);
        }
        _ => app.push_note("no tool output to copy", app::NoteLevel::Warn),
    }
}

fn handle_yank_key(action: &KeyAction, app: &mut AppState) -> bool {
    let cands = yank_candidate_indices(app);
    if cands.is_empty() {
        app.yank_mode = false;
        return true;
    }
    match action {
        KeyAction::Escape => {
            app.yank_mode = false;
            app.push_note("yank cancelled", app::NoteLevel::Info);
            true
        }
        KeyAction::Char('y') | KeyAction::Char('Y') => {
            app.yank_mode = false;
            true
        }
        KeyAction::Char('j') | KeyAction::HistoryDown | KeyAction::CursorRight => {
            app.yank_index = (app.yank_index + 1).min(cands.len().saturating_sub(1));
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('k') | KeyAction::HistoryUp | KeyAction::CursorLeft => {
            app.yank_index = app.yank_index.saturating_sub(1);
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('g') => {
            app.yank_index = 0;
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('G') => {
            app.yank_index = cands.len().saturating_sub(1);
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Submit => {
            if let Some(text) = yank_selected_text(app) {
                let n = text.chars().count();
                crate::clipboard::write_osc52(&text);
                app.push_note(
                    format!("yanked {n} chars to clipboard (OSC 52)"),
                    app::NoteLevel::Info,
                );
            } else {
                app.push_note("yank: nothing selected", app::NoteLevel::Warn);
            }
            app.yank_mode = false;
            true
        }
        _ => true,
    }
}

fn handle_compact_review_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    let Some(tx) = control_tx else { return };
    let Some(modal) = app.compact_review.as_mut() else {
        return;
    };
    use crate::compact_review_modal::CompactReviewMode;
    match modal.mode {
        CompactReviewMode::Viewing => match action {
            KeyAction::Submit => {
                let review_id = modal.pending.review_id.clone();
                let edited = if modal.summary_is_dirty() {
                    Some(modal.edited_summary())
                } else {
                    None
                };
                let _ = tx.send(TuiControl::CompactReviewAccept { review_id, edited });
                app.compact_review = None;
            }
            KeyAction::Char('e') => modal.enter_editing(),
            KeyAction::Char('r') | KeyAction::Escape => {
                let review_id = modal.pending.review_id.clone();
                let _ = tx.send(TuiControl::CompactReviewReject { review_id });
                app.compact_review = None;
            }
            KeyAction::PageUp => modal.scroll_up(),
            KeyAction::PageDown => modal.scroll_down(),
            _ => {}
        },
        CompactReviewMode::Editing => match action {
            KeyAction::Escape => modal.leave_editing(),
            KeyAction::Char(c) => modal.editor.insert_char(*c),
            KeyAction::Backspace => modal.editor.backspace(),
            KeyAction::DeleteWordBackward => modal.editor.delete_word_backward(),
            KeyAction::Newline | KeyAction::Submit => modal.editor.insert_newline(),
            KeyAction::CursorLeft => modal.editor.move_left(),
            KeyAction::CursorRight => modal.editor.move_right(),
            KeyAction::CursorHome => modal.editor.move_home(),
            KeyAction::CursorEnd => modal.editor.move_end(),
            _ => {}
        },
    }
}

fn is_approval_key(action: &KeyAction) -> bool {
    matches!(
        action,
        KeyAction::Char('1'..='9')
            | KeyAction::Char('a')
            | KeyAction::Char('A')
            | KeyAction::Char('d')
            | KeyAction::Char('D')
            | KeyAction::Escape
    )
}

fn handle_approval_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) -> bool {
    let Some(tx) = control_tx else {
        return false;
    };
    let queue = &app.pending_approvals;
    match action {
        KeyAction::Char(c) => match c {
            '1'..='9' => {
                let idx = (*c as u8 - b'1') as usize;
                if let Some(p) = queue.get(idx) {
                    let _ = tx.send(TuiControl::ApproveTool(p.tool_use_id.clone()));
                    app.push_note(
                        format!("approved {} ({})", p.tool_name, p.tool_use_id),
                        app::NoteLevel::Info,
                    );
                }
                true
            }
            'a' | 'A' => {
                let _ = tx.send(TuiControl::ApproveAllPending);
                app.push_note(
                    format!("approved all {} pending", queue.len()),
                    app::NoteLevel::Info,
                );
                true
            }
            'd' | 'D' => {
                if let Some(p) = queue.first() {
                    let _ = tx.send(TuiControl::DenyTool {
                        tool_use_id: p.tool_use_id.clone(),
                        reason: "denied by user".into(),
                    });
                    app.push_note(format!("denied {}", p.tool_name), app::NoteLevel::Warn);
                }
                true
            }
            _ => false,
        },
        KeyAction::Escape => {
            let _ = tx.send(TuiControl::DenyAllPending {
                reason: "user pressed Esc".into(),
            });
            app.push_note(
                format!("denied all {} pending", queue.len()),
                app::NoteLevel::Warn,
            );
            true
        }
        _ => false,
    }
}

fn handle_key(
    action: KeyAction,
    app: &mut AppState,
    editor: &mut InputEditor,
    interrupt_prompt: &mut bool,
    submit_tx: Option<&mpsc::UnboundedSender<String>>,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    if app.compact_review.is_some() {
        handle_compact_review_key(&action, app, control_tx);
        return;
    }
    if app.session_switcher.open {
        handle_session_switcher_key(&action, app, control_tx);
        return;
    }
    if app.history_search.open {
        handle_history_search_key(&action, app);
        return;
    }
    if app.palette.open {
        handle_palette_key(&action, app, control_tx);
        return;
    }
    if let KeyAction::OpenCommandPalette = action {
        app.palette.open();
        return;
    }
    if app.cheatsheet_open {
        match action {
            KeyAction::Escape | KeyAction::HelpModal => app.cheatsheet_open = false,
            KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
        return;
    }
    if app.popup.is_open() {
        match &action {
            KeyAction::Escape => {
                app.popup.close();
                return;
            }
            KeyAction::HistoryUp => {
                app.popup.prev();
                return;
            }
            KeyAction::HistoryDown => {
                app.popup.next();
                return;
            }
            KeyAction::Tab => {
                if let Some(item) = app.popup.accept() {
                    editor.replace_with(&item.insert);
                }
                app.refresh_popup(editor.buf());
                return;
            }
            _ => {
                app.popup.close();
            }
        }
    }
    if !app.pending_approvals.is_empty() && is_approval_key(&action) {
        handle_approval_key(&action, app, control_tx);
        return;
    }
    if app.yank_mode && handle_yank_key(&action, app) {
        return;
    }
    let mut edited = false;
    match action {
        KeyAction::Char(c) => {
            editor.insert_char(c);
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::OpenCommandPalette => {
            app.palette.open();
            *interrupt_prompt = false;
        }
        KeyAction::Backspace => {
            editor.backspace();
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::DeleteWordBackward => {
            editor.delete_word_backward();
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::Newline => {
            editor.insert_newline();
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::Submit => {
            if let Some(line) = editor.submit() {
                app.push_user_turn(line.clone());
                if let Some(tx) = submit_tx {
                    let _ = tx.send(line);
                }
            }
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::HistoryUp => {
            editor.history_up();
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::HistoryDown => {
            editor.history_down();
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::CursorLeft => {
            editor.move_left();
            *interrupt_prompt = false;
        }
        KeyAction::CursorRight => {
            editor.move_right();
            *interrupt_prompt = false;
        }
        KeyAction::CursorHome => {
            editor.move_home();
            *interrupt_prompt = false;
        }
        KeyAction::CursorEnd => {
            editor.move_end();
            *interrupt_prompt = false;
        }
        KeyAction::Tab => {
            *interrupt_prompt = false;
        }
        KeyAction::NudgePrefill => {
            editor.prefill("/nudge ");
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::CoursePrefill => {
            editor.prefill("/course ");
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::RedirectPrefill => {
            editor.prefill("/redirect ");
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::HardStop => {
            editor.prefill("/hard-stop ");
            *interrupt_prompt = false;
            edited = true;
        }
        KeyAction::ScrollUp | KeyAction::PageUp => {
            app.scroll_up(if matches!(action, KeyAction::PageUp) {
                10
            } else {
                1
            });
            *interrupt_prompt = false;
        }
        KeyAction::ScrollDown | KeyAction::PageDown => {
            app.scroll_down(if matches!(action, KeyAction::PageDown) {
                10
            } else {
                1
            });
            *interrupt_prompt = false;
        }
        KeyAction::Home => {
            app.scroll_to_top();
            *interrupt_prompt = false;
        }
        KeyAction::End => {
            app.scroll_to_tail();
            *interrupt_prompt = false;
        }
        KeyAction::Escape => {
            if app.streaming {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CancelFlow);
                }
                app.push_note("cancel requested", app::NoteLevel::Warn);
            } else if !editor.buf().is_empty() {
                editor.clear();
                edited = true;
            }
            *interrupt_prompt = false;
        }
        KeyAction::ToggleSidebar => {
            app.sidebar_mode = app.sidebar_mode.toggle();
            *interrupt_prompt = false;
        }
        KeyAction::ToggleMouseCapture => {
            let now_on = app.toggle_mouse_capture();
            if let Err(e) = crate::terminal_guard::set_mouse_capture(now_on) {
                app.push_note(
                    format!("mouse capture toggle failed: {e}"),
                    app::NoteLevel::Warn,
                );
            } else if !now_on && !app.select_mode_hinted {
                app.push_note(
                    "SELECT MODE — drag mouse to copy; press F3 to resume interaction",
                    app::NoteLevel::Info,
                );
                app.select_mode_hinted = true;
            }
            *interrupt_prompt = false;
        }
        KeyAction::ToggleLastTool => {
            app.toggle_last_tool_expansion();
            *interrupt_prompt = false;
        }
        KeyAction::HelpModal => {
            app.cheatsheet_open = true;
            *interrupt_prompt = false;
        }
        KeyAction::Interrupt => {
            if *interrupt_prompt {
                app.should_quit = true;
            } else {
                *interrupt_prompt = true;
                app.push_note("press Ctrl+C again to quit", app::NoteLevel::Warn);
            }
        }
        KeyAction::Quit => {
            app.should_quit = true;
        }
        KeyAction::Ignore => {
            *interrupt_prompt = false;
        }
    }
    if edited {
        app.refresh_popup(editor.buf());
    }
}

fn render_frame(f: &mut ratatui::Frame, app: &mut AppState, editor: &InputEditor) {
    let area = f.area();
    if area.width < 40 || area.height < 8 {
        let msg = Paragraph::new(Line::from("terminal too small (need 40×8)"))
            .style(Style::default().fg(Color::Yellow))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }
    let input_height = compute_input_height(editor.buf(), area.width);
    let wide_enough = area.width >= layout::SIDEBAR_MIN_TOTAL_WIDTH;
    let show_sidebar = app.sidebar_mode.resolve(wide_enough);
    let compact_status = !show_sidebar;
    let status_height = if compact_status { 2 } else { 1 };
    let pending_count = app.pending_approvals.len();
    let approvals_rows: u16 = if pending_count == 0 {
        0
    } else {
        (pending_count.min(9) as u16).saturating_add(1)
    };
    let l = layout::compute_ex(
        area,
        input_height,
        show_sidebar,
        status_height,
        approvals_rows,
    );
    f.render_widget(
        status::render_bar(status::StatusInputs {
            session_id: &app.session_id,
            goal: app.goal.as_deref(),
            streaming: app.streaming,
            context: &app.context,
            attach_count: app.attach_count,
            include_compact_line: compact_status,
        }),
        l.status,
    );
    let transcript_area = l.transcript;
    app.last_transcript_rect = Some(transcript_area);
    if app.items.is_empty() {
        app.resolve_scroll(0, transcript_area.height);
        app.last_item_ranges.clear();
        f.render_widget(output::empty_hint(), transcript_area);
    } else {
        let messages = app
            .session
            .as_ref()
            .map(|s| s.messages())
            .unwrap_or_default();
        let ctx = output::RenderCtx {
            expanded_tools: &app.expanded_tools,
            messages: &messages,
            animation_frame: app.animation_frame,
            panel_width: transcript_area.width,
        };
        let animation_key = if app.has_running_workflow() {
            Some(app.animation_frame)
        } else {
            None
        };
        let cache_key = output::LayoutKey {
            items_version: app.items_version,
            expanded_version: app.expanded_version,
            width: transcript_area.width,
            animation_frame: animation_key,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        let (lines, ranges, node_regions, total_rows) =
            cache.get_or_build(cache_key, &app.items, &ctx);
        app.last_item_ranges = ranges.to_vec();
        app.last_node_regions = node_regions.to_vec();
        let lines_owned = lines.to_vec();
        app.layout_cache = cache;
        app.resolve_scroll(total_rows, transcript_area.height);
        // No .wrap(): WordWrapper reflows every line 0..scroll.y each frame.
        // Truncator (no-wrap) skips lazily, trading long-line wrap for speed.
        let paragraph =
            ratatui::widgets::Paragraph::new(lines_owned).scroll((app.scroll_offset, 0));
        f.render_widget(paragraph, transcript_area);
    }
    if let Some(area) = l.approvals {
        approval_bar::render(f, area, &app.pending_approvals);
    }
    if let Some(area) = l.sidebar {
        sidebar::render(
            f,
            area,
            sidebar::SidebarInputs {
                goal: app.goal.as_deref(),
                context: &app.context,
                attach_count: app.attach_count,
                session_id: &app.session_id,
                session_dir: &app.session_dir,
                streaming: app.streaming,
                todos: &app.todos,
                plans: &app.plans,
            },
        );
    }
    f.render_widget(
        input_paragraph(
            editor.buf(),
            editor.cursor(),
            app.streaming,
            app.pending_below_rows(),
        ),
        l.input,
    );
    if let Some(hint_area) = l.hint {
        completion::render_hint_strip(
            f,
            hint_area,
            hint_area.width < 60,
            app.mouse_captured,
            app.yank_mode,
        );
    }
    if app.popup.is_open() {
        completion::render_popup(f, l.input, &app.popup);
    }
    if app.cheatsheet_open {
        completion::render_cheatsheet(f, area);
    }
    if app.palette.open {
        palette::render(f, area, &app.palette);
    }
    if app.session_switcher.open {
        session_switcher::render(f, area, &app.session_switcher);
    }
    if let Some(modal) = app.compact_review.as_ref() {
        compact_review_modal::render(f, area, modal);
    }
    if app.history_search.open {
        history_search_modal::render(f, area, &app.history_search);
    }
}

fn compute_input_height(buf: &str, width: u16) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let border_padding: u16 = 2;
    let prompt_len: u16 = 2;
    let usable = width.saturating_sub(border_padding + prompt_len).max(1) as usize;
    let mut wrapped_rows: usize = 0;
    for logical_line in buf.split('\n') {
        let w = logical_line.width().max(1);
        wrapped_rows += w.div_ceil(usable);
    }
    let content = wrapped_rows.max(1) as u16;
    (content + border_padding).clamp(3, 9)
}
