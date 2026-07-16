use crate::approval::{ApprovalOutcome, request_approval};
use crate::error::RuntimeError;
use crate::event::{Event, FlowRunId, FlowStatus, NodeEvent};
use crate::message::{Message, MessagePart, MessageRole};
use crate::provider::LlmRequest;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult, ToolSpec};
use crate::value::Value;
use std::path::PathBuf;

pub struct AgentSpawn;

const DEFAULT_MAX_ITER: u64 = 20;
const MAX_ITER_HARD_CAP: u64 = 200;

impl Tool for AgentSpawn {
    fn name(&self) -> &str {
        "agent.spawn"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        ApprovalLevel::Approve
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Spawn an independent sub-agent to handle a focused sub-goal. The sub-agent runs its \
             own message history and iteration counter, uses the same tool registry (or a subset \
             you pick), and returns its final assistant text as this tool's result. Prefer this \
             over doing large exploratory work directly when it would otherwise flood the main \
             conversation with search output or scratch reasoning. Parameters: \
             `goal` (required string), `tools` (optional list of tool-name strings — defaults \
             to all tools available to you), `max_iterations` (optional int, default 20, capped \
             at 200), `model` (optional model name — defaults to the last model this session \
             used, then configured models, then claude-opus-4.7), `flow` (optional .at file path \
             or command name; goal is passed as the first flow argument).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "goal": {"type": "string"},
                "tools": {"type": "array", "items": {"type": "string"}},
                "max_iterations": {"type": "integer"},
                "model": {"type": "string"},
                "flow": {"type": "string"}
            },
            "required": ["goal"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move { run_sub_agent(args, ctx).await })
    }
}

async fn run_sub_agent(args: ToolArgs, ctx: &ToolCtx) -> ToolResult {
    let goal = extract_goal(&args)?;
    if let Some(flow) = extract_flow(&args)? {
        return run_flow_agent(&flow, goal, ctx).await;
    }
    let max_iter = extract_max_iter(&args);
    let tool_filter = extract_tool_filter(&args)?;
    let model = pick_model(&args, ctx);
    let Some(providers) = ctx.providers.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "agent.spawn: no provider registry available on ctx".into(),
        ));
    };
    let Some(provider) = providers.resolve(&model) else {
        return Ok(Value::Str(format!(
            "[sub-agent failed: no provider for model `{model}`]"
        )));
    };
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "agent.spawn: no tool registry available on ctx".into(),
        ));
    };
    let tool_specs = build_tool_specs(registry.as_ref(), tool_filter.as_deref());
    let child_run_id = FlowRunId::now();
    emit_child_flow_start(ctx, &child_run_id, &goal);
    let turn = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);
    let mut messages: Vec<Message> = vec![Message::user_text(turn.clone(), goal.clone())];
    let mut final_text: Option<String> = None;
    let mut failure_reason: Option<String> = None;
    for iter in 0..max_iter {
        if ctx.cancel.is_cancelled() {
            failure_reason = Some("cancelled by parent".into());
            break;
        }
        let req = LlmRequest {
            model: model.clone(),
            messages: messages.clone(),
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
            tools: tool_specs.clone(),
            thinking_enabled: false,
        };
        let outcome = call_streaming_sub_agent(provider.as_ref(), req, ctx).await;
        match outcome {
            Ok(am) => {
                emit_child_llm_call(ctx, &child_run_id, &model, &am);
                let uses = extract_tool_uses(&am.message);
                messages.push(am.message.clone());
                emit_assistant_msg(ctx, &child_run_id, &am.message);
                if uses.is_empty() {
                    final_text = Some(am.text_concat());
                    break;
                }
                let tool_results = dispatch_child_tools(&uses, registry.as_ref(), ctx).await;
                let turn_for_results = am.message.turn_id.clone();
                let combined = Message {
                    turn_id: turn_for_results,
                    role: MessageRole::Tool,
                    parts: tool_results,
                };
                emit_tool_result_msg(ctx, &child_run_id, &combined);
                messages.push(combined);
            }
            Err(e) => {
                failure_reason = Some(format!("provider error at iter {iter}: {e}"));
                break;
            }
        }
    }
    let status = if final_text.is_some() {
        FlowStatus::Ok
    } else {
        FlowStatus::Errored {
            message: failure_reason
                .clone()
                .unwrap_or_else(|| format!("hit max iterations {max_iter} without a final answer")),
        }
    };
    emit_child_flow_end(ctx, &child_run_id, &status);
    if let Some(text) = final_text {
        Ok(Value::Str(text))
    } else {
        let reason = failure_reason
            .unwrap_or_else(|| format!("hit max iterations {max_iter} without a final answer"));
        let last = messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .map(|m| m.text_concat())
            .unwrap_or_default();
        let partial = if last.is_empty() {
            String::new()
        } else {
            format!("\n[partial output: {}]", truncate(&last, 400))
        };
        Ok(Value::Str(format!("[sub-agent failed: {reason}]{partial}")))
    }
}

