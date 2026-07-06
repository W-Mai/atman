use atman_runtime::stream::StreamFrame;

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
    pub scroll: u16,
    pub should_quit: bool,
    pub streaming: bool,
    pub goal: Option<String>,
    pub session_id: String,
}

impl AppState {
    pub fn new(session_id: String, goal: Option<String>) -> Self {
        Self {
            session_id,
            goal,
            ..Default::default()
        }
    }

    pub fn apply_stream_frame(&mut self, frame: StreamFrame) {
        match frame {
            StreamFrame::LlmChunk { text, .. } => {
                if let Some(OutputItem::AssistantMd { md, streaming, .. }) = self.items.last_mut()
                    && *streaming
                {
                    md.push_str(&text);
                    return;
                }
                self.items.push(OutputItem::AssistantMd {
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
            }
            StreamFrame::ToolUseStart { tool, args_preview } => {
                self.items.push(OutputItem::ToolCall {
                    tool,
                    args: args_preview,
                    status: ToolStatus::Running,
                    result: None,
                });
            }
            StreamFrame::ToolUseDone { tool, ok, preview } => {
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
                        return;
                    }
                }
            }
            StreamFrame::Note(text) => {
                self.items.push(OutputItem::SystemNote {
                    text,
                    level: NoteLevel::Info,
                });
            }
        }
    }

    pub fn push_user_turn(&mut self, text: String) {
        self.items.push(OutputItem::UserTurn { text });
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
        });
        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: true,
            preview: "12 bytes".into(),
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
        });
        app.apply_stream_frame(StreamFrame::ToolUseStart {
            tool: "fs.read".into(),
            args_preview: "b".into(),
        });
        app.apply_stream_frame(StreamFrame::ToolUseDone {
            tool: "fs.read".into(),
            ok: false,
            preview: "err".into(),
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
}
