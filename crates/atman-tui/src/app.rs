use std::time::{Duration, Instant};

use atman_runtime::stream::StreamFrame;

const LAG_COOLDOWN: Duration = Duration::from_millis(300);

#[derive(Debug, Clone)]
pub enum OutputItem {
    UserTurn {
        text: String,
    },
    AssistantMd {
        md: String,
        streaming: bool,
    },
    ToolCall {
        tool: String,
        args: String,
        status: ToolStatus,
        result: Option<String>,
        history_id: Option<String>,
    },
    SystemNote {
        text: String,
        level: NoteLevel,
    },
    Divider,
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

#[derive(Debug, Default)]
pub struct AppState {
    pub items: Vec<OutputItem>,
    pub input: String,
    pub scroll_offset: u16,
    pub follow_tail: bool,
    pub should_quit: bool,
    pub streaming: bool,
    pub goal: Option<String>,
    pub session_id: String,
    pub session_dir: String,
    pub attach_count: usize,
    pub context: atman_runtime::ContextSnapshot,
    pub sidebar_mode: crate::sidebar::SidebarMode,
    pub popup: crate::completion::PopupState,
    pub cheatsheet_open: bool,
    pub flow_names: Vec<(String, String)>,
    pub last_total_rows: u16,
    pub last_viewport_rows: u16,
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
            ..Default::default()
        }
    }

    pub fn with_initial_items(mut self, items: Vec<OutputItem>) -> Self {
        self.items = items;
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
        self.reset_lag_state();
    }

    pub fn push_note(&mut self, text: impl Into<String>, level: NoteLevel) {
        self.push_item(OutputItem::SystemNote {
            text: text.into(),
            level,
        });
    }

    pub fn apply_stream_frame(&mut self, frame: StreamFrame) {
        match frame {
            StreamFrame::LlmChunk { text, .. } => {
                if let Some(OutputItem::AssistantMd { md, streaming, .. }) = self.items.last_mut()
                    && *streaming
                {
                    md.push_str(&text);
                    self.reset_lag_state();
                    return;
                }
                self.push_item(OutputItem::AssistantMd {
                    md: text,
                    streaming: true,
                });
                self.streaming = true;
            }
            StreamFrame::LlmDone { .. } => {
                if let Some(OutputItem::AssistantMd { streaming, .. }) = self.items.last_mut() {
                    *streaming = false;
                }
                self.streaming = false;
                self.reset_lag_state();
            }
            StreamFrame::ToolUseStart {
                tool,
                args_preview,
                id,
            } => {
                self.push_item(OutputItem::ToolCall {
                    tool,
                    args: args_preview,
                    status: ToolStatus::Running,
                    result: None,
                    history_id: Some(id),
                });
            }
            StreamFrame::ToolUseDone {
                tool, ok, preview, ..
            } => {
                for item in self.items.iter_mut().rev() {
                    if let OutputItem::ToolCall {
                        tool: t,
                        status,
                        result,
                        ..
                    } = item
                        && t == &tool
                        && *status == ToolStatus::Running
                    {
                        *status = if ok { ToolStatus::Ok } else { ToolStatus::Err };
                        *result = Some(preview);
                        self.reset_lag_state();
                        return;
                    }
                }
            }
            StreamFrame::Note(text) => {
                self.push_item(OutputItem::SystemNote {
                    text,
                    level: NoteLevel::Info,
                });
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
            return;
        }
        self.last_lag_count = dropped;
        self.items.push(OutputItem::SystemNote {
            text: format!("dropped {dropped} stream frames"),
            level: NoteLevel::Warn,
        });
        self.last_lag_note_idx = Some(self.items.len() - 1);
        self.last_lag_at = Some(now);
    }

    fn reset_lag_state(&mut self) {
        self.last_lag_note_idx = None;
        self.last_lag_count = 0;
    }

    pub fn push_user_turn(&mut self, text: String) {
        self.push_item(OutputItem::UserTurn { text });
        self.items.push(OutputItem::Divider);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_accumulate_into_last_assistant_item() {
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
            OutputItem::AssistantMd { md, streaming, .. } => {
                assert_eq!(md, "hello world");
                assert!(*streaming);
            }
            other => panic!("expected assistant md, got {other:?}"),
        }
    }

    #[test]
    fn done_flips_streaming_flag() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::LlmChunk {
            text: "hi".into(),
            model: "m".into(),
        });
        app.apply_stream_frame(StreamFrame::LlmDone { total_tokens: 3 });
        match &app.items[0] {
            OutputItem::AssistantMd { streaming, .. } => assert!(!streaming),
            _ => panic!(),
        }
        assert!(!app.streaming);
    }

    #[test]
    fn tool_start_then_done_updates_same_item() {
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
        assert_eq!(app.items.len(), 1);
        match &app.items[0] {
            OutputItem::ToolCall {
                tool,
                status,
                result,
                ..
            } => {
                assert_eq!(tool, "fs.read");
                assert_eq!(*status, ToolStatus::Ok);
                assert_eq!(result.as_deref(), Some("12 bytes"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_done_matches_most_recent_running_call_of_same_name() {
        let mut app = AppState::new("s".into(), None);
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "a".into(),
            id: "tc_a".into(),
        });
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "b".into(),
            id: "tc_b".into(),
        });
        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: false,
            preview: "err".into(),
            id: "tc_b".into(),
        });
        assert_eq!(app.items.len(), 2);
        let statuses: Vec<_> = app
            .items
            .iter()
            .filter_map(|i| match i {
                OutputItem::ToolCall { status, .. } => Some(*status),
                _ => None,
            })
            .collect();
        assert_eq!(statuses, vec![ToolStatus::Running, ToolStatus::Err]);
    }

    #[test]
    fn user_turn_pushes_item_and_divider() {
        let mut app = AppState::new("s".into(), None);
        app.push_user_turn("hi".into());
        assert_eq!(app.items.len(), 2);
        assert!(matches!(app.items[0], OutputItem::UserTurn { .. }));
        assert!(matches!(app.items[1], OutputItem::Divider));
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
}
