use crate::approval::authorize_tool_invocation;
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
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
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
            let before_tokens = crate::compaction::estimate_tokens_for_messages(&messages);
            let seq_span = messages
                .get(start..end.min(messages.len()))
                .and_then(|slice| {
                    Some((
                        slice.first().map(|_| start as u64)?,
                        slice.last().map(|_| end.saturating_sub(1) as u64)?,
                    ))
                })
                .unwrap_or((start as u64, end.saturating_sub(1) as u64));
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
            let after_tokens = crate::compaction::estimate_tokens_for_messages(&out);
            if let Some(sink) = &ctx.events {
                sink.mark_compacted();
                sink.emit(crate::event::Event::ContextCompact {
                    session_id: ctx
                        .turn_id
                        .as_ref()
                        .map(|t| t.0.to_string())
                        .unwrap_or_default(),
                    flow_run_id: ctx.message_flow_run_id(),
                    before_tokens,
                    after_tokens,
                    compacted_range_start: seq_span.0,
                    compacted_range_end: seq_span.1,
                    summary_text: None,
                    replacement_msg_seq: None,
                });
            }
            if let Some(tx) = &ctx.lifecycle_fire_tx {
                let _ = tx.send(atman_dsl::ast::LifecycleEvent::ContextCompact);
            }
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

pub struct TextConcat;

impl Tool for TextConcat {
    fn name(&self) -> &str {
        "text_concat"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Flatten the text parts of a Message into a single string.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"description": "A Message value from an llm call."}},
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = match args.named("message") {
                Some(v) => v,
                None => args.positional(0)?,
            };
            match v {
                Value::Message(m) => Ok(Value::Str(m.text_concat())),
                Value::Str(s) => Ok(Value::Str(s.clone())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "message or string".into(),
                    actual: other.kind_name().into(),
                }),
            }
        })
    }
}

pub struct Concat;

impl Tool for Concat {
    fn name(&self) -> &str {
        "concat"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Concatenate two lists into a single new list.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "left": {"type": "array"},
                "right": {"type": "array"}
            },
            "required": ["left", "right"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let left = extract_list(&args, "left", 0)?;
            let right = extract_list(&args, "right", 1)?;
            let mut out = Vec::with_capacity(left.len() + right.len());
            out.extend(left);
            out.extend(right);
            Ok(Value::List(out))
        })
    }
}

pub struct MessageUser;

impl Tool for MessageUser {
    fn name(&self) -> &str {
        "message.user"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Construct a user-role Message from a text string. Use with session.push to inject user instructions into the session history before an llm.call(context: session) call.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = extract_string(&args, "text", 0)?;
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            Ok(Value::Message(crate::message::Message::user_text(
                turn_id, text,
            )))
        })
    }
}

pub struct MessageAssistant;

impl Tool for MessageAssistant {
    fn name(&self) -> &str {
        "message.assistant"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Construct an assistant-role Message from a text string.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = extract_string(&args, "text", 0)?;
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            Ok(Value::Message(crate::message::Message::assistant_text(
                turn_id, text,
            )))
        })
    }
}

pub struct MessageSystem;

impl Tool for MessageSystem {
    fn name(&self) -> &str {
        "message.system"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Construct a system-role Message from a text string.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = extract_string(&args, "text", 0)?;
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            Ok(Value::Message(crate::message::Message::system_text(
                turn_id, text,
            )))
        })
    }
}

pub struct MessageTool;

impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message.tool"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Construct a tool-role Message from a text string. Rarely needed directly — dispatch_all already returns tool-role Messages.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = extract_string(&args, "text", 0)?;
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            Ok(Value::Message(crate::message::Message {
                turn_id,
                role: crate::message::MessageRole::Tool,
                parts: vec![crate::message::MessagePart::Text { text }],
                origin: crate::message::MessageOrigin::User,
            }))
        })
    }
}

pub struct ExtractToolUses;

impl Tool for ExtractToolUses {
    fn name(&self) -> &str {
        "extract_tool_uses"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Pull the tool_use parts out of an assistant Message. Returns a list of \
             {id, name, input, intent?} structs suitable for dispatch_all.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"message": {"description": "Assistant Message value."}},
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let v = match args.named("message") {
                Some(v) => v,
                None => args.positional(0)?,
            };
            let m = match v {
                Value::Message(m) => m,
                Value::Str(_) => return Ok(Value::List(Vec::new())),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message or string".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let mut out = Vec::new();
            for part in &m.parts {
                if let crate::message::MessagePart::ToolUse {
                    id,
                    name,
                    input,
                    intent,
                } = part
                {
                    let mut fields = vec![
                        ("id".into(), Value::Str(id.clone())),
                        ("name".into(), Value::Str(name.clone())),
                        ("input".into(), Value::from_json(input.clone())),
                    ];
                    if let Some(intent) = intent {
                        fields.push(("intent".into(), Value::Str(intent.as_str().into())));
                    }
                    out.push(Value::Struct(fields));
                }
            }
            Ok(Value::List(out))
        })
    }
}