async fn run_flow_agent(flow_ref: &str, goal: String, ctx: &ToolCtx) -> ToolResult {
    let Some(registry) = ctx.registry.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "agent.spawn: no tool registry available on ctx".into(),
        ));
    };
    let Some(providers) = ctx.providers.as_ref() else {
        return Err(RuntimeError::ToolFailed(
            "agent.spawn: no provider registry available on ctx".into(),
        ));
    };
    let (path, src) = read_flow_source(flow_ref).await?;
    let file = atman_dsl::parse::parse_file(&src).map_err(|e| {
        RuntimeError::ToolFailed(format!("agent.spawn: parse {}: {e}", path.display()))
    })?;
    let Some(flow) = file.flows.first() else {
        return Err(RuntimeError::ToolFailed(format!(
            "agent.spawn: no flow in {}",
            path.display()
        )));
    };
    let args = flow
        .params
        .first()
        .map(|(ident, _)| vec![(ident.name.clone(), Value::Str(goal))])
        .unwrap_or_default();
    let flows = file
        .flows
        .iter()
        .map(|flow| (flow.name.name.clone(), flow.clone()))
        .collect();
    let run_id = FlowRunId::now();
    emit_flow_agent_start(ctx, &run_id, &flow.name.name);
    let out = crate::exec::exec_flow_with_siblings(
        flow,
        args,
        registry.as_ref(),
        ctx,
        providers.as_ref(),
        &flows,
        ctx.events.as_ref(),
        ctx.turn_id.clone(),
        Some(run_id.clone()),
        None,
        ctx.cancel.clone(),
        None,
    )
    .await;
    let status = match &out {
        Ok(_) => FlowStatus::Ok,
        Err(e) => FlowStatus::Errored {
            message: e.to_string(),
        },
    };
    emit_child_flow_end(ctx, &run_id, &status);
    out
}

