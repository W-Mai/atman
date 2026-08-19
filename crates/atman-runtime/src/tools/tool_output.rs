use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

#[derive(Clone, Default)]
pub struct OutputStore {
    session_dir: Option<Arc<PathBuf>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPage {
    pub content: String,
    pub mode: &'static str,
    pub offset: usize,
    pub next_offset: usize,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub has_more: bool,
}

impl OutputStore {
    pub fn at(session_dir: impl Into<PathBuf>) -> Self {
        Self {
            session_dir: Some(Arc::new(session_dir.into())),
        }
    }

    pub fn register(&self, _label: &str, content: &str) -> Option<String> {
        let session_dir = self.session_dir.as_deref()?;
        let output_id = format!("out_{}", uuid::Uuid::now_v7().simple());
        write_output_file(session_dir, &output_id, content)?;
        Some(output_id)
    }

    pub fn read_lines(
        &self,
        output_id: &str,
        offset: usize,
        limit: usize,
        budget: ToolOutputBudget,
    ) -> Result<OutputPage, RuntimeError> {
        let content = self.read_registered(output_id)?;
        let lines: Vec<&str> = content.split_inclusive('\n').collect();
        let start = offset.min(lines.len());
        let end = start
            .saturating_add(limit.min(budget.max_lines))
            .min(lines.len());
        let requested = lines[start..end].concat();
        let cut = bounded_prefix_len(&requested, budget);
        if cut < requested.len() {
            return Err(RuntimeError::ToolFailed(
                "output.read: selected line range exceeds the output budget; use byte_offset + byte_limit".into(),
            ));
        }
        Ok(OutputPage {
            content: requested,
            mode: "lines",
            offset: start,
            next_offset: end,
            total_lines: lines.len(),
            total_bytes: content.len(),
            has_more: end < lines.len(),
        })
    }

    pub fn read_bytes(
        &self,
        output_id: &str,
        offset: usize,
        limit: usize,
        budget: ToolOutputBudget,
    ) -> Result<OutputPage, RuntimeError> {
        let content = self.read_registered(output_id)?;
        if offset > content.len() || !content.is_char_boundary(offset) {
            return Err(RuntimeError::ToolFailed(
                "output.read: byte_offset is not a valid UTF-8 boundary".into(),
            ));
        }
        let mut end = offset
            .saturating_add(limit.min(budget.max_bytes).min(budget.max_line_bytes))
            .min(content.len());
        while end > offset && !content.is_char_boundary(end) {
            end -= 1;
        }
        Ok(OutputPage {
            content: content[offset..end].to_string(),
            mode: "bytes",
            offset,
            next_offset: end,
            total_lines: content.split_inclusive('\n').count(),
            total_bytes: content.len(),
            has_more: end < content.len(),
        })
    }

    pub(crate) fn validates_total_bytes(&self, output_id: &str, total_bytes: usize) -> bool {
        self.read_registered(output_id)
            .is_ok_and(|content| content.len() == total_bytes)
    }

    pub(crate) fn validates_pagination(
        &self,
        output_id: &str,
        mode: &str,
        offset: usize,
        total_bytes: usize,
        total_lines: Option<usize>,
        has_more: bool,
    ) -> bool {
        let Ok(content) = self.read_registered(output_id) else {
            return false;
        };
        if total_bytes != content.len() {
            return false;
        }
        match mode {
            "bytes" => has_more == (offset < content.len()) && offset <= content.len(),
            "lines" => {
                let actual_lines = content.split_inclusive('\n').count();
                total_lines == Some(actual_lines)
                    && has_more == (offset < actual_lines)
                    && offset <= actual_lines
            }
            _ => false,
        }
    }