pub struct DispatchAll;

impl Tool for DispatchAll {
    fn name(&self) -> &str {
        "dispatch_all"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Dispatch each tool_use in the list against the current tool registry and \
             return a list of tool_result Message values.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"tool_uses": {"type": "array"}},
            "required": ["tool_uses"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let uses = extract_list(&args, "tool_uses", 0)?;
            let Some(registry) = ctx.registry.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "dispatch_all: no tool registry available on ctx".into(),
                ));
            };
            let prepared = prepare_dispatch(&uses, registry.as_ref(), ctx)?;
            let (auto_batch, serial_batch, mut out_slots) = partition_and_gate(prepared, ctx).await;
            run_auto_parallel(auto_batch, ctx, &mut out_slots).await;
            run_serial(serial_batch, ctx, &mut out_slots).await;
            let out: Vec<Value> = out_slots.into_iter().flatten().collect();
            Ok(Value::List(out))
        })
    }
}

enum PreparedEntry {
    Ready {
        index: usize,
        id: String,
        name: String,
        tool: std::sync::Arc<dyn Tool>,
        call_args: ToolArgs,
        call_intent: Option<crate::message::ToolCallIntent>,
    },
    Failed {
        index: usize,
        msg: crate::message::Message,
    },
}

fn prepare_dispatch(
    uses: &[Value],
    registry: &crate::tool::ToolRegistry,
    ctx: &ToolCtx,
) -> Result<Vec<PreparedEntry>, RuntimeError> {
    let parsed = uses
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let Value::Struct(fields) = entry else {
                return Err(RuntimeError::TypeMismatch {
                    expected: "struct {id, name, input}".into(),
                    actual: entry.kind_name().into(),
                });
            };
            let get = |key: &str| {
                fields
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.clone())
            };
            let id = match get("id") {
                Some(Value::Str(id)) => id,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "dispatch_all: tool_use missing `id` string".into(),
                    ));
                }
            };
            let name = match get("name") {
                Some(Value::Str(name)) => name,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "dispatch_all: tool_use missing `name` string".into(),
                    ));
                }
            };
            let call_intent = match get("intent") {
                Some(Value::Str(value)) => crate::message::ToolCallIntent::new(value),
                _ => None,
            };
            Ok((
                index,
                id,
                name,
                get("input").unwrap_or(Value::Unit),
                call_intent,
            ))
        })
        .collect::<Result<Vec<_>, RuntimeError>>()?;

    let mut prepared = Vec::with_capacity(parsed.len());
    for (index, id, name, input, mut call_intent) in parsed {
        if ctx
            .model_tool_exposures
            .as_ref()
            .is_some_and(|exposures| !exposures.claim(ctx.flow_run_id.as_ref(), &id, &name))
        {
            emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
            prepared.push(PreparedEntry::Failed {
                index,
                msg: build_error_result(
                    ctx,
                    &id,
                    &format!(
                        "dispatch_all: tool `{name}` was not exposed by the LLM request that produced call `{id}`"
                    ),
                ),
            });
            continue;
        }
        let Some(tool) = registry.get(&name) else {
            emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
            prepared.push(PreparedEntry::Failed {
                index,
                msg: build_error_result(ctx, &id, &format!("dispatch_all: unknown tool `{name}`")),
            });
            continue;
        };
        let raw_schema = tool.input_schema();
        let named = match &input {
            Value::Struct(fields) => {
                let mut fields = fields.clone();
                if !crate::tool::tool_schema_uses_call_intent_field(&raw_schema)
                    && let Some(index) = fields
                        .iter()
                        .position(|(name, _)| name == crate::message::TOOL_CALL_INTENT_FIELD)
                {
                    let (_, value) = fields.remove(index);
                    if call_intent.is_none()
                        && let Value::Str(value) = value
                    {
                        call_intent = crate::message::ToolCallIntent::new(value);
                    }
                }
                fields
            }
            Value::Unit => Vec::new(),
            other => {
                emit_tool_node(ctx, &id, &name, &input, call_intent.as_ref());
                prepared.push(PreparedEntry::Failed {
                    index,
                    msg: build_error_result(
                        ctx,
                        &id,
                        &format!(
                            "tool `{name}` expected struct or unit input, got {}",
                            other.kind_name()
                        ),
                    ),
                });
                continue;
            }
        };
        emit_tool_node(
            ctx,
            &id,
            &name,
            &Value::Struct(named.clone()),
            call_intent.as_ref(),
        );
        let missing = missing_required_fields(&raw_schema, &named);
        if !missing.is_empty() {
            let content = format!(
                "tool `{name}` received empty/incomplete input. Missing required fields: {}. Retry with a complete argument object like {{{}}} — do NOT reuse an empty {{}} input.",
                missing.join(", "),
                missing
                    .iter()
                    .map(|f| format!("\"{f}\":\"...\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            prepared.push(PreparedEntry::Failed {
                index,
                msg: build_error_result(ctx, &id, &content),
            });
            continue;
        }
        prepared.push(PreparedEntry::Ready {
            index,
            id,
            name,
            tool,
            call_args: ToolArgs {
                positional: Vec::new(),
                named,
            },
            call_intent,
        });
    }
    Ok(prepared)
}

