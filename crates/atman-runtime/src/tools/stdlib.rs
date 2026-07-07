use crate::approval::{ApprovalOutcome, request_approval};
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
            let footer = format!(
                "\n\n[atman:compact seq_start={} seq_end={} count={}]",
                seq_span.0,
                seq_span.1,
                end - start
            );
            let annotated = format!("{summary}{footer}");
            let out = crate::compaction::replace_range_with_summary(
                &messages, &range, annotated, turn_id,
            );
            let after_tokens = crate::compaction::estimate_tokens_for_messages(&out);
            if let Some(sink) = &ctx.events {
                sink.mark_compacted();
                sink.emit(crate::event::Event::ContextCompact {
                    seq: 0,
                    session_id: ctx
                        .turn_id
                        .as_ref()
                        .map(|t| t.0.to_string())
                        .unwrap_or_default(),
                    before_tokens,
                    after_tokens,
                    compacted_range_start: seq_span.0,
                    compacted_range_end: seq_span.1,
                    ts: chrono::Utc::now(),
                });
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
             {id, name, input} structs suitable for dispatch_all.",
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
                if let crate::message::MessagePart::ToolUse { id, name, input } = part {
                    out.push(Value::Struct(vec![
                        ("id".into(), Value::Str(id.clone())),
                        ("name".into(), Value::Str(name.clone())),
                        ("input".into(), Value::from_json(input.clone())),
                    ]));
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
    let mut prepared = Vec::with_capacity(uses.len());
    for (index, entry) in uses.iter().enumerate() {
        let Value::Struct(fields) = entry else {
            return Err(RuntimeError::TypeMismatch {
                expected: "struct {id, name, input}".into(),
                actual: entry.kind_name().into(),
            });
        };
        let get = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let id = match get("id") {
            Some(Value::Str(s)) => s,
            _ => {
                return Err(RuntimeError::ToolFailed(
                    "dispatch_all: tool_use missing `id` string".into(),
                ));
            }
        };
        let name = match get("name") {
            Some(Value::Str(s)) => s,
            _ => {
                return Err(RuntimeError::ToolFailed(
                    "dispatch_all: tool_use missing `name` string".into(),
                ));
            }
        };
        let input = get("input").unwrap_or(Value::Unit);
        let Some(tool) = registry.get(&name) else {
            prepared.push(PreparedEntry::Failed {
                index,
                msg: build_error_result(ctx, &id, &format!("dispatch_all: unknown tool `{name}`")),
            });
            continue;
        };
        let named = match &input {
            Value::Struct(fields) => fields.clone(),
            Value::Unit => Vec::new(),
            other => {
                return Err(RuntimeError::TypeMismatch {
                    expected: "struct or unit for tool input".into(),
                    actual: other.kind_name().into(),
                });
            }
        };
        let missing = missing_required_fields(&tool.input_schema(), &named);
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
        emit_tool_node(ctx, &id, &name, &input);
        prepared.push(PreparedEntry::Ready {
            index,
            id,
            name,
            tool,
            call_args: ToolArgs {
                positional: Vec::new(),
                named,
            },
        });
    }
    Ok(prepared)
}

struct Approved {
    index: usize,
    id: String,
    tool: std::sync::Arc<dyn Tool>,
    call_args: ToolArgs,
}

async fn partition_and_gate(
    prepared: Vec<PreparedEntry>,
    ctx: &ToolCtx,
) -> (Vec<Approved>, Vec<Approved>, Vec<Option<Value>>) {
    let total = prepared.len();
    let mut out_slots: Vec<Option<Value>> = vec![None; total];
    let mut auto_batch = Vec::new();
    let mut serial_batch = Vec::new();
    for entry in prepared {
        match entry {
            PreparedEntry::Failed { index, msg } => {
                emit_tool_result(ctx, &msg);
                out_slots[index] = Some(Value::Message(msg));
            }
            PreparedEntry::Ready {
                index,
                id,
                name,
                tool,
                call_args,
            } => {
                let level = tool.approval_level();
                let approved =
                    request_approval(ctx, &id, &name, &call_args, level, Some(tool.as_ref())).await;
                match approved {
                    ApprovalOutcome::Approve => {
                        let a = Approved {
                            index,
                            id,
                            tool,
                            call_args,
                        };
                        if level == crate::tool::ApprovalLevel::Auto {
                            auto_batch.push(a);
                        } else {
                            serial_batch.push(a);
                        }
                    }
                    ApprovalOutcome::Deny { reason } => {
                        let msg = build_error_result(
                            ctx,
                            &id,
                            &format!("tool `{name}` denied by user: {reason}"),
                        );
                        emit_tool_result(ctx, &msg);
                        out_slots[index] = Some(Value::Message(msg));
                    }
                }
            }
        }
    }
    (auto_batch, serial_batch, out_slots)
}

async fn run_auto_parallel(batch: Vec<Approved>, ctx: &ToolCtx, out_slots: &mut [Option<Value>]) {
    if batch.is_empty() {
        return;
    }
    let futs = batch.iter().map(|a| a.tool.call(a.call_args.clone(), ctx));
    let results = futures::future::join_all(futs).await;
    for (a, r) in batch.into_iter().zip(results) {
        let (content, is_error) = match r {
            Ok(v) => (render_tool_result_text(&v), false),
            Err(e) => (format!("{e}"), true),
        };
        let msg = crate::message::Message {
            role: crate::message::MessageRole::Tool,
            parts: vec![crate::message::MessagePart::ToolResult {
                tool_use_id: a.id.clone(),
                content,
                is_error,
            }],
            turn_id: ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now),
        };
        emit_tool_result(ctx, &msg);
        out_slots[a.index] = Some(Value::Message(msg));
    }
}