    fn read_registered(&self, output_id: &str) -> Result<String, RuntimeError> {
        if !output_id.starts_with("out_")
            || output_id.len() != 36
            || !output_id[4..].chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(RuntimeError::ToolFailed(
                "output.read: unknown output_id".into(),
            ));
        }
        let session_dir = self.session_dir.as_deref().ok_or_else(|| {
            RuntimeError::ToolFailed("output.read: no session output store available".into())
        })?;
        let path = output_dir(session_dir).join(format!("{output_id}.txt"));
        std::fs::read_to_string(path)
            .map_err(|_| RuntimeError::ToolFailed("output.read: unknown output_id".into()))
    }
}

pub struct OutputRead;

impl Tool for OutputRead {
    fn name(&self) -> &str {
        "output.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read a registered oversized tool output from the current session. Paginate by either line_offset + line_limit or byte_offset + byte_limit; output_id cannot address arbitrary paths.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "output_id": {"type": "string", "description": "Opaque ID returned by a truncated tool result."},
                "line_offset": {"type": "integer", "minimum": 0, "description": "Zero-based line offset."},
                "line_limit": {"type": "integer", "minimum": 1, "description": "Maximum lines to return."},
                "byte_offset": {"type": "integer", "minimum": 0, "description": "Zero-based UTF-8 byte offset."},
                "byte_limit": {"type": "integer", "minimum": 1, "description": "Maximum bytes to return."}
            },
            "required": ["output_id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let output_id = string_arg(&args, "output_id")?;
            let store = ctx.output_store.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("output.read: no session output store available".into())
            })?;
            let line_offset = optional_usize(&args, "line_offset")?;
            let line_limit = optional_positive_usize(&args, "line_limit")?;
            let byte_offset = optional_usize(&args, "byte_offset")?;
            let byte_limit = optional_positive_usize(&args, "byte_limit")?;
            let uses_lines = line_offset.is_some() || line_limit.is_some();
            let uses_bytes = byte_offset.is_some() || byte_limit.is_some();
            if uses_lines && uses_bytes {
                return Err(RuntimeError::ToolFailed(
                    "output.read: choose line pagination or byte pagination, not both".into(),
                ));
            }
            let page = if uses_bytes {
                store.read_bytes(
                    &output_id,
                    byte_offset.unwrap_or(0),
                    byte_limit.unwrap_or(ctx.tool_output_budget.max_bytes),
                    ctx.tool_output_budget,
                )?
            } else {
                store.read_lines(
                    &output_id,
                    line_offset.unwrap_or(0),
                    line_limit.unwrap_or(ctx.tool_output_budget.max_lines),
                    ctx.tool_output_budget,
                )?
            };
            Ok(output_page_value(page))
        })
    }
}

fn string_arg(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: format!("string {name}"),
            actual: value.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn optional_usize(args: &ToolArgs, name: &str) -> Result<Option<usize>, RuntimeError> {
    match args.named(name) {
        Some(Value::Int(value)) if *value >= 0 => Ok(Some(*value as usize)),
        Some(Value::Unit) | None => Ok(None),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: format!("non-negative integer {name}"),
            actual: value.kind_name().into(),
        }),
    }
}

fn optional_positive_usize(args: &ToolArgs, name: &str) -> Result<Option<usize>, RuntimeError> {
    match args.named(name) {
        Some(Value::Int(value)) if *value > 0 => Ok(Some(*value as usize)),
        Some(Value::Unit) | None => Ok(None),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: format!("positive integer {name}"),
            actual: value.kind_name().into(),
        }),
    }
}

