use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::Event as CtEvent;
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
pub mod daemon_adapter;
mod directional_selector;
pub mod form_modal;
pub mod highlight;
pub mod history;
pub mod history_search_modal;
pub mod mcp_manager;
pub mod model_browser;
pub mod model_manager;
pub mod model_picker;
pub mod onboarding;
pub mod window;
pub mod wm;

pub mod input;
pub(crate) mod key_handler;
pub mod keys;
pub mod layout;
pub mod markdown;
pub mod mermaid;
pub mod output;
pub mod palette;
mod projection_adapter;
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
pub(crate) mod wrapped_text;

use app::NoteLevel;
use atman_runtime::stream::StreamFrame;
use event_loop::*;
use render::*;
use terminal_guard::TerminalGuard;
pub use ui_state::*;
mod event_loop;
mod ui_state;

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

#[non_exhaustive]
pub enum TuiControl {
    Domain(TuiDomainCommand),
    MetaCommand(String),
    AutoNameSession,
    SwitchSession {
        sid: String,
        intro: app::StartupIntro,
    },
    NewSession,
    ListSessions {
        scope: session_switcher::SessionScope,
    },
    MoveSession,
    MutateProvider(ProviderMutationRequest),
    UpsertConfigModel {
        old_name: Option<String>,
        name: String,
        model: String,
        provider: Option<String>,
        context_budget: u64,
        reasoning: atman_runtime::provider::ReasoningSelection,
        max_tokens: Option<u32>,
        enabled: bool,
    },
    OpenAliasManager {
        model: Option<String>,
    },
    OnboardingInit,
    SwitchModel {
        request_id: u64,
        model: String,
    },
    TestProvider {
        name: String,
        entry: atman_runtime::model_registry::ProviderEntry,
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
    LoadOlderHistory,
    LoadNewerHistory,
    LoadToolDetail {
        tool_use_id: String,
    },
}

/// Session mutations shared by embedded and daemon-backed TUI hosts.
#[non_exhaustive]
pub enum TuiDomainCommand {
    Submit(TuiSubmission),
    UpdateTrust(atman_runtime::trust::TrustConfig),
    CancelFlow,
    HardStop,
    ResolvePermission {
        selector: atman_runtime::permission::PermissionSelector,
        expected_revision: u64,
        action: atman_runtime::permission::PermissionAction,
        grant_scope: Option<atman_runtime::permission::GrantScope>,
        reason: Option<String>,
    },
    CompactNow,
    CompactReviewAccept {
        review_id: String,
        edited: Option<String>,
    },
    CompactReviewReject {
        review_id: String,
    },
    DeleteSession(String),
    RenameSession {
        session_id: String,
        title: Option<String>,
    },
    FormSubmit {
        form_id: String,
        submission: atman_runtime::form::FormSubmission,
    },
    TermResize {
        resource_id: atman_proto::ResourceId,
        rows: u16,
        cols: u16,
    },
}

impl From<TuiDomainCommand> for TuiControl {
    fn from(command: TuiDomainCommand) -> Self {
        Self::Domain(command)
    }
}

#[derive(Debug, Clone)]
pub struct SessionPickerRow {
    pub id: String,
    pub is_current: bool,
    pub name: Option<String>,
    pub project: Option<String>,
    pub message_count: usize,
    pub updated_at: String,
    pub goal: Option<String>,
}

#[non_exhaustive]
pub enum TuiCommand {
    SetSidebar(sidebar::SidebarMode),
    OpenSessionSwitcher,
    OpenTrustModePicker,
    OpenThemePicker,
    OpenModelPicker,
    AddDraftAttachment(atman_runtime::message::ImageSource),
    ClearDraftAttachments,
    ListDraftAttachments,
    ModelSwitchResult {
        request_id: u64,
        model: String,
        result: Result<String, String>,
    },
    /// Refresh catalog-backed UI after a successful configuration change.
    ///
    /// `added_provider` is `Some` only after a provider was added. Provider
    /// updates and model changes use `None`.
    ProviderCatalogChanged {
        added_provider: Option<String>,
    },
    ProviderCatalogRefreshResult {
        provider_id: String,
        result: Result<atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome, String>,
    },
    /// Resolve one mutation by echoing its original request unchanged.
    ProviderMutationResult {
        request: ProviderMutationRequest,
        result: Result<ProviderMutationSuccess, String>,
    },
    ProviderTestResult((String, bool)),
    McpTestResult {
        name: String,
        message: String,
        ok: bool,
    },
    McpReloaded {
        active_runs: Option<usize>,
    },
    OpenSessionMoveForm(atman_runtime::form::PendingForm),
    CloseSessionMoveForm(String),
    OpenSuggestionForm(atman_runtime::form::PendingForm),
    CloseSuggestionForm(String),
    SessionNameUpdated(String),
    SessionListUpdated {
        scope: session_switcher::SessionScope,
        rows: Vec<SessionPickerRow>,
    },
    McpResourcesResult {
        name: String,
        resources: Vec<atman_runtime::mcp::McpResource>,
    },
    McpPromptsResult {
        name: String,
        prompts: Vec<atman_runtime::mcp::McpPrompt>,
    },
}

/// Correlates a provider mutation with its eventual host result.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProviderMutationRequest {
    /// Monotonic identifier scoped to the current TUI provider manager.
    pub request_id: u64,
    /// Requested provider operation and identity.
    pub action: ProviderMutation,
}

