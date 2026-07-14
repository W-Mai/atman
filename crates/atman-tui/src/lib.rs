use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::{broadcast, mpsc};

pub mod app;
pub mod approval_bar;
pub mod boot_animation;
pub mod clipboard;
pub mod compact_review_modal;
pub mod completion;
pub mod form_modal;
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
pub mod prompt_resolver;
pub mod session_switcher;
pub mod sidebar;
pub mod status;
pub mod terminal_guard;
pub mod terminal_viewer_modal;
pub mod theme;
pub mod workflow_viewer_modal;

use app::{AppState, NoteLevel, OutputItem};
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
    SwitchSession {
        sid: String,
        intro: Option<app::StartupIntro>,
    },
    DeleteSession(String),
    RenameSession {
        session_id: String,
        title: Option<String>,
    },
    FormSubmit {
        form_id: String,
        answer: atman_runtime::form::FormAnswer,
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
    OpenSessionSwitcher,
    OpenTrustModePicker,
    OpenThemePicker,
    CycleOutside,
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
    pub form_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>>,
    pub flow_names: Vec<(String, String)>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub startup_intro: Option<app::StartupIntro>,
    pub trust: atman_runtime::trust::TrustConfig,
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
            form_rx: Some(session.forms().subscribe()),
            flow_names: Vec::new(),
            session: Some(session),
            startup_intro: None,
            trust: atman_runtime::trust::TrustConfig::default(),
        }
    }
}

pub type InheritedTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run_tui(handle: TuiHandle) -> Result<()> {
    run_tui_ex(handle, None).await
}