fn output_page_value(page: OutputPage) -> Value {
    Value::Struct(vec![
        ("content".into(), Value::Str(page.content)),
        ("mode".into(), Value::Str(page.mode.into())),
        ("offset".into(), Value::Int(page.offset as i64)),
        ("next_offset".into(), Value::Int(page.next_offset as i64)),
        ("total_lines".into(), Value::Int(page.total_lines as i64)),
        ("total_bytes".into(), Value::Int(page.total_bytes as i64)),
        ("has_more".into(), Value::Bool(page.has_more)),
    ])
}

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
    output_store: Option<&OutputStore>,
) -> String {
    truncate_tool_result_content_with_budget(
        content,
        label,
        output_store,
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
    output_store: Option<&OutputStore>,
    budget: ToolOutputBudget,
) -> String {
    if is_live_pagination_envelope(content, output_store)
        || is_live_pagination_notice(content, output_store)
    {
        return content.to_string();
    }

    let cut = bounded_prefix_len(content, budget);
    if cut == content.len() {
        return content.to_string();
    }

    let total = content.len();
    let head = &content[..cut];
    let output_id = output_store.and_then(|store| store.register(label, content));

    match output_id {
        Some(output_id) => format!(
            "{head}\n\n[Output truncated at configured budget: max_lines={max_lines}, max_bytes={max_bytes}, max_line_bytes={max_line_bytes}, total_bytes={total}. output_id={output_id}. Continue with output.read(output_id: {output_id}, line_offset: 0, line_limit: 100) or byte_offset/byte_limit.]",
            max_lines = budget.max_lines,
            max_bytes = budget.max_bytes,
            max_line_bytes = budget.max_line_bytes,
            total = total,
        ),
        None => format!(
            "{head}\n\n[Output truncated at configured budget: max_lines={max_lines}, max_bytes={max_bytes}, max_line_bytes={max_line_bytes}, total_bytes={total}. No output_id is available because the session output could not be written.]",
            max_lines = budget.max_lines,
            max_bytes = budget.max_bytes,
            max_line_bytes = budget.max_line_bytes,
            total = total,
        ),
    }
}

fn is_live_pagination_notice(content: &str, output_store: Option<&OutputStore>) -> bool {
    let Some(store) = output_store else {
        return false;
    };
    let Some((_, notice)) = content.rsplit_once("\n\n[Output truncated at configured budget:")
    else {
        return false;
    };
    if !notice.ends_with("or byte_offset/byte_limit.]") {
        return false;
    }
    let Some(total_bytes) = notice
        .split_once("total_bytes=")
        .and_then(|(_, value)| value.split_once('.').map(|(value, _)| value))
        .and_then(|value| value.parse::<usize>().ok())
    else {
        return false;
    };
    let Some(output_id) = notice
        .split_once("output_id=")
        .and_then(|(_, value)| value.split_once('.').map(|(value, _)| value))
    else {
        return false;
    };
    store.validates_total_bytes(output_id, total_bytes)
}

fn is_live_pagination_envelope(content: &str, output_store: Option<&OutputStore>) -> bool {
    let Some(store) = output_store else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(content) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(output_id) = object.get("output_id").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if object
        .get("content")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return false;
    }
    let Some(total_bytes) = object
        .get("total_bytes")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(next) = object.get("next").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(mode) = next.get("mode").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let Some(offset) = next
        .get("offset")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
    else {
        return false;
    };
    let Some(has_more) = next.get("has_more").and_then(serde_json::Value::as_bool) else {
        return false;
    };
    let total_lines = object
        .get("total_lines")
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| usize::try_from(value).ok());
    store.validates_pagination(output_id, mode, offset, total_bytes, total_lines, has_more)
}

fn output_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("tool_outputs")
}

