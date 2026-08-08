use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast, mpsc};

pub mod alias_manager;
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
pub mod mcp_manager;
pub mod model_picker;
pub mod onboarding;
pub mod window;
pub mod wm;

pub mod input;
mod key_handler;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod mermaid;
pub mod output;
pub mod palette;
pub mod prompt_resolver;
pub mod provider_manager;
pub mod render;
pub mod session_switcher;
pub mod sidebar;
pub mod states;
pub mod status;
pub mod task_panel;
pub mod terminal_guard;
pub mod theme;
pub mod width;

use app::{AppState, NoteLevel};
use atman_runtime::stream::StreamFrame;
use input::{InputEditor, cursor_from_wrapped};
use keys::map as map_key;
use render::*;
use terminal_guard::TerminalGuard;

pub enum TuiNote {
    Info(String),
    Warn(String),
    Error(String),
}

/// Rich notification sent from runtime/CLI to TUI.
#[derive(Debug, Clone)]
pub struct TuiNotification {
    pub level: atman_runtime::notify::NotifyLevel,
    pub location: atman_runtime::notify::NotifyLocation,
    pub lifecycle: atman_runtime::notify::NotifyLifecycle,
    pub stack: atman_runtime::notify::NotifyStack,
    pub message: String,
}

impl From<atman_runtime::notify::Notification> for TuiNotification {
    fn from(n: atman_runtime::notify::Notification) -> Self {
        Self {
            level: n.level,
            location: n.location,
            lifecycle: n.lifecycle,
            stack: n.stack,
            message: n.message,
        }
    }
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
    HardStop,
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
        intro: app::StartupIntro,
    },
    NewSession,
    MoveSession,
    DeleteSession(String),
    RenameSession {
        session_id: String,
        title: Option<String>,
    },
    FormSubmit {
        form_id: String,
        answer: atman_runtime::form::FormAnswer,
    },
    AuthLogin {
        kind: atman_runtime::auth_store::ProviderKind,
        name: String,
    },
    AddConfigProvider {
        name: String,
        provider_type: String,
        api_key: String,
        base_url: String,
        context_budget: Option<u64>,
        max_tokens: Option<u32>,
        thinking: bool,
        enabled: bool,
    },
    UpdateConfigProvider {
        name: String,
        provider_type: String,
        api_key: String,
        base_url: String,
        context_budget: Option<u64>,
        max_tokens: Option<u32>,
        thinking: bool,
        enabled: bool,
    },
    AuthLogout {
        id: String,
    },
    OpenAliasManager {
        model: Option<String>,
    },
    OnboardingInit,
    SwitchModel {
        model: String,
    },
    RefreshProviderModels {
        provider_id: String,
    },
    TestProvider {
        name: String,
        provider_type: String,
        api_key: String,
        base_url: String,
    },
    TermResize {
        handle: String,
        rows: u16,
        cols: u16,
    },
    McpTest {
        name: String,
    },
    McpReload,
    McpListResources {
        name: String,
    },
    McpListPrompts {
        name: String,
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
    OpenModelPicker,
    CycleOutside,
    ProviderModelsUpdated,
    ProviderTestResult((String, bool)),
    McpTestResult {
        name: String,
        message: String,
        ok: bool,
    },
    McpReloaded,
    McpResourcesResult {
        name: String,
        resources: Vec<atman_runtime::mcp::McpResource>,
    },
    McpPromptsResult {
        name: String,
        prompts: Vec<atman_runtime::mcp::McpPrompt>,
    },
}

pub struct TuiHandle {
    pub session_id: String,
    pub session_dir: String,
    pub goal: Option<String>,
    pub stream_rx: broadcast::Receiver<StreamFrame>,
    pub task_event_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::TaskEvent>>,
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
    pub injection_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::injection::Injection>>,
    pub flow_names: Vec<(String, String)>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub startup_intro: Option<app::StartupIntro>,
    pub onboarding_recommended: bool,
    pub trust: atman_runtime::trust::TrustConfig,
    pub task_registry: Option<atman_runtime::TaskRegistry>,
    /// Toasts collected during boot, to be pushed to app on start.
    pub boot_toasts: Vec<app::ToastNote>,
}

