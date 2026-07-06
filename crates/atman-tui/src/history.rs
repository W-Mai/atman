use std::collections::HashMap;
use std::time::Instant;

use atman_runtime::TranscriptEntry;
use atman_runtime::event::FlowNodeStatus;
use atman_runtime::message::{Message, MessagePart, MessageRole};

use crate::app::{NoteLevel, OutputItem, ToolStatus};

const TOOL_RESULT_MAX_CHARS: usize = 200;

pub fn flatten_transcript(entries: &[TranscriptEntry]) -> Vec<OutputItem> {
    let mut out: Vec<OutputItem> = Vec::new();
    let mut flow_panel_idx: HashMap<String, usize> = HashMap::new();
    for entry in entries {
        match entry {
            TranscriptEntry::Message(msg) => flatten_message(msg, &mut out),
            TranscriptEntry::FlowGraph {
                run_id,
                flow_name,
                graph,
            } => {
                let idx = out.len();
                out.push(OutputItem::FlowPanel {
                    run_id: run_id.clone(),
                    flow_name: flow_name.clone(),
                    graph: graph.clone(),
                    node_states: HashMap::new(),
                    started_at: Instant::now(),
                    ended_at: None,
                    expanded: false,
                });
                flow_panel_idx.insert(run_id.clone(), idx);
            }
            TranscriptEntry::FlowNodeStatus {
                run_id,
                node_id,
                status,
            } => {
                if let Some(&idx) = flow_panel_idx.get(run_id)
                    && let Some(OutputItem::FlowPanel { node_states, .. }) = out.get_mut(idx)
                {
                    node_states.insert(node_id.clone(), status.clone());
                }
            }
            TranscriptEntry::FlowDone { run_id, ok } => {
                if let Some(&idx) = flow_panel_idx.get(run_id)
                    && let Some(OutputItem::FlowPanel {
                        ended_at,
                        node_states,
                        ..
                    }) = out.get_mut(idx)
                {
                    *ended_at = Some(Instant::now());
                    if !*ok {
                        node_states
                            .entry("__flow__".to_string())
                            .or_insert(FlowNodeStatus::Err);
                    }
                }
            }
        }
    }
    out
}

fn flatten_message(msg: &Message, out: &mut Vec<OutputItem>) {
    match msg.role {
        MessageRole::User => {
            let text = msg.text_concat();
            if !text.trim().is_empty() {
                out.push(OutputItem::UserTurn { text });
            }
            out.push(OutputItem::Divider);
        }
        MessageRole::Assistant => {
            for part in &msg.parts {
                match part {
                    MessagePart::Text { text } => {
                        out.push(OutputItem::AssistantMd {
                            md: text.clone(),
                            streaming: false,
                        });
                    }
                    MessagePart::ToolUse { id, name, input } => {
                        out.push(OutputItem::ToolCall {
                            tool: name.clone(),
                            args: format_input_preview(input),
                            status: ToolStatus::Ok,
                            result: None,
                            tool_use_id: Some(id.clone()),
                        });
                    }
                    MessagePart::ToolResult { .. } | MessagePart::Image { .. } => {}
                }
            }
        }
        MessageRole::Tool => {
            for part in &msg.parts {
                if let MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = part
                {
                    attach_tool_result(out, tool_use_id, content, *is_error);
                }
            }
        }
        MessageRole::System => {}
    }
}

pub fn flatten_messages(messages: &[Message]) -> Vec<OutputItem> {
    let mut out: Vec<OutputItem> = Vec::new();
    for msg in messages {
        flatten_message(msg, &mut out);
    }
    out
}

fn attach_tool_result(items: &mut [OutputItem], tool_use_id: &str, content: &str, is_error: bool) {
    let truncated = truncate(content, TOOL_RESULT_MAX_CHARS);
    for item in items.iter_mut().rev() {
        if let OutputItem::ToolCall {
            tool_use_id: Some(id),
            status,
            result,
            ..
        } = item
            && id == tool_use_id
        {
            *status = if is_error {
                ToolStatus::Err
            } else {
                ToolStatus::Ok
            };
            *result = Some(truncated);
            return;
        }
    }
}

