//! Truncation + spill-to-file for oversized tool results.

use std::path::{Path, PathBuf};

use crate::message::{Message, MessagePart, MessageRole};

pub const MAX_TOOL_RESULT_CHARS: usize = 25_000;

pub fn truncate_tool_result_content(
    content: &str,
    label: &str,
    session_dir: Option<&Path>,
) -> String {
    if content.len() <= MAX_TOOL_RESULT_CHARS {
        return content.to_string();
    }

    let total = content.len();
    let mut cut = MAX_TOOL_RESULT_CHARS.min(content.len());
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    let head = &content[..cut];

    let spill_path = session_dir.map(|dir| {
        let out_dir = dir.join("tool_outputs");
        let _ = std::fs::create_dir_all(&out_dir);
        let safe_label: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let filename = format!("tool_output_{safe_label}.txt");
        let path = out_dir.join(filename);
        if std::fs::write(&path, content.as_bytes()).is_ok() {
            Some(path)
        } else {
            None
        }
    });

    match spill_path {
        Some(Some(path)) => format!(
            "{head}\n\n[Output truncated at {max} chars. Full output ({total} chars) spilled to: {path}. Use fs.read with offset/limit to inspect specific sections.]",
            max = MAX_TOOL_RESULT_CHARS,
            total = total,
            path = path.display()
        ),
        _ => format!(
            "{head}\n\n[Output truncated at {max} chars. Full output ({total} chars) not spilled to file — no session directory available.]",
            max = MAX_TOOL_RESULT_CHARS,
            total = total,
        ),
    }
}

pub fn truncate_tool_results_in_message(
    msg: &Message,
    session_dir: Option<&Path>,
) -> Option<Message> {
    let mut changed = false;
    let parts: Vec<MessagePart> = msg
        .parts
        .iter()
        .map(|part| match part {
            MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let truncated = truncate_tool_result_content(content, tool_use_id, session_dir);
                if truncated.len() != content.len() || truncated != *content {
                    changed = true;
                    MessagePart::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: truncated,
                        is_error: *is_error,
                    }
                } else {
                    part.clone()
                }
            }
            _ => part.clone(),
        })
        .collect();

    if !changed {
        return None;
    }
    Some(Message {
        role: msg.role,
        parts,
        turn_id: msg.turn_id.clone(),
    })
}

pub fn maybe_truncate_tool_message(msg: &Message, session_dir: Option<&Path>) -> Message {
    if !matches!(msg.role, MessageRole::Tool) {
        return msg.clone();
    }
    truncate_tool_results_in_message(msg, session_dir).unwrap_or_else(|| msg.clone())
}

pub fn spill_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("tool_outputs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::message::{Message, MessagePart, MessageRole};
    use tempfile::TempDir;

    fn tool_msg(content: &str) -> Message {
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_1".into(),
                content: content.into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
        }
    }

    #[test]
    fn small_content_unchanged() {
        let dir = TempDir::new().unwrap();
        let result = truncate_tool_result_content("hello", "tu_1", Some(dir.path()));
        assert_eq!(result, "hello");
    }

    #[test]
    fn at_threshold_unchanged() {
        let dir = TempDir::new().unwrap();
        let content = "x".repeat(MAX_TOOL_RESULT_CHARS);
        let result = truncate_tool_result_content(&content, "tu_1", Some(dir.path()));
        assert_eq!(result.len(), MAX_TOOL_RESULT_CHARS);
        assert_eq!(result, content);
    }

    #[test]
    fn over_threshold_truncates_and_spills() {
        let dir = TempDir::new().unwrap();
        let content = "A".repeat(MAX_TOOL_RESULT_CHARS + 1000);
        let result = truncate_tool_result_content(&content, "tu_1", Some(dir.path()));
        assert!(result.len() < content.len());
        assert!(result.starts_with(&"A".repeat(MAX_TOOL_RESULT_CHARS)));
        assert!(result.contains("[Output truncated at"));
        assert!(result.contains("spilled to:"));
        assert!(result.contains("fs.read"));
        let spill_path = spill_dir(dir.path()).join("tool_output_tu_1.txt");
        let spilled = std::fs::read_to_string(&spill_path).unwrap();
        assert_eq!(spilled.len(), content.len());
        assert_eq!(spilled, content);
    }

    #[test]
    fn no_session_dir_truncates_without_spill() {
        let content = "B".repeat(MAX_TOOL_RESULT_CHARS + 500);
        let result = truncate_tool_result_content(&content, "tu_1", None);
        assert!(result.starts_with(&"B".repeat(MAX_TOOL_RESULT_CHARS)));
        assert!(result.contains("[Output truncated at"));
        assert!(result.contains("not spilled"));
        assert!(!result.contains("fs.read"));
    }

    #[test]
    fn unsafe_label_sanitized() {
        let dir = TempDir::new().unwrap();
        let content = "C".repeat(MAX_TOOL_RESULT_CHARS + 10);
        let _ = truncate_tool_result_content(&content, "../../etc/passwd", Some(dir.path()));
        let entries = std::fs::read_dir(spill_dir(dir.path())).unwrap();
        let names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| n.starts_with("tool_output_")));
        assert!(names.iter().all(|n| !n.contains("..")));
        assert!(names.iter().all(|n| !n.contains('/')));
    }

    #[test]
    fn message_with_small_tool_result_unchanged() {
        let dir = TempDir::new().unwrap();
        let msg = tool_msg("small result");
        let result = truncate_tool_results_in_message(&msg, Some(dir.path()));
        assert!(result.is_none());
    }

    #[test]
    fn message_with_large_tool_result_truncated() {
        let dir = TempDir::new().unwrap();
        let content = "D".repeat(MAX_TOOL_RESULT_CHARS + 500);
        let msg = tool_msg(&content);
        let result = truncate_tool_results_in_message(&msg, Some(dir.path()));
        assert!(result.is_some());
        let result = result.unwrap();
        if let MessagePart::ToolResult { content, .. } = &result.parts[0] {
            assert!(content.len() < MAX_TOOL_RESULT_CHARS + 500);
            assert!(content.contains("[Output truncated at"));
        } else {
            panic!("expected ToolResult part");
        }
    }

    #[test]
    fn non_tool_message_unchanged() {
        let dir = TempDir::new().unwrap();
        let msg = Message {
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                text: "x".repeat(MAX_TOOL_RESULT_CHARS + 1000),
            }],
            turn_id: TurnId::now(),
        };
        let result = maybe_truncate_tool_message(&msg, Some(dir.path()));
        assert_eq!(result.parts.len(), 1);
        if let MessagePart::Text { text } = &result.parts[0] {
            assert_eq!(text.len(), MAX_TOOL_RESULT_CHARS + 1000);
        } else {
            panic!("expected Text part");
        }
    }
}