impl ProviderMutationRequest {
    /// Creates a provider mutation request with a host-opaque correlation ID.
    pub fn new(request_id: u64, action: ProviderMutation) -> Self {
        Self { request_id, action }
    }
}

/// Provider operations delegated by the TUI to its host.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderMutation {
    Login {
        kind: atman_runtime::auth_store::ProviderKind,
        name: String,
    },
    SetEnabled {
        provider_id: String,
        enabled: bool,
    },
    Remove {
        provider_id: String,
    },
    Refresh {
        provider_id: String,
    },
    UpsertConfig {
        name: String,
        kind: String,
        api_key: String,
        api_key_env: String,
        base_url: String,
        max_tokens: Option<u32>,
        reasoning_format: String,
        enabled: bool,
        create: bool,
    },
}

/// Result of a committed provider mutation with request identity preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderMutationSuccess {
    Installed {
        provider_id: String,
        name: String,
        kind: atman_runtime::auth_store::ProviderKind,
        delta: atman_runtime::model_registry::CatalogDelta,
    },
    StateChanged {
        provider_id: String,
        enabled: Option<bool>,
        change: atman_runtime::provider_lifecycle::ProviderStateChange,
        catalog: Option<atman_runtime::model_registry::CatalogDelta>,
    },
    Refreshed {
        provider_id: String,
        delta: atman_runtime::model_registry::CatalogDelta,
    },
    ConfigSaved {
        name: String,
        created: bool,
    },
}

#[derive(Debug, Clone)]
pub struct TuiSubmission {
    pub text: String,
    pub images: Vec<atman_runtime::message::ImageSource>,
    pub reasoning: Option<atman_runtime::provider::ReasoningSelection>,
}

pub struct TuiHandle {
    pub session_id: String,
    pub session_dir: String,
    pub session_name: Option<String>,
    pub project_root: Option<String>,
    pub goal: Option<String>,
    pub stream_rx: Option<broadcast::Receiver<StreamFrame>>,
    pub task_event_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::TaskEvent>>,
    pub submit_tx: Option<mpsc::UnboundedSender<TuiSubmission>>,
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
    pub trust_rx: Option<tokio::sync::watch::Receiver<atman_runtime::trust::TrustConfig>>,
    pub compact_review_rx:
        Option<tokio::sync::watch::Receiver<Vec<atman_runtime::PendingCompactReview>>>,
    pub form_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>>,
    pub injection_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::injection::Injection>>,
    pub daemon_state_rx: Option<tokio::sync::watch::Receiver<atman_client::SessionState>>,
    pub daemon_updates_rx: Option<tokio::sync::broadcast::Receiver<atman_client::SessionUpdate>>,
    pub flow_names: Vec<(String, String)>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub startup_intro: Option<app::StartupIntro>,
    pub onboarding_recommended: bool,
    pub trust: atman_runtime::trust::TrustConfig,
    pub task_registry: Option<atman_runtime::TaskRegistry>,
    pub permission_client: Option<atman_runtime::permission::PermissionClientGuard>,
    /// Toasts collected during boot, to be pushed to app on start.
    pub boot_toasts: Vec<app::ToastNote>,
}

impl TuiHandle {
    pub fn from_session(session: std::sync::Arc<atman_runtime::Session>) -> Self {
        let permission_client = session.permission_broker().register_client();
        Self {
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            session_name: atman_runtime::session_meta::SessionMeta::load(session.dir())
                .and_then(|meta| meta.title),
            project_root: atman_runtime::session_meta::SessionMeta::load(session.dir())
                .and_then(|meta| meta.project_root)
                .map(|path| path.display().to_string()),
            goal: session.goal(),
            stream_rx: Some(session.stream_subscribe()),
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
            trust_rx: Some(session.subscribe_trust()),
            compact_review_rx: Some(session.compact_reviews().subscribe()),
            form_rx: Some(session.forms().subscribe()),
            injection_rx: Some(session.subscribe_injections()),
            daemon_state_rx: None,
            daemon_updates_rx: None,
            flow_names: Vec::new(),
            session: Some(session),
            startup_intro: None,
            onboarding_recommended: false,
            trust: atman_runtime::trust::TrustConfig::default(),
            task_registry: None,
            permission_client: Some(permission_client),
            boot_toasts: Vec::new(),
        }
    }

    pub fn from_daemon(session: &atman_client::SessionClient) -> Self {
        let state = session.current();
        let projection = state.projection();
        Self {
            session_id: session.session_id().to_string(),
            session_dir: String::new(),
            session_name: (!projection.metadata.title.is_empty())
                .then(|| projection.metadata.title.clone()),
            project_root: projection.metadata.project_root.clone(),
            goal: projection.goal.clone(),
            stream_rx: None,
            task_event_rx: None,
            submit_tx: None,
            note_rx: None,
            shutdown_rx: None,
            control_tx: None,
            cmd_rx: None,
            initial_items: Vec::new(),
            goal_rx: None,
            context_rx: None,
            attach_rx: None,
            todos_rx: None,
            plans_rx: None,
            trust_rx: None,
            compact_review_rx: None,
            form_rx: None,
            injection_rx: None,
            daemon_state_rx: Some(session.subscribe()),
            daemon_updates_rx: Some(session.subscribe_updates()),
            flow_names: Vec::new(),
            session: None,
            startup_intro: None,
            onboarding_recommended: false,
            trust: atman_runtime::trust::TrustConfig::default(),
            task_registry: None,
            permission_client: None,
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

pub(crate) struct ReaderGuard(Arc<AtomicBool>);

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