impl TuiHandle {
    pub fn from_session(session: std::sync::Arc<atman_runtime::Session>) -> Self {
        Self {
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            goal: session.goal(),
            stream_rx: session.stream_subscribe(),
            task_event_rx: None,
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
            injection_rx: Some(session.subscribe_injections()),
            flow_names: Vec::new(),
            session: Some(session),
            startup_intro: None,
            onboarding_recommended: false,
            trust: atman_runtime::trust::TrustConfig::default(),
            task_registry: None,
            boot_toasts: Vec::new(),
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
    if let Some(tr) = handle.task_registry.take() {
        app = app.with_task_registry(tr);
    }
    let ui_state = crate::states::PersistedUiState::load();
    ui_state.apply(&mut app);
    if handle.onboarding_recommended && !app.onboarding_skipped {
        app.onboarding_open = true;
        if let Some(tx) = handle.control_tx.as_ref() {
            let _ = tx.send(TuiControl::OnboardingInit);
        }
    }
    app.startup_intro = handle.startup_intro.take();
    // Reset started_at so the 300ms fade begins now, not when the
    // switch was requested (which may have been seconds ago).
    if let Some(ref mut intro) = app.startup_intro {
        intro.started_at = std::time::Instant::now();
    }
    // Carry boot toasts into the live app so they persist seamlessly.
    if !handle.boot_toasts.is_empty() {
        for toast in std::mem::take(&mut handle.boot_toasts) {
            app.push_toast(toast.message, toast.level, toast.ttl, toast.position);
        }
    }
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
            .messages_full()
            .iter()
            .filter(|m| matches!(m.role, atman_runtime::message::MessageRole::User))
            .map(|m| m.text_concat())
            .filter(|s| !s.trim().is_empty())
            .collect();
        editor.seed_history(past);
    }
    let (mut key_events, reader_shutdown) = spawn_event_reader();
    // Flush stale input events accumulated during boot animation.
    // Without this, mouse events from the boot phase flood the first
    // render frame and cause a visible stutter.
    while key_events.try_recv().is_ok() {}
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
    let mut toast_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    toast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let _reader_guard = ReaderGuard(reader_shutdown);
    let mut update_check = tokio::spawn(check_latest_release());

    // Fan-in merge: session broadcast (session-level frames) + each FlowRun's
    // frame_tx (per-FlowRun frames). Forwarders are spawned on SubAgentStarted
    // / root FlowStart so the TUI receives every frame through one channel.
    let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<StreamFrame>();
    {
        let fwd_tx = merge_tx.clone();
        let mut srx = handle.stream_rx;
        tokio::spawn(async move {
            while let Ok(f) = srx.recv().await {
                let _ = fwd_tx.send(f);
            }
        });
    }
    let frame_merge_tx = merge_tx.clone();
    let session_for_sub = handle.session.clone();

    loop {
        app.tick_toasts();
        terminal.draw(|f| render_frame(f, &mut app, &editor))?;
        app.tick = app.tick.wrapping_add(1);

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
            _ = animation_tick.tick(), if app.has_active_animation() => {
                app.animation_frame = app.animation_frame.wrapping_add(1);
            }
            _ = intro_tick.tick(), if app.startup_intro.is_some() => {
                app.animation_frame = app.animation_frame.wrapping_add(1);
            }
            _ = toast_tick.tick(), if !app.toasts.is_empty() => {}
            latest = poll_update_check(&mut update_check), if !update_check.is_finished() => {
                app.latest_release = latest;
            }

            key = key_events.recv() => {
                if std::env::var_os("ATMAN_TRACE_EVENTS").is_some() {
                    atman_runtime::notify!(debug, "event: {key:?}");
                }
                let mut current = key;
                let mut scroll_delta: i32 = 0;
                let mut drained: u32 = 0;
                loop {
                    match current {
                        Some(Ok(CtEvent::Mouse(me)))
                            if app.history_search.open =>
                        {
                            match me.kind {
                                MouseEventKind::ScrollUp => {
                                    if let Some(crate::history_search_modal::HistoryArea::Preview) =
                                        app.history_search.hit_test(me.column, me.row)
                                    {
                                        app.history_search.scroll_preview(true, 3);
                                    } else {
                                        app.history_search.move_up();
                                        key_handler::refresh_history_preview(&mut app);
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    if let Some(crate::history_search_modal::HistoryArea::Preview) =
                                        app.history_search.hit_test(me.column, me.row)
                                    {
                                        app.history_search.scroll_preview(false, 3);
                                    } else {
                                        app.history_search.move_down();
                                        key_handler::refresh_history_preview(&mut app);
                                    }
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    if let Some(idx) =
                                        app.history_search.click_result(me.column, me.row)
                                    {
                                        if app.history_search.selected != idx {
                                            app.history_search.selected = idx;
                                            app.history_search.preview_scroll = 0;
                                            key_handler::refresh_history_preview(&mut app);
                                        }
                                    }
                                }
                                _ => {}
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Mouse(me)))
                            if matches!(
                                me.kind,
                                MouseEventKind::ScrollUp
                                    | MouseEventKind::ScrollDown
                                    | MouseEventKind::ScrollLeft
                                    | MouseEventKind::ScrollRight
                            ) =>
                        {
                            let over_floating = app
                                .wm
                                .hit_test_panel(me.column, me.row)
                                .is_some();
                            if over_floating {
                                let id = app
                                    .wm
                                    .hit_test_panel(me.column, me.row)
                                    .unwrap()
                                    .id;
                                if let Some(p) = app.wm.panels.iter_mut().find(|p| p.id == id) {
                                    match me.kind {
                                        MouseEventKind::ScrollUp => {
                                            p.scroll = p.scroll.saturating_sub(3);
                                        }
                                        MouseEventKind::ScrollDown => {
                                            p.scroll = p.scroll.saturating_add(3);
                                        }
                                        MouseEventKind::ScrollLeft => {
                                            p.h_scroll = p.h_scroll.saturating_sub(3);
                                        }
                                        MouseEventKind::ScrollRight => {
                                            p.h_scroll = p.h_scroll.saturating_add(3);
                                        }
                                        _ => {}
                                    }
                                }
                            } else {
                            let over_input = app
                                .input_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            let over_sidebar = app
                                .last_sidebar_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            if over_input {
                                let cw = if let Some(r) = app.input_rect {
                                    r.width.saturating_sub(layout::INPUT_H_OVERHEAD) as usize
                                } else {
                                    80
                                };
                                for _ in 0..3 {
                                    if matches!(me.kind, MouseEventKind::ScrollUp) {
                                        if !editor.move_line_up_visual(cw) {
                                            break;
                                        }
                                    } else if !editor.move_line_down_visual(cw) {
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
                            } else if matches!(me.kind, MouseEventKind::ScrollUp) {
                                scroll_delta = scroll_delta.saturating_sub(3);
                            } else if matches!(me.kind, MouseEventKind::ScrollDown) {
                                scroll_delta = scroll_delta.saturating_add(3);
                            }
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Key(ke)))
                            if matches!(ke.kind, crossterm::event::KeyEventKind::Press) =>
                        {
                            key_handler::handle_key(
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
                            app.sync_modal_stack();
                            // Check floating panels/modals BEFORE input_rect so clicks
                            // on overlapping panels don't pass through to the input box.
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind
                                && !(app.wm.hit_test_panel(me.column, me.row).is_some()
                                    || app.form_modal.open
                                    || app.compact_review.is_some()
                                    || app.session_switcher.open
                                    || app.history_search.open
                                    || app.provider_manager.open
                                    || app.alias_manager.open
                                    || app.model_picker.open
                                    || app.onboarding_open
                                    || app.palette.open
                                    || app.theme_picker_open)
                                && let Some(rect) = app.input_rect
                                && rect_contains(rect, me.column, me.row)
                            {
                                let inner_x = me.column.saturating_sub(rect.x + layout::INPUT_LEFT);
                                let inner_y = me.row.saturating_sub(rect.y + 1);
                                let cw = (rect.width.saturating_sub(layout::INPUT_H_OVERHEAD)) as usize;
                                let pos = cursor_from_wrapped(
                                    editor.buf(),
                                    inner_y as usize,
                                    inner_x as usize,
                                    cw,
                                );
                                editor.set_cursor(pos);
                            } else if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                let topmost = app
                                    .wm
                                    .hit_test_panel(me.column, me.row)
                                    .map(|p| (p.id, p.rect));
                                if let Some((panel_id, pr)) = topmost {
                                if let Some(min_id) =
                                    app.wm.hit_test_minimize(me.column, me.row)
                                    && min_id == panel_id
                                {
                                    app.wm.close(min_id);
                                } else if let Some(max_id) =
                                    app.wm.hit_test_maximize(me.column, me.row)
                                    && max_id == panel_id
                                {
                                    let canvas = app.maximized_canvas();
                                    app.wm.toggle_maximize(max_id, canvas);
                                    if let Some(p) = app.wm.panels.iter().find(|p| p.id == max_id) {
                                        if matches!(p.content_kind, crate::wm::WindowContent::Task { kind: atman_runtime::TaskKind::Terminal, .. }) {
                                            let inner_cols = p.rect.width.saturating_sub(8);
                                            let inner_rows = p.rect.height.saturating_sub(5);
                                            if inner_cols > 0 && inner_rows > 0 {
                                                if let Some(tx) = &handle.control_tx {
                                                    let _ = tx.send(TuiControl::TermResize {
                                                        handle: app
                                                            .wm
                                                            .content_kind(max_id)
                                                            .and_then(|kind| match kind {
                                                                crate::wm::WindowContent::Task { handle, .. } => Some(handle.clone()),
                                                                _ => None,
                                                            })
                                                            .unwrap_or_default(),
                                                        rows: inner_rows,
                                                        cols: inner_cols,
                                                    });
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(close_id) =
                                    app.wm.hit_test_close(me.column, me.row)
                                {
                                    let close_label = app.wm.label(close_id).unwrap_or_default().to_string();
                                    let armed = app.panel_close_armed_id.as_deref() == Some(close_label.as_str())
                                        && !app.panel_close_arm_expired();
                                    if armed {
                                        let tid = app
                                            .task_snapshots
                                            .iter()
                                            .find(|s| s.source_handle == close_label)
                                            .map(|s| s.id.clone());
                                        if let (Some(tr), Some(tid)) = (&app.task_registry, tid) {
                                            tr.kill(&tid);
                                        }
                                        app.clear_panel_close_arm();
                                    } else {
                                        let label = app
                                            .task_snapshots
                                            .iter()
                                            .find(|s| s.source_handle == close_label)
                                            .map(|s| s.label.clone())
                                            .unwrap_or_default();
                                        app.arm_panel_close(close_label);
                                        app.push_note(
                                            format!("press ✕ again to kill {label}"),
                                            app::NoteLevel::Warn,
                                        );
                                    }
                                } else if let Some(resize_id) =
                                    app.wm.hit_test_resize(me.column, me.row)
                                {
                                    app.wm.focus(resize_id);
                                    app.resize_target = Some(resize_id);
                                    app.resize_offset = (me.column, me.row);
                                } else if let Some(fp) = app
                                    .wm
                                    .hit_test_titlebar(me.column, me.row)
                                {
                                    let id = fp.id;
                                    let now = std::time::Instant::now();
                                    let is_double = app
                                        .last_titlebar_click
                                        .as_ref()
                                        .is_some_and(|(prev_id, ts)| {
                                            *prev_id == id && now.duration_since(*ts).as_millis() < 400
                                        });
                                    if is_double {
                                        let canvas = app.last_transcript_rect.unwrap_or_default();
                                        app.wm.toggle_maximize(id, canvas);
                                        app.last_titlebar_click = None;
                                    } else {
                                        app.wm.focus(id);
                                        app.drag_target = Some(id);
                                        app.drag_offset = (me.column, me.row);
                                        app.last_titlebar_click = Some((id, now));
                                    }
                                } else {
                                    let history_hit = app
                                        .last_wm_hitmap
                                        .history_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, h, _)| h.clone());
                                    let mcp_hit = app
                                        .last_wm_hitmap
                                        .mcp_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, n, _)| n.clone());
                                    let wf_hit = app
                                        .last_wm_hitmap
                                        .workflow_node_rects
                                        .iter()
                                        .find(|(pid, _, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, i, p, _)| (*i, p.clone()));
                                    if let Some(handle) = history_hit {
                                        let canvas =
                                            app.last_transcript_rect.unwrap_or_default();
                                        app.open_task_panel(&handle, canvas);
                                    } else if let Some(name) = mcp_hit {
                                        if !app.expanded_mcp_servers.remove(&name) {
                                            app.expanded_mcp_servers.insert(name);
                                        }
                                    } else if let Some((panel_idx, path)) = wf_hit {
                                        if path.is_empty() {
                                            app.toggle_workflow_panel_expansion(panel_idx);
                                        } else {
                                            app.toggle_workflow_node(panel_idx, &path);
                                        }
                                    } else if let Some((_pid, tool_id, _)) = app
                                        .last_wm_hitmap
                                        .tool_header_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                        })
                                        .cloned()
                                    {
                                        if !app.expanded_tools.remove(&tool_id) {
                                            app.expanded_tools.insert(tool_id);
                                        }
                                        app.expanded_version =
                                            app.expanded_version.wrapping_add(1);
                                    } else {
                                        app.wm.focus(panel_id);
                                    }
                                }
                            } else if app.modal_open() {
                                // Modal is open — swallow the click (blocks base layer).
                            } else {
                                if let Some(r) = app.last_upper_title_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.sidebar_upper_collapsed = !app.sidebar_upper_collapsed;
                                } else if let Some(r) = app.last_lower_title_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.sidebar_lower_collapsed = !app.sidebar_lower_collapsed;
                                } else if let Some(r) = app.last_sidebar_more_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    let canvas = app.last_transcript_rect.unwrap_or_default();
                                    app.wm.open(
                                        "mcp-manager",
                                        crate::wm::ContentKey::Mcp,
                                        crate::wm::WindowContent::Mcp,
                                        "MCP Servers",
                                        canvas,
                                    );
                                    if let Some(p) =
                                        app.wm.panels.iter_mut().find(|p| p.content_key == crate::wm::ContentKey::Mcp)
                                    {
                                        p.content = Some(Box::new(
                                            crate::window::mcp_panel::McpPanelContent { scroll: 0 },
                                        ));
                                    }
                                } else if let Some(r) = app.last_goal_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.goal_collapsed = !app.goal_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_plan_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.plan_collapsed = !app.plan_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_todo_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.todo_collapsed = !app.todo_collapsed;
                                    app.save_ui_state();
                                } else if app.sidebar_popup.is_some()
                                    && app
                                        .last_sidebar_popup_rect
                                        .map(|r| !rect_contains(r, me.column, me.row))
                                        .unwrap_or(true)
                                {
                                    app.sidebar_popup = None;
                                } else if let Some((key, _)) = app
                                    .last_sidebar_strip_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(**r, me.column, me.row))
                                {
                                    if let Some(idx) = key
                                        .strip_prefix("plan:")
                                        .and_then(|s| s.parse::<usize>().ok())
                                    {
                                        app.sidebar_popup =
                                            Some(crate::sidebar::SidebarPopupKind::Plan(idx));
                                    } else if let Some(idx) = key
                                        .strip_prefix("todo:")
                                        .and_then(|s| s.parse::<usize>().ok())
                                    {
                                        app.sidebar_popup =
                                            Some(crate::sidebar::SidebarPopupKind::Todo(idx));
                                    }
                                } else if let Some(r) = app.last_ctx_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.context_collapsed = !app.context_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_meta_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.meta_collapsed = !app.meta_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_mcp_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.mcp_collapsed = !app.mcp_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_collapse_btn_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.sidebar_collapsed = true;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_expand_btn_rect
                                    && rect_contains(r, me.column, me.row)
                                    && !app.sidebar_collapse_locked
                                {
                                    app.sidebar_collapsed = false;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_task_panel_rect
                                    && me.column >= r.x
                                    && me.column < r.x + 10
                                    && me.row == r.y + 1
                                {
                                    app.task_panel_collapsed = !app.task_panel_collapsed;
                                    app.save_ui_state();
                                } else if let Some(r) = app.last_task_panel_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    let hm = &app.last_task_panel_hitmap;
                                    let hit_kill = hm
                                        .kill_rects
                                        .iter()
                                        .find(|(_, kr)| rect_contains(*kr, me.column, me.row))
                                        .map(|(t, _)| t.clone());
                                    if let Some(tid) = hit_kill {
                                        if app.kill_armed_id == Some(tid.clone())
                                            && !app.kill_arm_expired()
                                        {
                                            if let Some(tr) = &app.task_registry {
                                                tr.kill(&tid);
                                            }
                                            app.clear_kill_arm();
                                        } else {
                                            let label = app
                                                .task_snapshots
                                                .iter()
                                                .find(|s| s.id == tid)
                                                .map(|s| s.label.clone())
                                                .unwrap_or_default();
                                            app.arm_kill(tid.clone());
                                            app.push_note(
                                                format!("press ✕ again to kill {label}"),
                                                app::NoteLevel::Warn,
                                            );
                                        }
                                    } else if let Some((handle, _)) = hm
                                        .insert_rects
                                        .iter()
                                        .find(|(_, ir)| rect_contains(*ir, me.column, me.row))
                                    {
                                        editor.prefill(&format!("{handle} "));
                                        app.refresh_popup(editor.buf());
                                        app.clear_kill_arm();
                                    } else if let Some((kind, _)) = hm
                                        .group_header_rects
                                        .iter()
                                        .find(|(_, hr)| rect_contains(*hr, me.column, me.row))
                                    {
                                        if app.task_panel_collapsed_groups.contains(kind) {
                                            app.task_panel_collapsed_groups.remove(kind);
                                        } else {
                                            app.task_panel_collapsed_groups.insert(*kind);
                                        }
                                    } else if let Some(hr) = &hm.history_btn_rect
                                        && rect_contains(*hr, me.column, me.row)
                                    {
                                        let canvas = app.last_transcript_rect.unwrap_or_default();
                                        let (pw, ph) = app
                                            .panel_sizes
                                            .get("__history__")
                                            .copied()
                                            .unwrap_or((0, 0));
                                        app.wm.open_with_size(
                                            "__history__",
                                            crate::wm::ContentKey::History,
                                            crate::wm::OpenPolicy::ReuseExisting,
                                            crate::wm::WindowContent::History,
                                            "History",
                                            canvas,
                                            pw,
                                            ph,
                                            false,
                                        );
                                        if let Some(p) =
                                            app.wm.panels.iter_mut().find(|p| p.content_key == crate::wm::ContentKey::History)
                                        {
                                            p.content = Some(Box::new(
                                                crate::window::history_panel::HistoryPanelContent {
                                                    scroll: 0,
                                                },
                                            ));
                                        }
                                    } else if let Some((run_id, node_id, _)) = hm
                                        .activity_rects
                                        .iter()
                                        .find(|(_, _, ar)| rect_contains(*ar, me.column, me.row))
                                        .map(|(r, n, a)| (r, n, a))
                                    {
                                        let canvas = app.last_transcript_rect.unwrap_or_default();
                                        let panel_id = format!("{run_id}:{node_id}");
                                        let node = app
                                            .activity_nodes
                                            .iter()
                                            .find(|n| n.run_id == *run_id && n.node_id == *node_id);
                                        if let Some(node) = node {
                                            app.wm.open(
                                                &panel_id,
                                                crate::wm::ContentKey::Activity(run_id.clone()),
                                                crate::wm::WindowContent::Activity { run_id: run_id.clone() },
                                                &node.label,
                                                canvas,
                                            );
                                            if let Some(p) = app
                                                .wm
                                                .panels
                                                .iter_mut()
                                                .find(|p| p.content_key == crate::wm::ContentKey::Activity(run_id.clone()))
                                            {
                                                p.content = Some(Box::new(
                                                    crate::window::activity_panel::ActivityPanelContent {
                                                        run_id: panel_id.clone(),
                                                        scroll: 0,
                                                    },
                                                ));
                                            }
                                        }
                                    } else {
                                        let hit_task = hm
                                            .task_rects
                                            .iter()
                                            .find(|(_, tr)| rect_contains(*tr, me.column, me.row))
                                            .map(|(h, _)| h.clone());
                                        let hit_header = hm
                                            .task_header_rects
                                            .iter()
                                            .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                            .map(|(h, _)| h.clone());
                                        if let Some(handle) = hit_task {
                                            app.clear_kill_arm();
                                            let is_header = hit_header.as_deref() == Some(&handle);
                                            let is_expanded =
                                                app.expanded_tasks.contains(&handle);
                                            if is_header && is_expanded {
                                                app.expanded_tasks.remove(&handle);
                                            } else if is_header && !is_expanded {
                                                app.expanded_tasks.insert(handle.clone());
                                            } else {
                                                let canvas =
                                                    app.last_transcript_rect.unwrap_or_default();
                                                app.open_task_panel(&handle, canvas);
                                            }
                                        }
                                    }
                                } else if let Some((panel_idx, node_id)) =
                                    app.hit_test_node(me.column, me.row)
                                {
                                    if node_id
                                        == crate::output::COLLAPSED_CARD_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) =
                                            app.workflow_panel_task_handle(panel_idx)
                                        {
                                            app.open_task_panel_maximized(&handle);
                                        }
                                    } else if node_id
                                        == crate::output::TERMINAL_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) = app.terminal_item_handle(panel_idx) {
                                            let canvas =
                                                app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id == crate::output::BASH_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) = app.bash_item_handle(panel_idx) {
                                            let canvas =
                                                app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id
                                        == crate::output::SUB_AGENT_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) =
                                            app.sub_agent_item_handle(panel_idx)
                                        {
                                            let canvas =
                                                app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id == crate::output::MERMAID_FULLSCREEN_KEY
                                    {
                                        let canvas =
                                            app.last_transcript_rect.unwrap_or_default();
                                        app.open_mermaid_panel(panel_idx, canvas);
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
                                        if let Some(handle) = app.workflow_panel_task_handle(idx) {
                                            app.open_task_panel_maximized(&handle);
                                        }
                                    } else {
                                        app.toggle_workflow_panel_expansion(idx);
                                    }
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Terminal { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_terminal_expand(idx);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Bash { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_bash_expand(idx);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::SubAgentActivity { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_sub_agent_expand(idx);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::DiffPreview { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_diff_preview_expand(idx);
                                } else if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::CompactionSummary { .. }) =
                                        app.items.get(idx)
                                {
                                    app.toggle_compaction_summary_expand(idx);
                                }
                                }
                            } else if let MouseEventKind::Drag(MouseButton::Left) = me.kind {
                                if let Some(id) = &app.resize_target {
                                    let (ox, oy) = app.resize_offset;
                                    let dx = me.column as i32 - ox as i32;
                                    let dy = me.row as i32 - oy as i32;
                                    if dx != 0 || dy != 0 {
                                        let was_maximized = app
                                            .wm
                                            .panels
                                            .iter()
                                            .find(|p| &p.id == id)
                                            .is_some_and(|p| p.maximized);
                                        if was_maximized {
                                            let canvas = app.last_transcript_rect.unwrap_or_default();
                                            app.wm.unmaximize(*id, canvas)
                                        }
                                        if let Some(p) = app
                                            .wm
                                            .panels
                                            .iter()
                                            .find(|p| &p.id == id)
                                        {
                                            let is_term = matches!(p.content_kind, crate::wm::WindowContent::Task { kind: atman_runtime::TaskKind::Terminal, .. });
                                            let nw = (p.rect.width as i32 + dx).max(20) as u16;
                                            let nh = (p.rect.height as i32 + dy).max(6) as u16;
                                            let canvas = app.last_transcript_rect.unwrap_or_default();
                                            app.wm.resize_panel(*id, nw, nh, canvas);
                                            app.resize_offset = (me.column, me.row);
                                            if is_term {
                                                let inner_cols = nw.saturating_sub(8);
                                                let inner_rows = nh.saturating_sub(5);
                                                if inner_cols > 0 && inner_rows > 0 {
                                                    if let Some(tx) = &handle.control_tx {
                                                        let _ = tx.send(TuiControl::TermResize {
                                                            handle: app
                                                                .wm
                                                                .content_kind(*id)
                                                                .and_then(|kind| match kind {
                                                                    crate::wm::WindowContent::Task { handle, .. } => Some(handle.clone()),
                                                                    _ => None,
                                                                })
                                                                .unwrap_or_default(),
                                                            rows: inner_rows,
                                                            cols: inner_cols,
                                                        });
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(id) = &app.drag_target {
                                    let (ox, oy) = app.drag_offset;
                                    let dx = me.column as i32 - ox as i32;
                                    let dy = me.row as i32 - oy as i32;
                                    if dx != 0 || dy != 0 {
                                        let was_maximized = app
                                            .wm
                                            .panels
                                            .iter()
                                            .find(|p| &p.id == id)
                                            .is_some_and(|p| p.maximized);
                                        if was_maximized {
                                            let canvas = app.last_transcript_rect.unwrap_or_default();
                                            app.wm.unmaximize(*id, canvas)
                                        }
                                        if let Some(p) = app
                                            .wm
                                            .panels
                                            .iter()
                                            .find(|p| &p.id == id)
                                        {
                                            let nx = (p.rect.x as i32 + dx).max(0) as u16;
                                            let ny = (p.rect.y as i32 + dy).max(0) as u16;
                                            let canvas = app.last_transcript_rect.unwrap_or_default();
                                            app.wm.move_panel(*id, nx, ny, canvas);
                                            app.drag_offset = (me.column, me.row);
                                        }
                                    }
                                }
                            } else if let MouseEventKind::Up(MouseButton::Left) = me.kind {
                                if app.resize_target.is_some() {
                                    let id = app.resize_target;
                                    if let Some(id) = id {
                                        if let Some(p) = app.wm.panels.iter().find(|p| p.id == id) {
                                            if let Some(label) = app.wm.label(id) {
                                                app.panel_sizes.insert(label.to_string(), (p.rect.width, p.rect.height));
                                            }
                                            app.save_ui_state();
                                        }
                                    }
                                }
                                app.drag_target = None;
                                app.resize_target = None;
                            } else if let MouseEventKind::Moved = me.kind {
                                let skip_hover = app.startup_intro.is_some()
                                    || matches!(
                                        app.items.first(),
                                        Some(crate::app::OutputItem::StartupCard { .. })
                                    );
                                if !skip_hover {
                                let topmost_panel = app
                                    .wm
                                    .hit_test_panel(me.column, me.row)
                                    .map(|p| (p.id, p.rect));
                                if let Some((panel_id, pr)) = topmost_panel {
                                    // floating panel button hover (topmost only)
                                    let btn_hover = app
                                        .wm
                                        .hit_test_btn(me.column, me.row)
                                        .filter(|(id, _)| {
                                            topmost_panel
                                                .as_ref()
                                                .map(|(pid, _)| id == pid)
                                                .unwrap_or(false)
                                        });
                                    if app.hovered_panel_btn != btn_hover {
                                        app.hovered_panel_btn = btn_hover;
                                        app.wm_visual_version = app.wm_visual_version.wrapping_add(1);
                                    }

                                    // floating panel history row hover (only in this panel)
                                    let history_hover = app
                                        .last_wm_hitmap
                                        .history_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, h, _)| h.clone());
                                    if app.hovered_history_row != history_hover {
                                        app.hovered_history_row = history_hover;
                                        app.items_version = app.items_version.wrapping_add(1);
                                    }

                                    let mcp_hover = app
                                        .last_wm_hitmap
                                        .mcp_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, n, _)| n.clone());
                                    if app.hovered_mcp_row != mcp_hover {
                                        app.hovered_mcp_row = mcp_hover;
                                    }

                                    // Over a floating panel — base-layer hovers cleared.
                                    app.hovered_sidebar_row = None;
                                    app.hovered_sidebar_hamburger = false;
                                    app.hovered_sidebar_lower = false;
                                    app.hovered_sidebar_more = false;
                                    app.set_hovered_thinking(None);
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                    app.set_hovered_history_btn(false);
                                    app.set_hovered_hamburger(false);
                                } else {
                                    // Not over a panel — panel hovers cleared.
                                    if app.hovered_panel_btn.is_some() {
                                        app.hovered_panel_btn = None;
                                        app.wm_visual_version = app.wm_visual_version.wrapping_add(1);
                                    }
                                    if app.hovered_history_row.is_some() {
                                        app.hovered_history_row = None;
                                        app.items_version = app.items_version.wrapping_add(1);
                                    }
                                    if app.hovered_mcp_row.is_some() {
                                        app.hovered_mcp_row = None;
                                    }
                                    // Sidebar strip hover
                                let sidebar_hover = app
                                    .last_sidebar_strip_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(**r, me.column, me.row))
                                    .map(|(k, _)| k.clone());
                                if app.hovered_sidebar_row != sidebar_hover {
                                    app.hovered_sidebar_row = sidebar_hover;
                                }

                                // Sidebar hamburger hover (upper panel title)
                                let ham_hover = app
                                    .last_collapse_btn_rect
                                    .or(app.last_upper_title_rect)
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.hovered_sidebar_hamburger != ham_hover {
                                    app.hovered_sidebar_hamburger = ham_hover;
                                }

                                // Sidebar lower panel title hover
                                let lower_hover = app
                                    .last_lower_title_rect
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.hovered_sidebar_lower != lower_hover {
                                    app.hovered_sidebar_lower = lower_hover;
                                }

                                // Sidebar MCP "more" row hover
                                let more_hover = app
                                    .last_sidebar_more_rect
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.hovered_sidebar_more != more_hover {
                                    app.hovered_sidebar_more = more_hover;
                                }

                                if let Some(idx) = app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Thinking { .. }) =
                                        app.items.get(idx)
                                {
                                    app.set_hovered_thinking(Some(idx));
                                } else {
                                    app.set_hovered_thinking(None);
                                }
                                let hm = &app.last_task_panel_hitmap;
                                if let Some(kr) = hm
                                    .kill_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.set_hovered_kill(Some(kr.0.clone()));
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                    app.set_hovered_history_btn(false);
                                } else if let Some(ir) = hm
                                    .insert_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.set_hovered_insert(Some(ir.0.clone()));
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_activity(None);
                                    app.set_hovered_history_btn(false);
                                } else if let Some(tr) = hm
                                    .task_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    let snap = app
                                        .task_snapshots
                                        .iter()
                                        .find(|s| s.source_handle == tr.0);
                                    app.set_hovered_task(snap.map(|s| s.id.clone()));
                                    app.set_hovered_kill(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                    app.set_hovered_history_btn(false);
                                } else if let Some(ar) = hm
                                    .activity_rects
                                    .iter()
                                    .find(|(_, _, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.set_hovered_activity(Some((ar.0.clone(), ar.1.clone())));
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_history_btn(false);
                                } else if let Some(hr) = &hm.history_btn_rect
                                    && rect_contains(*hr, me.column, me.row)
                                {
                                    app.set_hovered_history_btn(true);
                                    app.set_hovered_hamburger(false);
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                } else if let Some(hr) = &hm.hamburger_rect
                                    && rect_contains(*hr, me.column, me.row)
                                {
                                    app.set_hovered_hamburger(true);
                                    app.set_hovered_history_btn(false);
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                } else {
                                    app.set_hovered_kill(None);
                                    app.set_hovered_task(None);
                                    app.set_hovered_insert(None);
                                    app.set_hovered_activity(None);
                                    app.set_hovered_history_btn(false);
                                    app.set_hovered_hamburger(false);
                                }
                                }
                                } // if !skip_hover
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
                    app.scroll_up((-scroll_delta) as u32);
                } else if scroll_delta > 0 {
                    app.scroll_down(scroll_delta as u32);
                }
            }
            frame = merge_rx.recv() => {
                if let Some(frame) = frame {
                    // Spawn per-FlowRun frame_tx forwarders when new FlowRuns appear.
                    let new_handle: Option<String> = match &frame {
                        StreamFrame::FlowStart { parent_run_id: None, .. } => {
                            session_for_sub.as_ref().and_then(|s| s.current_root())
                        }
                        StreamFrame::SubAgentStarted { handle, .. } => Some(handle.clone()),
                        _ => None,
                    };
                    if let Some(h) = new_handle
                        && let Some(sess) = &session_for_sub
                        && let Ok(entry) = sess.flow_registry.lookup(&h)
                    {
                        let tx = frame_merge_tx.clone();
                        let mut rx = entry.frame_tx.subscribe();
                        tokio::spawn(async move {
                            while let Ok(f) = rx.recv().await {
                                let _ = tx.send(f);
                            }
                        });
                    }
                    app.apply_stream_frame(frame);
                    let mut drained = 0u32;
                    while drained < 256 {
                        match merge_rx.try_recv() {
                            Ok(extra) => {
                                app.apply_stream_frame(extra);
                                drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                } else {
                    break;
                }
            }
            ev = recv_task_event(handle.task_event_rx.as_mut()) => {
                if let Some(ev) = ev {
                    app.apply_task_event(ev);
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
            inj = recv_injection(handle.injection_rx.as_mut()) => {
                if let Some(inj) = inj {
                    // Keep only pending injections, drop consumed/cancelled ones.
                    if matches!(inj.state, atman_runtime::injection::InjectionState::Pending) {
                        if !app.pending_injections.iter().any(|i| i.id == inj.id) {
                            app.pending_injections.push(inj);
                        }
                    } else {
                        app.pending_injections.retain(|i| i.id != inj.id);
                    }
                    app.mark_items_dirty();
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
                            let rows = key_handler::enumerate_session_rows(&app, scope);
                            app.session_switcher.open_with(rows, scope);
                        }
                        TuiCommand::OpenTrustModePicker => {
                            app.trust_mode_picker_open = true;
                        }
                        TuiCommand::OpenThemePicker => {
                            app.theme_picker_open = true;
                        }
                        TuiCommand::OpenModelPicker => {
                            app.model_picker.open();
                        }
                        TuiCommand::CycleOutside => {
                            if app.trust.mode == atman_runtime::trust::TrustMode::Eager {
                                app.trust.outside = app.trust.outside.next();
                                app.mark_items_dirty();
                            } else {
                                app.push_note("outside switch only available in eager mode", app::NoteLevel::Warn);
                            }
                        }
                        TuiCommand::ProviderModelsUpdated => {
                            app.provider_manager.refresh_list();
                            app.push_toast(
                                "models refreshed",
                                app::NoteLevel::Success,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::ProviderTestResult((msg, ok)) => {
                            let level = if ok {
                                app::NoteLevel::Success
                            } else {
                                app::NoteLevel::Error
                            };
                            app.push_toast(
                                msg,
                                level,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpTestResult { name, message, ok } => {
                            let level = if ok {
                                app::NoteLevel::Success
                            } else {
                                app::NoteLevel::Error
                            };
                            app.push_toast(
                                format!("{name}: {message}"),
                                level,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpReloaded => {
                            app.push_toast(
                                "MCP servers reloaded",
                                app::NoteLevel::Success,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpResourcesResult { name, resources } => {
                            app.mcp_resources_cache.insert(name, resources);
                        }
                        TuiCommand::McpPromptsResult { name, prompts } => {
                            app.mcp_prompts_cache.insert(name, prompts);
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
pub(crate) fn build_sigterm_stream() -> Option<tokio::signal::unix::Signal> {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()
}

#[cfg(not(unix))]
pub(crate) fn build_sigterm_stream() -> Option<()> {
    None
}

#[cfg(unix)]
pub(crate) async fn wait_sigterm(sig: Option<&mut tokio::signal::unix::Signal>) {
    match sig {
        Some(s) => {
            let _ = s.recv().await;
        }
        None => std::future::pending().await,
    }
}

#[cfg(not(unix))]
pub(crate) async fn wait_sigterm(_sig: Option<&mut ()>) {
    std::future::pending().await
}

async fn recv_note(rx: Option<&mut mpsc::UnboundedReceiver<TuiNote>>) -> Option<TuiNote> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_injection(
    rx: Option<&mut tokio::sync::broadcast::Receiver<atman_runtime::injection::Injection>>,
) -> Option<atman_runtime::injection::Injection> {
    match rx {
        Some(r) => r.recv().await.ok(),
        None => std::future::pending().await,
    }
}

async fn recv_task_event(
    rx: Option<&mut tokio::sync::broadcast::Receiver<atman_runtime::TaskEvent>>,
) -> Option<atman_runtime::TaskEvent> {
    match rx {
        Some(r) => r.recv().await.ok(),
        None => std::future::pending().await,
    }
}

struct ReaderGuard(Arc<AtomicBool>);

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub(crate) fn spawn_event_reader() -> (
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

async fn poll_update_check(handle: &mut tokio::task::JoinHandle<Option<String>>) -> Option<String> {
    handle.await.ok().flatten()
}

/// Render a list of toast notes. Public so boot_animation can reuse it.
pub(crate) fn render_toast_notes(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    toasts: &[app::ToastNote],
) {
    if toasts.is_empty() {
        return;
    }
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};

    let theme = crate::theme::theme();
    let max_w = 44u16.min(area.width / 2);
    let item_h = 4u16;
    let fade_duration = std::time::Duration::from_millis(400);
    let now = std::time::Instant::now();

    let positions = [
        app::ToastPosition::TopRight,
        app::ToastPosition::TopLeft,
        app::ToastPosition::BottomRight,
        app::ToastPosition::BottomLeft,
        app::ToastPosition::TopCenter,
    ];

    for &pos in &positions {
        let group: Vec<&app::ToastNote> = toasts.iter().filter(|t| t.position == pos).collect();
        if group.is_empty() {
            continue;
        }

        let (base_x, y_start) = match pos {
            app::ToastPosition::TopRight => {
                (area.x + area.width.saturating_sub(max_w + 2), area.y + 1)
            }
            app::ToastPosition::TopLeft => (area.x + 1, area.y + 1),
            app::ToastPosition::BottomRight => (
                area.x + area.width.saturating_sub(max_w + 2),
                area.y + area.height.saturating_sub(group.len() as u16 * item_h + 1),
            ),
            app::ToastPosition::BottomLeft => (
                area.x + 1,
                area.y + area.height.saturating_sub(group.len() as u16 * item_h + 1),
            ),
            app::ToastPosition::TopCenter => {
                (area.x + (area.width.saturating_sub(max_w)) / 2, area.y + 1)
            }
        };

        // Calculate y offsets: fading toasts shift up, others close the gap
        let mut fade_offsets = vec![0u16; group.len()];
        for i in 0..group.len() {
            if group[i].fading {
                if let Some(fs) = group[i].fade_started {
                    let progress = (now.duration_since(fs).as_millis() as f64
                        / fade_duration.as_millis() as f64)
                        .clamp(0.0, 1.0);
                    fade_offsets[i] = (item_h as f64 * progress) as u16;
                }
            }
        }

        for (i, toast) in group.iter().enumerate() {
            let (glyph, color, bg, label) = match toast.level {
                NoteLevel::Error => ("✗", theme.error.into(), theme.note_error_bg, " ERROR "),
                NoteLevel::Warn => ("!", theme.warn.into(), theme.note_warn_bg, " WARN "),
                NoteLevel::Info => ("·", theme.accent.into(), theme.note_info_bg, " INFO "),
                NoteLevel::Success => ("✓", theme.success.into(), theme.note_success_bg, " OK "),
                NoteLevel::Debug => ("›", theme.tinted_fg.into(), theme.note_debug_bg, " DEBUG "),
            };

            // Accumulate offset from all fading toasts before this one
            let shift_up: u16 = fade_offsets[..i].iter().sum();

            let inner_w = max_w.saturating_sub(4);
            let rect = ratatui::layout::Rect {
                x: base_x,
                y: (y_start + i as u16 * item_h)
                    .saturating_sub(shift_up)
                    .saturating_sub(fade_offsets[i]),
                width: max_w,
                height: item_h,
            };

            let mut style = Style::default();
            let mut border_style = Style::default().fg(color);

            if toast.fading {
                if let Some(fs) = toast.fade_started {
                    let progress = (now.duration_since(fs).as_millis() as f64
                        / fade_duration.as_millis() as f64)
                        .clamp(0.0, 1.0);
                    if progress > 0.7 {
                        // Almost gone — hide completely
                        continue;
                    }
                    style = style.add_modifier(Modifier::DIM);
                    border_style = border_style.add_modifier(Modifier::DIM);
                }
            }

            f.render_widget(Clear, rect);

            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(border_style)
                .title(Span::styled(
                    label,
                    border_style.add_modifier(Modifier::BOLD),
                ))
                .style(style.bg(bg.into()));
            let inner = block.inner(rect);
            f.render_widget(block, rect);

            // Progress bar
            let elapsed = toast.created.elapsed();
            let remaining = toast.ttl.saturating_sub(elapsed);
            let pct = remaining.as_secs_f64() / toast.ttl.as_secs_f64().max(0.1);
            let bar_w = ((inner_w as f64 * pct.clamp(0.0, 1.0)) as u16).max(1);
            let bar_rect = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y,
                width: bar_w,
                height: 1,
            };
            f.render_widget(
                Block::default().style(Style::default().bg(color).add_modifier(if toast.fading {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                })),
                bar_rect,
            );

            let msg = format!(" {glyph} {}", toast.message);
            let text = Paragraph::new(Line::from(Span::styled(
                msg,
                style.add_modifier(Modifier::BOLD),
            )));
            let text_rect = ratatui::layout::Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: inner_w.saturating_sub(2),
                height: 1,
            };
            f.render_widget(text, text_rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn to_rgb(c: Color) -> (u8, u8, u8) {
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::DarkGray => (96, 96, 96),
            Color::Cyan => (0, 200, 200),
            Color::Green => (0, 200, 0),
            Color::Yellow => (200, 200, 0),
            Color::Red => (200, 0, 0),
            _ => (128, 128, 128),
        }
    }

    #[test]
    fn pulse_wave_boundary_colors_darkgray_to_fallback() {
        let border = Color::DarkGray;
        let peak = Color::Rgb(64, 192, 255);
        let _below = 0.009;
        let above = 0.011;

        let above_lerped = lerp_rgb(border, peak, above);
        let (br, bg, bb) = to_rgb(border);
        let (pr, pg, pb) = to_rgb(peak);
        let (lr, lg, lb) = to_rgb(above_lerped);

        // Below threshold: raw DarkGray (ANSI color variant).
        // Above threshold: lerp_rgb produces Rgb(...) — different Color variant.
        // Even if values are close, ANSI vs RGB renders differently in terminal.
        assert!(
            matches!(border, Color::DarkGray),
            "below threshold is ANSI DarkGray"
        );
        assert!(
            matches!(above_lerped, Color::Rgb(..)),
            "above threshold becomes RGB variant: {above_lerped:?} (border={br},{bg},{bb} peak={pr},{pg},{pb} → lerp={lr},{lg},{lb})"
        );
    }

    #[test]
    fn pulse_wave_boundary_colors_cyan_to_accent() {
        // Cyan border → peak = accent = Cyan → same color → uses fallback.
        let border = Color::Cyan;
        let peak = Color::Rgb(64, 192, 255);

        let _below = 0.009;
        let above = 0.011;

        eprintln!("=== Cyan border + fallback peak ===");
        eprintln!("border  (Cyan)        = {:?}", to_rgb(border));
        eprintln!("peak    (fallback)    = {:?}", to_rgb(peak));
        eprintln!("wave={_below:.3} → raw border = {:?}", to_rgb(border));
        eprintln!(
            "wave={above:.3} → lerp_rgb   = {:?}",
            to_rgb(lerp_rgb(border, peak, above))
        );
        eprintln!("jump: {:?} → {:?}", border, lerp_rgb(border, peak, above));
    }

    #[test]
    fn pulse_wave_boundary_colors_green_to_accent() {
        // Green border → peak = accent = Cyan.
        let border = Color::Green;
        let peak = Color::Cyan;

        let _below = 0.009;
        let above = 0.011;

        eprintln!("=== Green border + Cyan accent ===");
        eprintln!("border  (Green)       = {:?}", to_rgb(border));
        eprintln!("peak    (Cyan)        = {:?}", to_rgb(peak));
        eprintln!("wave={_below:.3} → raw border = {:?}", to_rgb(border));
        eprintln!(
            "wave={above:.3} → lerp_rgb   = {:?}",
            to_rgb(lerp_rgb(border, peak, above))
        );
        eprintln!("jump: {:?} → {:?}", border, lerp_rgb(border, peak, above));
    }
}