async fn run_serial(batch: Vec<Approved>, ctx: &ToolCtx, out_slots: &mut [Option<Value>]) {
    for a in batch {
        let (content, is_error) = match a.tool.call(a.call_args, ctx).await {
            Ok(v) => (render_tool_result_text(&v), false),
            Err(e) => (format!("{e}"), true),
        };
        let msg = crate::message::Message {
            role: crate::message::MessageRole::Tool,
            parts: vec![crate::message::MessagePart::ToolResult {
                tool_use_id: a.id.clone(),
                content,
                is_error,
            }],
            turn_id: ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now),
        };
        emit_tool_result(ctx, &msg);
        out_slots[a.index] = Some(Value::Message(msg));
    }
}

fn emit_tool_node(ctx: &ToolCtx, id: &str, name: &str, input: &Value) {
    if let (Some(sink), Some(run_id), Some(parent_node)) = (
        ctx.events.as_ref(),
        ctx.flow_run_id.clone(),
        &ctx.current_node_id,
    ) {
        let args_preview = format!("{:?}", input)
            .chars()
            .take(4000)
            .collect::<String>();
        sink.emit(crate::event::Event::ToolNode {
            seq: 0,
            run_id: run_id.clone(),
            parent_node_id: parent_node.clone(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: args_preview.clone(),
            ts: chrono::Utc::now(),
        });
        if let Some(tx) = &ctx.stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::ToolNode {
                run_id: run_id.0.to_string(),
                parent_node_id: parent_node.clone(),
                tool_use_id: id.to_string(),
                tool: name.to_string(),
                args_preview,
            });
        }
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

fn emit_tool_result(ctx: &ToolCtx, msg: &crate::message::Message) {
    let Some(sink) = &ctx.events else {
        return;
    };
    sink.emit(crate::event::Event::ToolResultMsg {
        seq: 0,
        turn_id: msg.turn_id.clone(),
        flow_run_id: ctx.flow_run_id.clone(),
        message: msg.clone(),
        ts: chrono::Utc::now(),
    });
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
            flow_run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
            message: msg.clone(),
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

async fn call_named_unary(
    ctx: &ToolCtx,
    fn_name: &str,
    element: Value,
) -> Result<Value, RuntimeError> {
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(format!(
            "list combinator: no tool registry available to resolve `{fn_name}`"
        )));
    };
    let Some(tool) = registry.get(fn_name) else {
        return Err(RuntimeError::UndefinedTool(fn_name.to_string()));
    };
    let args = ToolArgs {
        positional: vec![element],
        named: Vec::new(),
    };
    tool.call(args, ctx).await
}

async fn call_named_binary(
    ctx: &ToolCtx,
    fn_name: &str,
    a: Value,
    b: Value,
) -> Result<Value, RuntimeError> {
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(format!(
            "list combinator: no tool registry available to resolve `{fn_name}`"
        )));
    };
    let Some(tool) = registry.get(fn_name) else {
        return Err(RuntimeError::UndefinedTool(fn_name.to_string()));
    };
    let args = ToolArgs {
        positional: vec![a, b],
        named: Vec::new(),
    };
    tool.call(args, ctx).await
}

