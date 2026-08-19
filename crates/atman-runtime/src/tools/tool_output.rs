//! Truncation + spill-to-file for oversized tool results.

use std::path::{Path, PathBuf};

use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};

pub const MAX_TOOL_RESULT_CHARS: usize = 25_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolOutputBudget {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_line_bytes: usize,
}

impl Default for ToolOutputBudget {
    fn default() -> Self {
        Self {
            max_lines: 32,
            max_bytes: 1024,
            max_line_bytes: 384,
        }
    }
}

pub fn truncate_tool_result_content(
    content: &str,
    label: &str,
    session_dir: Option<&Path>,
) -> String {
    truncate_tool_result_content_with_budget(
        content,
        label,
        session_dir,
        ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: MAX_TOOL_RESULT_CHARS,
            max_line_bytes: MAX_TOOL_RESULT_CHARS,
        },
    )
}

pub fn truncate_tool_result_content_with_budget(
    content: &str,
    label: &str,
    session_dir: Option<&Path>,
    budget: ToolOutputBudget,
) -> String {
    if has_live_spill_notice(content) {
        return content.to_string();
    }

    let cut = bounded_prefix_len(content, budget);
    if cut == content.len() {
        return content.to_string();
    }

    let total = content.len();
    let head = &content[..cut];
    let spill_path = session_dir.and_then(|dir| write_spill_file(dir, label, content));

    match spill_path {
        Some(path) => {
            let path_arg = serde_json::to_string(&path.to_string_lossy()).unwrap();
            format!(
                "{head}\n\n[Output truncated at configured budget: max_lines={max_lines}, max_bytes={max_bytes}, max_line_bytes={max_line_bytes}. Full output ({total} bytes) spilled to: {path}. Continue with fs.read(path: {path_arg}); follow its TextLine continuation fields for later pages.]",
                max_lines = budget.max_lines,
                max_bytes = budget.max_bytes,
                max_line_bytes = budget.max_line_bytes,
                total = total,
                path = path.display()
            )
        }
        _ => format!(
            "{head}\n\n[Output truncated at configured budget: max_lines={max_lines}, max_bytes={max_bytes}, max_line_bytes={max_line_bytes}. Full output ({total} bytes) was not spilled because no session directory is available.]",
            max_lines = budget.max_lines,
            max_bytes = budget.max_bytes,
            max_line_bytes = budget.max_line_bytes,
            total = total,
        ),
    }
}

fn has_live_spill_notice(content: &str) -> bool {
    const PREFIX: &str = "Continue with fs.read(path: ";
    const SUFFIX: &str = "); follow its TextLine continuation fields for later pages.]";
    let Some((_, tail)) = content.rsplit_once(PREFIX) else {
        return false;
    };
    let Some(path_arg) = tail.strip_suffix(SUFFIX) else {
        return false;
    };
    let Ok(path) = serde_json::from_str::<String>(path_arg) else {
        return false;
    };
    Path::new(&path).is_file()
}

fn write_spill_file(session_dir: &Path, label: &str, content: &str) -> Option<PathBuf> {
    let out_dir = session_dir.join("tool_outputs");
    std::fs::create_dir_all(&out_dir).ok()?;
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

    for suffix in 0usize.. {
        let filename = if suffix == 0 {
            format!("tool_output_{safe_label}.txt")
        } else {
            format!("tool_output_{safe_label}_{suffix}.txt")
        };
        let path = out_dir.join(filename);
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return None,
        };
        if std::io::Write::write_all(&mut file, content.as_bytes()).is_ok()
            && file.sync_all().is_ok()
        {
            return Some(path);
        }
        drop(file);
        let _ = std::fs::remove_file(path);
        return None;
    }
    unreachable!()
}

pub fn bounded_text_prefix(content: &str, budget: ToolOutputBudget) -> usize {
    bounded_prefix_len(content, budget)
}

fn bounded_prefix_len(content: &str, budget: ToolOutputBudget) -> usize {
    let mut lines = 1usize;
    let mut line_bytes = 0usize;
    let mut used = 0usize;
    for (index, ch) in content.char_indices() {
        let width = ch.len_utf8();
        if ch == '\n' {
            if used + width > budget.max_bytes {
                return index;
            }
            used += width;
            if lines >= budget.max_lines {
                return index + width;
            }
            lines += 1;
            line_bytes = 0;
            continue;
        }
        if line_bytes + width > budget.max_line_bytes || used + width > budget.max_bytes {
            return index;
        }
        line_bytes += width;
        used += width;
    }
    content.len()
}

pub fn truncate_tool_results_in_message(
    msg: &Message,
    session_dir: Option<&Path>,
) -> Option<Message> {
    truncate_tool_results_in_message_with_budget(
        msg,
        session_dir,
        ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: MAX_TOOL_RESULT_CHARS,
            max_line_bytes: MAX_TOOL_RESULT_CHARS,
        },
    )
}

pub fn truncate_tool_results_in_message_with_budget(
    msg: &Message,
    session_dir: Option<&Path>,
    budget: ToolOutputBudget,
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
                let truncated = truncate_tool_result_content_with_budget(
                    content,
                    tool_use_id,
                    session_dir,
                    budget,
                );
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
        origin: MessageOrigin::User,
    })
}

pub fn maybe_truncate_tool_message(msg: &Message, session_dir: Option<&Path>) -> Message {
    maybe_truncate_tool_message_with_budget(
        msg,
        session_dir,
        ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: MAX_TOOL_RESULT_CHARS,
            max_line_bytes: MAX_TOOL_RESULT_CHARS,
        },
    )
}

