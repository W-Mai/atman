use std::collections::HashSet;
use std::time::{Duration, Instant};

use atman_runtime::stream::StreamFrame;
use atman_runtime::workflow::WorkflowGraph;

const LAG_COOLDOWN: Duration = Duration::from_millis(300);

#[derive(Debug, Clone)]
pub enum OutputItem {
    UserTurn {
        text: String,
    },
    Thinking {
        text: String,
        done: bool,
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
    },
    StartupCard {
        version: String,
        recent: Vec<StartupSessionEntry>,
    },
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
    pub attach_count: usize,
    pub context: atman_runtime::ContextSnapshot,
    pub todos: Vec<atman_runtime::memory::todo::Todo>,
    pub plans: Vec<atman_runtime::memory::plan::Plan>,
    pub pending_approvals: Vec<atman_runtime::session::PendingApproval>,
    pub yank_mode: bool,
    pub yank_index: usize,
    pub palette: crate::palette::CommandPalette,
    pub session_switcher: crate::session_switcher::SessionSwitcher,
    pub compact_review: Option<crate::compact_review_modal::CompactReviewModal>,
    pub history_search: crate::history_search_modal::HistorySearchModal,
    pub workflow_viewer: crate::workflow_viewer_modal::WorkflowViewerModal,
    pub sidebar_mode: crate::sidebar::SidebarMode,
    pub popup: crate::completion::PopupState,
    pub cheatsheet_open: bool,
    pub flow_names: Vec<(String, String)>,
    pub expanded_tools: HashSet<String>,
    pub session: Option<std::sync::Arc<atman_runtime::Session>>,
    pub last_item_ranges: Vec<crate::output::ItemRange>,
    pub last_node_regions: Vec<crate::output::NodeRegion>,
    pub last_transcript_rect: Option<ratatui::layout::Rect>,
    pub last_sidebar_rect: Option<ratatui::layout::Rect>,
    pub input_rect: Option<ratatui::layout::Rect>,
    pub startup_intro: Option<StartupIntro>,
    pub form_modal: crate::form_modal::FormModal,
    pub animation_frame: u32,
    pub deny_arm: Option<std::time::Instant>,
    pub items_version: u64,
    pub expanded_version: u64,
    pub layout_cache: crate::output::LayoutCache,
    pub last_total_rows: u16,
    pub last_viewport_rows: u16,
    pub mouse_captured: bool,
    pub goal_scroll: u16,
    pub plans_scroll: u16,
    pub todos_scroll: u16,
    pub last_goal_rect: Option<ratatui::layout::Rect>,
    pub last_plan_rect: Option<ratatui::layout::Rect>,
    pub last_todo_rect: Option<ratatui::layout::Rect>,
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
            .any(|item| matches!(item, OutputItem::WorkflowPanel { ended_at: None, .. }))
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
        let candidates = crate::completion::compute_candidates(
            editor_buf,
            &self.flow_names,
            crate::completion::BUILTINS,
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

    pub fn push_item(&mut self, item: OutputItem) {
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
                    self.push_item(OutputItem::Thinking { text, done: false });
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
                }
            }
            StreamFrame::LlmDone { .. } => {
                if let Some(OutputItem::AssistantMd { streaming, .. }) = self.items.last_mut() {
                    *streaming = false;
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
                let is_done = matches!(frame, StreamFrame::FlowDone { .. });
                self.ensure_workflow_panel_and_apply(&frame);
                if is_done {
                    self.close_current_workflow_panel();
                    self.streaming = false;
                }
            }
            StreamFrame::Unknown => {}
        }
    }

    fn route_to_workflow_panel(&mut self, frame: &StreamFrame) {
        let mut mutated = false;
        if let Some(OutputItem::WorkflowPanel { graph, .. }) = self
            .items
            .iter_mut()
            .rev()
            .find(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
        {
            graph.apply_stream_frame(frame);
            mutated = true;
        }
        if mutated {
            self.items_version = self.items_version.wrapping_add(1);
        }
    }

    fn ensure_workflow_panel_and_apply(&mut self, frame: &StreamFrame) {
        let mut panel_after_user_turn = false;
        for it in self.items.iter().rev() {
            match it {
                OutputItem::WorkflowPanel { .. } => {
                    panel_after_user_turn = true;
                    break;
                }
                OutputItem::UserTurn { .. } => break,
                _ => {}
            }
        }
        if !panel_after_user_turn {
            let turn_index = self
                .items
                .iter()
                .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
                .count();
            self.push_item(OutputItem::WorkflowPanel {
                turn_index,
                graph: WorkflowGraph::new(atman_runtime::event::TurnId::now()),
                expanded_nodes: HashSet::new(),
                panel_expanded: false,
                started_at: std::time::Instant::now(),
                ended_at: None,
            });
        }
        self.route_to_workflow_panel(frame);
    }

    pub fn close_current_workflow_panel(&mut self) {
        for it in self.items.iter_mut().rev() {
            if let OutputItem::WorkflowPanel { ended_at, .. } = it {
                if ended_at.is_none() {
                    *ended_at = Some(Instant::now());
                    self.items_version = self.items_version.wrapping_add(1);
                }
                return;
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
        self.close_current_workflow_panel();
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
}
