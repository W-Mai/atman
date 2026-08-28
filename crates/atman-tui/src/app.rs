use std::collections::HashSet;
use std::time::{Duration, Instant};

use atman_runtime::message::Message;
use atman_runtime::stream::CompactionPhase;
use atman_runtime::stream::StreamFrame;
use atman_runtime::tools::term::TerminalScreen;
use atman_runtime::workflow::WorkflowGraph;

const LAG_COOLDOWN: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TerminalViewMode {
    Stream,
    Capture,
}

#[derive(Debug, Clone)]
pub enum OutputItem {
    UserTurn {
        text: String,
    },
    Thinking {
        text: String,
        done: bool,
        expanded: bool,
        retried: bool,
    },
    AssistantMd {
        md: String,
        streaming: bool,
        retried: bool,
    },
    SystemNote {
        text: String,
        level: NoteLevel,
    },
    Divider,
    WorkflowPanel {
        turn_index: usize,
        graph: WorkflowGraph,
        expanded_nodes: HashSet<String>,
        panel_expanded: bool,
        started_at: Instant,
        ended_at: Option<Instant>,
        cancelled: bool,
    },
    StartupCard {
        version: String,
        recent: Vec<StartupSessionEntry>,
    },
    Terminal {
        handle: String,
        screen: TerminalScreen,
        accumulated_bytes: Vec<u8>,
        mode: TerminalViewMode,
        done: bool,
        expanded: bool,
        scroll_offset: Option<(u16, u16)>,
    },
    Bash {
        handle: String,
        output: String,
        done: bool,
        expanded: bool,
    },
    DiffPreview {
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
        expanded: bool,
    },
    CompactionSummary {
        phase: CompactionPhase,
        range_start: usize,
        range_end: usize,
        summary: String,
        before_tokens: u64,
        after_tokens: u64,
        compacted_count: usize,
        expanded: bool,
    },
    MermaidDiagram {
        source: String,
    },
    SubAgentActivity {
        handle: String,
        goal: String,
        child_run_id: String,
        model: String,
        status: String,
        output: String,
        iteration: u64,
        done: bool,
        expanded: bool,
        messages: Vec<Message>,
        workflow_graph: WorkflowGraph,
        expanded_nodes: HashSet<String>,
        workflow_expanded: bool,
    },
}

impl OutputItem {
    pub fn handle(&self) -> Option<&str> {
        match self {
            OutputItem::Terminal { handle, .. }
            | OutputItem::Bash { handle, .. }
            | OutputItem::SubAgentActivity { handle, .. } => Some(handle),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartupIntro {
    pub started_at: Instant,
    pub version: String,
    pub recent: Vec<StartupSessionEntry>,
}

#[derive(Debug, Clone)]
pub struct StartupSessionEntry {
    pub session_id: String,
    pub short_id: String,
    pub goal: Option<String>,
    pub project: Option<String>,
    pub age_label: String,
    pub event_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Err,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteLevel {
    Info,
    Warn,
    Error,
    Success,
    Debug,
}

/// Toast notification displayed in the top-right corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastPosition {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
    TopCenter,
}

#[derive(Debug, Clone)]
pub struct ToastNote {
    pub id: String,
    pub level: NoteLevel,
    pub message: String,
    pub ttl: std::time::Duration,
    pub created: std::time::Instant,
    pub position: ToastPosition,
    pub fading: bool,
    pub fade_started: Option<std::time::Instant>,
}

#[derive(Debug, Clone)]
pub struct PendingPermission {
    pub request_id: atman_runtime::permission::PermissionRequestId,
    pub revision: u64,
    pub payload: atman_runtime::permission_audit::PermissionRequestAudit,
}

#[derive(Debug, Clone)]
pub struct PendingPermissionGroup {
    pub group_id: atman_runtime::permission::PermissionGroupId,
    pub revision: u64,
    pub payload: atman_runtime::permission_audit::PermissionGroupAudit,
    pub expanded: bool,
}

#[derive(Default)]
pub struct AppState {
    pub items: Vec<OutputItem>,
    pub input: String,
    pub scroll_offset: u32,
    pub follow_tail: bool,
    pub should_quit: bool,
    pub streaming: bool,
    pub was_streaming: bool,
    pub border_fade_at: Option<std::time::Instant>,
    pub waiting_for_llm: bool,
    pub goal: Option<String>,
    pub session_id: String,
    pub session_dir: String,
    pub session_name: Option<String>,
    pub project_root: Option<String>,
    pub latest_release: Option<String>,
    pub attach_count: usize,
    pub context: atman_runtime::ContextSnapshot,
    pub todos: Vec<atman_runtime::memory::todo::Todo>,
    pub plans: Vec<atman_runtime::memory::plan::Plan>,
    pub pending_permissions: std::collections::BTreeMap<
        atman_runtime::permission::PermissionRequestId,
        PendingPermission,
    >,
    pub pending_permission_groups: std::collections::BTreeMap<
        atman_runtime::permission::PermissionGroupId,
        PendingPermissionGroup,
    >,
    pub pending_injections: Vec<atman_runtime::injection::Injection>,
    pub yank_mode: bool,
    pub yank_index: usize,
    pub sidebar_mode: crate::sidebar::SidebarMode,
    pub popup: crate::completion::PopupState,
    pub flow_names: Vec<(String, String)>,
    pub expanded_tools: HashSet<String>,
    /// Toast notifications (top-right corner, auto-dismiss).
    pub toasts: Vec<ToastNote>,
    /// Status bar notes keyed by slot id (e.g. "compact", "daemon").
    pub status_notes: std::collections::HashMap<String, String>,
    /// Active modal notification (dismiss with Esc).
    pub modal_notification: Option<String>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub trust: atman_runtime::trust::TrustConfig,
    pub picker_selected: usize,
    pub last_item_ranges: Vec<crate::output::ItemRange>,
    pub last_node_regions: Vec<crate::output::NodeRegion>,
    pub last_transcript_rect: Option<ratatui::layout::Rect>,
    pub last_full_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_rect: Option<ratatui::layout::Rect>,
    pub input_rect: Option<ratatui::layout::Rect>,
    pub hovered_thinking_idx: Option<usize>,
    pub hovered_task_id: Option<atman_runtime::TaskId>,
    pub hovered_kill_id: Option<atman_runtime::TaskId>,
    pub hovered_insert_handle: Option<String>,
    pub hovered_activity: Option<(String, String)>,
    pub hovered_history_btn: bool,
    pub hovered_hamburger: bool,
    pub kill_armed_id: Option<atman_runtime::TaskId>,
    pub kill_armed_at: Option<Instant>,
    pub startup_intro: Option<StartupIntro>,
    pub onboarding_skipped: bool,
    pub hints_dismissed: bool,
    pub animation_frame: u32,
    pub deny_arm: Option<std::time::Instant>,
    pub approval_scope_index: u8,
    pub selected_permission_group: Option<atman_runtime::permission::PermissionGroupId>,
    pub items_version: u64,
    pub wm_visual_version: u64,
    pub expanded_version: u64,
    pub terminal_throttle: Option<Instant>,
    pub layout_cache: crate::output::LayoutCache,
    pub last_total_rows: u32,
    pub last_document_visible_rows: u32,
    pub last_input_overlay_rows: u32,
    pub last_items_len: usize,
    pub mouse_captured: bool,
    pub handle_index: std::collections::HashMap<String, usize>,
    pub last_workflow_panel_idx: Option<usize>,
    pub workflow_run_to_panel: std::collections::HashMap<String, usize>,
    pub top_level_run_ids: std::collections::HashSet<String>,
    pub sub_agent_run_ids: std::collections::HashMap<String, usize>,
    pub goal_scroll: u16,
    pub plans_scroll: u16,
    pub todos_scroll: u16,
    pub goal_collapsed: bool,
    pub plan_collapsed: bool,
    pub todo_collapsed: bool,
    pub context_collapsed: bool,
    pub meta_collapsed: bool,
    pub mcp_collapsed: bool,
    pub sidebar_collapsed: bool,
    pub sidebar_upper_collapsed: bool,
    pub sidebar_lower_collapsed: bool,
    pub hovered_sidebar_row: Option<String>,
    pub hovered_sidebar_hamburger: bool,
    pub hovered_sidebar_lower: bool,
    pub hovered_sidebar_more: bool,
    pub sidebar_popup: Option<crate::sidebar::SidebarPopupKind>,
    pub last_sidebar_popup_rect: Option<ratatui::layout::Rect>,
    pub expanded_tasks: std::collections::HashSet<String>,
    pub sidebar_collapse_locked: bool,
    pub sidebar_upper_runtime_collapsed: bool,
    pub sidebar_lower_runtime_collapsed: bool,
    pub task_panel_runtime_collapsed: bool,
    pub task_registry: Option<atman_runtime::TaskRegistry>,
    pub task_snapshots: Vec<atman_runtime::TaskSnapshot>,
    pub activity_nodes: Vec<crate::task_panel::ActivityNode>,
    pub task_panel_collapsed: bool,
    pub task_panel_collapsed_groups: std::collections::HashSet<atman_runtime::TaskKind>,
    pub panel_sizes: std::collections::HashMap<String, (u16, u16)>,
    pub expanded_mcp_servers: HashSet<String>,
    pub mcp_selected: usize,
    pub mcp_remove_armed: Option<String>,
    pub mcp_add_form: Option<crate::mcp_manager::McpAddForm>,
    pub mcp_browser_tab: crate::mcp_manager::McpBrowserTab,
    pub mcp_resources_cache:
        std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>>,
    pub mcp_prompts_cache: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>>,
    pub last_task_panel_rect: Option<ratatui::layout::Rect>,
    pub last_task_panel_hitmap: crate::task_panel::TaskPanelHitMap,
    pub tick: u64,
    pub last_goal_rect: Option<ratatui::layout::Rect>,
    pub last_plan_rect: Option<ratatui::layout::Rect>,
    pub last_todo_rect: Option<ratatui::layout::Rect>,
    pub last_goal_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_plan_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_todo_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_ctx_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_meta_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_mcp_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_collapse_btn_rect: Option<ratatui::layout::Rect>,
    pub last_expand_btn_rect: Option<ratatui::layout::Rect>,
    pub last_upper_title_rect: Option<ratatui::layout::Rect>,
    pub last_lower_title_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_more_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_strip_rects: std::collections::HashMap<String, ratatui::layout::Rect>,
    pub select_mode_hinted: bool,
    last_lag_note_idx: Option<usize>,
    last_lag_at: Option<Instant>,
    last_lag_count: u64,
}

pub use atman_runtime::stream::frame_run_id;

impl AppState {
    pub fn new(session_id: String, goal: Option<String>) -> Self {
        Self {
            session_id,
            goal,
            follow_tail: true,
            mouse_captured: true,
            wm_visual_version: 0,
            ..Default::default()
        }
    }

    pub fn toggle_mouse_capture(&mut self) -> bool {
        self.mouse_captured = !self.mouse_captured;
        self.mark_items_dirty();
        self.mouse_captured
    }

    pub fn save_ui_state(&self) {
        let state = crate::states::PersistedUiState::snapshot(self);
        if let Err(e) = state.save() {
            atman_runtime::notify!(warn, "failed to save ui state: {e}");
        }
    }

    /// Background-task-completion open: never steals focus from a focused
    /// floating panel; if a panel is focused, keep its focus and notify via
    /// toast.
    /// Rebuild an `OutputItem::Bash` for a completed bash task whose in-memory
    /// item was evicted (e.g. after a history restore) but whose session log
    /// file still exists on disk at `<session_dir>/bg_<handle>.log`. Returns
    /// `None` when no snapshot/log is available.
    pub(crate) fn reconstruct_bash_item(
        &mut self,
        snap: &Option<atman_runtime::TaskSnapshot>,
    ) -> Option<OutputItem> {
        let snap = snap.as_ref()?;
        if snap.kind != atman_runtime::TaskKind::Bash {
            return None;
        }
        if snap.source_handle.is_empty() {
            return None;
        }
        let log_path =
            std::path::Path::new(&self.session_dir).join(format!("bg_{}.log", snap.source_handle));
        let raw = std::fs::read_to_string(&log_path).ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        // The reader writes `[out] `/`[err] ` prefixes to the log, matching the
        // `[err] `/no-prefix scheme used in the live `OutputItem::Bash.output`.
        let output = raw
            .lines()
            .map(|l| {
                l.strip_prefix("[out] ")
                    .or_else(|| l.strip_prefix("[err] "))
                    .map(str::to_string)
                    .unwrap_or_else(|| l.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(OutputItem::Bash {
            handle: snap.source_handle.clone(),
            output,
            done: snap.status.is_terminal(),
            expanded: false,
        })
    }

    pub fn with_initial_items(mut self, items: Vec<OutputItem>) -> Self {
        self.items = items;
        self.items_version = self.items_version.wrapping_add(1);
        self
    }

    pub fn with_session_dir(mut self, dir: String) -> Self {
        self.session_dir = dir;
        self
    }

    pub fn with_session_identity(
        mut self,
        name: Option<String>,
        project_root: Option<String>,
    ) -> Self {
        self.session_name = name;
        self.project_root = project_root;
        self
    }

    pub fn with_flow_names(mut self, flows: Vec<(String, String)>) -> Self {
        self.flow_names = flows;
        self
    }

    pub fn with_session(mut self, session: Option<std::sync::Arc<atman_runtime::Session>>) -> Self {
        self.session = session;
        self
    }

    pub fn with_trust(mut self, trust: atman_runtime::trust::TrustConfig) -> Self {
        self.trust = trust;
        self
    }

    pub fn with_task_registry(mut self, tr: atman_runtime::TaskRegistry) -> Self {
        self.task_registry = Some(tr);
        self
    }

    pub fn is_tool_expanded(&self, id: &str) -> bool {
        self.expanded_tools.contains(id)
    }

    pub fn toggle_tool_expansion(&mut self, id: &str) {
        if !self.expanded_tools.remove(id) {
            self.expanded_tools.insert(id.to_string());
        }
        self.expanded_version = self.expanded_version.wrapping_add(1);
    }

    pub fn toggle_last_tool_expansion(&mut self) -> bool {
        self.toggle_last_workflow_tool_node()
    }

    pub fn workflow_panel_task_handle(&self, panel_idx: usize) -> Option<String> {
        let item = self.items.get(panel_idx)?;
        if let crate::app::OutputItem::WorkflowPanel { graph, .. } = item {
            for node in &graph.root {
                if let atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. } = &node.kind {
                    return Some(run_id.clone());
                }
            }
        }
        None
    }

    pub fn terminal_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::Terminal { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn bash_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::Bash { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn sub_agent_item_handle(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::SubAgentActivity { handle, .. } = item {
            return Some(handle.clone());
        }
        None
    }

    pub fn mermaid_item_source(&self, idx: usize) -> Option<String> {
        let item = self.items.get(idx)?;
        if let crate::app::OutputItem::MermaidDiagram { source } = item {
            return Some(source.clone());
        }
        None
    }

    pub fn maximized_canvas(&self) -> ratatui::layout::Rect {
        let full = self.last_full_rect.unwrap_or_default();
        let transcript = self.last_transcript_rect.unwrap_or(full);
        let bottom_reserved = self
            .input_rect
            .map(|r| {
                full.y
                    .saturating_add(full.height)
                    .saturating_sub(r.y)
                    .saturating_add(1)
            })
            .unwrap_or(0);
        let top = full
            .y
            .saturating_add(full.height)
            .saturating_sub(bottom_reserved);
        let y = transcript.y.max(full.y);
        ratatui::layout::Rect {
            x: full.x,
            y,
            width: full.width,
            height: top.saturating_sub(y),
        }
    }

    pub fn mcp_browser_state(&self) -> crate::mcp_manager::McpBrowserState<'_> {
        crate::mcp_manager::McpBrowserState {
            tab: self.mcp_browser_tab,
            resources: &self.mcp_resources_cache,
            prompts: &self.mcp_prompts_cache,
        }
    }

    pub fn open_task_panel(
        &mut self,
        wm: &mut crate::wm::WindowManager,
        handle: &str,
        canvas: ratatui::layout::Rect,
        maximized: bool,
        background: bool,
    ) -> bool {
        let item = self
            .items
            .iter()
            .rev()
            .find(|it| it.handle() == Some(handle))
            .cloned();
        let snap = self
            .task_snapshots
            .iter()
            .find(|s| s.source_handle == handle)
            .cloned();

        // A completed bash task may have left the in-memory item list (e.g.
        // after a history restore) while its snapshot and session log file
        // survive. Reconstruct the output so the panel shows real content
        // instead of falling through to the empty placeholder.
        let item = self.reconstruct_bash_item(&snap).or(item);

        let (kind, label, pw, ph) = if let Some(item) = item {
            match item {
                crate::app::OutputItem::Terminal {
                    handle: h, screen, ..
                } => {
                    let (pw, ph) = (screen.cols + 8, screen.rows + 5);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Terminal, label, pw, ph)
                }
                crate::app::OutputItem::Bash { handle: h, .. } => {
                    let (pw, ph) = self.panel_sizes.get(&h).copied().unwrap_or((0, 0));
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Bash, label, pw, ph)
                }
                crate::app::OutputItem::SubAgentActivity { handle: h, .. } => {
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Flow, label, 0, 0)
                }
                crate::app::OutputItem::WorkflowPanel { .. } => {
                    let kind = snap
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(atman_runtime::TaskKind::Flow);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| handle.to_string());
                    (kind, label, 0, 0)
                }
                _ => unreachable!("handle() only returns Some for Terminal/Bash"),
            }
        } else {
            let kind = snap
                .as_ref()
                .map(|s| s.kind)
                .unwrap_or(atman_runtime::TaskKind::Flow);
            let label = snap
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_else(|| handle.to_string());
            (kind, label, 0, 0)
        };

        let content: Box<dyn crate::wm::WindowComponent> = match kind {
            atman_runtime::TaskKind::Bash => {
                Box::new(crate::window::bash_panel::BashPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                })
            }
            atman_runtime::TaskKind::Terminal => {
                Box::new(crate::window::terminal_panel::TerminalPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                })
            }
            atman_runtime::TaskKind::Flow => {
                Box::new(crate::window::flow_panel::FlowPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                    render_cache: None,
                })
            }
        };