fn value_as_bool(v: Value, fn_name: &str) -> Result<bool, RuntimeError> {
    match v {
        Value::Bool(b) => Ok(b),
        other => Err(RuntimeError::TypeMismatch {
            expected: format!("bool returned by `{fn_name}`"),
            actual: other.kind_name().into(),
        }),
    }
}

pub struct ListMap;

impl Tool for ListMap {
    fn name(&self) -> &str {
        "list_map"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Apply a named tool to each item in a list; returns the transformed list.")
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(call_named_unary(ctx, &fn_name, it).await?);
            }
            Ok(Value::List(out))
        })
    }
}

pub struct ListFilter;

impl Tool for ListFilter {
    fn name(&self) -> &str {
        "list_filter"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Keep items where the named predicate tool returns true.")
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            let mut out = Vec::new();
            for it in items {
                let keep =
                    value_as_bool(call_named_unary(ctx, &fn_name, it.clone()).await?, &fn_name)?;
                if keep {
                    out.push(it);
                }
            }
            Ok(Value::List(out))
        })
    }
}

pub struct ListFind;

impl Tool for ListFind {
    fn name(&self) -> &str {
        "list_find"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Return the first item where the named predicate tool returns true, else unit.")
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            for it in items {
                let hit =
                    value_as_bool(call_named_unary(ctx, &fn_name, it.clone()).await?, &fn_name)?;
                if hit {
                    return Ok(it);
                }
            }
            Ok(Value::Unit)
        })
    }
}

pub struct ListAny;

impl Tool for ListAny {
    fn name(&self) -> &str {
        "list_any"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("True if the named predicate tool returns true for any item.")
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            for it in items {
                let hit = value_as_bool(call_named_unary(ctx, &fn_name, it).await?, &fn_name)?;
                if hit {
                    return Ok(Value::Bool(true));
                }
            }
            Ok(Value::Bool(false))
        })
    }
}

pub struct ListAll;

impl Tool for ListAll {
    fn name(&self) -> &str {
        "list_all"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("True if the named predicate tool returns true for every item.")
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            for it in items {
                let hit = value_as_bool(call_named_unary(ctx, &fn_name, it).await?, &fn_name)?;
                if !hit {
                    return Ok(Value::Bool(false));
                }
            }
            Ok(Value::Bool(true))
        })
    }
}

pub struct ListReduce;

impl Tool for ListReduce {
    fn name(&self) -> &str {
        "list_reduce"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Fold a list left-to-right using a named binary tool: fn(acc, elem) -> acc'. \
             Takes an initial accumulator value.",
        )
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = extract_list(&args, "list", 0)?;
            let fn_name = extract_string(&args, "fn_name", 1)?;
            let init = match args.named("init") {
                Some(v) => v.clone(),
                None => args.positional(2)?.clone(),
            };
            let mut acc = init;
            for it in items {
                acc = call_named_binary(ctx, &fn_name, acc, it).await?;
            }
            Ok(acc)
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

    use crate::tool::ToolRegistry;
    use std::sync::Arc;

