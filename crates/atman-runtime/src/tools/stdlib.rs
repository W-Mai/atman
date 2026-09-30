use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::AtmanPayload;
use crate::value::Value;
use crate::value::ValueJson;

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
                            Value::Host(AtmanPayload::Message(m)) => msgs.push(m.clone()),
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
                Value::Host(AtmanPayload::Message(m)) => Ok(Value::Int(
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
                let _ = tx.send(atman_rt::ast::LifecycleEvent::ContextCompact);
            }
            let list: Vec<Value> = out
                .into_iter()
                .map(|message| Value::Host(AtmanPayload::Message(message)))
                .collect();
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
                    Value::Host(AtmanPayload::Message(m)) => out.push(m.clone()),
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
    v.to_json()
        .and_then(|json| {
            serde_json::to_string_pretty(&json)
                .map_err(|error| RuntimeError::ToolFailed(format!("JSON encoding: {error}")))
        })
        .unwrap_or_else(|error| format!("[JSON error: {error}]"))
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
            let json = v.to_json()?;
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
                Value::Host(AtmanPayload::Message(m)) => Ok(Value::Str(m.text_concat())),
                Value::Str(s) => Ok(Value::Str(s.clone())),
                other => Err(RuntimeError::TypeMismatch {
                    expected: "message or string".into(),
                    actual: other.kind_name().into(),
                }),
            }
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
            let mut message = crate::message::Message::user_text(turn_id, text);
            message.origin = crate::message::MessageOrigin::Internal;
            Ok(Value::Host(AtmanPayload::Message(message)))
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
            Ok(Value::Host(AtmanPayload::Message(
                crate::message::Message::assistant_text(turn_id, text),
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
            Ok(Value::Host(AtmanPayload::Message(
                crate::message::Message::system_text(turn_id, text),
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
            Ok(Value::Host(AtmanPayload::Message(
                crate::message::Message {
                    turn_id,
                    role: crate::message::MessageRole::Tool,
                    parts: vec![crate::message::MessagePart::Text { text }],
                    origin: crate::message::MessageOrigin::User,
                },
            )))
        })
    }
}

pub use super::dispatch::{DispatchAll, ExtractToolUses};

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

    #[tokio::test]
    async fn message_user_marks_injected_prompts_as_internal() {
        let turn_id = crate::event::TurnId::now();
        let ctx = ToolCtx::new().with_anchors(Some(turn_id.clone()), None, None);
        let value = MessageUser
            .call(
                ToolArgs {
                    positional: vec![Value::Str("provide the final answer".into())],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Host(AtmanPayload::Message(message)) = value else {
            panic!("message.user must return a message");
        };

        assert_eq!(message.turn_id, turn_id);
        assert_eq!(message.origin, crate::message::MessageOrigin::Internal);
    }

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

    #[tokio::test]
    async fn final_answer_without_intent_is_withheld_from_tool_dispatch() {
        let registry = std::sync::Arc::new(crate::tool::ToolRegistry::new());
        registry.register(std::sync::Arc::new(crate::tools::final_answer::FinalAnswer));
        let ctx = authorized_ctx(registry);
        let reply = Value::Host(AtmanPayload::Message(crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![crate::message::MessagePart::ToolUse {
                id: "answer-1".into(),
                name: crate::tools::final_answer::FINAL_ANSWER_TOOL.into(),
                input: serde_json::json!({"message": "Done."}),
                intent: None,
            }],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        }));
        let uses = ExtractToolUses
            .call(
                ToolArgs {
                    positional: vec![reply],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::List(uses) = uses else {
            panic!("tool use list");
        };
        assert!(uses.is_empty());
    }

    #[tokio::test]
    async fn mixed_final_answer_stays_in_dispatch_as_an_explicit_failure() {
        let message = Value::Host(AtmanPayload::Message(crate::message::Message {
            role: crate::message::MessageRole::Assistant,
            parts: vec![
                crate::message::MessagePart::ToolUse {
                    id: "answer-1".into(),
                    name: crate::tools::final_answer::FINAL_ANSWER_TOOL.into(),
                    input: serde_json::json!({"message": "Done."}),
                    intent: crate::message::ToolCallIntent::new("Completed requested work."),
                },
                crate::message::MessagePart::ToolUse {
                    id: "read-1".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": "README.md"}),
                    intent: crate::message::ToolCallIntent::new("Read documentation."),
                },
            ],
            turn_id: crate::event::TurnId::now(),
            origin: crate::message::MessageOrigin::User,
        }));

        let Value::List(uses) = ExtractToolUses
            .call(
                ToolArgs {
                    positional: vec![message],
                    named: Vec::new(),
                },
                &ToolCtx::new(),
            )
            .await
            .unwrap()
        else {
            panic!("tool use list");
        };

        assert_eq!(uses.len(), 2);
        let Value::Struct(final_fields) = &uses[0] else {
            panic!("final tool use");
        };
        assert!(final_fields.iter().any(|(name, value)| {
            name == "validation_error"
                && matches!(value, Value::Str(error) if error.contains("without sibling tool calls"))
        }));
    }

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