pub async fn run_tui_ex(handle: TuiHandle, inherited: Option<InheritedTerminal>) -> Result<()> {
    let _guard = TerminalGuard::install()?;
    let mut terminal = match inherited {
        Some(t) => t,
        None => {
            let backend = CrosstermBackend::new(stdout());
            let mut t = Terminal::new(backend)?;
            t.clear()?;
            t
        }
    };
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
        .with_session(handle.session.clone())
        .with_trust(handle.trust.clone());
    app.startup_intro = handle.startup_intro.take();
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
    if let Some(sess) = &app.session {
        sess.approval()
            .set_auto_ceiling(app.trust.mode.auto_ceiling());
    }
    let mut editor = InputEditor::default();
    if let Some(sess) = handle.session.as_ref() {
        let past: Vec<String> = sess
            .messages()
            .iter()
            .filter(|m| matches!(m.role, atman_runtime::message::MessageRole::User))
            .map(|m| m.text_concat())
            .filter(|s| !s.trim().is_empty())
            .collect();
        editor.seed_history(past);
    }
    let (mut key_events, reader_shutdown) = spawn_event_reader();
    let mut interrupt_prompt: Option<std::time::Instant> = None;
    let mut shutdown = handle.shutdown_rx.take();
    let mut sigterm = build_sigterm_stream();
    let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut intro_tick = tokio::time::interval(std::time::Duration::from_millis(ANIMATION_TICK_MS));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Without Skip the interval bursts every missed tick when it wakes,
    // so an idle timer that sat unpolled for 3 s while the user read
    // the splash would fire ~180 times in a row and the whole slide
    // would blow past in one frame.
    intro_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
            _ = wait_sigterm(sigterm.as_mut()) => {
                break;
            }
            _ = animation_tick.tick(), if app.has_running_workflow() => {
                app.animation_frame = app.animation_frame.wrapping_add(1);
            }
            _ = intro_tick.tick(), if app.startup_intro.is_some() => {
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
                            if app.workflow_viewer.open =>
                        {
                            match me.kind {
                                MouseEventKind::ScrollUp => app.workflow_viewer.scroll_up(3),
                                MouseEventKind::ScrollDown => app.workflow_viewer.scroll_down(3),
                                MouseEventKind::ScrollLeft => app.workflow_viewer.scroll_left(3),
                                MouseEventKind::ScrollRight => app.workflow_viewer.scroll_right(3),
                                MouseEventKind::Down(MouseButton::Left) => {
                                    if let Some((panel_idx, path)) =
                                        app.workflow_viewer_hit_test(me.column, me.row)
                                    {
                                        app.toggle_workflow_node(panel_idx, &path);
                                    }
                                }
                                _ => {}
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Mouse(me)))
                            if matches!(me.kind, MouseEventKind::ScrollUp | MouseEventKind::ScrollDown) =>
                        {
                            let over_input = app
                                .input_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            let over_sidebar = app
                                .last_sidebar_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            if over_input {
                                for _ in 0..3 {
                                    if matches!(me.kind, MouseEventKind::ScrollUp) {
                                        if !editor.move_line_up() {
                                            break;
                                        }
                                    } else if !editor.move_line_down() {
                                        break;
                                    }
                                }
                            } else if over_sidebar {
                                let over_goal = app.last_goal_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let over_plan = app.last_plan_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let over_todo = app.last_todo_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                                if over_goal {
                                    if up { app.goal_scroll = app.goal_scroll.saturating_sub(1); }
                                    else { app.goal_scroll = app.goal_scroll.saturating_add(1); }
                                } else if over_plan {
                                    if up { app.plans_scroll = app.plans_scroll.saturating_sub(1); }
                                    else { app.plans_scroll = app.plans_scroll.saturating_add(1); }
                                } else if over_todo {
                                    if up { app.todos_scroll = app.todos_scroll.saturating_sub(1); }
                                    else { app.todos_scroll = app.todos_scroll.saturating_add(1); }
                                }
                            } else if let Some(idx) = app.hit_test(me.column, me.row) {
                                if matches!(app.items.get(idx), Some(OutputItem::Terminal { .. })) {
                                    app.scroll_terminal(idx, matches!(me.kind, MouseEventKind::ScrollUp), 1);
                                } else if matches!(me.kind, MouseEventKind::ScrollUp) {
                                    scroll_delta = scroll_delta.saturating_sub(3);
                                } else {
                                    scroll_delta = scroll_delta.saturating_add(3);
                                }
                            } else if matches!(me.kind, MouseEventKind::ScrollUp) {
                                scroll_delta = scroll_delta.saturating_sub(3);
                            } else {
                                scroll_delta = scroll_delta.saturating_add(3);
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Key(ke)))
                            if matches!(ke.kind, crossterm::event::KeyEventKind::Press) =>
                        {
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
                            editor.ingest_paste(&s);
                            interrupt_prompt = None;
                            app.refresh_popup(editor.buf());
                        }
                        Some(Ok(CtEvent::Mouse(me))) => {
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind
                                && let Some(rect) = app.input_rect
                                && rect_contains(rect, me.column, me.row)
                            {
                                let inner_x = me.column.saturating_sub(rect.x + 1);
                                let inner_y = me.row.saturating_sub(rect.y + 1);
                                // Strip the prompt "❯ " when clicking on the
                                // first row so column 0 lands on the first
                                // char, not on the arrow itself.
                                let display_col = if inner_y == 0 {
                                    inner_x.saturating_sub(2)
                                } else {
                                    inner_x
                                };
                                editor.set_cursor_by_display(inner_y as usize, display_col);
                            } else if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                if let Some((panel_idx, node_id)) =
                                    app.hit_test_node(me.column, me.row)
                                {
                                    if node_id
                                        == crate::output::COLLAPSED_CARD_FULLSCREEN_KEY
                                    {
                                        app.open_workflow_viewer(panel_idx);
                                    } else if node_id.is_empty() {
                                        app.toggle_workflow_panel_expansion(panel_idx);
                                    } else {
                                        app.toggle_workflow_node(panel_idx, &node_id);
                                    }
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Thinking { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_thinking_expanded(idx);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::WorkflowPanel { .. }) =
                                        app.items.get(idx)
                                {
                                    if me.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.open_workflow_viewer(idx);
                                    } else {
                                        app.toggle_workflow_panel_expansion(idx);
                                    }
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Terminal { .. }) =
                                        app.items.get(idx)
                                {
                                    if me.modifiers.contains(KeyModifiers::SHIFT) {
                                        app.open_terminal_viewer(idx);
                                    } else {
                                        app.toggle_terminal_expand(idx);
                                    }
                                }
                            } else if let MouseEventKind::Moved = me.kind {
                                if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Thinking { .. }) =
                                        app.items.get(idx)
                                {
                                    app.set_hovered_thinking(Some(idx));
                                } else {
                                    app.set_hovered_thinking(None);
                                }
                            }
                            interrupt_prompt = None;
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
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        app.record_lag(n, std::time::Instant::now());
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
                    let first_pending = app.todos.iter().position(|t| !matches!(t.status, atman_runtime::memory::todo::TodoStatus::Done | atman_runtime::memory::todo::TodoStatus::Cancelled));
                    if let Some(idx) = first_pending {
                        if idx > 0 {
                            app.todos_scroll = ((idx - 1) * 2) as u16;
                        } else {
                            app.todos_scroll = 0;
                        }
                    }
                }
            }
            _ = wait_plans_change(handle.plans_rx.as_mut()) => {
                if let Some(rx) = handle.plans_rx.as_mut() {
                    app.plans = rx.borrow().clone();
                    let first_pending_step = app.plans.iter().max_by_key(|p| p.updated_at)
                        .and_then(|p| p.steps.iter().position(|s| !s.done));
                    if let Some(idx) = first_pending_step {
                        if idx > 0 {
                            app.plans_scroll = idx as u16;
                        } else {
                            app.plans_scroll = 0;
                        }
                    }
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
            _ = wait_form_change(handle.form_rx.as_mut()) => {
                if let Some(rx) = handle.form_rx.as_mut() {
                    let latest = rx.borrow().clone();
                    if latest.is_empty() {
                        if !app.form_modal.try_show_confirm(true)
                            && app.form_modal.confirm_form.is_none()
                        {
                            app.form_modal.end_batch();
                        }
                    } else {
                        let ids: Vec<String> =
                            latest.iter().map(|p| p.form_id.clone()).collect();
                        app.form_modal.merge_batch_ids(&ids);
                        let current = app.form_modal.active_form_id().map(String::from);
                        let want: Option<String> = current
                            .filter(|id| ids.iter().any(|x| x == id))
                            .or_else(|| {
                                app.form_modal
                                    .batch_ids
                                    .iter()
                                    .zip(app.form_modal.batch_statuses.iter())
                                    .find(|(_, s)| matches!(s, crate::form_modal::BatchStatus::Pending))
                                    .map(|(id, _)| id.clone())
                                    .filter(|id| ids.iter().any(|x| x == id))
                            })
                            .or_else(|| latest.first().map(|p| p.form_id.clone()));
                        if let Some(want_id) = want
                            && let Some(target) =
                                latest.iter().find(|p| p.form_id == want_id).cloned()
                            && app.form_modal.active_form_id() != Some(target.form_id.as_str())
                        {
                            app.form_modal.attach(target, &ids);
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
                        TuiCommand::OpenSessionSwitcher => {
                            let scope = crate::session_switcher::SessionScope::Project;
                            let rows = enumerate_session_rows(&app, scope);
                            app.session_switcher.open_with(rows, scope);
                        }
                        TuiCommand::OpenTrustModePicker => {
                            app.trust_mode_picker_open = true;
                        }
                        TuiCommand::OpenThemePicker => {
                            app.theme_picker_open = true;
                        }
                        TuiCommand::CycleOutside => {
                            if app.trust.mode == atman_runtime::trust::TrustMode::Eager {
                                app.trust.outside = app.trust.outside.next();
                                app.mark_items_dirty();
                            } else {
                                app.push_note("outside switch only available in eager mode", app::NoteLevel::Warn);
                            }
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

async fn wait_form_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>>,
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

fn handle_workflow_viewer_key(action: &KeyAction, app: &mut AppState) {
    let step: u16 = 3;
    let page: u16 = 20;
    match action {
        KeyAction::Escape | KeyAction::Quit => app.close_workflow_viewer(),
        KeyAction::CursorLeft | KeyAction::Char('h') => app.workflow_viewer.scroll_left(step),
        KeyAction::CursorRight | KeyAction::Char('l') => app.workflow_viewer.scroll_right(step),
        KeyAction::HistoryUp | KeyAction::ScrollUp | KeyAction::Char('k') => {
            app.workflow_viewer.scroll_up(step)
        }
        KeyAction::HistoryDown | KeyAction::ScrollDown | KeyAction::Char('j') => {
            app.workflow_viewer.scroll_down(step)
        }
        KeyAction::PageUp => app.workflow_viewer.scroll_up(page),
        KeyAction::PageDown => app.workflow_viewer.scroll_down(page),
        KeyAction::CursorHome | KeyAction::Home => app.workflow_viewer.home(),
        KeyAction::CursorEnd | KeyAction::End => app.workflow_viewer.end(),
        _ => {}
    }
}

fn handle_terminal_viewer_key(action: &KeyAction, app: &mut AppState) {
    let step: u16 = 3;
    let page: u16 = 20;
    let max_h = app
        .items
        .get(app.terminal_viewer.panel_item_index)
        .and_then(|item| {
            if let OutputItem::Terminal { screen, .. } = item {
                Some(screen.cols)
            } else {
                None
            }
        })
        .unwrap_or(0);
    let max_v = app
        .terminal_viewer
        .last_inner_rect
        .map(|r| r.height)
        .unwrap_or(0);
    match action {
        KeyAction::Escape | KeyAction::Quit => app.close_terminal_viewer(),
        KeyAction::CursorLeft | KeyAction::Char('h') => app.terminal_viewer.scroll_left(step),
        KeyAction::CursorRight | KeyAction::Char('l') => {
            app.terminal_viewer.scroll_right(step, max_h)
        }
        KeyAction::HistoryUp | KeyAction::ScrollUp | KeyAction::Char('k') => {
            app.terminal_viewer.scroll_up(step)
        }
        KeyAction::HistoryDown | KeyAction::ScrollDown | KeyAction::Char('j') => {
            app.terminal_viewer.scroll_down(step, max_v)
        }
        KeyAction::PageUp => app.terminal_viewer.scroll_up(page),
        KeyAction::PageDown => app.terminal_viewer.scroll_down(page, max_v),
        _ => {}
    }
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
                app.session_switcher.close();
                request_session_switch(app, control_tx, sid.clone());
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
        PaletteEntryId::SetTrustMode => {
            app.trust_mode_picker_open = true;
        }
        PaletteEntryId::SetModeTheme => {
            app.theme_picker_open = true;
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

fn handle_form_key(
    action: &keys::KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    use atman_runtime::form::FormKind;
    let Some(form_id) = app.form_modal.active_form_id().map(String::from) else {
        return;
    };
    let is_text = matches!(
        app.form_modal.pending.as_ref().map(|p| &p.kind),
        Some(FormKind::Text { .. })
    );
    let is_confirm = matches!(
        app.form_modal.pending.as_ref().map(|p| &p.kind),
        Some(FormKind::Confirm { .. })
    );
    let is_multi = matches!(
        app.form_modal.pending.as_ref().map(|p| &p.kind),
        Some(FormKind::MultiSelect { .. })
    );
    let dispatch_outcome = |app: &mut AppState,
                            control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
                            outcome: crate::form_modal::SubmitOutcome| {
        use crate::form_modal::SubmitOutcome;
        match outcome {
            SubmitOutcome::Single { form_id, answer } => {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::FormSubmit { form_id, answer });
                }
            }
            SubmitOutcome::BatchConfirmed => {
                for (i, answer) in app.form_modal.batch_answers.iter().enumerate() {
                    if let Some(a) = answer
                        && let Some(tx) = control_tx
                    {
                        let id = app.form_modal.batch_ids.get(i).cloned().unwrap_or_default();
                        let _ = tx.send(TuiControl::FormSubmit {
                            form_id: id,
                            answer: a.clone(),
                        });
                    }
                }
            }
            SubmitOutcome::BatchCancelled => {
                for id in &app.form_modal.batch_ids {
                    if id == "__batch_confirm" {
                        continue;
                    }
                    if let Some(tx) = control_tx {
                        let _ = tx.send(TuiControl::FormSubmit {
                            form_id: id.clone(),
                            answer: atman_runtime::form::FormAnswer::Cancelled,
                        });
                    }
                }
            }
            SubmitOutcome::None => {}
        }
    };
    match action {
        KeyAction::Escape => {
            let outcome = app.form_modal.cancel();
            dispatch_outcome(app, control_tx, outcome);
        }
        KeyAction::Submit => {
            let outcome = app.form_modal.submit();
            dispatch_outcome(app, control_tx, outcome);
        }
        KeyAction::Char('y') | KeyAction::Char('Y') if is_confirm => {
            let outcome = app.form_modal.submit();
            dispatch_outcome(app, control_tx, outcome);
        }
        KeyAction::Char('n') | KeyAction::Char('N') if is_confirm => {
            let outcome = app.form_modal.confirm_no();
            dispatch_outcome(app, control_tx, outcome);
        }
        KeyAction::Char(' ') if is_multi => {
            app.form_modal.toggle_current();
        }
        KeyAction::Tab => {
            if let Some(target_id) = app.form_modal.switch_to(1)
                && target_id != form_id
            {
                let in_registry = app
                    .session
                    .as_ref()
                    .map(|s| {
                        s.forms()
                            .list_pending()
                            .iter()
                            .any(|p| p.form_id == target_id)
                    })
                    .unwrap_or(false);
                if in_registry {
                    if let Some(sess) = app.session.as_ref() {
                        sess.forms().promote(&target_id);
                    }
                } else if let Some(cached) = app.form_modal.cached_forms.get(&target_id).cloned() {
                    let ids = app.form_modal.batch_ids.clone();
                    app.form_modal.attach(cached, &ids);
                }
            }
        }
        KeyAction::HistoryUp | KeyAction::Char('k') if !is_text => {
            app.form_modal.move_cursor(-1);
        }
        KeyAction::HistoryDown | KeyAction::Char('j') if !is_text => {
            app.form_modal.move_cursor(1);
        }
        KeyAction::CursorLeft if is_confirm => {
            app.form_modal.move_cursor(-1);
        }
        KeyAction::CursorRight if is_confirm => {
            app.form_modal.move_cursor(1);
        }
        KeyAction::Char(c) if is_text => {
            app.form_modal.text_editor.insert_char(*c);
        }
        KeyAction::Backspace if is_text => {
            app.form_modal.text_editor.backspace();
        }
        KeyAction::Newline if is_text => {
            app.form_modal.text_editor.insert_newline();
        }
        _ => {}
    }
}

fn handle_compact_review_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
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
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CompactReviewAccept { review_id, edited });
                }
                app.compact_review = None;
            }
            KeyAction::Char('e') => modal.enter_editing(),
            KeyAction::Char('r') | KeyAction::Escape => {
                let review_id = modal.pending.review_id.clone();
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CompactReviewReject { review_id });
                }
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
    let deny_armed = app
        .deny_arm
        .map(|t| t.elapsed() < std::time::Duration::from_millis(2000))
        .unwrap_or(false);
    if !deny_armed {
        app.deny_arm = None;
    }
    match action {
        KeyAction::Char(c) => match c {
            '1'..='9' if deny_armed => {
                let idx = (*c as u8 - b'1') as usize;
                if let Some(p) = queue.get(idx) {
                    let _ = tx.send(TuiControl::DenyTool {
                        tool_use_id: p.tool_use_id.clone(),
                        reason: "denied by user".into(),
                    });
                    app.push_note(format!("denied {}", p.tool_name), app::NoteLevel::Warn);
                }
                app.deny_arm = None;
                true
            }
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
                app.deny_arm = None;
                true
            }
            'd' | 'D' => {
                let deny_first = queue.len() <= 1 || deny_armed;
                if deny_first {
                    if let Some(p) = queue.first() {
                        let _ = tx.send(TuiControl::DenyTool {
                            tool_use_id: p.tool_use_id.clone(),
                            reason: "denied by user".into(),
                        });
                        app.push_note(format!("denied {}", p.tool_name), app::NoteLevel::Warn);
                    }
                    app.deny_arm = None;
                } else {
                    app.deny_arm = Some(std::time::Instant::now());
                    app.push_note(
                        format!("d + N to deny nth, dd to deny first (of {})", queue.len()),
                        app::NoteLevel::Info,
                    );
                }
                true
            }
            _ => {
                app.deny_arm = None;
                false
            }
        },
        KeyAction::Escape => {
            let _ = tx.send(TuiControl::DenyAllPending {
                reason: "user pressed Esc".into(),
            });
            let _ = tx.send(TuiControl::CancelFlow);
            app.push_note(
                format!("denied all {} pending, flow cancelled", queue.len()),
                app::NoteLevel::Warn,
            );
            app.deny_arm = None;
            true
        }
        _ => false,
    }
}

fn handle_key(
    action: KeyAction,
    app: &mut AppState,
    editor: &mut InputEditor,
    interrupt_prompt: &mut Option<std::time::Instant>,
    submit_tx: Option<&mpsc::UnboundedSender<String>>,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    if app.form_modal.open {
        handle_form_key(&action, app, control_tx);
        return;
    }
    if app.compact_review.is_some() {
        handle_compact_review_key(&action, app, control_tx);
        return;
    }
    if app.workflow_viewer.open {
        handle_workflow_viewer_key(&action, app);
        return;
    }
    if app.terminal_viewer.open {
        handle_terminal_viewer_key(&action, app);
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
    if let Some(crate::app::OutputItem::StartupCard { recent, .. }) = app.items.first() {
        // The overlay only animates away when the user actually starts
        // a session:
        //   * a digit 1-9 → resume that recent session
        //   * Enter (Submit) with input in the editor → begin a new
        //     session interaction
        // Plain char keys just type into the editor and the overlay
        // stays put with the growing text visible in its input slot.
        if editor.buf().is_empty()
            && let KeyAction::Char(c) = &action
            && let Some(digit) = c.to_digit(10)
            && (1..=9).contains(&digit)
        {
            let idx = (digit as usize) - 1;
            if let Some(entry) = recent.get(idx) {
                request_session_switch(app, control_tx, entry.session_id.clone());
                return;
            }
        }
        if matches!(action, KeyAction::Submit)
            && !editor.buf().trim().is_empty()
            && app.startup_intro.is_none()
        {
            let (version, recent) = match app.items.first() {
                Some(crate::app::OutputItem::StartupCard { version, recent }) => {
                    (version.clone(), recent.clone())
                }
                _ => (String::new(), Vec::new()),
            };
            app.items.remove(0);
            app.items_version = app.items_version.wrapping_add(1);
            app.startup_intro = Some(crate::app::StartupIntro {
                started_at: std::time::Instant::now(),
                version,
                recent,
            });
        }
    }
    if app.cheatsheet_open {
        match action {
            KeyAction::Escape | KeyAction::HelpModal => app.cheatsheet_open = false,
            KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
        return;
    }
    if app.trust_mode_picker_open {
        let modes = atman_runtime::trust::TrustMode::all();
        let max = modes.len();
        match action {
            KeyAction::Escape => {
                app.trust_mode_picker_open = false;
            }
            KeyAction::HistoryUp | KeyAction::CursorLeft => {
                app.picker_selected = app.picker_selected.checked_sub(1).unwrap_or(max - 1);
            }
            KeyAction::HistoryDown | KeyAction::CursorRight => {
                app.picker_selected = (app.picker_selected + 1) % max;
            }
            KeyAction::Submit | KeyAction::Char('\r') => {
                let new_mode = modes[app.picker_selected.min(max - 1)];
                let prev = app.trust.mode;
                app.trust.mode = new_mode;
                app.trust_mode_picker_open = false;
                if new_mode != prev {
                    if let Some(sess) = app.session.as_ref() {
                        sess.approval().set_auto_ceiling(new_mode.auto_ceiling());
                    }
                    let display = app.trust.theme.display(new_mode);
                    if let Some(warning) = new_mode.warning(&display) {
                        app.push_note(&warning, app::NoteLevel::Warn);
                    }
                }
            }
            KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
        return;
    }
    if app.theme_picker_open {
        let themes = [
            atman_runtime::trust::Theme::Default,
            atman_runtime::trust::Theme::Wuxia,
            atman_runtime::trust::Theme::Animal,
            atman_runtime::trust::Theme::Weather,
            atman_runtime::trust::Theme::Drink,
        ];
        let max = themes.len();
        match action {
            KeyAction::Escape => {
                app.theme_picker_open = false;
            }
            KeyAction::HistoryUp | KeyAction::CursorLeft => {
                app.picker_selected = app.picker_selected.checked_sub(1).unwrap_or(max - 1);
            }
            KeyAction::HistoryDown | KeyAction::CursorRight => {
                app.picker_selected = (app.picker_selected + 1) % max;
            }
            KeyAction::Submit | KeyAction::Char('\r') => {
                app.trust.theme = themes[app.picker_selected.min(max - 1)];
                app.theme_picker_open = false;
            }
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
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::OpenCommandPalette => {
            app.palette.open();
            *interrupt_prompt = None;
        }
        KeyAction::Backspace => {
            editor.backspace();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::DeleteWordBackward => {
            editor.delete_word_backward();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::Newline => {
            editor.insert_newline();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::Submit => {
            if let Some(line) = editor.submit() {
                app.push_user_turn(line.clone());
                if let Some(tx) = submit_tx {
                    let _ = tx.send(line);
                }
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HistoryUp => {
            if !editor.move_line_up() {
                editor.history_up();
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HistoryDown => {
            if !editor.move_line_down() {
                editor.history_down();
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::CursorLeft => {
            editor.move_left();
            *interrupt_prompt = None;
        }
        KeyAction::CursorRight => {
            editor.move_right();
            *interrupt_prompt = None;
        }
        KeyAction::CursorHome => {
            editor.move_home();
            *interrupt_prompt = None;
        }
        KeyAction::CursorEnd => {
            editor.move_end();
            *interrupt_prompt = None;
        }
        KeyAction::Tab => {
            if app.trust.mode == atman_runtime::trust::TrustMode::Eager {
                app.trust.outside = app.trust.outside.next();
                app.mark_items_dirty();
            } else if editor.expand_paste_at_cursor() {
                edited = true;
            }
            *interrupt_prompt = None;
        }
        KeyAction::NudgePrefill => {
            editor.prefill("/nudge ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::CoursePrefill => {
            editor.prefill("/course ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::RedirectPrefill => {
            editor.prefill("/redirect ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HardStop => {
            editor.prefill("/hard-stop ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::ScrollUp | KeyAction::PageUp => {
            app.scroll_up(if matches!(action, KeyAction::PageUp) {
                10
            } else {
                1
            });
            *interrupt_prompt = None;
        }
        KeyAction::ScrollDown | KeyAction::PageDown => {
            app.scroll_down(if matches!(action, KeyAction::PageDown) {
                10
            } else {
                1
            });
            *interrupt_prompt = None;
        }
        KeyAction::Home => {
            app.scroll_to_top();
            *interrupt_prompt = None;
        }
        KeyAction::End => {
            app.scroll_to_tail();
            *interrupt_prompt = None;
        }
        KeyAction::Escape => {
            if app.streaming || app.has_running_workflow() {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CancelFlow);
                }
                app.push_note("cancel requested", app::NoteLevel::Warn);
            }
            *interrupt_prompt = None;
        }
        KeyAction::ToggleSidebar => {
            app.sidebar_mode = app.sidebar_mode.toggle();
            *interrupt_prompt = None;
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
            *interrupt_prompt = None;
        }
        KeyAction::ToggleLastTool => {
            app.toggle_last_tool_expansion();
            *interrupt_prompt = None;
        }
        KeyAction::HelpModal => {
            app.cheatsheet_open = true;
            *interrupt_prompt = None;
        }
        KeyAction::Interrupt => {
            if !editor.buf().is_empty() {
                editor.clear();
                edited = true;
                *interrupt_prompt = None;
            } else {
                let within_window = interrupt_prompt
                    .map(|t| t.elapsed() < std::time::Duration::from_millis(1500))
                    .unwrap_or(false);
                if within_window {
                    app.should_quit = true;
                } else {
                    *interrupt_prompt = Some(std::time::Instant::now());
                    app.push_note("press Ctrl+C again to quit", app::NoteLevel::Warn);
                }
            }
        }
        KeyAction::Quit => {
            app.should_quit = true;
        }
        KeyAction::Ignore => {
            *interrupt_prompt = None;
        }
    }
    if edited {
        app.refresh_popup(editor.buf());
    }
}

// The outgoing tui exits fast; the incoming tui plays the fade+slide
// intro on top of the freshly rendered new session so content appears
// first, then the banner/sessions fade out and input docks bottom.
fn request_session_switch(
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
    sid: String,
) {
    let intro = match app.items.first() {
        Some(crate::app::OutputItem::StartupCard { version, recent }) => {
            Some(crate::app::StartupIntro {
                started_at: std::time::Instant::now(),
                version: version.clone(),
                recent: recent.clone(),
            })
        }
        _ => None,
    };
    if let Some(tx) = control_tx {
        let _ = tx.send(TuiControl::SwitchSession {
            sid,
            intro: intro.clone(),
        });
    }
    app.should_quit = true;
}

fn rect_union(a: ratatui::layout::Rect, b: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    ratatui::layout::Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

// Startup input eases from the overlay's centered slot to the normal
// bottom position. 300 ms sits inside the 200–400 ms band that feels
// like a real transition rather than a snap or a lag.
const STARTUP_SLIDE_MS: u128 = 300;
// Animation frame cadence while a slide is in flight. 60 fps so the
// panel's x / y / width interpolation looks continuous instead of
// two-or-three discrete jumps.
const ANIMATION_TICK_MS: u64 = 16;

// ease-out-quad — motion is immediately visible from the first frame
// and gently decelerates into the end. ease-in-out was the wrong pick:
// its slow start hides the animation in the crucial "did anything just
// happen?" first 100 ms.
fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

// Replace wide-glyph halves straddling a floating widget's edges with
// spaces so CJK / emoji from lower layers can't bleed through the
// overlay's border. Call before each Clear + render pass on a modal.
pub fn sanitize_widget_edges(f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::buffer::CellDiffOption;
    use unicode_width::UnicodeWidthStr;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    let buf_area = *buf.area();
    let inside_left = area.x;
    let inside_right = area.x + area.width - 1;
    let outside_left = area.x.checked_sub(1);
    let outside_right = if area.x + area.width < buf_area.x + buf_area.width {
        Some(area.x + area.width)
    } else {
        None
    };
    let clear_wide = |cell: &mut ratatui::buffer::Cell| {
        cell.set_symbol(" ");
        cell.set_diff_option(CellDiffOption::None);
    };
    for y in area.y..area.y + area.height {
        if y < buf_area.y || y >= buf_area.y + buf_area.height {
            continue;
        }
        if let Some(ox) = outside_left {
            let cell = &mut buf[(ox, y)];
            if UnicodeWidthStr::width(cell.symbol()) > 1 {
                clear_wide(cell);
            }
        }
        {
            let cell = &mut buf[(inside_left, y)];
            if cell.symbol().is_empty() {
                clear_wide(cell);
            }
        }
        if inside_right != inside_left {
            let cell = &mut buf[(inside_right, y)];
            if UnicodeWidthStr::width(cell.symbol()) > 1 {
                clear_wide(cell);
            }
        }
        if let Some(rx) = outside_right {
            let cell = &mut buf[(rx, y)];
            if cell.symbol().is_empty() {
                clear_wide(cell);
            }
        }
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
    let startup_active = matches!(
        app.items.first(),
        Some(crate::app::OutputItem::StartupCard { .. })
    );
    let intro_progress = app
        .startup_intro
        .as_ref()
        .map(|i| {
            (i.started_at.elapsed().as_millis().min(STARTUP_SLIDE_MS) as f32)
                / STARTUP_SLIDE_MS as f32
        })
        .unwrap_or(1.0);
    let intro_active = app.startup_intro.is_some() && intro_progress < 1.0;
    let wide_enough = area.width >= layout::SIDEBAR_MIN_TOTAL_WIDTH;
    let show_sidebar = if startup_active || intro_active {
        false
    } else {
        app.sidebar_mode.resolve(wide_enough)
    };
    let status_height: u16 = 1;
    let pending_count = app.pending_approvals.len();
    let approvals_rows: u16 = if pending_count == 0 {
        0
    } else {
        let items = pending_count.min(9) as u16;
        let overflow = if pending_count > 9 { 1 } else { 0 };
        items + overflow + 2
    };
    let l = layout::compute_ex(area, status_height);
    let sidebar_rect = layout::compute_sidebar_rect(l.transcript, show_sidebar);
    let transcript_content = layout::compute_content_rect(l.transcript);
    let content_w = layout::input_content_width(l.transcript.width);
    let total_input_lines = crate::input::wrapped_line_count(editor.buf(), content_w) as u16;
    let input_buf_lines = total_input_lines.min(12);
    let bottom_rect = layout::compute_input_rect(l.transcript, input_buf_lines);
    let cursor_row = crate::input::cursor_display_row(editor.buf(), editor.cursor());
    let visible_rows = input_buf_lines.max(3);
    let scroll_row = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let startup_slot = if startup_active {
        let recent = match app.items.first() {
            Some(crate::app::OutputItem::StartupCard { recent, .. }) => recent.clone(),
            _ => Vec::new(),
        };
        Some(output::compute_startup_overlay(l.transcript, &recent).input_slot)
    } else {
        None
    };
    let intro_slot = if intro_active {
        app.startup_intro
            .as_ref()
            .map(|i| output::compute_startup_overlay(l.transcript, &i.recent).input_slot)
    } else {
        None
    };
    let intro_overlay_area = if intro_active {
        app.startup_intro
            .as_ref()
            .map(|i| output::compute_startup_overlay(l.transcript, &i.recent).area)
    } else {
        None
    };
    let input_rect = if let Some(slot) = startup_slot {
        slot
    } else if let Some(slot) = intro_slot {
        let eased = ease_out(intro_progress);
        let mix = |a: u16, b: u16| ((a as f32) + (b as f32 - a as f32) * eased).round() as u16;
        ratatui::layout::Rect {
            x: mix(slot.x, bottom_rect.x),
            y: mix(slot.y, bottom_rect.y),
            width: mix(slot.width, bottom_rect.width),
            height: mix(slot.height, bottom_rect.height),
        }
    } else {
        bottom_rect
    };
    let approvals_rect = layout::compute_approvals_rect(l.transcript, input_rect, approvals_rows);
    app.input_rect = Some(input_rect);
    f.render_widget(
        status::render_bar(status::StatusInputs {
            session_id: &app.session_id,
            goal: app.goal.as_deref(),
            streaming: app.streaming,
            waiting_for_llm: app.waiting_for_llm,
        }),
        l.status,
    );
    let transcript_area = transcript_content;
    app.last_transcript_rect = Some(transcript_area);
    let effective_viewport = input_rect.y.saturating_sub(transcript_area.y).max(1);
    if startup_active {
        if let Some(crate::app::OutputItem::StartupCard { version, recent }) = app.items.first() {
            let base = output::compute_startup_overlay(l.transcript, recent).area;
            f.render_widget(ratatui::widgets::Clear, l.transcript);
            output::render_startup_overlay(f, base, version, recent, false, recent.len());
        }
        app.resolve_scroll(0, effective_viewport);
        app.last_item_ranges.clear();
    } else if app.items.is_empty() {
        app.resolve_scroll(0, effective_viewport);
        app.last_item_ranges.clear();
        // Clear the full unpadded transcript rect first — otherwise the
        // 2-col padding strip on each side of transcript_area keeps
        // whatever the previous frame's overlay painted there, and the
        // startup card's animated edges leak through for one frame
        // after the slide completes.
        f.render_widget(ratatui::widgets::Clear, l.transcript);
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
            hovered_thinking_idx: app.hovered_thinking_idx,
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
        app.resolve_scroll(total_rows, effective_viewport);
        // No .wrap(): WordWrapper reflows every line 0..scroll.y each frame.
        // Truncator (no-wrap) skips lazily, trading long-line wrap for speed.
        let paragraph =
            ratatui::widgets::Paragraph::new(lines_owned).scroll((app.scroll_offset, 0));
        f.render_widget(paragraph, transcript_area);
    }
    if let Some(area) = sidebar_rect {
        let project_root = app
            .session
            .as_ref()
            .and_then(|s| s.meta())
            .and_then(|m| m.project_root)
            .map(|p| p.display().to_string());
        let goal_scroll = app.goal_scroll;
        let plans_scroll = app.plans_scroll;
        let todos_scroll = app.todos_scroll;
        let sr = sidebar::render(
            f,
            area,
            sidebar::SidebarInputs {
                goal: app.goal.as_deref(),
                context: &app.context,
                attach_count: app.attach_count,
                session_id: &app.session_id,
                session_dir: &app.session_dir,
                project_root: project_root.as_deref(),
                streaming: app.streaming,
                todos: &app.todos,
                plans: &app.plans,
                goal_scroll,
                plans_scroll,
                todos_scroll,
                on_goal_scroll: &|_c| {},
                on_plans_scroll: &|_c| {},
                on_todos_scroll: &|_c| {},
            },
        );
        app.last_sidebar_rect = Some(area);
        app.last_goal_rect = sr.goal_rect;
        app.last_plan_rect = sr.plan_rect;
        app.last_todo_rect = sr.todo_rect;
    }
    if intro_active && let Some(intro) = app.startup_intro.as_ref() {
        output::render_startup_intro_fade(
            f,
            l.transcript,
            &intro.version,
            &intro.recent,
            intro_progress,
        );
    }
    if let Some(area) = approvals_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        approval_bar::render(f, area, &app.pending_approvals);
    }
    // Wipe splash overlay ∪ docked rect for the entire lifetime of the intro,
    // including the very last frame where progress hits 1.0 and the banner /
    // sessions stop being drawn but still linger on screen from the frame before.
    let clear_target = if app.startup_intro.is_some() {
        if let Some(overlay) = intro_overlay_area {
            rect_union(overlay, bottom_rect)
        } else {
            input_rect
        }
    } else {
        input_rect
    };
    sanitize_widget_edges(f, clear_target);
    f.render_widget(ratatui::widgets::Clear, clear_target);
    f.render_widget(
        input_paragraph(
            editor.buf(),
            editor.cursor(),
            app.streaming,
            app.pending_below_rows(),
            scroll_row,
            &app.trust,
        ),
        input_rect,
    );
    if !app.streaming {
        let raw_row = crate::input::cursor_display_row(editor.buf(), editor.cursor());
        let raw_col = crate::input::cursor_display_col(editor.buf(), editor.cursor());
        let prompt_w: u16 = 2;
        let inner_x = input_rect.x.saturating_add(1).saturating_add(1);
        let inner_y = input_rect.y.saturating_add(1);
        if raw_row >= scroll_row {
            let cy = inner_y + (raw_row - scroll_row);
            let cx = inner_x + prompt_w + raw_col;
            if cy < input_rect.y + input_rect.height.saturating_sub(1)
                && cx < input_rect.x + input_rect.width.saturating_sub(1)
            {
                f.set_cursor_position((cx, cy));
            }
        }
    }
    if app.popup.is_open() {
        completion::render_popup(f, input_rect, &app.popup);
    }
    if app.cheatsheet_open {
        completion::render_cheatsheet(f, area);
    }
    if app.trust_mode_picker_open {
        render_trust_mode_picker(f, area, app);
    }
    if app.theme_picker_open {
        render_theme_picker(f, area, app);
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
    if app.workflow_viewer.open {
        workflow_viewer_modal::render(f, area, app);
    }
    if app.terminal_viewer.open {
        terminal_viewer_modal::render(f, area, app);
    }
    if app.form_modal.open {
        form_modal::render(f, area, &app.form_modal);
    }
    if intro_progress >= 1.0 && app.startup_intro.is_some() {
        app.startup_intro = None;
    }
}

fn render_trust_mode_picker(f: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &AppState) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState};

    let modes = atman_runtime::trust::TrustMode::all();
    let items: Vec<ListItem> = modes
        .iter()
        .map(|&m| {
            let d = app.trust.theme.display(m);
            let color = match d.color {
                atman_runtime::trust::ModeColor::Cyan => ratatui::style::Color::Cyan,
                atman_runtime::trust::ModeColor::Green => ratatui::style::Color::Green,
                atman_runtime::trust::ModeColor::Yellow => ratatui::style::Color::Yellow,
                atman_runtime::trust::ModeColor::Orange => ratatui::style::Color::Rgb(208, 135, 22),
                atman_runtime::trust::ModeColor::Red => ratatui::style::Color::Red,
            };
            let marker = if m == app.trust.mode {
                "← current"
            } else {
                ""
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", d.emoji), Style::default().fg(color)),
                Span::styled(
                    format!("{:<14}", d.name),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}  ", d.description)),
                Span::raw(marker),
            ]))
        })
        .collect();

    let h = items.len() as u16 + 4;
    let w = 70u16.min(area.width);
    let popup = ratatui::layout::Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Trust Mode — j/k select, Enter confirm, Esc cancel ")
        .title_alignment(Alignment::Center);
    let mut state = ListState::default();
    state.select(Some(app.picker_selected.min(items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).block(block).highlight_style(
            Style::default()
                .bg(ratatui::style::Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        popup,
        &mut state,
    );
}

fn render_theme_picker(f: &mut ratatui::Frame, area: ratatui::layout::Rect, app: &AppState) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState};

    let themes = [
        ("default", "calm / steady / eager / reckless"),
        ("wuxia", "守拙 / 行云 / 破竹 / 逍遥"),
        ("animal", "🦔 hedgehog / 🐱 cat / 🐶 dog / 🦡 honey-badger"),
        ("weather", "🌧 drizzle / ☀️ clear / ⛈ storm / 🌪 tornado"),
        ("drink", "💧 water / ☕ coffee / ☕ espresso / 🧪 bleach"),
    ];
    let items: Vec<ListItem> = themes
        .iter()
        .map(|(id, desc)| {
            let is_current = app.trust.theme.to_string() == *id;
            let marker = if is_current { "  ← current" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<10}", id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}{}", desc, marker)),
            ]))
        })
        .collect();

    let h = items.len() as u16 + 4;
    let w = 70u16.min(area.width);
    let popup = ratatui::layout::Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" Theme — j/k select, Enter confirm, Esc cancel ")
        .title_alignment(Alignment::Center);
    let mut state = ListState::default();
    state.select(Some(app.picker_selected.min(items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).block(block).highlight_style(
            Style::default()
                .bg(ratatui::style::Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
        popup,
        &mut state,
    );
}