fn format_input_preview(input: &serde_json::Value) -> String {
    let compact = serde_json::to_string(input).unwrap_or_default();
    truncate(&compact, 80)
}

fn truncate(s: &str, max_chars: usize) -> String {
    for (char_count, (byte_idx, _)) in s.char_indices().enumerate() {
        if char_count == max_chars {
            let mut out = String::with_capacity(byte_idx + 3);
            out.push_str(&s[..byte_idx]);
            out.push('…');
            return out;
        }
    }
    s.to_string()
}

pub fn history_note(item_count: usize, message_count: usize) -> Option<OutputItem> {
    if item_count == 0 {
        return None;
    }
    Some(OutputItem::SystemNote {
        text: format!(
            "resumed with {message_count} prior message(s), {item_count} item(s) restored"
        ),
        level: NoteLevel::Info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::TurnId;
    use serde_json::json;

    fn assistant(parts: Vec<MessagePart>) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts,
            turn_id: TurnId::now(),
        }
    }

    fn user(text: &str) -> Message {
        Message::user_text(TurnId::now(), text)
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> Message {
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error,
            }],
            turn_id: TurnId::now(),
        }
    }

    #[test]
    fn user_message_becomes_turn_plus_divider() {
        let out = flatten_messages(&[user("hi")]);
        assert_eq!(out.len(), 2);
        matches!(out[0], OutputItem::UserTurn { .. });
        matches!(out[1], OutputItem::Divider);
    }

    #[test]
    fn assistant_multi_part_preserves_order() {
        let msgs = vec![assistant(vec![
            MessagePart::Text {
                text: "thinking".into(),
            },
            MessagePart::ToolUse {
                id: "toolu_1".into(),
                name: "fs.read".into(),
                input: json!({"path": "foo"}),
            },
            MessagePart::Text {
                text: "done".into(),
            },
        ])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
        assert!(matches!(out[1], OutputItem::ToolCall { .. }));
        assert!(matches!(out[2], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn tool_result_attaches_to_matching_tool_use_id() {
        let msgs = vec![
            assistant(vec![MessagePart::ToolUse {
                id: "toolu_1".into(),
                name: "fs.read".into(),
                input: json!({}),
            }]),
            tool_result("toolu_1", "12 bytes", false),
        ];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        match &out[0] {
            OutputItem::ToolCall { status, result, .. } => {
                assert_eq!(*status, ToolStatus::Ok);
                assert_eq!(result.as_deref(), Some("12 bytes"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_is_error_marks_toolcall_err() {
        let msgs = vec![
            assistant(vec![MessagePart::ToolUse {
                id: "toolu_2".into(),
                name: "fs.read".into(),
                input: json!({}),
            }]),
            tool_result("toolu_2", "nope", true),
        ];
        let out = flatten_messages(&msgs);
        match &out[0] {
            OutputItem::ToolCall { status, .. } => {
                assert_eq!(*status, ToolStatus::Err);
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn tool_result_truncates_long_content() {
        let long = "x".repeat(500);
        let msgs = vec![
            assistant(vec![MessagePart::ToolUse {
                id: "id".into(),
                name: "t".into(),
                input: json!({}),
            }]),
            tool_result("id", &long, false),
        ];
        let out = flatten_messages(&msgs);
        match &out[0] {
            OutputItem::ToolCall { result, .. } => {
                let r = result.as_deref().unwrap();
                assert!(r.chars().count() <= TOOL_RESULT_MAX_CHARS + 1);
                assert!(r.ends_with('…'));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn image_part_is_skipped_silently() {
        use atman_runtime::message::{ImageData, ImageSource};
        use std::path::PathBuf;
        let msgs = vec![assistant(vec![
            MessagePart::Text {
                text: "here".into(),
            },
            MessagePart::Image {
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Path {
                        path: PathBuf::from("/tmp/x.png"),
                    },
                },
            },
        ])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn truncate_handles_utf8_boundary() {
        let s = "你好世界"; // 4 chars, 12 bytes
        assert_eq!(truncate(s, 4), "你好世界");
        assert_eq!(truncate(s, 2), "你好…");
    }
}