        let existing_ids: std::collections::HashSet<crate::wm::WindowId> =
            wm.panels.iter().map(|p| p.id).collect();
        let window_id = if background {
            wm.open_background_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        } else {
            wm.open_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        };
        let is_new = !existing_ids.contains(&window_id);
        if let Some(panel) = wm.panels.iter_mut().find(|panel| panel.id == window_id) {
            panel.content = Some(content);
        }
        is_new
    }

    pub fn toggle_workflow_node(&mut self, panel_index: usize, node_id: &str) {
        if let Some(item) = self.items.get_mut(panel_index) {
            let expanded_nodes = match item {
                OutputItem::WorkflowPanel { expanded_nodes, .. } => expanded_nodes,
                OutputItem::SubAgentActivity { expanded_nodes, .. } => expanded_nodes,
                _ => return,
            };
            if !expanded_nodes.remove(node_id) {
                expanded_nodes.insert(node_id.to_string());
            }
            self.expanded_version = self.expanded_version.wrapping_add(1);
        }
    }

    pub fn toggle_thinking_expanded(&mut self, item_idx: usize) {
        if let Some(OutputItem::Thinking { expanded, .. }) = self.items.get_mut(item_idx) {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_thinking(&mut self, idx: Option<usize>) {
        if self.hovered_thinking_idx != idx {
            self.hovered_thinking_idx = idx;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_task(&mut self, id: Option<atman_runtime::TaskId>) {
        if self.hovered_task_id != id {
            self.hovered_task_id = id;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_kill(&mut self, id: Option<atman_runtime::TaskId>) {
        if self.hovered_kill_id != id {
            self.hovered_kill_id = id;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_insert(&mut self, handle: Option<String>) {
        if self.hovered_insert_handle != handle {
            self.hovered_insert_handle = handle;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_activity(&mut self, key: Option<(String, String)>) {
        if self.hovered_activity != key {
            self.hovered_activity = key;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_history_btn(&mut self, hovered: bool) {
        if self.hovered_history_btn != hovered {
            self.hovered_history_btn = hovered;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn set_hovered_hamburger(&mut self, hovered: bool) {
        if self.hovered_hamburger != hovered {
            self.hovered_hamburger = hovered;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn arm_kill(&mut self, id: atman_runtime::TaskId) {
        self.kill_armed_id = Some(id);
        self.kill_armed_at = Some(Instant::now());
        self.items_version = self.items_version.wrapping_add(1);
    }

    pub fn clear_kill_arm(&mut self) {
        if self.kill_armed_id.is_some() {
            self.kill_armed_id = None;
            self.kill_armed_at = None;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn kill_arm_expired(&self) -> bool {
        match self.kill_armed_at {
            Some(t) => t.elapsed() > std::time::Duration::from_secs(2),
            None => true,
        }
    }

    fn toggle_last_workflow_tool_node(&mut self) -> bool {
        use atman_runtime::workflow::WorkflowNode;
        fn last_tool_path(nodes: &[WorkflowNode], prefix: &str) -> Option<String> {
            for (i, n) in nodes.iter().enumerate().rev() {
                let cur = if prefix.is_empty() {
                    format!("{i}")
                } else {
                    format!("{prefix}/{i}")
                };
                if let Some(hit) = last_tool_path(&n.children, &cur) {
                    return Some(hit);
                }
                if matches!(
                    n.kind,
                    atman_runtime::workflow::WorkflowNodeKind::ToolCall { .. }
                ) {
                    return Some(cur);
                }
            }
            None
        }
        for (idx, item) in self.items.iter().enumerate().rev() {
            if let OutputItem::WorkflowPanel { graph, .. } = item
                && let Some(path) = last_tool_path(&graph.root, "")
            {
                self.toggle_workflow_node(idx, &path);
                return true;
            }
        }
        false
    }

    pub fn toggle_workflow_panel_expansion(&mut self, panel_index: usize) {
        if let Some(item) = self.items.get_mut(panel_index) {
            match item {
                OutputItem::WorkflowPanel { panel_expanded, .. } => {
                    *panel_expanded = !*panel_expanded;
                }
                OutputItem::SubAgentActivity {
                    workflow_expanded, ..
                } => {
                    *workflow_expanded = !*workflow_expanded;
                }
                _ => return,
            }
            self.expanded_version = self.expanded_version.wrapping_add(1);
        }
    }

    pub fn has_running_workflow(&self) -> bool {
        self.items
            .iter()
            .any(|it| matches!(it, OutputItem::WorkflowPanel { ended_at: None, .. }))
    }

    pub fn has_active_animation(&self) -> bool {
        self.has_running_workflow()
            || self.items.iter().any(|item| {
                matches!(
                    item,
                    OutputItem::Terminal { done: false, .. }
                        | OutputItem::Bash { done: false, .. }
                        | OutputItem::SubAgentActivity { done: false, .. }
                        | OutputItem::CompactionSummary {
                            phase: CompactionPhase::Running,
                            ..
                        }
                )
            })
    }

    pub fn hit_test(&self, col: u16, row: u16) -> Option<usize> {
        let rect = self.last_transcript_rect?;
        if col < rect.x
            || col >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return None;
        }
        let rel = u32::from(row.saturating_sub(rect.y)).saturating_add(self.scroll_offset);
        self.last_item_ranges
            .iter()
            .find(|r| rel >= r.start_row && rel < r.end_row)
            .map(|r| r.item_index)
    }

    pub fn scroll_terminal(&mut self, item_index: usize, up: bool, amount: u16) {
        if let Some(OutputItem::Terminal {
            screen,
            scroll_offset,
            ..
        }) = self.items.get_mut(item_index)
        {
            let max_row = screen.rows;
            let current_row = scroll_offset.map(|(r, _)| r).unwrap_or(0);
            let new_row = if up {
                current_row.saturating_sub(amount)
            } else {
                (current_row + amount).min(max_row.saturating_sub(1))
            };
            if new_row == 0 && !up {
                *scroll_offset = None;
            } else {
                *scroll_offset = Some((new_row, 0));
            }
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_terminal_mode(&mut self, item_index: usize) {
        if let Some(OutputItem::Terminal { mode, .. }) = self.items.get_mut(item_index) {
            *mode = match *mode {
                TerminalViewMode::Capture => TerminalViewMode::Stream,
                TerminalViewMode::Stream => TerminalViewMode::Capture,
            };
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_terminal_expand(&mut self, item_index: usize) {
        if let Some(OutputItem::Terminal { expanded, .. }) = self.items.get_mut(item_index) {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_bash_expand(&mut self, item_index: usize) {
        if let Some(OutputItem::Bash { expanded, .. }) = self.items.get_mut(item_index) {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_sub_agent_expand(&mut self, item_index: usize) {
        if let Some(OutputItem::SubAgentActivity { expanded, .. }) = self.items.get_mut(item_index)
        {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_diff_preview_expand(&mut self, item_index: usize) {
        if let Some(OutputItem::DiffPreview { expanded, .. }) = self.items.get_mut(item_index) {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn toggle_compaction_summary_expand(&mut self, item_index: usize) {
        if let Some(OutputItem::CompactionSummary { expanded, .. }) = self.items.get_mut(item_index)
        {
            *expanded = !*expanded;
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    pub fn hit_test_node(&self, col: u16, row: u16) -> Option<(usize, String)> {
        let rect = self.last_transcript_rect?;
        if col < rect.x
            || col >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return None;
        }
        let rel = u32::from(row.saturating_sub(rect.y)).saturating_add(self.scroll_offset);
        let rel_col = col.saturating_sub(rect.x);
        self.last_node_regions
            .iter()
            .filter(|r| rel >= r.start_row && rel < r.end_row)
            .filter(|r| rel_col >= r.col_start && rel_col < r.col_end)
            .max_by_key(|r| r.path_key.len())
            .map(|r| (r.panel_item_index, r.path_key.clone()))
    }

    pub fn refresh_popup(&mut self, editor_buf: &str) {
        let builtins = crate::completion::builtins();
        let candidates = crate::completion::compute_candidates(
            editor_buf,
            &self.flow_names,
            &builtins,
            crate::completion::INTERJECTIONS,
            self.streaming,
        );
        self.popup.set(candidates);
    }

    pub fn max_scroll_offset(&self) -> u32 {
        let visible_above = self
            .last_document_visible_rows
            .saturating_sub(self.last_input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        self.last_total_rows.saturating_sub(visible_above)
    }

    pub fn scroll_up(&mut self, rows: u32) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.follow_tail = false;
    }

    pub fn scroll_down(&mut self, rows: u32) {
        let max = self.max_scroll_offset();
        let next = self.scroll_offset.saturating_add(rows);
        if next >= max {
            self.scroll_offset = max;
            self.follow_tail = true;
        } else {
            self.scroll_offset = next;
        }
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll_offset = 0;
        self.follow_tail = false;
    }

    pub fn scroll_to_tail(&mut self) {
        self.follow_tail = true;
    }

    pub fn resolve_scroll(
        &mut self,
        total_rows: u32,
        document_visible_rows: u32,
        input_overlay_rows: u32,
        items_len: usize,
    ) {
        let new_items = items_len > self.last_items_len;
        let old_visible_above = self
            .last_document_visible_rows
            .saturating_sub(self.last_input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        let old_max = self.last_total_rows.saturating_sub(old_visible_above);
        let was_at_bottom = self.scroll_offset >= old_max || self.last_total_rows == 0;
        self.last_total_rows = total_rows;
        self.last_document_visible_rows = document_visible_rows;
        self.last_input_overlay_rows = input_overlay_rows;
        self.last_items_len = items_len;
        let visible_above = document_visible_rows
            .saturating_sub(input_overlay_rows)
            .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
            .max(1);
        let max = total_rows.saturating_sub(visible_above);
        if self.follow_tail && (new_items || was_at_bottom) {
            self.scroll_offset = max;
        } else {
            self.scroll_offset = self.scroll_offset.min(max);
        }
    }

    pub fn pending_below_rows(&self) -> u32 {
        if self.follow_tail {
            0
        } else {
            self.max_scroll_offset().saturating_sub(self.scroll_offset)
        }
    }

    fn find_item_by_handle(&self, handle: &str) -> Option<usize> {
        let idx = *self.handle_index.get(handle)?;
        if idx < self.items.len() {
            Some(idx)
        } else {
            None
        }
    }

    pub fn push_item(&mut self, item: OutputItem) {
        let idx = self.items.len();
        match &item {
            OutputItem::Terminal { handle, .. } | OutputItem::Bash { handle, .. } => {
                self.handle_index.insert(handle.clone(), idx);
            }
            OutputItem::WorkflowPanel { ended_at: None, .. } => {
                self.last_workflow_panel_idx = Some(idx);
            }
            _ => {}
        }
        self.items.push(item);
        self.items_version = self.items_version.wrapping_add(1);
        self.reset_lag_state();
    }

    pub fn mark_items_dirty(&mut self) {
        self.items_version = self.items_version.wrapping_add(1);
    }

    pub fn mark_expanded_dirty(&mut self) {
        self.expanded_version = self.expanded_version.wrapping_add(1);
    }

    pub fn push_note(&mut self, text: impl Into<String>, level: NoteLevel) {
        self.push_item(OutputItem::SystemNote {
            text: text.into(),
            level,
        });
    }

    pub fn push_toast(
        &mut self,
        text: impl Into<String>,
        level: NoteLevel,
        ttl: std::time::Duration,
        position: ToastPosition,
    ) {
        let id = format!("toast-{}", self.toasts.len());
        self.toasts.push(ToastNote {
            id,
            level,
            message: text.into(),
            ttl,
            created: std::time::Instant::now(),
            position,
            fading: false,
            fade_started: None,
        });
        // Keep at most 5 toasts, remove oldest.
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    pub fn push_status(&mut self, key: impl Into<String>, text: impl Into<String>) {
        self.status_notes.insert(key.into(), text.into());
    }

    pub fn tick_toasts(&mut self) {
        let now = std::time::Instant::now();
        let fade_duration = std::time::Duration::from_millis(400);

        // Start fading for expired toasts that aren't already fading
        for toast in &mut self.toasts {
            if !toast.fading && now.duration_since(toast.created) >= toast.ttl {
                toast.fading = true;
                toast.fade_started = Some(now);
            }
        }

        // Remove toasts that have finished fading
        self.toasts.retain(|t| {
            if !t.fading {
                return true;
            }
            let fade_start = t.fade_started.unwrap_or(now);
            now.duration_since(fade_start) < fade_duration
        });
    }

    pub fn apply_task_event(&mut self, event: atman_runtime::TaskEvent) {
        match event {
            atman_runtime::TaskEvent::Registered(snap) => {
                self.task_snapshots.push(snap);
                self.items_version = self.items_version.wrapping_add(1);
            }
            atman_runtime::TaskEvent::StatusChanged { id, new, .. } => {
                if let Some(s) = self.task_snapshots.iter_mut().find(|s| s.id == id) {
                    s.status = new;
                    s.ended_at = Some(std::time::Instant::now());
                    self.items_version = self.items_version.wrapping_add(1);
                }
            }
            atman_runtime::TaskEvent::Reaped { id } => {
                self.task_snapshots.retain(|s| s.id != id);
                self.items_version = self.items_version.wrapping_add(1);
            }
        }
    }

    fn apply_permission_projection(&mut self, frame: &StreamFrame) {
        use atman_runtime::stream::StreamFrame;

        match frame {
            StreamFrame::PermissionRequestCreated { payload, .. }
            | StreamFrame::PermissionRequestTargeted { payload, .. }
            | StreamFrame::PermissionRequestDeferred { payload, .. } => {
                if !matches!(
                    payload.target,
                    atman_runtime::permission_audit::PermissionAuditTarget::User
                ) {
                    return;
                }
                if let Some(request_id) = payload.request_id.clone() {
                    self.pending_permissions.insert(
                        request_id.clone(),
                        PendingPermission {
                            request_id,
                            revision: payload.revision,
                            payload: payload.clone(),
                        },
                    );
                }
            }
            StreamFrame::PermissionRequestApproved { payload, .. }
            | StreamFrame::PermissionRequestDenied { payload, .. }
            | StreamFrame::PermissionRequestCancelled { payload, .. } => {
                if let Some(request_id) = &payload.request_id {
                    self.pending_permissions.remove(request_id);
                }
            }
            StreamFrame::PermissionGroupCreated { payload, .. }
            | StreamFrame::PermissionGroupUpdated { payload, .. } => {
                self.pending_permission_groups.insert(
                    payload.group_id.clone(),
                    PendingPermissionGroup {
                        group_id: payload.group_id.clone(),
                        revision: payload.revision,
                        payload: payload.clone(),
                        expanded: self
                            .pending_permission_groups
                            .get(&payload.group_id)
                            .is_some_and(|group| group.expanded),
                    },
                );
            }
            StreamFrame::PermissionGroupResolved { payload, .. } => {
                self.pending_permission_groups.remove(&payload.group_id);
                if self.selected_permission_group.as_ref() == Some(&payload.group_id) {
                    self.selected_permission_group = None;
                }
            }
            _ => {}
        }
    }

    pub fn apply_stream_frame(&mut self, frame: StreamFrame) {
        self.apply_permission_projection(&frame);
        match frame {
            StreamFrame::ThinkingChunk { text, run_id, .. } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                self.waiting_for_llm = false;
                if let Some(OutputItem::Thinking { text: t, .. }) = self.items.last_mut() {
                    t.push_str(&text);
                    self.items_version = self.items_version.wrapping_add(1);
                    self.streaming = true;
                    self.reset_lag_state();
                } else {
                    self.push_item(OutputItem::Thinking {
                        text,
                        done: false,
                        expanded: false,
                        retried: false,
                    });
                    self.streaming = true;
                }
            }
            StreamFrame::LlmChunk { text, run_id, .. } => {
                if let Some(rid) = &run_id
                    && let Some(&idx) = self.sub_agent_run_ids.get(rid)
                {
                    if let Some(OutputItem::SubAgentActivity { output, .. }) =
                        self.items.get_mut(idx)
                    {
                        output.push_str(&text);
                        self.items_version = self.items_version.wrapping_add(1);
                    }
                    self.streaming = true;
                    self.reset_lag_state();
                    return;
                }
                self.waiting_for_llm = false;
                if let Some(OutputItem::Thinking { done, .. }) = self.items.last_mut()
                    && !*done
                {
                    *done = true;
                    self.items_version = self.items_version.wrapping_add(1);
                }
                if let Some(OutputItem::AssistantMd { md, streaming, .. }) = self.items.last_mut()
                    && *streaming
                {
                    md.push_str(&text);
                    self.items_version = self.items_version.wrapping_add(1);
                    self.streaming = true;
                    self.reset_lag_state();
                } else {
                    self.push_item(OutputItem::AssistantMd {
                        md: text,
                        streaming: true,
                        retried: false,
                    });
                    self.streaming = true;
                    self.terminal_throttle = Some(Instant::now());
                    self.items_version = self.items_version.wrapping_add(1);
                }
            }
            StreamFrame::LlmRetry => {
                for item in self.items.iter_mut().rev() {
                    match item {
                        OutputItem::Thinking { done, retried, .. } => {
                            *done = true;
                            *retried = true;
                        }
                        OutputItem::AssistantMd {
                            streaming, retried, ..
                        } => {
                            *streaming = false;
                            *retried = true;
                        }
                        _ => break,
                    }
                }
                self.items_version = self.items_version.wrapping_add(1);
                self.waiting_for_llm = true;
            }
            StreamFrame::LlmDone { run_id, .. } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                let mut changed = false;
                for item in self.items.iter_mut().rev() {
                    let touched = match item {
                        OutputItem::Thinking { done, .. } if !*done => {
                            *done = true;
                            true
                        }
                        OutputItem::AssistantMd { streaming, .. } if *streaming => {
                            *streaming = false;
                            true
                        }
                        _ => false,
                    };
                    if touched {
                        changed = true;
                    } else {
                        break;
                    }
                }
                if changed {
                    self.items_version = self.items_version.wrapping_add(1);
                }
                self.streaming = false;
                self.reset_lag_state();
            }
            StreamFrame::ToolUseStart { .. } | StreamFrame::ToolUseDone { .. } => {}
            StreamFrame::Note(text) => {
                self.push_item(OutputItem::SystemNote {
                    text,
                    level: NoteLevel::Info,
                });
            }
            StreamFrame::Notification(frame) => {
                let text = frame.message;
                let level = match frame.level {
                    atman_runtime::notify::NotifyLevel::Error => NoteLevel::Error,
                    atman_runtime::notify::NotifyLevel::Warn => NoteLevel::Warn,
                    atman_runtime::notify::NotifyLevel::Info => NoteLevel::Info,
                    atman_runtime::notify::NotifyLevel::Success => NoteLevel::Success,
                    atman_runtime::notify::NotifyLevel::Debug => NoteLevel::Debug,
                };
                match frame.location {
                    atman_runtime::notify::NotifyLocation::Toast => {
                        let ttl = match frame.lifecycle {
                            atman_runtime::notify::NotifyLifecycle::Ttl(d) => d,
                            _ => std::time::Duration::from_secs(3),
                        };
                        self.push_toast(text, level, ttl, ToastPosition::TopRight);
                    }
                    atman_runtime::notify::NotifyLocation::Status => {
                        let key = match &frame.stack {
                            atman_runtime::notify::NotifyStack::Replace { key } => key.clone(),
                            atman_runtime::notify::NotifyStack::Coalesce { key } => key.clone(),
                            _ => "default".into(),
                        };
                        self.push_status(key, text);
                    }
                    atman_runtime::notify::NotifyLocation::Modal => {
                        self.modal_notification = Some(text);
                    }
                    _ => {
                        self.push_item(OutputItem::SystemNote { text, level });
                    }
                }
            }
            frame @ (StreamFrame::FlowGraph { .. }
            | StreamFrame::FlowStart { .. }
            | StreamFrame::FlowNodeStart { .. }
            | StreamFrame::FlowNodeEnd { .. }
            | StreamFrame::FlowDone { .. }
            | StreamFrame::ToolNode { .. }
            | StreamFrame::LlmCallStats { .. }
            | StreamFrame::AssistantMsg { .. }
            | StreamFrame::ToolResultMsg { .. }
            | StreamFrame::ToolPendingApproval { .. }
            | StreamFrame::ToolApproved { .. }
            | StreamFrame::ToolDenied { .. }
            | StreamFrame::PermissionRequestCreated { .. }
            | StreamFrame::PermissionRequestTargeted { .. }
            | StreamFrame::PermissionRequestDeferred { .. }
            | StreamFrame::PermissionRequestApproved { .. }
            | StreamFrame::PermissionRequestDenied { .. }
            | StreamFrame::PermissionRequestCancelled { .. }
            | StreamFrame::PermissionGroupCreated { .. }
            | StreamFrame::PermissionGroupUpdated { .. }
            | StreamFrame::PermissionGroupResolved { .. }
            | StreamFrame::PermissionGrantCreated { .. }
            | StreamFrame::PermissionGrantExpired { .. }
            | StreamFrame::UnrestrictedExecution { .. }) => {
                match &frame {
                    StreamFrame::FlowNodeStart {
                        run_id,
                        node_id,
                        kind,
                        label,
                        ..
                    } => {
                        self.activity_nodes.push(crate::task_panel::ActivityNode {
                            run_id: run_id.clone(),
                            node_id: node_id.clone(),
                            label: label.clone(),
                            kind: kind.clone(),
                            status: crate::task_panel::ActivityStatus::Running,
                            started_at: std::time::Instant::now(),
                            ended_at: None,
                        });
                        if self.activity_nodes.len() > 128 {
                            self.activity_nodes.remove(0);
                        }
                    }
                    StreamFrame::FlowNodeEnd {
                        run_id,
                        node_id,
                        status,
                        ..
                    } => {
                        let st = match status {
                            atman_runtime::event::FlowNodeStatus::Ok => {
                                crate::task_panel::ActivityStatus::Ok
                            }
                            atman_runtime::event::FlowNodeStatus::Err => {
                                crate::task_panel::ActivityStatus::Err
                            }
                            atman_runtime::event::FlowNodeStatus::Cancelled => {
                                crate::task_panel::ActivityStatus::Cancelled
                            }
                        };
                        for n in self.activity_nodes.iter_mut().rev() {
                            if n.run_id == *run_id && n.node_id == *node_id {
                                n.status = st;
                                n.ended_at = Some(std::time::Instant::now());
                                break;
                            }
                        }
                    }
                    _ => {}
                }
                let (is_done, cancelled, done_run_id) = match &frame {
                    StreamFrame::FlowDone {
                        cancelled, run_id, ..
                    } => (true, *cancelled, Some(run_id.as_str())),
                    _ => (false, false, None),
                };
                self.ensure_workflow_panel_and_apply(&frame);
                if is_done {
                    if let Some(rid) = done_run_id {
                        let st = if cancelled {
                            crate::task_panel::ActivityStatus::Cancelled
                        } else {
                            crate::task_panel::ActivityStatus::Ok
                        };
                        let now = std::time::Instant::now();
                        for n in self.activity_nodes.iter_mut() {
                            if n.run_id == rid
                                && n.status == crate::task_panel::ActivityStatus::Running
                            {
                                n.status = st;
                                n.ended_at = Some(now);
                            }
                        }
                    }
                    // Only close the panel for top-level flows — subflow
                    // FlowDone must not close the parent panel.
                    if done_run_id.is_some_and(|rid| self.top_level_run_ids.contains(rid)) {
                        let panel_idx = done_run_id
                            .and_then(|rid| self.workflow_run_to_panel.get(rid).copied());
                        self.close_current_workflow_panel(cancelled, panel_idx);
                        if let Some(rid) = done_run_id {
                            self.top_level_run_ids.remove(rid);
                        }
                    }
                    self.streaming = false;
                }
            }
            StreamFrame::TerminalChunk {
                handle,
                bytes,
                screen,
                state: _,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                self.waiting_for_llm = false;
                if self.scroll_offset >= self.max_scroll_offset() {
                    self.follow_tail = true;
                }
                let existing = self
                    .find_item_by_handle(&handle)
                    .and_then(|idx| match &self.items[idx] {
                        OutputItem::Terminal { done: false, .. } => Some(idx),
                        _ => None,
                    })
                    .and_then(|idx| self.items.get_mut(idx));
                if let Some(OutputItem::Terminal {
                    screen: s,
                    accumulated_bytes: ab,
                    ..
                }) = existing
                {
                    if let Some(new_screen) = screen {
                        *s = new_screen;
                    }
                    ab.extend_from_slice(&bytes);
                    self.items_version = self.items_version.wrapping_add(1);
                    self.reset_lag_state();
                } else {
                    self.push_item(OutputItem::Terminal {
                        handle,
                        screen: screen.unwrap_or_else(|| {
                            atman_runtime::tools::term::TerminalScreen {
                                rows: 0,
                                cols: 0,
                                cells: Vec::new(),
                                cursor: None,
                                alt_screen: false,
                            }
                        }),
                        accumulated_bytes: bytes,
                        mode: TerminalViewMode::Capture,
                        done: false,
                        expanded: false,
                        scroll_offset: None,
                    });
                    self.items_version = self.items_version.wrapping_add(1);
                    self.reset_lag_state();
                }
            }
            StreamFrame::TerminalExited { handle, run_id, .. } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                if let Some(idx) = self.find_item_by_handle(&handle) {
                    if let Some(OutputItem::Terminal { done, .. }) = self.items.get_mut(idx) {
                        *done = true;
                        self.items_version = self.items_version.wrapping_add(1);
                    }
                }
            }
            StreamFrame::BashChunk {
                handle,
                kind,
                line,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                self.waiting_for_llm = false;
                if self.scroll_offset >= self.max_scroll_offset() {
                    self.follow_tail = true;
                }
                let existing = self
                    .find_item_by_handle(&handle)
                    .and_then(|idx| match &self.items[idx] {
                        OutputItem::Bash { done: false, .. } => Some(idx),
                        _ => None,
                    })
                    .and_then(|idx| self.items.get_mut(idx));
                let prefix = if kind == "stderr" { "[err] " } else { "" };
                if let Some(OutputItem::Bash { output, .. }) = existing {
                    output.push_str(prefix);
                    output.push_str(&line);
                    self.items_version = self.items_version.wrapping_add(1);
                    self.reset_lag_state();
                } else {
                    let mut output = String::new();
                    output.push_str(prefix);
                    output.push_str(&line);
                    self.push_item(OutputItem::Bash {
                        handle,
                        output,
                        done: false,
                        expanded: false,
                    });
                    self.items_version = self.items_version.wrapping_add(1);
                    self.reset_lag_state();
                }
            }
            StreamFrame::BashExited { handle, run_id, .. } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                if let Some(idx) = self.find_item_by_handle(&handle) {
                    if let Some(OutputItem::Bash { done, .. }) = self.items.get_mut(idx) {
                        *done = true;
                        self.items_version = self.items_version.wrapping_add(1);
                    }
                }
            }
            StreamFrame::DiffPreview {
                title,
                old_content,
                new_content,
                unified_diff,
                run_id,
            } => {
                if let Some(rid) = &run_id
                    && self.sub_agent_run_ids.contains_key(rid)
                {
                    return;
                }
                self.push_item(OutputItem::DiffPreview {
                    title,
                    old_content,
                    new_content,
                    unified_diff,
                    expanded: false,
                });
            }
            StreamFrame::CompactionSummary {
                phase,
                range_start,
                range_end,
                summary,
                before_tokens,
                after_tokens,
                compacted_count,
            } => {
                if let Some(OutputItem::CompactionSummary {
                    phase: current_phase,
                    range_start: current_start,
                    range_end: current_end,
                    summary: current_summary,
                    before_tokens: current_before,
                    after_tokens: current_after,
                    compacted_count: current_count,
                    expanded,
                }) = self.items.last_mut()
                    && *current_start == range_start
                    && *current_end == range_end
                {
                    *current_phase = phase;
                    *current_summary = summary;
                    *current_before = before_tokens;
                    *current_after = after_tokens;
                    *current_count = compacted_count;
                    if matches!(phase, CompactionPhase::Finished) {
                        *expanded = false;
                    }
                    self.items_version = self.items_version.wrapping_add(1);
                } else {
                    self.push_item(OutputItem::CompactionSummary {
                        phase,
                        range_start,
                        range_end,
                        summary,
                        before_tokens,
                        after_tokens,
                        compacted_count,
                        expanded: false,
                    });
                }
            }
            StreamFrame::MermaidDiagram { source } => {
                self.push_item(OutputItem::MermaidDiagram { source });
                self.reset_lag_state();
            }
            StreamFrame::SubAgentStarted {
                handle,
                goal,
                child_run_id,
                model,
            } => {
                let idx = self.items.len();
                self.push_item(OutputItem::SubAgentActivity {
                    handle: handle.clone(),
                    goal,
                    child_run_id: child_run_id.clone(),
                    model,
                    status: "running".into(),
                    output: String::new(),
                    iteration: 0,
                    done: false,
                    expanded: false,
                    messages: Vec::new(),
                    workflow_graph: WorkflowGraph::new(atman_runtime::event::TurnId::now()),
                    expanded_nodes: HashSet::new(),
                    workflow_expanded: false,
                });
                self.sub_agent_run_ids.insert(child_run_id, idx);
            }
            StreamFrame::SubAgentDone {
                handle,
                status,
                final_text,
            } => {
                for item in self.items.iter_mut() {
                    if let OutputItem::SubAgentActivity {
                        handle: h,
                        status: s,
                        output,
                        done,
                        ..
                    } = item
                        && h == &handle
                    {
                        *s = status.clone();
                        if !final_text.is_empty() {
                            *output = final_text.clone();
                        }
                        *done = true;
                        break;
                    }
                }
                self.items_version = self.items_version.wrapping_add(1);
            }
            StreamFrame::Unknown => {}
        }
    }

    fn route_to_workflow_panel(&mut self, frame: &StreamFrame) {
        let target_idx = match frame_run_id(frame) {
            Some(run_id) => self.workflow_run_to_panel.get(run_id).copied(),
            None => self
                .items
                .iter()
                .rposition(|item| matches!(item, OutputItem::WorkflowPanel { .. })),
        };
        let Some(idx) = target_idx else {
            return;
        };
        if let Some(OutputItem::WorkflowPanel { graph, .. }) = self.items.get_mut(idx) {
            graph.apply_stream_frame(frame);
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    fn ensure_workflow_panel_and_apply(&mut self, frame: &StreamFrame) {
        // Subflow recursion creates a new run_id per iteration. If a FlowStart's
        // parent_run_id is already mapped to a SubAgentActivity, chain the new
        // run_id to the same item so subsequent LlmChunk/ThinkingChunk/etc.
        // frames route to the sub-agent instead of the main document flow.
        if let StreamFrame::FlowStart {
            run_id,
            parent_run_id: Some(parent_rid),
            ..
        } = frame
        {
            if self.sub_agent_run_ids.contains_key(run_id) {
                // already mapped (e.g. subagent flow's own FlowStart)
            } else if let Some(&idx) = self.sub_agent_run_ids.get(parent_rid) {
                self.sub_agent_run_ids.insert(run_id.clone(), idx);
            }
        }
        if let Some(rid) = frame_run_id(frame)
            && let Some(&idx) = self.sub_agent_run_ids.get(rid)
            && let Some(OutputItem::SubAgentActivity {
                workflow_graph,
                messages,
                ..
            }) = self.items.get_mut(idx)
        {
            workflow_graph.apply_stream_frame(frame);
            if let StreamFrame::AssistantMsg { message, .. }
            | StreamFrame::ToolResultMsg { message, .. } = frame
            {
                messages.push(message.clone());
                if messages.len() > 100 {
                    let start = messages.len() - 100;
                    messages.drain(..start);
                }
            }
            self.items_version = self.items_version.wrapping_add(1);
            return;
        }
        let is_panel_creator = matches!(
            frame,
            StreamFrame::FlowStart { .. } | StreamFrame::FlowGraph { .. }
        );

        // For subflows (FlowStart with parent_run_id), find and reuse the parent's panel.
        if let StreamFrame::FlowStart {
            run_id,
            parent_run_id: Some(parent_rid),
            ..
        } = frame
        {
            if let Some(&parent_idx) = self.workflow_run_to_panel.get(parent_rid) {
                if let Some(OutputItem::WorkflowPanel {
                    ended_at,
                    cancelled,
                    ..
                }) = self.items.get_mut(parent_idx)
                {
                    // Don't reopen a cancelled panel — late subflow events
                    // after a hard stop must not resurrect the spinner.
                    if ended_at.is_some() && !*cancelled {
                        *ended_at = None;
                    }
                }
                // Insert subflow run_id so nested subflows (e.g. flow.spawn)
                // can find the parent panel. The top_level_run_ids guard in
                // apply_stream_frame prevents subflow FlowDone from closing it.
                self.workflow_run_to_panel
                    .insert(run_id.clone(), parent_idx);
                if let Some(OutputItem::WorkflowPanel { graph, .. }) =
                    self.items.get_mut(parent_idx)
                {
                    graph.apply_stream_frame(frame);
                    self.items_version = self.items_version.wrapping_add(1);
                }
                return;
            }
            // Parent not in HashMap (e.g. after session resume).
            // Create a new panel for this subflow instead of falling through.
            let turn_index = self
                .items
                .iter()
                .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
                .count();
            let idx = self.items.len();
            self.push_item(OutputItem::WorkflowPanel {
                turn_index,
                graph: WorkflowGraph::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                panel_expanded: false,
                started_at: std::time::Instant::now(),
                ended_at: None,
                cancelled: false,
            });
            self.top_level_run_ids.insert(run_id.clone());
            self.workflow_run_to_panel.insert(run_id.clone(), idx);
            self.route_to_workflow_panel(frame);
            return;
        }

        // Non-panel-creator events (FlowNodeStart, FlowNodeEnd, etc.) must not
        // create phantom panels. Just route to the last open panel.
        if !is_panel_creator {
            self.route_to_workflow_panel(frame);
            return;
        }
        let mut panel_after_user_turn = false;
        let mut reopen_idx: Option<usize> = None;
        for (i, it) in self.items.iter().enumerate().rev() {
            match it {
                OutputItem::WorkflowPanel { ended_at: None, .. } => {
                    // FlowGraph reuses an open panel; FlowStart creates a new one
                    // unless its run_id is already mapped (from a prior FlowGraph).
                    if matches!(frame, StreamFrame::FlowGraph { .. })
                        || matches!(frame, StreamFrame::FlowStart { run_id, .. }
                            if self.workflow_run_to_panel.contains_key(run_id))
                    {
                        panel_after_user_turn = true;
                    }
                    break;
                }
                OutputItem::WorkflowPanel {
                    ended_at: Some(_),
                    cancelled: true,
                    ..
                } => {
                    reopen_idx = Some(i);
                    panel_after_user_turn = true;
                    break;
                }
                OutputItem::WorkflowPanel { .. } => {}
                OutputItem::UserTurn { .. } => break,
                _ => {}
            }
        }
        if let Some(idx) = reopen_idx {
            if let Some(OutputItem::WorkflowPanel {
                ended_at,
                cancelled,
                ..
            }) = self.items.get_mut(idx)
            {
                *ended_at = None;
                *cancelled = false;
            }
            if let StreamFrame::FlowStart { run_id, .. } = frame {
                self.workflow_run_to_panel.insert(run_id.clone(), idx);
                self.top_level_run_ids.insert(run_id.clone());
            }
        }
        if !panel_after_user_turn {
            let turn_index = self
                .items
                .iter()
                .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
                .count();
            let idx = self.items.len();
            self.push_item(OutputItem::WorkflowPanel {
                turn_index,
                graph: WorkflowGraph::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                panel_expanded: false,
                started_at: std::time::Instant::now(),
                ended_at: None,
                cancelled: false,
            });
            if let StreamFrame::FlowStart { run_id, .. } = frame {
                self.workflow_run_to_panel.insert(run_id.clone(), idx);
                self.top_level_run_ids.insert(run_id.clone());
            }
        }
        self.route_to_workflow_panel(frame);
    }

    pub fn close_current_workflow_panel(&mut self, cancelled: bool, panel_idx: Option<usize>) {
        let idx = panel_idx.or(self.last_workflow_panel_idx);
        if let Some(idx) = idx {
            if let Some(OutputItem::WorkflowPanel {
                ended_at,
                cancelled: cancelled_flag,
                ..
            }) = self.items.get_mut(idx)
            {
                let was_open = ended_at.is_none();
                if was_open {
                    *ended_at = Some(Instant::now());
                }
                *cancelled_flag = cancelled;
                self.items_version = self.items_version.wrapping_add(1);
            }
        }
    }

    pub fn cancel_running_activities(&mut self) {
        let now = std::time::Instant::now();
        for n in self.activity_nodes.iter_mut() {
            if n.status == crate::task_panel::ActivityStatus::Running {
                n.status = crate::task_panel::ActivityStatus::Cancelled;
                n.ended_at = Some(now);
            }
        }
        self.streaming = false;
        self.waiting_for_llm = false;
        self.reset_lag_state();
    }

    pub fn record_lag(&mut self, dropped: u64, now: Instant) {
        let within_cooldown = self
            .last_lag_at
            .map(|t| now.duration_since(t) < LAG_COOLDOWN)
            .unwrap_or(false);
        if within_cooldown
            && let Some(idx) = self.last_lag_note_idx
            && let Some(OutputItem::SystemNote { text, .. }) = self.items.get_mut(idx)
        {
            self.last_lag_count = self.last_lag_count.saturating_add(dropped);
            *text = format!("dropped {} stream frames", self.last_lag_count);
            self.last_lag_at = Some(now);
            self.items_version = self.items_version.wrapping_add(1);
            return;
        }
        self.last_lag_count = dropped;
        self.items.push(OutputItem::SystemNote {
            text: format!("dropped {dropped} stream frames"),
            level: NoteLevel::Warn,
        });
        self.items_version = self.items_version.wrapping_add(1);
        self.last_lag_note_idx = Some(self.items.len() - 1);
        self.last_lag_at = Some(now);
    }

    fn reset_lag_state(&mut self) {
        self.last_lag_note_idx = None;
        self.last_lag_count = 0;
    }

    pub fn push_user_turn(&mut self, text: String) {
        self.close_current_workflow_panel(false, None);
        self.push_item(OutputItem::UserTurn { text });
        self.waiting_for_llm = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s8_permission_frames_route_to_root_and_spawned_workflow_owners() {
        use atman_runtime::event::FlowRunId;
        use atman_runtime::permission::PermissionRequestId;
        use atman_runtime::permission_audit::{
            PermissionAuditTarget, PermissionPolicyReference, PermissionProvenanceSummary,
            PermissionRequestAudit,
        };
        use atman_runtime::tool::Tier;
        use chrono::Utc;

        fn payload(run_id: &str, tool_use_id: &str) -> PermissionRequestAudit {
            let run_id = FlowRunId(uuid::Uuid::parse_str(run_id).unwrap());
            PermissionRequestAudit {
                request_id: Some(PermissionRequestId::now()),
                revision: 1,
                session_id: "session".into(),
                requesting_run_id: run_id.clone(),
                parent_run_id: None,
                root_run_id: run_id,
                tool_use_id: tool_use_id.into(),
                tool: "fs.read".into(),
                tier: Tier::Two,
                provenance: PermissionProvenanceSummary::default(),
                target: PermissionAuditTarget::User,
                group_ids: Vec::new(),
                policy: PermissionPolicyReference {
                    snapshot_id: "snapshot".into(),
                    rule_id: "rule".into(),
                },
                escalation_path: Vec::new(),
                decision_id: None,
                actor: None,
                scope: None,
                reason: None,
                at: Utc::now(),
            }
        }

        let root = uuid::Uuid::now_v7().to_string();
        let spawned = uuid::Uuid::now_v7().to_string();
        let descendant = uuid::Uuid::now_v7().to_string();
        let mut app = AppState::new("session".into(), None);
        app.apply_stream_frame(StreamFrame::FlowStart {
            run_id: root.clone(),
            flow_name: "root".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent-1".into(),
            goal: "research".into(),
            child_run_id: spawned.clone(),
            model: "model".into(),
        });
        app.apply_stream_frame(StreamFrame::FlowStart {
            run_id: descendant.clone(),
            flow_name: "research_loop".into(),
            parent_run_id: Some(spawned),
            parent_node_id: None,
        });
        let root_payload = payload(&root, "root-tool");
        let child_payload = payload(&descendant, "child-tool");
        app.apply_stream_frame(StreamFrame::PermissionRequestCreated {
            run_id: root,
            payload: root_payload.clone(),
        });
        app.apply_stream_frame(StreamFrame::PermissionRequestCreated {
            run_id: descendant,
            payload: child_payload.clone(),
        });

        let root_graph = app
            .items
            .iter()
            .find_map(|item| match item {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .unwrap();
        let child_graph = app
            .items
            .iter()
            .find_map(|item| match item {
                OutputItem::SubAgentActivity { workflow_graph, .. } => Some(workflow_graph),
                _ => None,
            })
            .unwrap();
        assert_eq!(root_graph.permission_requests.len(), 1);
        assert_eq!(child_graph.permission_requests.len(), 1);
        assert!(root_graph.permission_requests.values().any(|request| {
            request.payload == root_payload && request.payload.policy.rule_id == "rule"
        }));
        assert!(child_graph.permission_requests.values().any(|request| {
            request.payload == child_payload
                && request.payload.provenance == PermissionProvenanceSummary::default()
        }));
        assert!(
            root_graph
                .permission_requests
                .values()
                .all(|request| { request.payload.tool_use_id != "child-tool" })
        );
        assert!(
            child_graph
                .permission_requests
                .values()
                .all(|request| { request.payload.tool_use_id != "root-tool" })
        );
        assert_eq!(app.pending_permissions.len(), 2);
        let root_id = root_payload.request_id.clone().unwrap();
        assert_eq!(app.pending_permissions[&root_id].revision, 1);

        let mut approved = root_payload;
        approved.revision = 2;
        app.apply_stream_frame(StreamFrame::PermissionRequestApproved {
            run_id: approved.requesting_run_id.to_string(),
            payload: approved,
        });
        assert!(!app.pending_permissions.contains_key(&root_id));
        assert_eq!(app.pending_permissions.len(), 1);
    }

    #[test]
    fn toggle_mouse_capture_flips_state() {
        let mut app = AppState::new("s".into(), None);
        assert!(app.mouse_captured, "default is captured");
        let now_on = app.toggle_mouse_capture();
        assert!(!now_on);
        assert!(!app.mouse_captured);
        let now_on2 = app.toggle_mouse_capture();
        assert!(now_on2);
        assert!(app.mouse_captured);
    }

    #[test]
    fn chunks_stream_incrementally_into_single_markdown_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hello ".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "world".into(),
            model: "m".into(),
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "hello world");
                assert!(*streaming);
            }
            _ => panic!("expected streaming assistant md"),
        }
        assert!(app.streaming);
    }

    #[test]
    fn llm_done_flips_streaming_flag_without_duplicating_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hi".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 3,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1, "no extra markdown item after done");
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "hi");
                assert!(!streaming);
            }
            _ => panic!(),
        }
        assert!(!app.streaming);
    }

    #[test]
    fn llm_done_finalizes_thinking_without_text_chunks() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "hmm".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done, "thinking must be finalized by LlmDone");
                assert_eq!(text, "hmm");
            }
            _ => panic!("expected Thinking item"),
        }
    }

    #[test]
    fn llm_done_finalizes_thinking_then_tool_use_no_text() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "thinking...".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "\"x\"".into(),
            id: "tc1".into(),
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::Thinking { done, .. } => assert!(*done, "thinking stuck spinning"),
            _ => panic!("expected Thinking"),
        }
    }

    #[test]
    fn llm_done_finalizes_text_then_thinking_both_unfinalized() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "partial".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "rethink".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 9,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::AssistantMd { streaming, md, .. } => {
                assert!(!*streaming, "AssistantMd must be finalized");
                assert_eq!(md, "partial");
            }
            _ => panic!("expected AssistantMd at items[0]"),
        }
        match &app.items[1] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done, "Thinking must be finalized");
                assert_eq!(text, "rethink");
            }
            _ => panic!("expected Thinking at items[1]"),
        }
    }

    #[test]
    fn llm_done_stops_at_first_finalized_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "prev".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 1,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "new turn".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 2,
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "prev");
                assert!(!*streaming, "prev must stay finalized");
            }
            _ => panic!(),
        }
        match &app.items[1] {
            OutputItem::Thinking { done, text, .. } => {
                assert!(*done);
                assert_eq!(text, "new turn");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_use_stream_frames_no_longer_push_items() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "\"foo\"".into(),
            id: "tc_1".into(),
        });
        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: true,
            preview: "12 bytes".into(),
            id: "tc_1".into(),
        });
        assert!(
            app.items.is_empty(),
            "tool traffic flows through workflow panel now"
        );
    }

    #[test]
    fn llm_retry_marks_items_as_retried() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "let me think".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hello".into(),
            model: "m".into(),
            run_id: None,
        });
        assert_eq!(app.items.len(), 2);
        app.apply_stream_frame(StreamFrame::LlmRetry);
        assert_eq!(app.items.len(), 2, "LlmRetry keeps items but marks them");
        match &app.items[0] {
            OutputItem::Thinking { retried, done, .. } => {
                assert!(*retried, "Thinking should be marked retried");
                assert!(*done, "Thinking should be done");
            }
            _ => panic!("expected Thinking"),
        }
        match &app.items[1] {
            OutputItem::AssistantMd {
                retried, streaming, ..
            } => {
                assert!(*retried, "AssistantMd should be marked retried");
                assert!(!streaming, "AssistantMd should not be streaming");
            }
            _ => panic!("expected AssistantMd"),
        }
        assert!(app.waiting_for_llm);
    }

    #[test]
    fn llm_retry_preserves_non_llm_items() {
        let mut app = AppState::new("s".into(), None);
        app.push_item(OutputItem::Bash {
            handle: "h".into(),
            output: "done".into(),
            done: true,
            expanded: false,
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "done thinking".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 5,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "final answer".into(),
            model: "m".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::LlmDone {
            total_tokens: 10,
            run_id: None,
        });
        assert_eq!(app.items.len(), 3);
        app.apply_stream_frame(StreamFrame::LlmRetry);
        assert_eq!(
            app.items.len(),
            3,
            "LlmRetry keeps all items, only marks LLM items as retried"
        );
        match &app.items[0] {
            OutputItem::Bash { .. } => {}
            _ => panic!("expected Bash item preserved"),
        }
        match &app.items[1] {
            OutputItem::Thinking { retried, .. } => assert!(*retried),
            _ => panic!("expected Thinking"),
        }
        match &app.items[2] {
            OutputItem::AssistantMd { retried, .. } => assert!(*retried),
            _ => panic!("expected AssistantMd"),
        }
    }

    #[test]
    fn user_turn_pushes_only_item() {
        let mut app = AppState::new("s".into(), None);
        app.push_user_turn("hi".into());
        assert_eq!(app.items.len(), 1);
        assert!(matches!(app.items[0], OutputItem::UserTurn { .. }));
    }

    #[test]
    fn toggle_tool_expansion_flips_membership() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.is_tool_expanded("x"));
        app.toggle_tool_expansion("x");
        assert!(app.is_tool_expanded("x"));
        app.toggle_tool_expansion("x");
        assert!(!app.is_tool_expanded("x"));
    }

    #[test]
    fn compaction_summary_frames_mutate_same_item() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::CompactionSummary {
            phase: CompactionPhase::Running,
            range_start: 2,
            range_end: 8,
            summary: String::new(),
            before_tokens: 100,
            after_tokens: 0,
            compacted_count: 7,
        });
        app.apply_stream_frame(StreamFrame::CompactionSummary {
            phase: CompactionPhase::Finished,
            range_start: 2,
            range_end: 8,
            summary: "## Objective\n- keep it short".into(),
            before_tokens: 100,
            after_tokens: 40,
            compacted_count: 7,
        });

        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::CompactionSummary {
                phase,
                range_start,
                range_end,
                summary,
                before_tokens,
                after_tokens,
                compacted_count,
                ..
            } => {
                assert!(matches!(phase, CompactionPhase::Finished));
                assert_eq!(*range_start, 2);
                assert_eq!(*range_end, 8);
                assert!(summary.contains("Objective"));
                assert_eq!(*before_tokens, 100);
                assert_eq!(*after_tokens, 40);
                assert_eq!(*compacted_count, 7);
            }
            _ => panic!("expected compaction summary item"),
        }
    }

    #[test]
    fn hit_test_maps_absolute_row_to_item_index() {
        use crate::output::ItemRange;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(0, 2, 80, 20));
        app.last_item_ranges = vec![
            ItemRange {
                item_index: 0,
                start_row: 0,
                end_row: 2,
            },
            ItemRange {
                item_index: 1,
                start_row: 2,
                end_row: 5,
            },
        ];
        app.scroll_offset = 0;
        assert_eq!(app.hit_test(10, 2), Some(0));
        assert_eq!(app.hit_test(10, 3), Some(0));
        assert_eq!(app.hit_test(10, 4), Some(1));
        assert_eq!(app.hit_test(10, 6), Some(1));
    }

    #[test]
    fn hit_test_node_maps_row_to_workflow_node_id() {
        use crate::output::NodeRegion;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(0, 2, 80, 20));
        app.last_node_regions = vec![
            NodeRegion {
                panel_item_index: 3,
                path_key: "0".into(),
                start_row: 1,
                end_row: 2,
                col_start: 0,
                col_end: 80,
            },
            NodeRegion {
                panel_item_index: 3,
                path_key: "0/0".into(),
                start_row: 2,
                end_row: 3,
                col_start: 0,
                col_end: 80,
            },
        ];
        app.scroll_offset = 0;
        assert_eq!(app.hit_test_node(10, 3), Some((3, "0".to_string())));
        assert_eq!(app.hit_test_node(10, 4), Some((3, "0/0".to_string())));
        assert_eq!(app.hit_test_node(10, 5), None);
    }

    #[test]
    fn hit_test_returns_none_outside_transcript() {
        use crate::output::ItemRange;
        use ratatui::layout::Rect;
        let mut app = AppState::new("s".into(), None);
        app.last_transcript_rect = Some(Rect::new(5, 2, 80, 20));
        app.last_item_ranges = vec![ItemRange {
            item_index: 0,
            start_row: 0,
            end_row: 2,
        }];
        assert_eq!(app.hit_test(10, 1), None, "row above rect");
        assert_eq!(app.hit_test(10, 30), None, "row below rect");
        assert_eq!(app.hit_test(2, 3), None, "col left of rect");
        assert_eq!(app.hit_test(200, 3), None, "col right of rect");
    }

    #[test]
    fn resolve_scroll_follows_tail_by_default() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20, 0, 5);
        assert_eq!(app.scroll_offset, 82);
        assert!(app.follow_tail);
    }

    #[test]
    fn anchor_with_input_overlay() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        assert_eq!(app.scroll_offset, 77);
        assert_eq!(app.max_scroll_offset(), 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_up_goes_toward_older_content() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(20);
        assert_eq!(app.scroll_offset, 57);
        assert!(!app.follow_tail);
    }

    #[test]
    fn scroll_down_reaches_anchor_and_reenables_tail() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(30);
        assert_eq!(app.scroll_offset, 47);
        app.scroll_down(30);
        assert_eq!(app.scroll_offset, 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_down_past_anchor_clamped_to_anchor() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_down(30);
        assert_eq!(app.scroll_offset, 77);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_up_from_older_content_preserves_offset() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(200, 30, 5, 5);
        app.scroll_up(50);
        assert_eq!(app.scroll_offset, 127);
        app.resolve_scroll(300, 30, 5, 5);
        assert_eq!(app.scroll_offset, 127);
    }

    #[test]
    fn pending_below_rows_with_overlay() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 30, 5, 5);
        app.scroll_up(20);
        assert_eq!(app.pending_below_rows(), 20);
        app.scroll_to_tail();
        app.resolve_scroll(100, 30, 5, 5);
        assert_eq!(app.pending_below_rows(), 0);
    }

    #[test]
    fn max_scroll_offset_zero_when_content_shorter_than_viewport() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(10, 30, 5, 5);
        assert_eq!(app.max_scroll_offset(), 0);
    }

    #[test]
    fn record_lag_within_cooldown_merges_into_last_note() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.record_lag(10, t0 + Duration::from_millis(100));
        app.record_lag(20, t0 + Duration::from_millis(200));
        let notes: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, level } => Some((text.clone(), *level)),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, "dropped 35 stream frames");
        assert_eq!(notes[0].1, NoteLevel::Warn);
    }

    #[test]
    fn record_lag_after_cooldown_starts_new_note() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.record_lag(7, t0 + Duration::from_millis(400));
        let lag_notes: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, .. } if text.starts_with("dropped ") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lag_notes,
            vec!["dropped 5 stream frames", "dropped 7 stream frames"]
        );
    }

    #[test]
    fn record_lag_state_resets_when_new_stream_frame_arrives() {
        let mut app = AppState::new("s".into(), None);
        let t0 = Instant::now();
        app.record_lag(5, t0);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hi".into(),
            model: "m".into(),
            run_id: None,
        });
        app.record_lag(3, t0 + Duration::from_millis(50));
        let lag_texts: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::SystemNote { text, .. } if text.starts_with("dropped ") => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            lag_texts,
            vec!["dropped 5 stream frames", "dropped 3 stream frames"]
        );
    }

    #[test]
    fn flow_start_populates_workflow_panel_with_root() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("look_into", "r1"));
        let graph = atman_runtime::nodegraph::FlowGraph {
            flow_name: "look_into".into(),
            root: Vec::new(),
        };
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph,
        });
        let panel = app
            .items
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .expect("workflow panel present");
        assert_eq!(panel.root.len(), 1);
        assert_eq!(panel.root[0].label, "look_into");
    }

    #[test]
    fn toggle_workflow_node_flips_expanded_membership() {
        let mut app = AppState::new("s".into(), None);
        app.push_item(OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: atman_runtime::workflow::WorkflowGraph::new(atman_runtime::event::TurnId::now()),
            expanded_nodes: HashSet::new(),
            panel_expanded: true,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        });
        let idx = app.items.len() - 1;
        app.toggle_workflow_node(idx, "node_x");
        if let OutputItem::WorkflowPanel { expanded_nodes, .. } = &app.items[idx] {
            assert!(expanded_nodes.contains("node_x"));
        }
        app.toggle_workflow_node(idx, "node_x");
        if let OutputItem::WorkflowPanel { expanded_nodes, .. } = &app.items[idx] {
            assert!(!expanded_nodes.contains("node_x"));
        }
    }

    #[test]
    fn workflow_stream_mutations_bump_items_version() {
        let mut app = AppState::new("s".into(), None);
        let baseline = app.items_version;
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        let after_flow = app.items_version;
        assert_ne!(after_flow, baseline, "FlowGraph should bump version");
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "missing".into(),
            tool_use_id: "tu".into(),
            tool: "t".into(),
            args_preview: "{}".into(),
        });
        assert_ne!(
            app.items_version, after_flow,
            "ToolNode routed to graph should still bump version"
        );
    }

    #[test]
    fn ctrl_o_targets_latest_workflow_tool_node() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "stmt_0".into(),
            kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
            label: "stmt_0".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_last".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
        });
        assert!(app.toggle_last_tool_expansion());
        let expanded = app.items.iter().find_map(|it| match it {
            OutputItem::WorkflowPanel { expanded_nodes, .. } => Some(expanded_nodes.clone()),
            _ => None,
        });
        assert!(expanded.unwrap().contains("0/0/0"));
    }

    #[test]
    fn nested_node_start_attaches_under_parent() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("f", "r1"));
        app.apply_stream_frame(StreamFrame::FlowGraph {
            run_id: "r1".into(),
            graph: atman_runtime::nodegraph::FlowGraph {
                flow_name: "f".into(),
                root: Vec::new(),
            },
        });
        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "stmt_0".into(),
            kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
            label: "stmt_0".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "stmt_0".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: "{}".into(),
        });
        let panel = app
            .items
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { graph, .. } => Some(graph),
                _ => None,
            })
            .unwrap();
        let stmt = panel.find_node("r1::stmt_0").unwrap();
        assert_eq!(stmt.children.len(), 1);
        assert_eq!(stmt.children[0].id, "tool:r1:tu_1");
    }

    // ── Workflow panel state machine tests ──

    fn flow_start(name: &str, run_id: &str) -> StreamFrame {
        StreamFrame::FlowStart {
            run_id: run_id.into(),
            flow_name: name.into(),
            parent_run_id: None,
            parent_node_id: None,
        }
    }

    fn flow_done(run_id: &str, cancelled: bool) -> StreamFrame {
        StreamFrame::FlowDone {
            run_id: run_id.into(),
            flow_name: "test".into(),
            ok: !cancelled,
            cancelled,
        }
    }

    fn subflow_start(name: &str, run_id: &str, parent_run_id: &str) -> StreamFrame {
        StreamFrame::FlowStart {
            run_id: run_id.into(),
            flow_name: name.into(),
            parent_run_id: Some(parent_run_id.into()),
            parent_node_id: None,
        }
    }

    fn workflow_panels(app: &AppState) -> Vec<(usize, bool)> {
        app.items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| match it {
                OutputItem::WorkflowPanel { ended_at, .. } => Some((i, ended_at.is_none())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn normal_flow_lifecycle_creates_and_closes_panel() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.has_running_workflow());

        app.apply_stream_frame(flow_start("agent", "r1"));
        assert!(app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(panels[0].1, "panel should be open");

        app.apply_stream_frame(flow_done("r1", false));
        assert!(!app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(!panels[0].1, "panel should be closed");
    }

    #[test]
    fn subflow_reuses_parent_panel() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        assert_eq!(workflow_panels(&app).len(), 1);

        // Subflow with parent_run_id should reuse parent's panel.
        app.apply_stream_frame(subflow_start("agent_loop", "r2", "r1"));
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            1,
            "subflow must reuse parent panel, not create new one"
        );
    }

    #[test]
    fn subflow_orphan_parent_not_in_map_creates_new_panel() {
        let mut app = AppState::new("s".into(), None);
        // No parent flow Start → parent not in HashMap.
        app.apply_stream_frame(subflow_start("orphan", "r2", "nonexistent"));
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "orphan subflow creates new panel");
    }

    #[test]
    fn course_correct_reopens_cancelled_panel() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        // Flow is cancelled.
        app.apply_stream_frame(flow_done("r1", true));
        assert!(!app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1);
        assert!(!panels[0].1, "panel closed after cancelled FlowDone");

        // New flow starts (course-correct restart).
        app.apply_stream_frame(flow_start("agent", "r2"));
        assert!(app.has_running_workflow());
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            1,
            "must reopen cancelled panel, not create new"
        );
        assert!(panels[0].1, "panel must be reopened");
    }

    #[test]
    fn course_correct_after_push_user_turn_peeks_past_userturn() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // push_user_turn means a new user turn started — this only happens
        // when has_running_workflow() is false. Course-correct during a
        // running flow does NOT call push_user_turn, so this scenario
        // represents a normal new message after a cancelled flow.
        app.push_user_turn("new msg".into());
        app.close_current_workflow_panel(true, None);

        // UserTurn between the old panel and FlowStart means new turn →
        // new panel, not reopen.
        app.apply_stream_frame(flow_start("agent", "r2"));
        let panels = workflow_panels(&app);
        assert_eq!(
            panels.len(),
            2,
            "UserTurn means new turn — new panel expected"
        );
        assert!(panels[1].1, "new panel must be open");
        assert!(app.workflow_run_to_panel.contains_key("r2"));
    }

    #[test]
    fn scoped_tool_frames_route_to_their_run_panel() {
        use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
        use atman_runtime::workflow::NodeStatus;

        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("first", "r1"));
        app.push_user_turn("next".into());
        app.apply_stream_frame(flow_start("second", "r2"));

        app.apply_stream_frame(StreamFrame::FlowNodeStart {
            run_id: "r1".into(),
            node_id: "dispatch_all".into(),
            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch_all".into(),
            parent_node_id: None,
        });
        app.apply_stream_frame(StreamFrame::ToolNode {
            run_id: "r1".into(),
            parent_node_id: "dispatch_all".into(),
            tool_use_id: "tu_1".into(),
            tool: "fs.read".into(),
            args_preview: String::new(),
        });
        app.apply_stream_frame(StreamFrame::ToolResultMsg {
            flow_run_id: Some("r1".into()),
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: "done".into(),
                    is_error: false,
                }],
                turn_id: atman_runtime::event::TurnId::now(),
                origin: MessageOrigin::User,
            },
        });

        let first_idx = app.workflow_run_to_panel["r1"];
        let second_idx = app.workflow_run_to_panel["r2"];
        let OutputItem::WorkflowPanel {
            graph: first_graph, ..
        } = &app.items[first_idx]
        else {
            panic!("first workflow panel");
        };
        let tool = first_graph.find_node("tool:r1:tu_1").expect("r1 tool");
        assert_eq!(tool.status, NodeStatus::Ok);
        let OutputItem::WorkflowPanel {
            graph: second_graph,
            ..
        } = &app.items[second_idx]
        else {
            panic!("second workflow panel");
        };
        assert!(second_graph.find_node("tool:r1:tu_1").is_none());
        assert!(second_graph.find_node("r1::dispatch_all").is_none());
    }

    #[test]
    fn has_running_workflow_false_when_all_closed() {
        let mut app = AppState::new("s".into(), None);
        assert!(!app.has_running_workflow());

        app.apply_stream_frame(flow_start("agent", "r1"));
        assert!(app.has_running_workflow());

        app.apply_stream_frame(flow_done("r1", false));
        assert!(!app.has_running_workflow());
    }

    #[test]
    fn has_running_workflow_true_with_two_open_panels() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("a", "r1"));
        app.apply_stream_frame(flow_start("b", "r2"));
        assert!(app.has_running_workflow());
        assert_eq!(workflow_panels(&app).len(), 2);

        // Close one panel — still running.
        app.apply_stream_frame(flow_done("r1", false));
        assert!(app.has_running_workflow());

        // Close the other — done.
        app.apply_stream_frame(flow_done("r2", false));
        assert!(!app.has_running_workflow());
    }

    #[test]
    fn close_current_workflow_panel_upgrades_cancelled_flag() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // push_user_turn closes with cancelled:false
        app.push_user_turn("msg".into());

        let panels = workflow_panels(&app);
        assert!(!panels[0].1, "closed by push_user_turn");

        // Interjection handler closes with cancelled:true — must upgrade.
        app.close_current_workflow_panel(true, None);

        let panel = match &app.items[panels[0].0] {
            OutputItem::WorkflowPanel { cancelled, .. } => *cancelled,
            _ => panic!("expected WorkflowPanel"),
        };
        assert!(panel, "cancelled flag must be upgraded from false to true");
    }

    #[test]
    fn flow_done_for_non_flowstart_event_does_not_create_panel() {
        let mut app = AppState::new("s".into(), None);
        // FlowNodeEnd is not FlowStart or FlowDone — should not create panel.
        app.apply_stream_frame(StreamFrame::FlowNodeEnd {
            run_id: "r1".into(),
            node_id: "n1".into(),
            status: atman_runtime::event::FlowNodeStatus::Ok,
            output_preview: None,
            parent_node_id: None,
        });
        assert_eq!(workflow_panels(&app).len(), 0);
    }

    #[test]
    fn two_consecutive_flows_without_userturn_reuses_panel() {
        // Scenario: course-correct restarts flow immediately (no UserTurn).
        let mut app = AppState::new("s".into(), None);

        // First flow.
        app.apply_stream_frame(flow_start("agent", "r1"));
        app.apply_stream_frame(flow_done("r1", true)); // cancelled

        // Second flow starts immediately.
        app.apply_stream_frame(flow_start("agent", "r2"));
        app.apply_stream_frame(flow_done("r2", false));

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "should reuse panel, not create second");
    }

    #[test]
    fn multiple_flow_starts_without_done_only_one_open_panel() {
        // Simulates L1 nudges: flow restarts LLM internally, new FlowStart
        // for each restart within same run.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));

        // L1 nudge 1: internal LLM restart — a new subflow might start
        // but the top-level flow is unchanged.
        app.apply_stream_frame(subflow_start("agent_loop", "s1", "r1"));
        app.apply_stream_frame(StreamFrame::FlowDone {
            run_id: "s1".into(),
            flow_name: "agent_loop".into(),
            ok: true,
            cancelled: false,
        });

        // L1 nudge 2: another internal restart.
        app.apply_stream_frame(subflow_start("agent_loop", "s2", "r1"));
        app.apply_stream_frame(StreamFrame::FlowDone {
            run_id: "s2".into(),
            flow_name: "agent_loop".into(),
            ok: true,
            cancelled: false,
        });

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 1, "L1 nudges must not create extra panels");
        assert!(panels[0].1, "top-level panel still open");
    }

    #[test]
    fn flow_done_routes_to_correct_panel_by_run_id() {
        // Two concurrent flows: FlowDone for r1 closes panel[0], r2 stays open.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("a", "r1"));
        app.apply_stream_frame(flow_start("b", "r2"));

        app.apply_stream_frame(flow_done("r1", false));

        let panels = workflow_panels(&app);
        assert_eq!(panels.len(), 2);
        assert!(!panels[0].1, "panel 0 (r1) should be closed");
        assert!(panels[1].1, "panel 1 (r2) should still be open");
        assert!(app.has_running_workflow());
    }

    #[test]
    fn cancel_escalates_through_all_levels() {
        // L4 hard stop: FlowDone(cancelled:true) → panel marked cancelled.
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(flow_start("agent", "r1"));
        app.apply_stream_frame(flow_done("r1", true));

        let panel = match &app.items[0] {
            OutputItem::WorkflowPanel { cancelled, .. } => *cancelled,
            _ => panic!("expected WorkflowPanel"),
        };
        assert!(panel, "L4 hard stop must mark panel as cancelled");
    }
}