struct Approved {
    index: usize,
    id: String,
    name: String,
    tool: std::sync::Arc<dyn Tool>,
    call_args: ToolArgs,
    call_ctx: ToolCtx,
}

async fn partition_and_gate(
    prepared: Vec<PreparedEntry>,
    ctx: &ToolCtx,
) -> (Vec<Approved>, Vec<Approved>, Vec<Option<Value>>) {
    let total = prepared.len();
    let mut out_slots: Vec<Option<Value>> = vec![None; total];
    struct ReadyEntry {
        index: usize,
        id: String,
        name: String,
        tool: std::sync::Arc<dyn Tool>,
        call_args: ToolArgs,
        /// Classified once, before the gate. Re-deriving it after approval would
        /// let a level that depends on ctx or args drift between the verdict and
        /// the auto/serial routing, so a call could be gated as one level and run
        /// as another.
        level: crate::tool::ApprovalLevel,
        invocation_ctx: ToolCtx,
    }
    let mut ready: Vec<ReadyEntry> = Vec::new();
    for entry in prepared {
        match entry {
            PreparedEntry::Failed { index, msg } => {
                out_slots[index] = Some(Value::Message(emit_tool_result(ctx, &msg)));
            }
            PreparedEntry::Ready {
                index,
                id,
                name,
                tool,
                call_args,
                call_intent,
            } => {
                let invocation_ctx = ctx
                    .clone()
                    .for_tool_invocation(tool.tier())
                    .with_call_intent(call_intent);
                let level = tool.approval_level(&call_args, &invocation_ctx);
                ready.push(ReadyEntry {
                    index,
                    id,
                    name,
                    tool,
                    call_args,
                    level,
                    invocation_ctx,
                });
            }
        }
    }
    // Parallel gating exposes every pending ordinary request to the UI before
    // execution begins; permission-control calls authenticate without queuing.
    let gates = ready.iter().map(|r| {
        authorize_tool_invocation(
            &r.invocation_ctx,
            &r.id,
            &r.name,
            &r.call_args,
            r.tool.as_ref(),
        )
    });
    let outcomes = futures::future::join_all(gates).await;
    let mut auto_batch = Vec::new();
    let mut serial_batch = Vec::new();
    for (r, outcome) in ready.into_iter().zip(outcomes) {
        match outcome {
            Ok(call_ctx) => {
                let is_control =
                    r.tool.invocation_plane() == crate::tool::InvocationPlane::PermissionControl;
                let a = Approved {
                    index: r.index,
                    id: r.id,
                    name: r.name.clone(),
                    tool: r.tool,
                    call_args: r.call_args,
                    call_ctx,
                };
                if r.level == crate::tool::ApprovalLevel::Auto && !is_control {
                    auto_batch.push(a);
                } else {
                    serial_batch.push(a);
                }
            }
            Err(reason) => {
                let msg =
                    build_error_result(ctx, &r.id, &format!("tool `{}` denied: {reason}", r.name));
                out_slots[r.index] = Some(Value::Message(emit_tool_result(ctx, &msg)));
            }
        }
    }
    (auto_batch, serial_batch, out_slots)
}

async fn run_auto_parallel(batch: Vec<Approved>, ctx: &ToolCtx, out_slots: &mut [Option<Value>]) {
    use futures::StreamExt;

    let mut pending = futures::stream::FuturesUnordered::new();
    for a in batch {
        pending.push(async move {
            let result = a.tool.call(a.call_args, &a.call_ctx).await;
            (a.index, a.id, a.name, result)
        });
    }
    while let Some((index, id, name, result)) = pending.next().await {
        out_slots[index] = Some(finish_dispatch(ctx, &id, &name, result));
    }
}

async fn run_serial(batch: Vec<Approved>, ctx: &ToolCtx, out_slots: &mut [Option<Value>]) {
    for a in batch {
        let result = a.tool.call(a.call_args, &a.call_ctx).await;
        out_slots[a.index] = Some(finish_dispatch(ctx, &a.id, &a.name, result));
    }
}

