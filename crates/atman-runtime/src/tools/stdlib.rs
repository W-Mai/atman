use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct ShellQuote;

impl Tool for ShellQuote {
    fn name(&self) -> &str {
        "shell_quote"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let s = extract_string(&args, "s", 0)?;
            Ok(Value::Str(shell_quote(&s)))
        })
    }
}

pub fn shell_quote(s: &str) -> String {
    // POSIX-safe: wrap in single quotes, escape any internal ' as '\''.
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

pub struct Len;

impl Tool for Len {
    fn name(&self) -> &str {
        "len"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            match v {
                Value::List(items) => Ok(Value::Int(items.len() as i64)),
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list or string".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct Head;

impl Tool for Head {
    fn name(&self) -> &str {
        "head"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            match args.positional(0)? {
                Value::List(items) => items
                    .first()
                    .cloned()
                    .ok_or_else(|| RuntimeError::ToolFailed("head: empty list".into())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct Tail;

impl Tool for Tail {
    fn name(&self) -> &str {
        "tail"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            match args.positional(0)? {
                Value::List(items) if !items.is_empty() => Ok(Value::List(items[1..].to_vec())),
                Value::List(_) => Err(RuntimeError::ToolFailed("tail: empty list".into())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct IsEmpty;

impl Tool for IsEmpty {
    fn name(&self) -> &str {
        "is_empty"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            match v {
                Value::List(items) => Ok(Value::Bool(items.is_empty())),
                Value::Str(s) => Ok(Value::Bool(s.is_empty())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list or string".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct EstimateTokens;

impl Tool for EstimateTokens {
    fn name(&self) -> &str {
        "estimate_tokens"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            match v {
                Value::List(items) => {
                    let mut msgs = Vec::with_capacity(items.len());
                    for it in items {
                        match it {
                            Value::Message(m) => msgs.push(m.clone()),
                            other => {
                                return Err(RuntimeError::TypeMismatch {
                                    expected: "list of message".into(),
                                    actual: other.kind_name().into(),
                                });
                            }
                        }
                    }
                    let n = crate::compaction::estimate_tokens_for_messages(&msgs);
                    Ok(Value::Int(n as i64))
                }
                Value::Message(m) => Ok(Value::Int(
                    crate::compaction::estimate_tokens_for_message(m) as i64,
                )),
                Value::Str(s) => {
                    let approx = ((s.len() as f64) / 3.5).ceil() as i64;
                    Ok(Value::Int(approx))
                }
                other => Err(RuntimeError::TypeMismatch {
                    expected: "message | list of message | string".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct FindCompactRange;

impl Tool for FindCompactRange {
    fn name(&self) -> &str {
        "find_compact_range"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let messages = extract_message_list(&args, "messages", 0)?;
            let budget = extract_int(&args, "budget", 1)? as u64;
            match crate::compaction::find_compact_range(&messages, budget) {
                Some(range) => Ok(Value::Struct(vec![
                    ("start".into(), Value::Int(range.start as i64)),
                    ("end".into(), Value::Int(range.end as i64)),
                    (
                        "tokens_saved".into(),
                        Value::Int(range.tokens_saved_estimate as i64),
                    ),
                    ("found".into(), Value::Bool(true)),
                ])),
                None => Ok(Value::Struct(vec![
                    ("start".into(), Value::Int(0)),
                    ("end".into(), Value::Int(0)),
                    ("tokens_saved".into(), Value::Int(0)),
                    ("found".into(), Value::Bool(false)),
                ])),
            }
        })
    }
}

pub struct ReplaceMessagesRange;

impl Tool for ReplaceMessagesRange {
    fn name(&self) -> &str {
        "replace_messages_range"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let messages = extract_message_list(&args, "messages", 0)?;
            let start = extract_int(&args, "start", 1)? as usize;
            let end = extract_int(&args, "end", 2)? as usize;
            let summary = extract_string_arg(&args, "summary", 3)?;
            if start > end || end > messages.len() {
                return Err(RuntimeError::ToolFailed(format!(
                    "replace_messages_range: invalid range start={start} end={end} len={}",
                    messages.len()
                )));
            }
            let range = crate::compaction::CompactRange {
                start,
                end,
                tokens_saved_estimate: 0,
            };
            let turn_id = messages
                .first()
                .map(|m| m.turn_id.clone())
                .unwrap_or_else(crate::event::TurnId::now);
            let out =
                crate::compaction::replace_range_with_summary(&messages, &range, summary, turn_id);
            let list: Vec<Value> = out.into_iter().map(Value::Message).collect();
            Ok(Value::List(list))
        })
    }
}

fn extract_message_list(
    args: &ToolArgs,
    name: &str,
    pos: usize,
) -> Result<Vec<crate::message::Message>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Message(m) => out.push(m.clone()),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "list of message".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        other => Err(RuntimeError::TypeMismatch {
            expected: "list of message".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_int(args: &ToolArgs, name: &str, pos: usize) -> Result<i64, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Int(n) => Ok(*n),
        other => Err(RuntimeError::TypeMismatch {
            expected: "int".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_string_arg(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

pub struct RenderPromptXml;
pub struct RenderPromptMarkdown;
pub struct RenderPromptTerse;

fn extract_prompt_spec(v: &Value) -> Result<PromptSpec<'_>, RuntimeError> {
    let Value::Struct(fields) = v else {
        return Err(RuntimeError::TypeMismatch {
            expected: "struct { role?, context?, task, examples?, schema? }".into(),
            actual: v.kind_name().into(),
        });
    };
    let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v);
    let task = match get("task") {
        Some(Value::Str(s)) => s.clone(),
        Some(other) => {
            return Err(RuntimeError::TypeMismatch {
                expected: "string (task)".into(),
                actual: other.kind_name().into(),
            });
        }
        None => return Err(RuntimeError::MissingArg("prompt.task".into())),
    };
    let role = match get("role") {
        Some(Value::Str(s)) => Some(s.clone()),
        Some(Value::Unit) | None => None,
        Some(other) => {
            return Err(RuntimeError::TypeMismatch {
                expected: "string (role)".into(),
                actual: other.kind_name().into(),
            });
        }
    };
    let context = get("context");
    let schema = match get("schema") {
        Some(Value::Str(s)) => Some(s.clone()),
        _ => None,
    };
    let examples = match get("examples") {
        Some(Value::List(items)) => items.iter().collect(),
        _ => Vec::new(),
    };
    Ok(PromptSpec {
        role,
        context,
        task,
        examples,
        schema,
    })
}

struct PromptSpec<'a> {
    role: Option<String>,
    context: Option<&'a Value>,
    task: String,
    examples: Vec<&'a Value>,
    schema: Option<String>,
}

fn json_str(v: &Value) -> String {
    serde_json::to_string_pretty(&v.to_json()).unwrap_or_default()
}

fn render_xml(spec: &PromptSpec<'_>) -> String {
    let mut out = String::new();
    if let Some(role) = &spec.role {
        out.push_str(&format!("<role>{}</role>\n", role));
    }
    if let Some(ctx) = spec.context {
        out.push_str(&format!("<context>\n{}\n</context>\n", json_str(ctx)));
    }
    if !spec.examples.is_empty() {
        out.push_str("<examples>\n");
        for (i, ex) in spec.examples.iter().enumerate() {
            out.push_str(&format!(
                "  <example n=\"{}\">\n{}\n  </example>\n",
                i + 1,
                json_str(ex)
            ));
        }
        out.push_str("</examples>\n");
    }
    out.push_str(&format!("<task>{}</task>\n", spec.task));
    if let Some(schema) = &spec.schema {
        out.push_str(&format!("<schema>{}</schema>\n", schema));
    }
    out
}

fn render_markdown(spec: &PromptSpec<'_>) -> String {
    let mut out = String::new();
    if let Some(role) = &spec.role {
        out.push_str(&format!("# Role\n{}\n\n", role));
    }
    if let Some(ctx) = spec.context {
        out.push_str(&format!("# Context\n```json\n{}\n```\n\n", json_str(ctx)));
    }
    if !spec.examples.is_empty() {
        out.push_str("# Examples\n");
        for (i, ex) in spec.examples.iter().enumerate() {
            out.push_str(&format!(
                "{}. `{}`\n",
                i + 1,
                json_str(ex).replace('\n', " ")
            ));
        }
        out.push('\n');
    }
    out.push_str(&format!("# Task\n{}\n", spec.task));
    if let Some(schema) = &spec.schema {
        out.push_str(&format!("\n# Schema\n{}\n", schema));
    }
    out
}

fn render_terse(spec: &PromptSpec<'_>) -> String {
    let mut out = String::new();
    if let Some(role) = &spec.role {
        out.push_str(&format!("Role: {}\n", role));
    }
    if let Some(ctx) = spec.context {
        out.push_str(&format!("Context: {}\n", json_str(ctx).replace('\n', " ")));
    }
    out.push_str(&format!("Task: {}\n", spec.task));
    if let Some(schema) = &spec.schema {
        out.push_str(&format!("Schema: {}\n", schema));
    }
    for (i, ex) in spec.examples.iter().enumerate() {
        out.push_str(&format!(
            "Example {}: {}\n",
            i + 1,
            json_str(ex).replace('\n', " ")
        ));
    }
    out
}

impl Tool for RenderPromptXml {
    fn name(&self) -> &str {
        "render_prompt_xml"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            let spec = extract_prompt_spec(v)?;
            Ok(Value::Str(render_xml(&spec)))
        })
    }
}

impl Tool for RenderPromptMarkdown {
    fn name(&self) -> &str {
        "render_prompt_markdown"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            let spec = extract_prompt_spec(v)?;
            Ok(Value::Str(render_markdown(&spec)))
        })
    }
}

impl Tool for RenderPromptTerse {
    fn name(&self) -> &str {
        "render_prompt_terse"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?;
            let spec = extract_prompt_spec(v)?;
            Ok(Value::Str(render_terse(&spec)))
        })
    }
}

pub struct ToJsonString;

impl Tool for ToJsonString {
    fn name(&self) -> &str {
        "to_json_string"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = args.positional(0)?.clone();
            let json = v.to_json();
            let s = serde_json::to_string_pretty(&json)
                .map_err(|e| RuntimeError::ToolFailed(format!("to_json_string: {e}")))?;
            Ok(Value::Str(s))
        })
    }
}

pub struct ComposeEmailPreview;

impl Tool for ComposeEmailPreview {
    fn name(&self) -> &str {
        "compose_email_preview"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let subject = extract_string(&args, "subject", 0)?;
            let body = extract_string(&args, "body", 1)?;
            let to = extract_string_list(&args, "to", 2)?;
            Ok(Value::Str(compose_email_preview(&subject, &body, &to)))
        })
    }
}

pub fn compose_email_preview(subject: &str, body: &str, to: &[String]) -> String {
    format!("To: {}\nSubject: {subject}\n---\n{body}", to.join(", "))
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_string_list(
    args: &ToolArgs,
    name: &str,
    pos: usize,
) -> Result<Vec<String>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::List(items) => items
            .iter()
            .map(|v| match v {
                Value::Str(s) => Ok(s.clone()),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "list of string".into(),
                    actual: other.kind_name().into(),
                }),
            })
            .collect(),
        other => Err(RuntimeError::TypeMismatch {
            expected: "list".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("It's fine"), "'It'\\''s fine'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }

    #[test]
    fn compose_email_preview_formats_headers() {
        let preview = compose_email_preview(
            "Deploy status",
            "See attached",
            &["a@x.com".into(), "b@x.com".into()],
        );
        assert_eq!(
            preview,
            "To: a@x.com, b@x.com\nSubject: Deploy status\n---\nSee attached"
        );
    }
}