#[cfg(test)]
mod terminal_stream_tests {
    use super::*;
    use atman_runtime::tools::term::{TermStateSnapshot, TerminalScreen};

    fn dummy_screen() -> TerminalScreen {
        TerminalScreen {
            rows: 2,
            cols: 3,
            cells: vec![atman_runtime::tools::term::TerminalCell::default(); 6],
            cursor: None,
            alt_screen: false,
        }
    }

    #[test]
    fn terminal_chunk_creates_output_item() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::Terminal {
                handle, mode, done, ..
            } => {
                assert_eq!(handle, "term_s_0");
                assert_eq!(*mode, TerminalViewMode::Capture);
                assert!(!*done);
            }
            _ => panic!("expected Terminal item"),
        }
    }

    #[test]
    fn terminal_chunk_updates_existing_item() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b" world".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            run_id: None,
        });
        assert_eq!(app.items.len(), 1, "should update existing, not create new");
        match &app.items[0] {
            OutputItem::Terminal {
                accumulated_bytes, ..
            } => {
                assert_eq!(accumulated_bytes, b"hi world");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn terminal_exited_marks_done() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen),
            state: TermStateSnapshot::Running,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
            run_id: None,
        });
        match &app.items[0] {
            OutputItem::Terminal { done, .. } => assert!(*done),
            _ => panic!(),
        }
    }

    #[test]
    fn sub_agent_tool_frames_do_not_mutate_main_document_items() {
        let mut app = AppState::new("s".into(), None);
        let screen = dummy_screen();
        app.apply_stream_frame(StreamFrame::SubAgentStarted {
            handle: "agent_1".into(),
            goal: "check".into(),
            child_run_id: "child_run".into(),
            model: "m".into(),
        });
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"main".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "main\n".into(),
            run_id: None,
        });

        let baseline = app.items.len();
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"sub".to_vec(),
            screen: Some(screen),
            state: TermStateSnapshot::Running,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "sub\n".into(),
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            run_id: Some("child_run".into()),
        });
        app.apply_stream_frame(StreamFrame::DiffPreview {
            title: "sub diff".into(),
            old_content: None,
            new_content: None,
            unified_diff: Some("diff".into()),
            run_id: Some("child_run".into()),
        });

        assert_eq!(app.items.len(), baseline);
        match &app.items[1] {
            OutputItem::Terminal {
                accumulated_bytes,
                done,
                ..
            } => {
                assert_eq!(accumulated_bytes, b"main");
                assert!(!*done);
            }
            _ => panic!("expected Terminal item"),
        }
        match &app.items[2] {
            OutputItem::Bash { output, done, .. } => {
                assert_eq!(output, "main\n");
                assert!(!*done);
            }
            _ => panic!("expected Bash item"),
        }
    }

    #[test]
    fn open_task_panel_different_handles_create_different_panels() {
        use atman_runtime::tools::term::{TerminalCell, TerminalScreen};
        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.items.push(OutputItem::Terminal {
            handle: "term_s_0".into(),
            screen: TerminalScreen {
                rows: 2,
                cols: 5,
                cells: vec![TerminalCell::default(); 10],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"hi".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        app.items.push(OutputItem::Terminal {
            handle: "term_s_1".into(),
            screen: TerminalScreen {
                rows: 3,
                cols: 7,
                cells: vec![TerminalCell::default(); 21],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"bye".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.open_task_panel("term_s_0", canvas);
        app.open_task_panel("term_s_1", canvas);
        assert_eq!(app.wm.panels.len(), 2);
        assert_eq!(app.wm.panels[0].label, "term_s_0");
        assert_eq!(app.wm.panels[1].label, "term_s_1");
    }

    #[test]
    fn open_task_panel_background_preserves_focus() {
        use atman_runtime::tools::term::{TerminalCell, TerminalScreen};
        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.items.push(OutputItem::Terminal {
            handle: "term_s_0".into(),
            screen: TerminalScreen {
                rows: 2,
                cols: 5,
                cells: vec![TerminalCell::default(); 10],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"hi".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        app.items.push(OutputItem::Terminal {
            handle: "term_s_1".into(),
            screen: TerminalScreen {
                rows: 3,
                cols: 7,
                cells: vec![TerminalCell::default(); 21],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: b"bye".to_vec(),
            mode: TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        });
        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);

        // Open the first panel via a foreground click (steals focus).
        app.open_task_panel("term_s_0", canvas);
        let focused_a = app.wm.focused_id();
        assert!(focused_a.is_some());

        // A background completion for term_s_1 must NOT change focus.
        app.open_task_panel_background("term_s_1");
        assert_eq!(app.wm.panels.len(), 2);
        assert_eq!(app.wm.focused_id(), focused_a, "must keep foreground focus");
        assert!(
            app.toasts
                .iter()
                .any(|t| t.message.contains("background task")),
            "should push a toast when focus preserved"
        );
    }
}

#[cfg(test)]
mod terminal_e2e_tests {
    use super::*;
    use crate::output::{LayoutCache, LayoutKey, RenderCtx};
    use atman_runtime::tools::term::{TermStateSnapshot, TerminalCell, TerminalScreen};

    #[test]
    fn full_pipeline_terminal_chunk_to_rendered_lines() {
        let mut app = AppState::new("s".into(), None);
        let screen = TerminalScreen {
            rows: 2,
            cols: 5,
            cells: {
                let mut v = vec![TerminalCell::default(); 10];
                v[0].chars = "h".into();
                v[1].chars = "i".into();
                v
            },
            cursor: Some((0, 2)),
            alt_screen: false,
        };
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b"hi".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
            run_id: None,
        });

        let cache_key = LayoutKey {
            items_version: app.items_version,
            expanded_version: app.expanded_version,
            width: 80,
            animation_frame: None,
        };
        let empty_set = std::collections::HashSet::new();
        let ctx = RenderCtx {
            expanded_tools: &empty_set,
            messages: &[],
            panel_width: 80,
            hovered_thinking_idx: None,
            animation_frame: 0,
        };
        let mut cache = LayoutCache::default();
        let (lines, _ranges, _regions, _total) =
            cache.get_or_build(cache_key, &app.items, &ctx, 0, 50);
        assert!(
            lines.len() > 2,
            "should render header + blank + screen rows"
        );
        let header = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(header.contains("term_s_0"), "header should contain handle");
        assert!(header.contains("capture"), "should be capture mode");
    }

    #[test]
    fn bash_panel_from_history_renders_content() {
        use ratatui::backend::TestBackend;
        use std::collections::HashSet;

        let mut app = crate::UiState::new(AppState::new("s".into(), None));
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 24));

        // Simulate a completed bash task: item in items, snapshot in task_snapshots.
        app.apply_stream_frame(StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: "hello from bash\n".into(),
            run_id: None,
        });
        app.apply_stream_frame(StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            run_id: None,
        });
        app.apply_task_event(atman_runtime::TaskEvent::Registered(
            TaskSnapshotHolder::snap("bg_s_0"),
        ));

        let canvas = ratatui::layout::Rect::new(0, 0, 80, 24);
        app.open_task_panel("bg_s_0", canvas);

        // Find the opened panel.
        let panel = app
            .wm
            .panels
            .iter_mut()
            .find(|p| p.label == "bg_s_0")
            .unwrap();
        assert!(
            panel.content.is_some(),
            "open_task_panel must set panel.content for bash"
        );

        let backend = TestBackend::new(60, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();

        // Split the UiState into its field borrows so the panel (in wm) can be
        // mutated independently of the read-only AppState fields.
        let wm = &mut app.wm;
        let snapshots = &app.app.task_snapshots;
        let items = &app.app.items;
        let activity_nodes = &app.app.activity_nodes;
        let items_version = app.app.items_version;
        let expanded_version = app.app.expanded_version;

        terminal
            .draw(|f| {
                let mut hitmap = crate::wm::WmHitmap::default();
                let empty_mcp: std::collections::HashSet<String> = HashSet::new();
                let empty_resources = std::collections::HashMap::new();
                let empty_prompts = std::collections::HashMap::new();
                let browser = crate::mcp_manager::McpBrowserState {
                    tab: crate::mcp_manager::McpBrowserTab::Resources,
                    resources: &empty_resources,
                    prompts: &empty_prompts,
                };
                crate::wm::content::render_panel_content(
                    f,
                    f.area(),
                    &mut wm.panels[0],
                    snapshots,
                    items,
                    activity_nodes,
                    &None,
                    &None,
                    &mut hitmap,
                    0,
                    true,
                    false,
                    &[],
                    &empty_mcp,
                    0,
                    &None,
                    &browser,
                    items_version,
                    expanded_version,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let joined: String = buf
            .content
            .chunks(60)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            joined.contains("hello from bash"),
            "bash output must appear in panel, got: {joined}"
        );
    }

    struct TaskSnapshotHolder;
    impl TaskSnapshotHolder {
        fn snap(src: &str) -> atman_runtime::TaskSnapshot {
            atman_runtime::TaskSnapshot {
                id: atman_runtime::TaskId::now(),
                kind: atman_runtime::TaskKind::Bash,
                label: format!("bash {src}"),
                status: atman_runtime::TaskStatus::Ok,
                started_at: std::time::Instant::now(),
                ended_at: Some(std::time::Instant::now()),
                source_handle: src.to_string(),
                session_id: "s".to_string(),
                workspace_id: None,
            }
        }
    }
}