fn finish_dispatch(ctx: &ToolCtx, id: &str, name: &str, result: ToolResult) -> Value {
    let (content, is_error) = match &result {
        Ok(v) => (render_tool_result_text(v), false),
        Err(e) => (format!("{e}"), true),
    };
    if let Ok(v) = &result {
        emit_diff_preview_if_relevant(ctx, name, v);
    }
    let msg = crate::message::Message {
        role: crate::message::MessageRole::Tool,
        parts: vec![crate::message::MessagePart::ToolResult {
            tool_use_id: id.to_string(),
            content,
            is_error,
        }],
        turn_id: ctx
            .turn_id
            .clone()
            .unwrap_or_else(crate::event::TurnId::now),
        origin: crate::message::MessageOrigin::User,
    };
    Value::Message(emit_tool_result(ctx, &msg))
}

type DiffPreviewData = (String, Option<String>, Option<String>, Option<String>);

fn emit_diff_preview_if_relevant(ctx: &ToolCtx, tool_name: &str, value: &Value) {
    let Some(sink) = ctx.events.as_ref() else {
        return;
    };
    let data: Option<DiffPreviewData> = match tool_name {
        "fs.edit" => {
            let path = value_struct_string(value, "summary").and_then(|s| {
                s.strip_prefix("[fs.edit(")
                    .and_then(|s| s.split(':').next())
                    .map(|s| s.trim_end_matches(')').to_string())
            });
            let diff = value_struct_string(value, "diff");
            diff.map(|d| (path.unwrap_or_default(), None, None, Some(d)))
        }
        "fs.write" => {
            let path = value_struct_string(value, "path").unwrap_or_default();
            let diff = value_struct_string(value, "diff");
            diff.map(|d| (path, None, None, Some(d)))
        }
        "git.diff" => {
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some(("git diff".into(), None, None, Some(diff)))
        }
        "git.show" => {
            let sha = value_struct_string(value, "sha").unwrap_or_default();
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some((format!("git show {sha}"), None, None, Some(diff)))
        }
        "git.log" => {
            let Some(diff) = value_struct_string(value, "diff") else {
                return;
            };
            Some(("git log HEAD".into(), None, None, Some(diff)))
        }
        _ => None,
    };
    if let Some((title, old_content, new_content, unified_diff)) = data {
        sink.emit(crate::event::Event::DiffPreview {
            turn_id: ctx.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            title,
            old_content,
            new_content,
            unified_diff,
        });
    }
}

fn value_struct_string(value: &Value, field: &str) -> Option<String> {
    if let Value::Struct(fields) = value {
        fields
            .iter()
            .find(|(k, _)| k == field)
            .and_then(|(_, v)| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
    } else {
        None
    }
}

fn emit_tool_node(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    input: &Value,
    call_intent: Option<&crate::message::ToolCallIntent>,
) {
    let (Some(run_id), Some(parent_node)) = (&ctx.flow_run_id, &ctx.current_node_id) else {
        return;
    };
    let args_preview = format!("{:?}", input)
        .chars()
        .take(4000)
        .collect::<String>();
    if let Some(sink) = &ctx.events {
        sink.emit(crate::event::Event::ToolNode {
            run_id: run_id.clone(),
            parent_node_id: parent_node.clone(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: args_preview.clone(),
            call_intent: call_intent.cloned(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolNode {
            run_id: run_id.0.to_string(),
            parent_node_id: parent_node.clone(),
            tool_use_id: id.to_string(),
            tool: name.to_string(),
            args_preview,
            call_intent: call_intent.cloned(),
        });
    }
}

fn build_error_result(ctx: &ToolCtx, tool_use_id: &str, content: &str) -> crate::message::Message {
    crate::message::Message {
        role: crate::message::MessageRole::Tool,
        parts: vec![crate::message::MessagePart::ToolResult {
            tool_use_id: tool_use_id.to_string(),
            content: content.to_string(),
            is_error: true,
        }],
        turn_id: ctx
            .turn_id
            .clone()
            .unwrap_or_else(crate::event::TurnId::now),
        origin: crate::message::MessageOrigin::User,
    }
}

fn missing_required_fields(schema: &serde_json::Value, named: &[(String, Value)]) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let have: std::collections::HashSet<&str> = named.iter().map(|(k, _)| k.as_str()).collect();
    required
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|k| !have.contains(k))
        .map(String::from)
        .collect()
}

fn emit_tool_result(ctx: &ToolCtx, msg: &crate::message::Message) -> crate::message::Message {
    let excerpt = crate::tools::tool_output::maybe_truncate_tool_message_with_budget(
        msg,
        ctx.output_store.as_deref(),
        ctx.tool_output_budget,
    );
    emit_tool_result_metrics(ctx, msg, &excerpt);
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
            flow_run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
            message: excerpt.clone(),
        });
    } else if let Some(sink) = &ctx.events {
        sink.emit(crate::event::Event::ToolResultMsg {
            turn_id: excerpt.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            message: excerpt.clone(),
        });
    }
    excerpt
}