    struct IsBig;
    impl Tool for IsBig {
        fn name(&self) -> &str {
            "is_big"
        }
        fn tier(&self) -> Tier {
            Tier::Zero
        }
        fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                match args.positional(0)? {
                    Value::Int(n) => Ok(Value::Bool(*n > 10)),
                    other => Err(RuntimeError::TypeMismatch {
                        expected: "int".into(),
                        actual: other.kind_name().into(),
                    }),
                }
            })
        }
    }

    struct Double;
    impl Tool for Double {
        fn name(&self) -> &str {
            "double"
        }
        fn tier(&self) -> Tier {
            Tier::Zero
        }
        fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                match args.positional(0)? {
                    Value::Int(n) => Ok(Value::Int(n * 2)),
                    other => Err(RuntimeError::TypeMismatch {
                        expected: "int".into(),
                        actual: other.kind_name().into(),
                    }),
                }
            })
        }
    }

    struct AddInts;
    impl Tool for AddInts {
        fn name(&self) -> &str {
            "add_ints"
        }
        fn tier(&self) -> Tier {
            Tier::Zero
        }
        fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async move {
                let a = match args.positional(0)? {
                    Value::Int(n) => *n,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "int".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                };
                let b = match args.positional(1)? {
                    Value::Int(n) => *n,
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "int".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                };
                Ok(Value::Int(a + b))
            })
        }
    }

    fn combinator_ctx() -> ToolCtx {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(IsBig));
        reg.register(Arc::new(Double));
        reg.register(Arc::new(AddInts));
        ToolCtx::new().with_registry(Arc::new(reg))
    }

    fn call_args(items: Vec<Value>, fn_name: &str) -> ToolArgs {
        ToolArgs {
            positional: vec![Value::List(items), Value::Str(fn_name.into())],
            named: Vec::new(),
        }
    }

    fn ints(xs: &[i64]) -> Vec<Value> {
        xs.iter().copied().map(Value::Int).collect()
    }

    fn expect_int(v: &Value) -> i64 {
        match v {
            Value::Int(n) => *n,
            other => panic!("want int, got {other:?}"),
        }
    }

    fn expect_bool(v: &Value) -> bool {
        match v {
            Value::Bool(b) => *b,
            other => panic!("want bool, got {other:?}"),
        }
    }

    fn expect_ints(v: &Value) -> Vec<i64> {
        match v {
            Value::List(xs) => xs.iter().map(expect_int).collect(),
            other => panic!("want list, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_map_applies_named_tool_to_every_item() {
        let ctx = combinator_ctx();
        let out = ListMap
            .call(call_args(ints(&[1, 2, 3]), "double"), &ctx)
            .await
            .unwrap();
        assert_eq!(expect_ints(&out), vec![2, 4, 6]);
    }

    #[tokio::test]
    async fn list_filter_keeps_only_true_predicates() {
        let ctx = combinator_ctx();
        let out = ListFilter
            .call(call_args(ints(&[1, 20, 3, 30]), "is_big"), &ctx)
            .await
            .unwrap();
        assert_eq!(expect_ints(&out), vec![20, 30]);
    }

    #[tokio::test]
    async fn list_find_returns_first_hit_or_unit() {
        let ctx = combinator_ctx();
        let hit = ListFind
            .call(call_args(ints(&[1, 20, 3]), "is_big"), &ctx)
            .await
            .unwrap();
        assert_eq!(expect_int(&hit), 20);
        let miss = ListFind
            .call(call_args(ints(&[1, 2, 3]), "is_big"), &ctx)
            .await
            .unwrap();
        assert!(matches!(miss, Value::Unit));
    }

    #[tokio::test]
    async fn list_any_and_all_short_circuit_correctly() {
        let ctx = combinator_ctx();
        let any_hit = ListAny
            .call(call_args(ints(&[1, 20, 3]), "is_big"), &ctx)
            .await
            .unwrap();
        assert!(expect_bool(&any_hit));
        let any_miss = ListAny
            .call(call_args(ints(&[1, 2, 3]), "is_big"), &ctx)
            .await
            .unwrap();
        assert!(!expect_bool(&any_miss));
        let all_hit = ListAll
            .call(call_args(ints(&[20, 30]), "is_big"), &ctx)
            .await
            .unwrap();
        assert!(expect_bool(&all_hit));
        let all_miss = ListAll
            .call(call_args(ints(&[20, 1]), "is_big"), &ctx)
            .await
            .unwrap();
        assert!(!expect_bool(&all_miss));
    }

    #[tokio::test]
    async fn list_reduce_folds_with_init() {
        let ctx = combinator_ctx();
        let args = ToolArgs {
            positional: vec![
                Value::List(ints(&[1, 2, 3, 4])),
                Value::Str("add_ints".into()),
                Value::Int(0),
            ],
            named: Vec::new(),
        };
        let out = ListReduce.call(args, &ctx).await.unwrap();
        assert_eq!(expect_int(&out), 10);
    }

    #[tokio::test]
    async fn combinator_reports_undefined_tool_by_name() {
        let ctx = combinator_ctx();
        let err = ListMap
            .call(call_args(ints(&[1]), "nope"), &ctx)
            .await
            .unwrap_err();
        match &err {
            RuntimeError::UndefinedTool(n) => assert_eq!(n, "nope"),
            other => panic!("want UndefinedTool(nope), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn combinator_rejects_non_bool_from_predicate() {
        let ctx = combinator_ctx();
        let err = ListFilter
            .call(call_args(ints(&[1, 2]), "double"), &ctx)
            .await
            .unwrap_err();
        match &err {
            RuntimeError::TypeMismatch { expected, .. } => {
                assert!(
                    expected.contains("bool"),
                    "want bool-mismatch, got expected={expected:?}"
                );
            }
            other => panic!("want TypeMismatch, got {other:?}"),
        }
    }
}