fn extract_goal(args: &ToolArgs) -> Result<String, RuntimeError> {
    match args.named("goal").or_else(|| args.positional.first()) {
        Some(Value::Str(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "non-empty goal string".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg("agent.spawn.goal".into())),
    }
}

fn extract_max_iter(args: &ToolArgs) -> u64 {
    match args.named("max_iterations") {
        Some(Value::Int(n)) if *n > 0 => (*n as u64).min(MAX_ITER_HARD_CAP),
        _ => DEFAULT_MAX_ITER,
    }
}

fn extract_flow(args: &ToolArgs) -> Result<Option<String>, RuntimeError> {
    match args.named("flow") {
        Some(Value::Str(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "flow string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_tool_filter(args: &ToolArgs) -> Result<Option<Vec<String>>, RuntimeError> {
    match args.named("tools") {
        Some(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                match it {
                    Value::Str(s) => out.push(s.clone()),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "string tool name".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                }
            }
            Ok(Some(out))
        }
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "list of tool names".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn pick_model(args: &ToolArgs, ctx: &ToolCtx) -> String {
    if let Some(Value::Str(s)) = args.named("model")
        && !s.is_empty()
    {
        return crate::model_registry::resolve_alias(s);
    }
    if let Some(model) = &ctx.current_model {
        return model.clone();
    }
    if let Some((name, _)) = crate::model_registry::all_model_entries().first() {
        return name.clone();
    }
    "claude-opus-4.7".into()
}

async fn read_flow_source(flow_ref: &str) -> Result<(PathBuf, String), RuntimeError> {
    for path in flow_candidates(flow_ref) {
        match tokio::fs::read_to_string(&path).await {
            Ok(src) => return Ok((path, src)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "agent.spawn: read {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(RuntimeError::ToolFailed(format!(
        "agent.spawn: flow `{flow_ref}` not found"
    )))
}

fn flow_candidates(flow_ref: &str) -> Vec<PathBuf> {
    let path = PathBuf::from(flow_ref);
    if path.is_absolute() {
        return vec![path];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut out = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        out.push(
            PathBuf::from(home)
                .join(".config")
                .join("atman")
                .join("commands")
                .join(&file_name),
        );
    }
    out.push(PathBuf::from(file_name));
    out
}

async fn call_streaming_sub_agent(
    provider: &dyn crate::provider::Provider,
    req: LlmRequest,
    ctx: &ToolCtx,
) -> Result<crate::provider::AssistantMessage, RuntimeError> {
    let model_name = req.model.clone();
    let obs = provider.call_streaming(req);
    let mut events = obs.events;
    let output = obs.output;
    tokio::pin!(output);
    let result = loop {
        tokio::select! {
            biased;
            ev = events.recv() => forward_stream_event(ev, ctx, &model_name),
            result = &mut output => break result,
        }
    };
    while let Ok(ev) = events.try_recv() {
        forward_stream_event(Ok(ev), ctx, &model_name);
    }
    result
}

fn forward_stream_event(
    ev: Result<NodeEvent, tokio::sync::broadcast::error::RecvError>,
    ctx: &ToolCtx,
    model: &str,
) {
    let Some(tx) = &ctx.stream_tx else {
        return;
    };
    match ev {
        Ok(NodeEvent::LlmChunk { text, .. }) => {
            let _ = tx.send(crate::stream::StreamFrame::LlmChunk {
                text,
                model: model.to_string(),
            });
        }
        Ok(NodeEvent::ThinkingChunk { text }) => {
            let _ = tx.send(crate::stream::StreamFrame::ThinkingChunk { text });
        }
        Ok(NodeEvent::LlmDone { total_tokens }) => {
            let _ = tx.send(crate::stream::StreamFrame::LlmDone { total_tokens });
        }
        _ => {}
    }
}

fn build_tool_specs(
    registry: &crate::tool::ToolRegistry,
    filter: Option<&[String]>,
) -> Vec<ToolSpec> {
    let mut specs = Vec::new();
    for (name, tool) in registry.iter() {
        if let Some(allow) = filter
            && !allow.iter().any(|n| n == name)
        {
            continue;
        }
        specs.push(crate::tool::tool_spec(tool.as_ref()));
    }
    specs
}

fn extract_tool_uses(msg: &Message) -> Vec<(String, String, Value)> {
    let mut out = Vec::new();
    for part in &msg.parts {
        if let MessagePart::ToolUse { id, name, input } = part {
            let value = Value::from_json(input.clone());
            out.push((id.clone(), name.clone(), value));
        }
    }
    out
}

async fn dispatch_child_tools(
    uses: &[(String, String, Value)],
    registry: &crate::tool::ToolRegistry,
    ctx: &ToolCtx,
) -> Vec<MessagePart> {
    struct Ready {
        idx: usize,
        id: String,
        name: String,
        tool: std::sync::Arc<dyn crate::tool::Tool>,
        call_args: ToolArgs,
    }
    let mut out: Vec<Option<MessagePart>> = vec![None; uses.len()];
    let mut ready: Vec<Ready> = Vec::new();
    for (idx, (id, name, input)) in uses.iter().enumerate() {
        let Some(tool) = registry.get(name) else {
            out[idx] = Some(MessagePart::ToolResult {
                tool_use_id: id.clone(),
                content: format!("sub-agent: unknown tool `{name}`"),
                is_error: true,
            });
            continue;
        };
        let named = match input {
            Value::Struct(fields) => fields.clone(),
            Value::Unit => Vec::new(),
            _ => Vec::new(),
        };
        ready.push(Ready {
            idx,
            id: id.clone(),
            name: name.clone(),
            tool,
            call_args: ToolArgs {
                positional: Vec::new(),
                named,
            },
        });
    }
    // Parallel: serial awaits hid all but the first pending node from the UI.
    for r in &ready {
        emit_tool_use_start(ctx, &r.name, &r.id, &r.call_args);
    }
    let gates = ready.iter().map(|r| {
        let level = r.tool.approval_level(&r.call_args, ctx);
        request_approval(
            ctx,
            &r.id,
            &r.name,
            &r.call_args,
            level,
            Some(r.tool.as_ref()),
        )
    });
    let outcomes = futures::future::join_all(gates).await;
    for (r, gate) in ready.into_iter().zip(outcomes) {
        let part = match gate {
            ApprovalOutcome::Deny { reason } => MessagePart::ToolResult {
                tool_use_id: r.id.clone(),
                content: format!("sub-agent: tool `{}` denied — {reason}", r.name),
                is_error: true,
            },
            ApprovalOutcome::Approve => match r.tool.call(r.call_args, ctx).await {
                Ok(v) => MessagePart::ToolResult {
                    tool_use_id: r.id.clone(),
                    content: format_value(&v),
                    is_error: false,
                },
                Err(e) => MessagePart::ToolResult {
                    tool_use_id: r.id.clone(),
                    content: format!("{e}"),
                    is_error: true,
                },
            },
        };
        emit_tool_use_done(ctx, &r.name, &r.id, &part);
        out[r.idx] = Some(part);
    }
    out.into_iter().flatten().collect()
}

fn emit_tool_use_start(ctx: &ToolCtx, name: &str, id: &str, args: &ToolArgs) {
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolUseStart {
            tool: name.to_string(),
            args_preview: preview_tool_args(args),
            id: id.to_string(),
        });
    }
}

fn emit_tool_use_done(ctx: &ToolCtx, name: &str, id: &str, part: &MessagePart) {
    if let Some(tx) = &ctx.stream_tx {
        let (ok, preview) = match part {
            MessagePart::ToolResult {
                content, is_error, ..
            } => (!is_error, truncate(content, 400)),
            _ => (false, String::new()),
        };
        let _ = tx.send(crate::stream::StreamFrame::ToolUseDone {
            tool: name.to_string(),
            ok,
            preview,
            id: id.to_string(),
        });
    }
}

fn preview_tool_args(args: &ToolArgs) -> String {
    let mut parts: Vec<String> = args.positional.iter().map(preview_value).collect();
    for (k, v) in &args.named {
        parts.push(format!("{k}={}", preview_value(v)));
    }
    truncate(&parts.join(", "), 4000)
}

fn preview_value(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("{s:?}"),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Unit => "()".into(),
        Value::List(items) => format!("list[{}]", items.len()),
        Value::Struct(items) => format!("struct[{}]", items.len()),
        Value::Message(_) => "<message>".into(),
        Value::Path(p) => p.display().to_string(),
        Value::EditProposal(_) => "<edit proposal>".into(),
        Value::Err(e) => format!("error({e})"),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

fn emit_child_flow_start(ctx: &ToolCtx, run_id: &FlowRunId, goal: &str) {
    let parent_run_id = ctx.flow_run_id.clone();
    let parent_node_id = ctx.current_node_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowStart {
            seq: 0,
            run_id: run_id.clone(),
            flow_name: "agent.sub".into(),
            parent_run_id: parent_run_id.clone(),
            parent_node_id: parent_node_id.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowStart {
            run_id: run_id.0.to_string(),
            flow_name: format!("agent.sub · {}", truncate(goal, 60)),
            parent_run_id: parent_run_id.as_ref().map(|r| r.0.to_string()),
            parent_node_id,
        });
    }
}

fn emit_flow_agent_start(ctx: &ToolCtx, run_id: &FlowRunId, flow_name: &str) {
    let parent_run_id = ctx.flow_run_id.clone();
    let parent_node_id = ctx.current_node_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowStart {
            seq: 0,
            run_id: run_id.clone(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.clone(),
            parent_node_id: parent_node_id.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowStart {
            run_id: run_id.0.to_string(),
            flow_name: flow_name.into(),
            parent_run_id: parent_run_id.as_ref().map(|r| r.0.to_string()),
            parent_node_id,
        });
    }
}

fn emit_child_flow_end(ctx: &ToolCtx, run_id: &FlowRunId, status: &FlowStatus) {
    if let Some(sink) = &ctx.events {
        sink.emit(Event::FlowEnd {
            seq: 0,
            run_id: run_id.clone(),
            flow_name: "agent.sub".into(),
            status: status.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FlowDone {
            run_id: run_id.0.to_string(),
            flow_name: "agent.sub".into(),
            ok: matches!(status, FlowStatus::Ok),
        });
    }
}

fn emit_child_llm_call(
    ctx: &ToolCtx,
    _run_id: &FlowRunId,
    model: &str,
    am: &crate::provider::AssistantMessage,
) {
    if let Some(sink) = &ctx.events {
        sink.emit(Event::LlmCall {
            seq: 0,
            model: model.into(),
            provider: "sub".into(),
            usage: am.token_usage.clone(),
            wallclock_ms: 0,
            ttft_ms: am.timing.ttft_ms,
            tokens_per_second: am.timing.tokens_per_second(am.token_usage.output),
            status: crate::event::LlmCallStatus::Ok,
            run_id: None,
            node_id: None,
            ts: chrono::Utc::now(),
        });
    }
}

fn emit_assistant_msg(ctx: &ToolCtx, run_id: &FlowRunId, message: &Message) {
    let turn_id = message.turn_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::AssistantMsg {
            seq: 0,
            turn_id: turn_id.clone(),
            flow_run_id: Some(run_id.clone()),
            message: message.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::AssistantMsg {
            flow_run_id: Some(run_id.0.to_string()),
            message: message.clone(),
        });
    }
}

fn emit_tool_result_msg(ctx: &ToolCtx, run_id: &FlowRunId, message: &Message) {
    let turn_id = message.turn_id.clone();
    if let Some(sink) = &ctx.events {
        sink.emit(Event::ToolResultMsg {
            seq: 0,
            turn_id: turn_id.clone(),
            flow_run_id: Some(run_id.clone()),
            message: message.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolResultMsg {
            flow_run_id: Some(run_id.0.to_string()),
            message: message.clone(),
        });
    }
}

fn truncate(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        s.to_string()
    } else {
        chars.iter().take(n).collect::<String>() + "…"
    }
}