fn emit_tool_result_metrics(
    ctx: &ToolCtx,
    raw: &crate::message::Message,
    excerpt: &crate::message::Message,
) {
    let Some(sink) = &ctx.events else {
        return;
    };
    for part in &raw.parts {
        let crate::message::MessagePart::ToolResult {
            tool_use_id,
            content: raw_content,
            ..
        } = part
        else {
            continue;
        };
        let excerpt_content = excerpt.parts.iter().find_map(|part| match part {
            crate::message::MessagePart::ToolResult {
                tool_use_id: excerpt_id,
                content,
                ..
            } if excerpt_id == tool_use_id => Some(content.as_str()),
            _ => None,
        });
        sink.emit(crate::event::Event::ToolResultMetrics {
            turn_id: raw.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            tool_use_id: tool_use_id.clone(),
            raw_bytes: raw_content.len() as u64,
            excerpt_bytes: excerpt_content.map_or(0, |content| content.len() as u64),
            truncated: excerpt_content != Some(raw_content.as_str()),
        });
    }
}

fn render_tool_result_text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Message(m) => m.text_concat(),
        other => other.to_json().to_string(),
    }
}

fn extract_list(args: &ToolArgs, name: &str, pos: usize) -> Result<Vec<Value>, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::List(items) => Ok(items.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "list".into(),
            actual: other.kind_name().into(),
        }),
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
    format!(
        "To: {}
Subject: {subject}
---
{body}",
        to.join(", ")
    )
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

    fn authorized_ctx(registry: std::sync::Arc<crate::tool::ToolRegistry>) -> ToolCtx {
        let trust = crate::trust::TrustConfig {
            mode: crate::trust::TrustMode::Reckless,
            ..crate::trust::TrustConfig::default()
        };
        let flows = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let run_id = crate::event::FlowRunId::now();
        let identity = flows
            .register_root(
                "stdlib-test".into(),
                run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let mut ctx = ToolCtx::new()
            .with_registry(registry)
            .with_flow_registry(std::sync::Arc::clone(&flows))
            .with_permission_broker(crate::permission::PermissionBroker::shared(flows))
            .with_approval(std::sync::Arc::new(crate::session::ApprovalRegistry::new()))
            .with_trust(trust)
            .with_anchors(None, Some(run_id), None);
        ctx.flow_identity = Some(identity);
        ctx
    }

    #[test]
    fn shell_quote_wraps_and_escapes() {
        assert_eq!(shell_quote("hello"), "'hello'");
        assert_eq!(shell_quote("It's fine"), "'It'\\''s fine'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }

    struct PermitProbeTool;

    impl Tool for PermitProbeTool {
        fn name(&self) -> &str {
            "permit.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let authorized = ctx
                    .invocation_authorization()
                    .is_some_and(|permit| permit.is_for_call("probe_id", "permit.probe"));
                Ok(Value::Bool(authorized))
            })
        }
    }

    struct ControlProbeTool;

    impl Tool for ControlProbeTool {
        fn name(&self) -> &str {
            "permission.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn invocation_plane(&self) -> crate::tool::InvocationPlane {
            crate::tool::InvocationPlane::PermissionControl
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    struct UnexposedProbeTool;

    impl Tool for UnexposedProbeTool {
        fn name(&self) -> &str {
            "hidden.probe"
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { panic!("unexposed tool must not execute") })
        }
    }

    #[tokio::test]
    async fn dispatch_all_rejects_calls_outside_their_request_exposure() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(UnexposedProbeTool));
        let mut ctx = authorized_ctx(std::sync::Arc::new(registry));
        let response = crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![crate::message::MessagePart::ToolUse {
                id: "hidden-call".into(),
                name: "hidden.probe".into(),
                input: serde_json::json!({}),
                intent: None,
            }],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        };
        let exposures = crate::tool::ToolExposureRegistry::default();
        exposures.register_response(ctx.flow_run_id.as_ref(), &response, ["allowed.probe"]);
        ctx.model_tool_exposures = Some(exposures);
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("hidden-call".into())),
            ("name".into(), Value::Str("hidden.probe".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list")
        };
        assert!(matches!(
            &results[0],
            Value::Message(crate::message::Message { parts, .. })
                if matches!(
                    &parts[..],
                    [crate::message::MessagePart::ToolResult { content, is_error: true, .. }]
                        if content.contains("was not exposed")
                )
        ));
    }

    #[tokio::test]
    async fn dispatch_all_routes_permission_control_calls_to_serial_without_recursive_requests() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ControlProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let before = ctx.permission_broker.as_ref().unwrap().list().len();
        let uses = vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("control-1".into())),
                ("name".into(), Value::Str("permission.probe".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("control-2".into())),
                ("name".into(), Value::Str("permission.probe".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
        ];
        let prepared = prepare_dispatch(&uses, ctx.registry.as_ref().unwrap(), &ctx).unwrap();
        let (parallel, serial, _) = partition_and_gate(prepared, &ctx).await;

        assert!(parallel.is_empty());
        assert_eq!(serial.len(), 2);
        assert_eq!(ctx.permission_broker.as_ref().unwrap().list().len(), before);
    }

    #[tokio::test]
    async fn auto_parallel_call_receives_its_own_authorization() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PermitProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("probe_id".into())),
            ("name".into(), Value::Str("permit.probe".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Message(message) = &results[0] else {
            panic!("tool result message");
        };
        assert!(message.parts.iter().any(|part| matches!(
            part,
            crate::message::MessagePart::ToolResult { content, is_error: false, .. }
                if content == "true"
        )));
    }

    #[tokio::test]
    async fn dispatch_all_strips_wire_intent_and_isolates_parallel_contexts() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(PermitProbeTool));
        let ctx = authorized_ctx(std::sync::Arc::new(registry));
        let uses = vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("first".into())),
                ("name".into(), Value::Str("permit.probe".into())),
                (
                    "input".into(),
                    Value::Struct(vec![(
                        crate::message::TOOL_CALL_INTENT_FIELD.into(),
                        Value::Str("Inspect first target".into()),
                    )]),
                ),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("second".into())),
                ("name".into(), Value::Str("permit.probe".into())),
                (
                    "input".into(),
                    Value::Struct(vec![(
                        crate::message::TOOL_CALL_INTENT_FIELD.into(),
                        Value::Str("Inspect second target".into()),
                    )]),
                ),
            ]),
        ];
        let prepared = prepare_dispatch(&uses, ctx.registry.as_ref().unwrap(), &ctx).unwrap();
        let (parallel, serial, _) = partition_and_gate(prepared, &ctx).await;

        assert!(serial.is_empty());
        assert_eq!(parallel.len(), 2);
        assert!(parallel.iter().all(|call| {
            call.call_args
                .named(crate::message::TOOL_CALL_INTENT_FIELD)
                .is_none()
        }));
        assert_eq!(
            parallel[0]
                .call_ctx
                .call_intent
                .as_ref()
                .map(|intent| intent.as_str()),
            Some("Inspect first target")
        );
        assert_eq!(
            parallel[1]
                .call_ctx
                .call_intent
                .as_ref()
                .map(|intent| intent.as_str()),
            Some("Inspect second target")
        );
    }

    struct ControlledTool {
        name: &'static str,
        release: std::sync::Arc<tokio::sync::Semaphore>,
    }

    impl Tool for ControlledTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let _permit = self.release.acquire().await.unwrap();
                Ok(Value::Str(self.name.to_string()))
            })
        }
    }

    #[tokio::test]
    async fn dispatch_all_emits_each_scoped_result_as_its_tool_finishes() {
        let fast_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let slow_release = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(ControlledTool {
            name: "fast",
            release: fast_release.clone(),
        }));
        registry.register(std::sync::Arc::new(ControlledTool {
            name: "slow",
            release: slow_release.clone(),
        }));
        let run_id = crate::event::FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(32);
        let ctx = authorized_ctx(std::sync::Arc::new(registry))
            .with_anchors(None, Some(run_id.clone()), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_stream_tx(stream_tx);
        let uses = Value::List(vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("slow_id".into())),
                ("name".into(), Value::Str("slow".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
            Value::Struct(vec![
                ("id".into(), Value::Str("fast_id".into())),
                ("name".into(), Value::Str("fast".into())),
                ("input".into(), Value::Struct(Vec::new())),
            ]),
        ]);
        let task = tokio::spawn(async move {
            DispatchAll
                .call(
                    ToolArgs {
                        positional: vec![uses],
                        named: Vec::new(),
                    },
                    &ctx,
                )
                .await
                .unwrap()
        });

        fast_release.add_permits(1);
        let fast_result = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let crate::stream::StreamFrame::ToolResultMsg {
                    flow_run_id,
                    message,
                } = stream_rx.recv().await.unwrap()
                    && message.parts.iter().any(|part| {
                        matches!(
                            part,
                            crate::message::MessagePart::ToolResult { tool_use_id, .. }
                                if tool_use_id == "fast_id"
                        )
                    })
                {
                    break flow_run_id;
                }
            }
        })
        .await
        .expect("fast result before slow release");
        assert_eq!(fast_result.as_deref(), Some(run_id.0.to_string().as_str()));
        assert!(!task.is_finished());

        slow_release.add_permits(1);
        let Value::List(results) = task.await.unwrap() else {
            panic!("dispatch result list");
        };
        let ids: Vec<&str> = results
            .iter()
            .map(|value| match value {
                Value::Message(message) => match &message.parts[0] {
                    crate::message::MessagePart::ToolResult { tool_use_id, .. } => {
                        tool_use_id.as_str()
                    }
                    _ => panic!("tool result part"),
                },
                _ => panic!("tool result message"),
            })
            .collect();
        assert_eq!(ids, vec!["slow_id", "fast_id"]);
    }

    struct TextTool {
        name: &'static str,
        output: String,
    }

    impl Tool for TextTool {
        fn name(&self) -> &str {
            self.name
        }

        fn tier(&self) -> Tier {
            Tier::Zero
        }

        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            let output = self.output.clone();
            Box::pin(async move { Ok(Value::Str(output)) })
        }
    }

    #[tokio::test]
    async fn dispatch_all_preserves_fs_read_pagination_for_full_utf8_reassembly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.txt");
        let expected = "界".repeat(349_525) + "a";
        assert_eq!(expected.len(), 1_048_576);
        tokio::fs::write(&path, &expected).await.unwrap();

        let registry = crate::tool::ToolRegistry::new();
        crate::tools::register_tier_zero(&registry);
        let ctx = authorized_ctx(std::sync::Arc::new(registry))
            .with_session_dir(dir.path().to_path_buf())
            .with_workspace(crate::git_workspace::WorkspaceBinding {
                workspace_id: "stdlib-test".into(),
                repository_root: dir.path().to_path_buf(),
                path: dir.path().to_path_buf(),
                branch: None,
            });
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("read_id".into())),
            ("name".into(), Value::Str("fs.read".into())),
            (
                "input".into(),
                Value::Struct(vec![("path".into(), Value::Path(path))]),
            ),
        ])]);
        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Message(message) = &results[0] else {
            panic!("tool result message");
        };
        let crate::message::MessagePart::ToolResult { content, .. } = &message.parts[0] else {
            panic!("tool result part");
        };
        let envelope: serde_json::Value = serde_json::from_str(content).unwrap();
        let output_id = envelope["output_id"].as_str().unwrap();
        let mut reconstructed = envelope["content"].as_str().unwrap().to_string();
        let mut offset = envelope["next"]["offset"].as_u64().unwrap() as usize;
        while envelope["next"]["has_more"].as_bool().unwrap() || offset < expected.len() {
            let page = ctx
                .output_store
                .as_ref()
                .unwrap()
                .read_bytes(output_id, offset, usize::MAX, ctx.tool_output_budget)
                .unwrap();
            reconstructed.push_str(&page.content);
            if !page.has_more {
                break;
            }
            assert!(page.next_offset > offset);
            offset = page.next_offset;
        }
        assert_eq!(reconstructed.len(), 1_048_576);
        assert_eq!(reconstructed, expected);
    }

    #[tokio::test]
    async fn dispatch_all_returns_the_same_truncated_message_it_emits() {
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(TextTool {
            name: "text",
            output: "0123456789".repeat(20),
        }));
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new()
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx)
            .with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("text_id".into())),
            ("name".into(), Value::Str("text".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);
        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Message(returned) = &results[0] else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };
        assert_eq!(returned, &emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        let returned_bytes = match &returned.parts[0] {
            crate::message::MessagePart::ToolResult { content, .. } => content.len() as u64,
            _ => unreachable!(),
        };
        let metrics = events
            .snapshot()
            .into_iter()
            .find_map(|event| match event {
                crate::event::Event::ToolResultMetrics {
                    tool_use_id,
                    raw_bytes,
                    excerpt_bytes,
                    truncated,
                    ..
                } if tool_use_id == "text_id" => Some((raw_bytes, excerpt_bytes, truncated)),
                _ => None,
            })
            .expect("tool result metrics");
        assert!(metrics.0 > 0);
        assert_eq!((metrics.1, metrics.2), (returned_bytes, true));
    }

    #[test]
    fn finish_dispatch_returns_the_budgeted_message_it_emits() {
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new()
            .with_stream_tx(stream_tx)
            .with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 24,
            max_line_bytes: 24,
        };
        let Value::Message(returned) = finish_dispatch(
            &ctx,
            "text_id",
            "text",
            Ok(Value::Str("0123456789".repeat(20))),
        ) else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        let excerpt_bytes = match &returned.parts[0] {
            crate::message::MessagePart::ToolResult { content, .. } => content.len() as u64,
            _ => unreachable!(),
        };
        assert!(events.snapshot().into_iter().any(|event| matches!(
            event,
            crate::event::Event::ToolResultMetrics {
                tool_use_id,
                raw_bytes: 200,
                excerpt_bytes: observed,
                truncated: true,
                ..
            } if tool_use_id == "text_id" && observed == excerpt_bytes
        )));
    }

    #[test]
    fn finish_dispatch_preserves_error_flag_and_message_consistency() {
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new().with_stream_tx(stream_tx);
        let Value::Message(returned) = finish_dispatch(
            &ctx,
            "error_id",
            "failing",
            Err(RuntimeError::ToolFailed("expected failure".into())),
        ) else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            } if content.contains("expected failure")
        ));
    }

    #[test]
    fn finish_dispatch_emits_full_diff_preview_before_result_truncation() {
        let events = crate::event::EventSink::new();
        let mut ctx = ToolCtx::new().with_events(events.clone());
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 1,
            max_bytes: 16,
            max_line_bytes: 16,
        };
        let diff = "-old\n+new\n".repeat(20);
        let Value::Message(returned) = finish_dispatch(
            &ctx,
            "edit_id",
            "fs.edit",
            Ok(Value::Struct(vec![
                (
                    "summary".into(),
                    Value::Str("[fs.edit(example.txt): updated]".into()),
                ),
                ("diff".into(), Value::Str(diff.clone())),
            ])),
        ) else {
            panic!("tool result message");
        };

        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult { content, .. }
                if content.contains("Output truncated")
        ));
        assert!(events.snapshot().iter().any(|event| matches!(
            event,
            crate::event::Event::DiffPreview {
                unified_diff: Some(preview),
                ..
            } if preview == &diff
        )));
    }

    #[tokio::test]
    async fn dispatch_all_returns_the_same_truncated_preflight_error_it_emits() {
        let registry = crate::tool::ToolRegistry::new();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let mut ctx = ToolCtx::new()
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx);
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 32,
            max_bytes: 32,
            max_line_bytes: 32,
        };
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("unknown_id".into())),
            ("name".into(), Value::Str("missing".repeat(40))),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        let Value::List(results) = DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap()
        else {
            panic!("dispatch result list");
        };
        let Value::Message(returned) = &results[0] else {
            panic!("tool result message");
        };
        let crate::stream::StreamFrame::ToolResultMsg {
            message: emitted, ..
        } = stream_rx.try_recv().unwrap()
        else {
            panic!("tool result stream frame");
        };

        assert_eq!(returned, &emitted);
        assert!(matches!(
            &returned.parts[0],
            crate::message::MessagePart::ToolResult {
                content,
                is_error: true,
                ..
            } if content.contains("Output truncated")
        ));
    }

    #[tokio::test]
    async fn dispatch_all_unknown_tool_finishes_its_workflow_node_with_error() {
        use crate::workflow::{NodeStatus, WorkflowGraph};

        let registry = crate::tool::ToolRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let run = run_id.0.to_string();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(None, Some(run_id), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_registry(std::sync::Arc::new(registry))
            .with_stream_tx(stream_tx);
        let uses = Value::List(vec![Value::Struct(vec![
            ("id".into(), Value::Str("unknown_id".into())),
            ("name".into(), Value::Str("missing.tool".into())),
            ("input".into(), Value::Struct(Vec::new())),
        ])]);

        DispatchAll
            .call(
                ToolArgs {
                    positional: vec![uses],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();

        let mut graph = WorkflowGraph::new(crate::event::TurnId::now());
        graph.apply_stream_frame(&crate::stream::StreamFrame::FlowStart {
            run_id: run.clone(),
            flow_name: "agent_loop".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        graph.apply_stream_frame(&crate::stream::StreamFrame::FlowNodeStart {
            run_id: run.clone(),
            node_id: "dispatch_all".into(),
            kind: crate::nodegraph::NodeKind::ToolCall {
                path: "dispatch_all".into(),
            },
            label: "dispatch_all".into(),
            parent_node_id: None,
        });
        while let Ok(frame) = stream_rx.try_recv() {
            graph.apply_stream_frame(&frame);
        }

        let node = graph
            .find_node(&format!("tool:{run}:unknown_id"))
            .expect("unknown tool node");
        assert_eq!(node.status, NodeStatus::Err);
        assert!(matches!(
            &node.kind,
            crate::workflow::WorkflowNodeKind::ToolCall {
                result_preview: Some(preview),
                ..
            } if preview.contains("unknown tool")
        ));
        let result = node.output_preview.as_deref().unwrap();
        assert!(result.contains("unknown tool"));
        assert!(
            graph
                .root
                .iter()
                .flat_map(|root| &root.children)
                .all(|child| {
                    !matches!(
                        &child.kind,
                        crate::workflow::WorkflowNodeKind::ToolCall { tool_use_id, .. }
                            if tool_use_id == "unknown_id"
                    )
                })
        );
    }

    #[test]
    fn prepare_dispatch_does_not_emit_partial_nodes_for_malformed_batch() {
        let registry = crate::tool::ToolRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(None, Some(run_id), None)
            .with_current_node(Some("dispatch_all".into()))
            .with_stream_tx(stream_tx);
        let uses = vec![
            Value::Struct(vec![
                ("id".into(), Value::Str("valid_id".into())),
                ("name".into(), Value::Str("missing.tool".into())),
            ]),
            Value::Struct(vec![("name".into(), Value::Str("missing.tool".into()))]),
        ];

        assert!(prepare_dispatch(&uses, &registry, &ctx).is_err());
        assert!(matches!(
            stream_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
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