fn write_output_file(session_dir: &Path, output_id: &str, content: &str) -> Option<()> {
    let out_dir = output_dir(session_dir);
    std::fs::create_dir_all(&out_dir).ok()?;
    let path = out_dir.join(format!("{output_id}.txt"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .ok()?;
    if std::io::Write::write_all(&mut file, content.as_bytes()).is_ok() && file.sync_all().is_ok() {
        return Some(());
    }
    drop(file);
    let _ = std::fs::remove_file(path);
    None
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
    output_store: Option<&OutputStore>,
) -> Option<Message> {
    truncate_tool_results_in_message_with_budget(
        msg,
        output_store,
        ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: MAX_TOOL_RESULT_CHARS,
            max_line_bytes: MAX_TOOL_RESULT_CHARS,
        },
    )
}

pub fn truncate_tool_results_in_message_with_budget(
    msg: &Message,
    output_store: Option<&OutputStore>,
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
                    output_store,
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

pub fn maybe_truncate_tool_message(msg: &Message, output_store: Option<&OutputStore>) -> Message {
    maybe_truncate_tool_message_with_budget(
        msg,
        output_store,
        ToolOutputBudget {
            max_lines: usize::MAX,
            max_bytes: MAX_TOOL_RESULT_CHARS,
            max_line_bytes: MAX_TOOL_RESULT_CHARS,
        },
    )
}

pub fn maybe_truncate_tool_message_with_budget(
    msg: &Message,
    output_store: Option<&OutputStore>,
    budget: ToolOutputBudget,
) -> Message {
    if !matches!(msg.role, MessageRole::Tool) {
        return msg.clone();
    }
    truncate_tool_results_in_message_with_budget(msg, output_store, budget)
        .unwrap_or_else(|| msg.clone())
}

pub fn spill_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("tool_outputs")
}

#[cfg(test)]
mod output_store_tests {
    use super::*;
    use tempfile::TempDir;

    fn budget() -> ToolOutputBudget {
        ToolOutputBudget {
            max_lines: 2,
            max_bytes: 32,
            max_line_bytes: 16,
        }
    }

    fn id(notice: &str) -> String {
        notice
            .split_once("output_id=")
            .unwrap()
            .1
            .split([',', '.'])
            .next()
            .unwrap()
            .to_string()
    }

    #[test]
    fn opaque_id_hides_path_and_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let store = OutputStore::at(dir.path());
        let notice = truncate_tool_result_content_with_budget(
            "one\ntwo\nthree\n",
            "x",
            Some(&store),
            budget(),
        );
        let output_id = id(&notice);
        assert!(!notice.contains(dir.path().to_string_lossy().as_ref()));
        assert!(!notice.contains("fs.read"));
        let page = OutputStore::at(dir.path())
            .read_lines(&output_id, 1, 1, ToolOutputBudget::default())
            .unwrap();
        assert_eq!(page.content, "two\n");
        assert_eq!(page.next_offset, 2);
    }

    #[test]
    fn ids_are_session_scoped_and_ranges_are_exact() {
        let left = TempDir::new().unwrap();
        let right = TempDir::new().unwrap();
        let store = OutputStore::at(left.path());
        let output_id = store.register("x", "one\ntwo\nthree").unwrap();
        assert!(
            OutputStore::at(right.path())
                .read_bytes(&output_id, 0, 8, ToolOutputBudget::default())
                .is_err()
        );
        let page = store
            .read_bytes(&output_id, 4, 4, ToolOutputBudget::default())
            .unwrap();
        assert_eq!(page.content, "two\n");
        assert_eq!(page.next_offset, 8);
    }

    #[test]
    fn valid_pagination_envelope_is_preserved_only_for_readable_output() {
        let dir = TempDir::new().unwrap();
        let store = OutputStore::at(dir.path());
        let output_id = store.register("x", "完整内容").unwrap();
        let envelope = serde_json::json!({
            "content": "完整",
            "output_id": output_id,
            "total_bytes": "完整内容".len(),
            "next": {"mode": "bytes", "offset": 12, "has_more": false}
        })
        .to_string();
        assert_eq!(
            truncate_tool_result_content_with_budget(&envelope, "x", Some(&store), budget()),
            envelope
        );

        let unrelated_id = store.register("unrelated", "另一个注册输出").unwrap();
        let forged = serde_json::json!({
            "content": "完整",
            "output_id": unrelated_id,
            "total_bytes": "完整内容".len() + 1,
            "next": {"mode": "bytes", "offset": 12, "has_more": false}
        })
        .to_string();
        let truncated =
            truncate_tool_result_content_with_budget(&forged, "x", Some(&store), budget());
        assert!(truncated.contains("Output truncated"));
        assert_ne!(truncated, forged);
    }

    #[test]
    fn repeated_truncation_keeps_one_opaque_output() {
        let dir = TempDir::new().unwrap();
        let store = OutputStore::at(dir.path());
        let original = "x".repeat(100);
        let first =
            truncate_tool_result_content_with_budget(&original, "x", Some(&store), budget());
        let second = truncate_tool_result_content_with_budget(&first, "x", Some(&store), budget());
        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(spill_dir(dir.path())).unwrap().count(), 1);

        let forged = first.replace("total_bytes=100", "total_bytes=101");
        let retruncated =
            truncate_tool_result_content_with_budget(&forged, "x", Some(&store), budget());
        assert_ne!(retruncated, forged);
        assert_eq!(std::fs::read_dir(spill_dir(dir.path())).unwrap().count(), 2);
    }

    fn field<'a>(fields: &'a [(String, Value)], name: &str) -> &'a Value {
        fields
            .iter()
            .find_map(|(field, value)| (field == name).then_some(value))
            .unwrap_or_else(|| panic!("missing {name}"))
    }

    #[tokio::test]
    async fn output_read_tool_returns_exact_byte_and_line_envelopes() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(OutputStore::at(dir.path()));
        let output_id = store.register("x", "one\ntwo\nthree").unwrap();
        let ctx = ToolCtx::new().with_output_store(store);

        let bytes = OutputRead
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![
                        ("output_id".into(), Value::Str(output_id.clone())),
                        ("byte_offset".into(), Value::Int(4)),
                        ("byte_limit".into(), Value::Int(4)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(bytes) = bytes else {
            panic!("expected byte page");
        };
        assert!(matches!(field(&bytes, "content"), Value::Str(value) if value == "two\n"));
        assert!(matches!(field(&bytes, "mode"), Value::Str(value) if value == "bytes"));
        assert!(matches!(field(&bytes, "offset"), Value::Int(4)));
        assert!(matches!(field(&bytes, "next_offset"), Value::Int(8)));
        assert!(matches!(field(&bytes, "has_more"), Value::Bool(true)));

        let lines = OutputRead
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![
                        ("output_id".into(), Value::Str(output_id)),
                        ("line_offset".into(), Value::Int(1)),
                        ("line_limit".into(), Value::Int(1)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(lines) = lines else {
            panic!("expected line page");
        };
        assert!(matches!(field(&lines, "content"), Value::Str(value) if value == "two\n"));
        assert!(matches!(field(&lines, "mode"), Value::Str(value) if value == "lines"));
        assert!(matches!(field(&lines, "offset"), Value::Int(1)));
        assert!(matches!(field(&lines, "next_offset"), Value::Int(2)));
        assert!(matches!(field(&lines, "has_more"), Value::Bool(true)));
    }

    #[tokio::test]
    async fn output_read_tool_rejects_non_progressing_and_mixed_pagination() {
        let dir = TempDir::new().unwrap();
        let store = Arc::new(OutputStore::at(dir.path()));
        let output_id = store.register("x", "content").unwrap();
        let ctx = ToolCtx::new().with_output_store(store);

        for limit in ["line_limit", "byte_limit"] {
            let error = OutputRead
                .call(
                    ToolArgs {
                        positional: vec![],
                        named: vec![
                            ("output_id".into(), Value::Str(output_id.clone())),
                            (limit.into(), Value::Int(0)),
                        ],
                    },
                    &ctx,
                )
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                RuntimeError::TypeMismatch { expected, .. }
                    if expected == format!("positive integer {limit}")
            ));
        }

        let error = OutputRead
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![
                        ("output_id".into(), Value::Str(output_id)),
                        ("line_offset".into(), Value::Int(0)),
                        ("byte_offset".into(), Value::Int(0)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::ToolFailed(message) if message.contains("not both")
        ));
    }

    #[test]
    fn output_read_is_registered_in_tier_zero() {
        let registry = crate::tool::ToolRegistry::new();
        crate::tools::register_tier_zero(&registry);
        let tool = registry.get("output.read").expect("registered output.read");
        assert_eq!(tool.tier(), Tier::Zero);
    }
}