pub fn maybe_truncate_tool_message_with_budget(
    msg: &Message,
    session_dir: Option<&Path>,
    budget: ToolOutputBudget,
) -> Message {
    if !matches!(msg.role, MessageRole::Tool) {
        return msg.clone();
    }
    truncate_tool_results_in_message_with_budget(msg, session_dir, budget)
        .unwrap_or_else(|| msg.clone())
}

pub fn spill_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("tool_outputs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
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
            origin: MessageOrigin::User,
        }
    }

    #[test]
    fn budget_limits_lines_and_long_utf8_lines() {
        let budget = ToolOutputBudget {
            max_lines: 2,
            max_bytes: 64,
            max_line_bytes: 12,
        };
        let content = "你好世界🚀你好世界🚀\nsecond line\nthird line";
        let result = truncate_tool_result_content_with_budget(content, "tu_budget", None, budget);
        let body = result.split_once("\n\n[Output truncated").unwrap().0;
        assert!(body.is_char_boundary(body.len()));
        assert!(body.lines().count() <= 2);
        assert!(body.len() <= 64);
        assert!(body.lines().next().unwrap().len() <= 12);
        assert!(result.contains("Output truncated"));
    }

    #[test]
    fn repeated_truncation_never_overwrites_full_spill() {
        let dir = TempDir::new().unwrap();
        let budget = ToolOutputBudget {
            max_lines: 2,
            max_bytes: 64,
            max_line_bytes: 64,
        };
        let original = "complete-output-".repeat(100);
        let first = truncate_tool_result_content_with_budget(
            &original,
            "same_tool_use",
            Some(dir.path()),
            budget,
        );
        let _ = truncate_tool_result_content_with_budget(
            &first,
            "same_tool_use",
            Some(dir.path()),
            budget,
        );
        let spilled =
            std::fs::read_to_string(spill_dir(dir.path()).join("tool_output_same_tool_use.txt"))
                .unwrap();
        assert_eq!(spilled, original);
        assert!(first.contains("Continue with fs.read(path:"));
        assert!(first.contains("TextLine continuation"));
    }

    #[test]
    fn same_tool_use_id_gets_distinct_spills() {
        let dir = TempDir::new().unwrap();
        let budget = ToolOutputBudget {
            max_lines: 1,
            max_bytes: 16,
            max_line_bytes: 16,
        };
        let first = "first-output-".repeat(20);
        let second = "second-output-".repeat(20);
        let first_notice =
            truncate_tool_result_content_with_budget(&first, "same", Some(dir.path()), budget);
        let second_notice =
            truncate_tool_result_content_with_budget(&second, "same", Some(dir.path()), budget);

        assert!(first_notice.contains("tool_output_same.txt"));
        assert!(second_notice.contains("tool_output_same_1.txt"));
        assert_eq!(
            std::fs::read_to_string(spill_dir(dir.path()).join("tool_output_same.txt")).unwrap(),
            first
        );
        assert_eq!(
            std::fs::read_to_string(spill_dir(dir.path()).join("tool_output_same_1.txt")).unwrap(),
            second
        );
    }

    #[cfg(unix)]
    #[test]
    fn spill_notice_escapes_and_recovers_quoted_paths() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("quoted\"and\\backslash");
        std::fs::create_dir(&dir).unwrap();
        let budget = ToolOutputBudget {
            max_lines: 1,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let original = "full-output-".repeat(100);
        let first =
            truncate_tool_result_content_with_budget(&original, "quoted", Some(&dir), budget);

        let (_, tail) = first.rsplit_once("Continue with fs.read(path: ").unwrap();
        let path_arg = tail
            .strip_suffix("); follow its TextLine continuation fields for later pages.]")
            .unwrap();
        let recovered: String = serde_json::from_str(path_arg).unwrap();
        assert!(recovered.contains("quoted\"and\\backslash"));
        assert!(Path::new(&recovered).is_file());
        assert_eq!(
            truncate_tool_result_content_with_budget(&first, "quoted", Some(&dir), budget),
            first
        );
        assert_eq!(std::fs::read_dir(spill_dir(&dir)).unwrap().count(), 1);
    }

    #[test]
    fn repeated_truncation_preserves_live_spill_notice() {
        let dir = TempDir::new().unwrap();
        let budget = ToolOutputBudget {
            max_lines: 1,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let original = "full-output-".repeat(100);
        let first =
            truncate_tool_result_content_with_budget(&original, "repeat", Some(dir.path()), budget);
        let second =
            truncate_tool_result_content_with_budget(&first, "repeat", Some(dir.path()), budget);

        assert_eq!(second, first);
        assert!(second.contains("Continue with fs.read(path:"));
        assert_eq!(std::fs::read_dir(spill_dir(dir.path())).unwrap().count(), 1);
    }

    #[test]
    fn failed_spill_write_removes_partial_file() {
        let dir = TempDir::new().unwrap();
        let output_dir = spill_dir(dir.path());
        std::fs::create_dir_all(&output_dir).unwrap();
        let target = output_dir.join("tool_output_fail.txt");
        std::fs::create_dir(&target).unwrap();
        let content = "content".repeat(100);

        let result = truncate_tool_result_content_with_budget(
            &content,
            "fail",
            Some(dir.path()),
            ToolOutputBudget {
                max_lines: 1,
                max_bytes: 8,
                max_line_bytes: 8,
            },
        );

        assert!(result.contains("tool_output_fail_1.txt"));
        assert_eq!(
            std::fs::read_to_string(output_dir.join("tool_output_fail_1.txt")).unwrap(),
            content
        );
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
            origin: MessageOrigin::User,
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
