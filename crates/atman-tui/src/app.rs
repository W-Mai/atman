use std::collections::HashSet;
use std::time::{Duration, Instant};

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
    },
    AssistantMd {
        md: String,
        streaming: bool,
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
}

impl OutputItem {
    pub fn handle(&self) -> Option<&str> {
        match self {
            OutputItem::Terminal { handle, .. } | OutputItem::Bash { handle, .. } => Some(handle),
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
}

#[derive(Default)]
pub struct AppState {
    pub items: Vec<OutputItem>,
    pub input: String,
    pub scroll_offset: u16,
    pub follow_tail: bool,
    pub should_quit: bool,
    pub streaming: bool,
    pub waiting_for_llm: bool,
    pub goal: Option<String>,
    pub session_id: String,
    pub session_dir: String,
    pub latest_release: Option<String>,
    pub attach_count: usize,
    pub context: atman_runtime::ContextSnapshot,
    pub todos: Vec<atman_runtime::memory::todo::Todo>,
    pub plans: Vec<atman_runtime::memory::plan::Plan>,
    pub pending_approvals: Vec<atman_runtime::session::PendingApproval>,
    pub pending_injections: Vec<atman_runtime::injection::Injection>,
    pub yank_mode: bool,
    pub yank_index: usize,
    pub palette: crate::palette::CommandPalette,
    pub session_switcher: crate::session_switcher::SessionSwitcher,
    pub compact_review: Option<crate::compact_review_modal::CompactReviewModal>,
    pub history_search: crate::history_search_modal::HistorySearchModal,
    pub workflow_viewer: crate::workflow_viewer_modal::WorkflowViewerModal,
    pub terminal_viewer: crate::terminal_viewer_modal::TerminalViewerModal,
    pub sidebar_mode: crate::sidebar::SidebarMode,
    pub popup: crate::completion::PopupState,
    pub cheatsheet_open: bool,
    pub flow_names: Vec<(String, String)>,
    pub expanded_tools: HashSet<String>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub trust: atman_runtime::trust::TrustConfig,
    pub trust_mode_picker_open: bool,
    pub theme_picker_open: bool,
    pub picker_selected: usize,
    pub last_item_ranges: Vec<crate::output::ItemRange>,
    pub last_node_regions: Vec<crate::output::NodeRegion>,
    pub last_transcript_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_rect: Option<ratatui::layout::Rect>,
    pub input_rect: Option<ratatui::layout::Rect>,
    pub hovered_thinking_idx: Option<usize>,
    pub startup_intro: Option<StartupIntro>,
    pub form_modal: crate::form_modal::FormModal,
    pub animation_frame: u32,
    pub deny_arm: Option<std::time::Instant>,
    pub items_version: u64,
    pub expanded_version: u64,
    pub terminal_throttle: Option<Instant>,
    pub layout_cache: crate::output::LayoutCache,
    pub last_total_rows: u16,
    pub last_viewport_rows: u16,
    pub mouse_captured: bool,
    pub handle_index: std::collections::HashMap<String, usize>,
    pub last_workflow_panel_idx: Option<usize>,
    pub workflow_run_to_panel: std::collections::HashMap<String, usize>,
    pub top_level_run_ids: std::collections::HashSet<String>,
    pub goal_scroll: u16,
    pub plans_scroll: u16,
    pub todos_scroll: u16,
    pub goal_collapsed: bool,
    pub plan_collapsed: bool,
    pub todo_collapsed: bool,
    pub context_collapsed: bool,
    pub meta_collapsed: bool,
    pub sidebar_collapsed: bool,
    pub last_goal_rect: Option<ratatui::layout::Rect>,
    pub last_plan_rect: Option<ratatui::layout::Rect>,
    pub last_todo_rect: Option<ratatui::layout::Rect>,
    pub last_goal_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_plan_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_todo_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_ctx_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_meta_hdr_rect: Option<ratatui::layout::Rect>,
    pub last_collapse_btn_rect: Option<ratatui::layout::Rect>,
    pub last_expand_btn_rect: Option<ratatui::layout::Rect>,
    pub select_mode_hinted: bool,
    last_lag_note_idx: Option<usize>,
    last_lag_at: Option<Instant>,
    last_lag_count: u64,
}

impl AppState {
    pub fn new(session_id: String, goal: Option<String>) -> Self {
        Self {
            session_id,
            goal,
            follow_tail: true,
            mouse_captured: true,
            ..Default::default()
        }
    }

    pub fn toggle_mouse_capture(&mut self) -> bool {
        self.mouse_captured = !self.mouse_captured;
        self.mark_items_dirty();
        self.mouse_captured
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

    pub fn open_workflow_viewer(&mut self, panel_item_index: usize) {
        self.workflow_viewer.open(panel_item_index);
    }

    pub fn close_workflow_viewer(&mut self) {
        self.workflow_viewer.close();
    }

    pub fn open_terminal_viewer(&mut self, panel_item_index: usize) {
        self.terminal_viewer.open(panel_item_index);
    }

    pub fn close_terminal_viewer(&mut self) {
        self.terminal_viewer.close();
    }

    pub fn workflow_viewer_hit_test(&self, col: u16, row: u16) -> Option<(usize, String)> {
        let inner = self.workflow_viewer.last_inner_rect?;
        if col < inner.x
            || col >= inner.x.saturating_add(inner.width)
            || row < inner.y
            || row >= inner.y.saturating_add(inner.height)
        {
            return None;
        }
        let rel_col = col
            .saturating_sub(inner.x)
            .saturating_add(self.workflow_viewer.h_offset);
        let rel_row = row
            .saturating_sub(inner.y)
            .saturating_add(self.workflow_viewer.v_offset);
        self.workflow_viewer
            .last_node_regions
            .iter()
            .filter(|r| rel_row >= r.start_row && rel_row < r.end_row)
            .filter(|r| rel_col >= r.col_start && rel_col < r.col_end)
            .max_by_key(|r| r.path_key.len())
            .map(|r| (self.workflow_viewer.panel_item_index, r.path_key.clone()))
    }

    pub fn toggle_workflow_node(&mut self, panel_index: usize, node_id: &str) {
        if let Some(OutputItem::WorkflowPanel { expanded_nodes, .. }) =
            self.items.get_mut(panel_index)
        {
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
        if let Some(OutputItem::WorkflowPanel { panel_expanded, .. }) =
            self.items.get_mut(panel_index)
        {
            *panel_expanded = !*panel_expanded;
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
        let rel = row
            .saturating_sub(rect.y)
            .saturating_add(self.scroll_offset);
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
        let rel = row
            .saturating_sub(rect.y)
            .saturating_add(self.scroll_offset);
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

    pub fn max_scroll_offset(&self) -> u16 {
        self.last_total_rows.saturating_sub(self.last_viewport_rows)
    }

    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.follow_tail = false;
    }

    pub fn scroll_down(&mut self, rows: u16) {
        let max = self.max_scroll_offset();
        let next = self.scroll_offset.saturating_add(rows);
        if next >= max {
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

    pub fn resolve_scroll(&mut self, total_rows: u16, viewport_rows: u16) {
        self.last_total_rows = total_rows;
        self.last_viewport_rows = viewport_rows;
        let max = total_rows.saturating_sub(viewport_rows);
        if self.follow_tail {
            self.scroll_offset = max;
        } else {
            self.scroll_offset = self.scroll_offset.min(max);
        }
    }

    pub fn pending_below_rows(&self) -> u16 {
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

    pub fn apply_stream_frame(&mut self, frame: StreamFrame) {
        match frame {
            StreamFrame::ThinkingChunk { text } => {
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
                    });
                    self.streaming = true;
                }
            }
            StreamFrame::LlmChunk { text, .. } => {
                self.waiting_for_llm = false;
                if let Some(OutputItem::Thinking { done, .. }) = self.items.last_mut()
                    && !*done
                {
                    *done = true;
                    self.items_version = self.items_version.wrapping_add(1);
                }
                if let Some(OutputItem::AssistantMd { md, streaming }) = self.items.last_mut()
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
                    });
                    self.streaming = true;
                    self.terminal_throttle = Some(Instant::now());
                    self.items_version = self.items_version.wrapping_add(1);
                }
            }
            StreamFrame::LlmDone { .. } => {
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
            | StreamFrame::ToolDenied { .. }) => {
                let (is_done, cancelled, done_run_id) = match &frame {
                    StreamFrame::FlowDone {
                        cancelled, run_id, ..
                    } => (true, *cancelled, Some(run_id.as_str())),
                    _ => (false, false, None),
                };
                self.ensure_workflow_panel_and_apply(&frame);
                if is_done {
                    // Only close the panel for top-level flows — subflow
                    // FlowDone must not close the parent panel.
                    if done_run_id.is_some_and(|rid| self.top_level_run_ids.contains(rid)) {
                        let panel_idx = done_run_id
                            .and_then(|rid| self.workflow_run_to_panel.get(rid).copied());
                        self.close_current_workflow_panel(cancelled, panel_idx);
                        // Clean up the top-level run id.
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
            } => {
                self.waiting_for_llm = false;
                self.follow_tail = true;
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
            StreamFrame::TerminalExited { handle, .. } => {
                if let Some(idx) = self.find_item_by_handle(&handle) {
                    if let Some(OutputItem::Terminal { done, .. }) = self.items.get_mut(idx) {
                        *done = true;
                        self.items_version = self.items_version.wrapping_add(1);
                    }
                }
            }
            StreamFrame::BashChunk { handle, kind, line } => {
                self.waiting_for_llm = false;
                self.follow_tail = true;
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
            StreamFrame::BashExited { handle, .. } => {
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
            } => {
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
            StreamFrame::Unknown => {}
        }
    }

    fn route_to_workflow_panel(&mut self, frame: &StreamFrame) {
        let mut mutated = false;
        let target = if let StreamFrame::FlowDone { run_id, .. } = frame {
            self.workflow_run_to_panel
                .get(run_id)
                .and_then(|&idx| self.items.get_mut(idx))
        } else {
            self.items
                .iter_mut()
                .rev()
                .find(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
        };
        if let Some(OutputItem::WorkflowPanel { graph, .. }) = target {
            graph.apply_stream_frame(frame);
            mutated = true;
        }
        if mutated {
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    fn ensure_workflow_panel_and_apply(&mut self, frame: &StreamFrame) {
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
                // Insert subflow run_id so nested subflows (e.g. agent.spawn)
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
        });
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "world".into(),
            model: "m".into(),
        });
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming } => {
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
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 3 });
        assert_eq!(app.items.len(), 1, "no extra markdown item after done");
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming } => {
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
        app.apply_stream_frame(StreamFrame::ThinkingChunk { text: "hmm".into() });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 5 });
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
        });
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "\"x\"".into(),
            id: "tc1".into(),
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 5 });
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
        });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "rethink".into(),
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 9 });
        match &app.items[0] {
            OutputItem::AssistantMd { streaming, md } => {
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
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 1 });
        app.apply_stream_frame(StreamFrame::ThinkingChunk {
            text: "new turn".into(),
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 2 });
        match &app.items[0] {
            OutputItem::AssistantMd { md, streaming } => {
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
        app.resolve_scroll(100, 20);
        assert_eq!(app.scroll_offset, 80);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_up_disables_follow_tail() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20);
        app.scroll_up(5);
        assert_eq!(app.scroll_offset, 75);
        assert!(!app.follow_tail);
    }

    #[test]
    fn scroll_down_reaching_bottom_reenables_follow_tail() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20);
        app.scroll_up(30);
        assert_eq!(app.scroll_offset, 50);
        app.scroll_down(30);
        assert!(app.follow_tail);
    }

    #[test]
    fn scroll_down_partial_leaves_follow_tail_off() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(200, 20);
        app.scroll_up(100);
        app.scroll_down(20);
        assert_eq!(app.scroll_offset, 100);
        assert!(!app.follow_tail);
    }

    #[test]
    fn resolve_scroll_preserves_offset_when_not_following() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20);
        app.scroll_up(30);
        app.resolve_scroll(200, 20);
        assert_eq!(app.scroll_offset, 50);
    }

    #[test]
    fn pending_below_rows_reports_diff_when_not_following() {
        let mut app = AppState::new("s".into(), None);
        app.resolve_scroll(100, 20);
        app.scroll_up(15);
        assert_eq!(app.pending_below_rows(), 15);
        app.scroll_to_tail();
        app.resolve_scroll(100, 20);
        assert_eq!(app.pending_below_rows(), 0);
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
        });
        app.apply_stream_frame(StreamFrame::TerminalChunk {
            handle: "term_s_0".into(),
            bytes: b" world".to_vec(),
            screen: Some(screen.clone()),
            state: TermStateSnapshot::Running,
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
        });
        app.apply_stream_frame(StreamFrame::TerminalExited {
            handle: "term_s_0".into(),
            exit_code: Some(0),
        });
        match &app.items[0] {
            OutputItem::Terminal { done, .. } => assert!(*done),
            _ => panic!(),
        }
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
}
