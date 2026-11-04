use std::io::{Stdout, stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use crossterm::event::Event as CtEvent;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{broadcast, mpsc};

pub mod agent_presentation;
pub mod alias_manager;
pub mod app;
pub mod approval_bar;
pub mod boot_animation;
pub mod clipboard;
pub mod compact_review_modal;
pub mod completion;
mod composer;
pub mod control;
mod instance_view;
pub use control::ControlSender;
pub use instance_view::{FlowViewContent, FlowViewSource};
mod directional_selector;
mod flow_activity;
pub mod flow_hub;
pub mod flow_view;
mod focus_rail;
pub mod form_modal;
pub mod highlight;
pub mod history;
pub mod history_search_modal;
pub mod mcp_manager;
pub mod model_browser;
pub mod model_manager;
pub mod model_picker;
pub mod new_session;
pub mod onboarding;
pub mod project_hub;
mod project_sidebar;
pub mod settings;
mod settings_form;
mod surface;
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
mod panel_registry;
pub mod prompt_resolver;
pub mod provider_manager;
pub mod render;
pub mod selection;
pub mod selection_menu;
pub mod session_switcher;
pub mod sidebar;
pub mod sidebar_selection;
pub mod states;
pub mod status;
pub mod submission_queue;
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
    SearchHistory {
        request: atman_runtime::history_search::SearchRequest,
        cancellation: tokio_util::sync::CancellationToken,
    },
    ListSessionChoices,
    OpenLegacySession(uuid::Uuid),
    OpenSession(uuid::Uuid),
    RecoverIndexedHistory(Box<atman_runtime::history_index::IndexReview>),
    RenameSessionChoice(Box<session_switcher::RenameChoice>),
    AutoNameSessionChoice(Box<session_switcher::SessionSelection>),
    ChangeSessionProject(Box<flow_hub::SessionProjectChange>),
    SelectDefaultFlow(Box<flow_hub::FlowCatalogSelection>),
    ArchiveFlow(Box<flow_hub::FlowArchiveChange>),
    SubmitControls(Box<flow_hub::OperatorControls>),
    RetryControls(uuid::Uuid),
    UpdateManagement(Box<atman_runtime::flow_store::profile::ReplaceManagement>),
    SelectManagement(Box<flow_hub::ManagementSelection>),
    SetWorkspaceRetention(Box<flow_hub::WorkspaceRetention>),
    ReleaseWorkspace(Box<flow_hub::WorkspaceRemoval>),
    EditInbox(Box<atman_runtime::flow_store::inbox::InboxEditRequest>),
    BrowseFlows(flow_hub::HubRequest),
    CancelWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        id: uuid::Uuid,
    },
    RegisterWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        registration: flow_hub::WatchDefinition,
    },
    ReplaceWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        expected: flow_hub::WatchDefinition,
        replacement: Box<flow_hub::WatchDefinition>,
    },
    FocusFlow {
        flow: control::ViewAddress,
        placement: control::FlowPlacement,
        reuse: Option<control::ViewBinding>,
    },
    Submit(TuiSubmission),
    UpdateTrust(atman_runtime::trust::TrustConfig),
    CancelFlow,
    GoalControl {
        request_id: Option<u64>,
        target: Option<String>,
        goal_id: Option<String>,
        revision: Option<u64>,
        action: atman_runtime::goal::GoalAction,
    },
    HardStop,
    EditQueuedSubmission {
        id: atman_runtime::SubmissionId,
        expected_revision: u64,
        text: String,
    },
    MoveQueuedSubmission {
        id: atman_runtime::SubmissionId,
        expected_revision: u64,
        direction: atman_runtime::SubmissionMove,
    },
    DeleteQueuedSubmission {
        id: atman_runtime::SubmissionId,
        expected_revision: u64,
    },
    InsertQueuedSubmission {
        id: atman_runtime::SubmissionId,
        expected_revision: u64,
    },
    InterveneQueuedSubmission {
        id: atman_runtime::SubmissionId,
        expected_revision: u64,
    },
    ResolvePermission {
        selector: atman_runtime::permission::PermissionSelector,
        expected_revision: u64,
        action: atman_runtime::permission::PermissionAction,
        grant_scope: Option<atman_runtime::permission::GrantScope>,
        reason: Option<String>,
    },
    CompactNow,
    AutoNameSession,
    RemoveSession(Box<crate::flow_hub::SessionRemovalRequest>),
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
        project_root: Option<std::path::PathBuf>,
    },
    NewSession,
    NewProjectSession {
        fingerprint: String,
    },
    CreateSession {
        request: uuid::Uuid,
        selection: Option<new_session::Selection>,
    },
    OpenManagement,
    MoveSession,
    DeleteSession(String),
    Projects(project_hub::ProjectRequest),
    RenameSession {
        session_id: String,
        title: Option<String>,
    },
    FormSubmit {
        request_id: u64,
        form_id: String,
        submission: atman_runtime::form::FormSubmission,
    },
    /// Execute a provider mutation; completed operations return one matching result.
    MutateProvider(ProviderMutationRequest),
    MutateModel(ModelMutationRequest),
    MutateSettings(settings::SettingsMutationRequest),
    OpenAliasManager {
        model: Option<String>,
    },
    OnboardingInit,
    SwitchModel {
        request_id: u64,
        model: String,
    },
    TestProviderConnection(ProviderConnectionTestRequest),
    TestModel(ModelTestRequest),
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
    ListKnowledge,
    ReloadRules,
    GetConfessionHistory(atman_runtime::memory::MemoryId),
    ReviseConfession {
        id: atman_runtime::memory::MemoryId,
        base_revision: u64,
        fields: atman_runtime::memory::confession::ConfessionFields,
    },
    ArchiveConfession {
        id: atman_runtime::memory::MemoryId,
        base_revision: u64,
    },
    SuggestOrganization {
        request_id: u64,
    },
    CancelOrganization,
    OrganizeConfessions(Vec<atman_runtime::memory::confession::ConfessionChange>),
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RuleView {
    pub name: String,
    pub description: String,
    pub scope: String,
    pub source: String,
    pub source_path: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct OrganizationProposal {
    pub id: atman_runtime::memory::MemoryId,
    pub base_revision: u64,
    pub category: String,
    pub related_ids: Vec<atman_runtime::memory::MemoryId>,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct SessionPickerRow {
    pub selection: Option<session_switcher::SessionSelection>,
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
    HistorySearchResults {
        result: Result<atman_runtime::history_search::SearchResults, String>,
    },
    SessionChoices {
        result: Result<Vec<SessionPickerRow>, String>,
    },
    SessionChoiceRenamed {
        result: Result<session_switcher::RenameChoice, String>,
    },
    /// Resolve a logical destination before reusing or attaching its view.
    FlowResolved {
        flow: control::ViewAddress,
    },
    FlowCatalog {
        result: Result<Box<flow_hub::FlowHubPage>, String>,
    },
    FocusFlow {
        flow: control::ViewAddress,
        placement: control::FlowPlacement,
        attachment: Option<Box<TuiHandle>>,
    },
    /// Returned through the captured flow reply mailbox after the host accepts or rejects input.
    SubmissionResult {
        result: Result<(), String>,
    },
    /// A historical submission was accepted by a separate executable conversation.
    SubmissionContinued {
        flow: control::ViewAddress,
        attachment: Box<TuiHandle>,
    },
    InboxEdited {
        request_id: uuid::Uuid,
        result: Result<atman_runtime::event_log::journal::CommitReceipt, String>,
    },
    /// A captured file import returned to the requesting composer without accepting a message.
    AttachmentImported {
        result: Result<atman_runtime::message::ImageSource, String>,
    },
    FormSubmissionResult {
        request_id: u64,
        form_id: String,
        result: Result<bool, String>,
    },
    SetSidebar(sidebar::SidebarMode),
    /// Copy captured source text using the requesting view's clipboard handling.
    CopyText(String),
    OpenSessionSwitcher,
    OpenProjectHub,
    OpenProjectStorageScopePicker,
    OpenTrustModePicker,
    OpenThemePicker,
    OpenModelPicker,
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
    ModelMutationResult {
        request: ModelMutationRequest,
        result: Result<ModelMutationSuccess, String>,
    },
    ProviderTestResult {
        request_id: u64,
        result: Result<String, String>,
    },
    ModelTestResult {
        request_id: u64,
        result: Result<String, String>,
    },
    McpTestResult {
        name: String,
        message: String,
        ok: bool,
    },
    McpReloaded,
    SettingsMutationResult {
        request: settings::SettingsMutationRequest,
        result: Result<String, String>,
    },
    NewSessionLocations(new_session::Locations),
    SessionCreated {
        flow: control::ViewAddress,
        attachment: Box<TuiHandle>,
    },
    NewSessionFailed {
        request: uuid::Uuid,
        error: String,
    },
    KnowledgeResult(
        Result<
            (
                Vec<atman_runtime::memory::confession::ConfessionView>,
                Vec<RuleView>,
            ),
            String,
        >,
    ),
    ConfessionHistoryResult {
        id: atman_runtime::memory::MemoryId,
        result: Result<Vec<atman_runtime::memory::confession::ConfessionChange>, String>,
    },
    ConfessionSaved(atman_runtime::memory::MemoryId),
    ConfessionSaveFailed {
        id: atman_runtime::memory::MemoryId,
        error: String,
    },
    ConfessionArchived,
    ConfessionArchiveFailed(String),
    OrganizationProgress {
        request_id: u64,
        completed: usize,
        total: usize,
    },
    OrganizationResult {
        request_id: u64,
        result: Result<Vec<OrganizationProposal>, String>,
    },
    OrganizationApplied,
    OrganizationApplyFailed(String),
    ProjectCatalogUpdated {
        request_id: uuid::Uuid,
        success_message: String,
        result: Result<project_hub::ProjectDirectory, String>,
    },
    QueueMutationRejected(String),
    GoalSaved {
        request_id: u64,
        result: Result<(), String>,
    },
    Toast {
        message: String,
        level: app::NoteLevel,
    },
    SessionNameUpdated(String),
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
    RemoveConfig {
        name: String,
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
        api: Option<atman_runtime::provider::OpenAiApi>,
        max_tools: Option<usize>,
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
    ConfigRemoved {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMutationRequest {
    pub request_id: u64,
    pub action: ModelMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMutation {
    Upsert {
        old_name: Option<String>,
        name: String,
        model: String,
        provider: Option<String>,
        context_budget: u64,
        reasoning: atman_runtime::provider::ReasoningSelection,
        max_tokens: Option<u32>,
        api: Option<atman_runtime::provider::OpenAiApi>,
        enabled: bool,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMutationSuccess {
    Saved { name: String },
    Removed { name: String },
}

/// Tests a model draft without saving it or executing returned tool calls.
#[derive(Debug, Clone)]
pub struct ModelTestRequest {
    pub request_id: u64,
    pub identity: Option<String>,
    pub provider: String,
    pub model: String,
    pub api: Option<atman_runtime::provider::OpenAiApi>,
    pub reasoning: atman_runtime::provider::ReasoningSelection,
    pub max_tokens: Option<u32>,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// Checks a provider draft's directory access without saving its configuration.
#[derive(Debug, Clone)]
pub struct ProviderConnectionTestRequest {
    pub request_id: u64,
    pub name: String,
    pub entry: atman_runtime::model_registry::ProviderEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectMutation {
    SetPinned { fingerprint: String, pinned: bool },
    SetArchived { fingerprint: String, archived: bool },
    Delete { fingerprint: String },
}

impl ProjectMutation {
    pub fn fingerprint(&self) -> &str {
        match self {
            Self::SetPinned { fingerprint, .. }
            | Self::SetArchived { fingerprint, .. }
            | Self::Delete { fingerprint } => fingerprint,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TuiSubmission {
    pub text: String,
    pub images: Vec<atman_runtime::message::ImageSource>,
    pub reasoning: Option<atman_runtime::provider::ReasoningSelection>,
    pub presentation: Option<atman_runtime::user_input::UserInputPresentation>,
}

impl TuiSubmission {
    /// Read control intent from the authored prompt, independently of quoted context.
    pub fn control_command(&self) -> Option<&str> {
        let prompt = self
            .presentation
            .as_ref()
            .map_or(self.text.as_str(), |value| &value.prompt)
            .trim_start();
        prompt.starts_with(':').then_some(prompt)
    }
}

pub struct TuiHandle {
    pub settings: settings::SettingsSource,
    pub session_id: String,
    pub session_dir: String,
    pub session_name: Option<String>,
    /// Committed container metadata shared by views of the same persistent Session.
    pub session_name_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    /// The immutable entry that records this Session's historical provenance, independent of its default or focused flow.
    pub continued_session: Option<atman_runtime::flow_store::FlowRef>,
    pub project_root: Option<String>,
    pub projects: Vec<atman_runtime::project_catalog::ProjectRecord>,
    pub goal: Option<String>,
    pub stream_rx: Option<broadcast::Receiver<StreamFrame>>,
    pub task_event_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::TaskEvent>>,
    pub activity: Option<atman_runtime::flow_runtime::activity::ActivityReader>,
    pub submit_tx: Option<mpsc::UnboundedSender<TuiSubmission>>,
    pub note_rx: Option<mpsc::UnboundedReceiver<TuiNote>>,
    pub shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    pub control_tx: Option<crate::ControlSender>,
    pub cmd_rx: Option<mpsc::UnboundedReceiver<TuiCommand>>,
    pub control_reply_rx: Option<mpsc::UnboundedReceiver<control::ControlReply>>,
    pub flow_view: Option<FlowViewSource>,
    pub live_output: Option<atman_runtime::flow_runtime::live::LiveOutputReader>,
    pub initial_items: Vec<app::OutputItem>,
    pub goal_rx: Option<tokio::sync::watch::Receiver<Option<String>>>,
    pub context_rx: Option<tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
    pub attach_rx: Option<tokio::sync::watch::Receiver<usize>>,
    pub todos_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>>,
    pub plans_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>>,
    pub trust_rx: Option<tokio::sync::watch::Receiver<atman_runtime::trust::TrustConfig>>,
    pub compact_review_rx:
        Option<tokio::sync::watch::Receiver<Option<atman_runtime::PendingCompactReview>>>,
    pub form_rx: Option<tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>>,
    pub injection_rx: Option<tokio::sync::broadcast::Receiver<atman_runtime::injection::Injection>>,
    pub queued_submission_rx:
        Option<tokio::sync::watch::Receiver<Vec<atman_runtime::QueuedSubmissionView>>>,
    pub flow_names: Vec<(String, String)>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub startup_card: Option<app::StartupCard>,
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
            settings: Default::default(),
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            session_name: atman_runtime::session_meta::SessionMeta::load(session.dir())
                .and_then(|meta| meta.title),
            session_name_rx: None,
            continued_session: None,
            project_root: atman_runtime::session_meta::SessionMeta::load(session.dir())
                .and_then(|meta| meta.project_root)
                .map(|path| path.display().to_string()),
            projects: Vec::new(),
            goal: session.goal(),
            stream_rx: Some(session.stream_subscribe()),
            task_event_rx: None,
            activity: None,
            submit_tx: None,
            note_rx: None,
            shutdown_rx: None,
            control_tx: None,
            cmd_rx: None,
            control_reply_rx: None,
            flow_view: None,
            live_output: None,
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
            queued_submission_rx: Some(session.subscribe_queued_submissions()),
            flow_names: Vec::new(),
            session: Some(session),
            startup_card: None,
            startup_intro: None,
            onboarding_recommended: false,
            trust: atman_runtime::trust::TrustConfig::default(),
            task_registry: None,
            permission_client: Some(permission_client),
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

pub(crate) struct ReaderGuard {
    shutdown: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn spawn_event_reader() -> (
    tokio::sync::mpsc::UnboundedReceiver<std::io::Result<CtEvent>>,
    ReaderGuard,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_thread = shutdown.clone();
    let thread = std::thread::Builder::new()
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
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        break;
                    }
                }
            }
        })
        .expect("spawn tui input thread");
    (
        rx,
        ReaderGuard {
            shutdown,
            thread: Some(thread),
        },
    )
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
    use ratatui::text::Span;
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let theme = crate::theme::theme();
    let max_w = 44u16.min(area.width.saturating_sub(4));
    let available_h = area.height.saturating_sub(2);
    if max_w < 8 || available_h < 4 {
        return;
    }
    let text_w = max_w.saturating_sub(4);
    fn paragraph(toast: &app::ToastNote) -> Paragraph<'_> {
        Paragraph::new(toast.message.as_str()).wrap(Wrap { trim: false })
    }
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
        // Keep the newest notices that fit; size text with the renderer itself.
        let mut group = Vec::new();
        let mut heights = Vec::new();
        let mut total_h = 0u16;
        for toast in toasts.iter().rev().filter(|t| t.position == pos) {
            let height = paragraph(toast)
                .line_count(text_w)
                .saturating_add(3)
                .min(available_h as usize) as u16;
            if total_h + height > available_h {
                break;
            }
            group.push(toast);
            heights.push(height);
            total_h += height;
        }
        group.reverse();
        heights.reverse();
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
                area.y + area.height.saturating_sub(total_h + 1),
            ),
            app::ToastPosition::BottomLeft => {
                (area.x + 1, area.y + area.height.saturating_sub(total_h + 1))
            }
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
                    fade_offsets[i] = (heights[i] as f64 * progress) as u16;
                }
            }
        }

        for (i, toast) in group.iter().enumerate() {
            let (color, bg, label) = match toast.level {
                NoteLevel::Error => (theme.error.into(), theme.note_error_bg, " ERROR "),
                NoteLevel::Warn => (theme.warn.into(), theme.note_warn_bg, " WARN "),
                NoteLevel::Info => (theme.accent.into(), theme.note_info_bg, " INFO "),
                NoteLevel::Success => (theme.success.into(), theme.note_success_bg, " OK "),
                NoteLevel::Debug => (theme.tinted_fg.into(), theme.note_debug_bg, " DEBUG "),
            };

            // Accumulate offset from all fading toasts before this one
            let shift_up: u16 = fade_offsets[..i].iter().sum();

            let item_h = heights[i];
            let preceding_h: u16 = heights[..i].iter().sum();
            let rect = ratatui::layout::Rect {
                x: base_x,
                y: (y_start + preceding_h)
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
            let bar_w = ((text_w as f64 * pct.clamp(0.0, 1.0)) as u16).max(1);
            let bar_rect = ratatui::layout::Rect {
                x: inner.x,
                y: inner.y,
                width: bar_w,
                height: 1,
            };
            f.render_widget(
                Block::default().style(
                    Style::default()
                        .bg(theme.work_hover_bg.into())
                        .add_modifier(if toast.fading {
                            Modifier::DIM
                        } else {
                            Modifier::empty()
                        }),
                ),
                bar_rect,
            );

            let text = paragraph(toast).style(style);
            let text_rect = ratatui::layout::Rect {
                x: inner.x + 1,
                y: inner.y + 1,
                width: text_w,
                height: item_h.saturating_sub(3),
            };
            f.render_widget(text, text_rect);
            if paragraph(toast).line_count(text_w) > text_rect.height as usize {
                f.render_widget(
                    Paragraph::new("…").style(style.bg(bg.into())),
                    ratatui::layout::Rect::new(
                        text_rect.x,
                        text_rect.bottom().saturating_sub(1),
                        text_rect.width,
                        1,
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn toast_layout_wraps_complete_messages_and_keeps_newest_notices_visible() {
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};

        for position in [
            app::ToastPosition::TopRight,
            app::ToastPosition::BottomRight,
        ] {
            for width in [30, 100] {
                let mut state = app::AppState::default();
                for i in 0..5 {
                    state.push_toast(
                        format!(
                            "Input not accepted: unknown persistent flow command; use :help ({i})"
                        ),
                        NoteLevel::Error,
                        std::time::Duration::from_secs(10),
                        position,
                    );
                }
                let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
                terminal
                    .draw(|frame| {
                        render_toast_notes(frame, Rect::new(0, 0, width, 24), &state.toasts);
                    })
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let visible: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
                let compact: String = visible
                    .chars()
                    .filter(|c| !c.is_whitespace() && !"│┌┐└┘─".contains(*c))
                    .collect();
                assert!(compact.contains("use:help(4)"), "{visible}");
                assert!(
                    !compact.contains("use:help(0)"),
                    "old notices must yield space"
                );
            }
        }
    }

    #[test]
    fn input_reader_shutdown_finishes_before_owner_returns() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let (finished, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            finished.send(()).unwrap();
        });
        drop(ReaderGuard {
            shutdown,
            thread: Some(thread),
        });
        assert_eq!(
            rx.try_recv(),
            Ok(()),
            "a subsequent view must own stdin exclusively"
        );
    }

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
