use atman_dsl::ast::{Arg, BinOp, Expr, Literal, Node, UnOp};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::tool::{BoxFut, ToolArgs, ToolCtx, ToolRegistry};
use crate::value::Value;

#[derive(Clone)]
pub struct EvalCtx<'a> {
    pub tools: &'a ToolRegistry,
    pub tool_ctx: &'a ToolCtx,
    pub providers: &'a crate::provider::ProviderRegistry,
    pub flows: &'a std::collections::HashMap<String, atman_dsl::ast::FlowDecl>,
    pub contract: Option<&'a atman_dsl::ast::Contract>,
    pub events: Option<&'a crate::event::EventSink>,
    pub turn_id: Option<crate::event::TurnId>,
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub session: Option<&'a crate::session::Session>,
    pub flow_cancel: tokio_util::sync::CancellationToken,
    pub safety: Option<&'a crate::safety::SafetyConfig>,
    pub current_node_id: Option<String>,
}

impl<'a> EvalCtx<'a> {
    pub fn with_node(&self, node_id: impl Into<String>) -> Self {
        let mut c = self.clone();
        c.current_node_id = Some(node_id.into());
        c
    }
}

pub fn eval_expr<'a>(expr: &'a Expr, env: &'a Env, ctx: &'a EvalCtx<'a>) -> BoxFut<'a, Value> {
    Box::pin(async move { eval_expr_inner(expr, env, ctx).await })
}

async fn eval_expr_inner<'a>(expr: &'a Expr, env: &'a Env, ctx: &'a EvalCtx<'a>) -> Value {
    match expr {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::Ident(id) => match env.lookup(&id.name) {
            Some(v) => v.clone(),
            None => Value::Err(RuntimeError::UndefinedVar(id.name.clone())),
        },
        Expr::FileRef(f) => match tokio::fs::read_to_string(&f.path).await {
            Ok(s) => Value::Str(s),
            Err(e) => Value::Err(RuntimeError::ToolFailed(format!("@\"{}\": {e}", f.path))),
        },
        Expr::Member { base, field } => {
            let base_v = eval_expr(base, env, ctx).await;
            if base_v.is_err() {
                return base_v;
            }
            match base_v.field(&field.name) {
                Some(v) => v.clone(),
                None => Value::Err(RuntimeError::UndefinedVar(format!(".{}", field.name))),
            }
        }
        Expr::Binary { op, left, right } => {
            let l = eval_expr(left, env, ctx).await;
            if l.is_err() {
                return l;
            }
            let r = eval_expr(right, env, ctx).await;
            if r.is_err() {
                return r;
            }
            eval_binop(*op, &l, &r)
        }
        Expr::Unary { op, operand } => {
            let v = eval_expr(operand, env, ctx).await;
            if v.is_err() {
                return v;
            }
            eval_unop(*op, &v)
        }
        Expr::List(items) => {
            let mut acc = Vec::with_capacity(items.len());
            for item in items {
                let v = eval_expr(item, env, ctx).await;
                if v.is_err() {
                    return v;
                }
                acc.push(v);
            }
            Value::List(acc)
        }
        Expr::Struct(fields) => {
            let mut acc = Vec::with_capacity(fields.len());
            for (k, v) in fields {
                let val = eval_expr(v, env, ctx).await;
                if val.is_err() {
                    return val;
                }
                acc.push((k.name.clone(), val));
            }
            Value::Struct(acc)
        }
        Expr::Node(node) => eval_node(node, env, ctx).await,
        Expr::Call { .. } => Value::Err(RuntimeError::ToolFailed(
            "bare function call not supported; use namespaced tool call".into(),
        )),
        Expr::Pipe { lhs, rhs } => eval_pipe(lhs, rhs, env, ctx).await,
    }
}

async fn eval_pipe<'a>(lhs: &'a Expr, rhs: &'a Expr, env: &'a Env, ctx: &'a EvalCtx<'a>) -> Value {
    let piped = eval_expr(lhs, env, ctx).await;
    if piped.is_err() {
        return piped;
    }
    match rhs {
        Expr::Node(Node::ToolCall { path, args }) => {
            dispatch_tool_call(path, args, vec![piped], env, ctx).await
        }
        other => Value::Err(RuntimeError::ToolFailed(format!(
            "pipe rhs must be a tool call like `ns.tool(...)`, got {}",
            expr_shape(other)
        ))),
    }
}

fn expr_shape(e: &Expr) -> &'static str {
    match e {
        Expr::Literal(_) => "literal",
        Expr::Ident(_) => "identifier",
        Expr::FileRef(_) => "file ref",
        Expr::Member { .. } => "member access",
        Expr::Binary { .. } => "binary expr",
        Expr::Unary { .. } => "unary expr",
        Expr::Call { .. } => "bare call",
        Expr::Pipe { .. } => "pipe expr",
        Expr::Struct(_) => "struct literal",
        Expr::List(_) => "list literal",
        Expr::Node(_) => "flow node",
    }
}

async fn dispatch_tool_call<'a>(
    path: &'a [atman_dsl::ast::Ident],
    args: &'a [Arg],
    prefix_positional: Vec<Value>,
    env: &'a Env,
    ctx: &'a EvalCtx<'a>,
) -> Value {
    if ctx.flow_cancel.is_cancelled() {
        return Value::Err(RuntimeError::Cancelled("flow cancelled by user".into()));
    }
    let name = tool_name(path);
    let tool = match ctx.tools.get(&name) {
        Some(t) => t,
        None => {
            if is_type_annotation(path) {
                return Value::Unit;
            }
            return Value::Err(RuntimeError::UndefinedTool(name));
        }
    };
    if matches!(tool.tier(), crate::tool::Tier::Four) && !contract_allows_shell(ctx.contract) {
        return Value::Err(RuntimeError::ToolFailed(format!(
            "tool `{name}` is Tier 4 (shell); flow contract must declare `capabilities {{ shell: true }}`"
        )));
    }
    let mut positional = prefix_positional;
    let mut named = Vec::new();
    for arg in args {
        match arg {
            Arg::Positional(e) => {
                let v = eval_expr(e, env, ctx).await;
                if v.is_err() {
                    return v;
                }
                positional.push(v);
            }
            Arg::Named { name, value } => {
                let v = eval_expr(value, env, ctx).await;
                if v.is_err() {
                    return v;
                }
                named.push((name.name.clone(), v));
            }
        }
    }
    let ctx_with_anchors = ctx
        .tool_ctx
        .clone()
        .with_anchors(
            ctx.turn_id.clone(),
            ctx.flow_run_id.clone(),
            ctx.events.map(|s| s.next_seq_peek()),
        )
        .with_registry(std::sync::Arc::new(ctx.tools.clone()));
    let ctx_with_anchors = if let Some(sink) = ctx.events {
        ctx_with_anchors.with_events(sink.clone())
    } else {
        ctx_with_anchors
    };
    let ctx_with_anchors = if matches!(tool.tier(), crate::tool::Tier::Four) {
        ctx_with_anchors
    } else {
        let mut c = ctx_with_anchors;
        c.sandbox = None;
        c
    };
    let ctx_with_anchors = if let Some(session) = ctx.session {
        ctx_with_anchors.with_session_messages(std::sync::Arc::new(session.messages()))
    } else {
        ctx_with_anchors
    };
    let ctx_with_anchors = ctx_with_anchors.with_current_node(ctx.current_node_id.clone());
    let ctx_with_anchors = if let Some(session) = ctx.session {
        ctx_with_anchors
            .with_stream_tx(session.stream_tx())
            .with_read_files(session.read_files())
            .with_approval(session.approval())
    } else {
        ctx_with_anchors
    };
    let stream_tx = ctx.session.map(|s| s.stream_tx());
    let tool_call_id = uuid::Uuid::now_v7().to_string();
    let args_preview = preview_tool_args(&positional, &named);
    if let (Some(sink), Some(run_id), Some(parent_node)) =
        (ctx.events, ctx.flow_run_id.clone(), &ctx.current_node_id)
    {
        sink.emit(crate::event::Event::ToolNode {
            seq: 0,
            run_id: run_id.clone(),
            parent_node_id: parent_node.clone(),
            tool_use_id: tool_call_id.clone(),
            tool_name: name.clone(),
            args_preview: args_preview.clone(),
            ts: chrono::Utc::now(),
        });
        if let Some(tx) = &stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::ToolNode {
                run_id: run_id.0.to_string(),
                parent_node_id: parent_node.clone(),
                tool_use_id: tool_call_id.clone(),
                tool: name.clone(),
                args_preview: args_preview.clone(),
            });
        }
    }
    if let Some(tx) = &stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolUseStart {
            tool: name.clone(),
            args_preview: args_preview.clone(),
            id: tool_call_id.clone(),
        });
    }
    let call_args = ToolArgs { positional, named };
    let level = tool.approval_level();
    let gate = crate::approval::request_approval(
        &ctx_with_anchors,
        &tool_call_id,
        &name,
        &call_args,
        level,
        Some(tool.as_ref()),
    )
    .await;
    let outcome = match gate {
        crate::approval::ApprovalOutcome::Deny { reason } => Err(RuntimeError::ToolFailed(
            format!("tool `{name}` denied by user: {reason}"),
        )),
        crate::approval::ApprovalOutcome::Approve => tool.call(call_args, &ctx_with_anchors).await,
    };
    if let Some(tx) = &stream_tx {
        let (ok, preview) = match &outcome {
            Ok(v) => (true, preview_tool_value(v)),
            Err(e) => (false, format!("{e}")),
        };
        let _ = tx.send(crate::stream::StreamFrame::ToolUseDone {
            tool: name.clone(),
            ok,
            preview,
            id: tool_call_id,
        });
    }
    if let Some(session) = ctx.session
        && (name == "memory.todo.set" || name == "memory.todo.done")
    {
        session.refresh_todos_from_store_async().await;
    }
    match outcome {
        Ok(v) => v,
        Err(e) => Value::Err(e),
    }
}

async fn call_and_maybe_stream(
    provider: &dyn crate::provider::Provider,
    req: crate::provider::LlmRequest,
    session: Option<&crate::session::Session>,
) -> Result<crate::provider::AssistantMessage, RuntimeError> {
    let result = call_and_maybe_stream_inner(provider, req, session).await;
    if let (Some(sess), Err(RuntimeError::AttachmentError { reason })) = (session, &result) {
        let count = sess.record_attachment_degrade(reason);
        if count > 0 {
            let _ = sess.stream_tx().send(crate::stream::StreamFrame::Note(
                format!(
                    "attachment degraded ({reason}); {count} image part(s) replaced. re-issue your last message to retry without them."
                ),
            ));
        }
    }
    result
}

async fn call_and_maybe_stream_inner(
    provider: &dyn crate::provider::Provider,
    req: crate::provider::LlmRequest,
    session: Option<&crate::session::Session>,
) -> Result<crate::provider::AssistantMessage, RuntimeError> {
    let Some(session) = session else {
        return provider.call(req).await;
    };
    let stream_tx = session.stream_tx();
    let model_name = req.model.clone();
    let obs = provider.call_streaming(req);
    let mut events = obs.events;
    let output = obs.output;
    tokio::pin!(output);
    loop {
        tokio::select! {
            biased;
            ev = events.recv() => {
                match ev {
                    Ok(crate::event::NodeEvent::LlmChunk { text, .. }) => {
                        session.mark_streamed();
                        let _ = stream_tx.send(crate::stream::StreamFrame::LlmChunk {
                            text,
                            model: model_name.clone(),
                        });
                    }
                    Ok(crate::event::NodeEvent::LlmDone { total_tokens }) => {
                        let _ = stream_tx.send(crate::stream::StreamFrame::LlmDone { total_tokens });
                    }
                    Ok(_) | Err(_) => {}
                }
            }
            result = &mut output => {
                while let Ok(ev) = events.try_recv() {
                    match ev {
                        crate::event::NodeEvent::LlmChunk { text, .. } => {
                            session.mark_streamed();
                            let _ = stream_tx.send(crate::stream::StreamFrame::LlmChunk {
                                text,
                                model: model_name.clone(),
                            });
                        }
                        crate::event::NodeEvent::LlmDone { total_tokens } => {
                            let _ = stream_tx.send(crate::stream::StreamFrame::LlmDone { total_tokens });
                        }
                        _ => {}
                    }
                }
                return result;
            }
        }
    }
}

fn preview_tool_args(positional: &[Value], named: &[(String, Value)]) -> String {
    let mut parts: Vec<String> = positional.iter().map(preview_tool_value).collect();
    for (k, v) in named {
        parts.push(format!("{k}={}", preview_tool_value(v)));
    }
    truncate(&parts.join(", "), 4000)
}

fn preview_tool_value(v: &Value) -> String {
    let raw = match v {
        Value::Str(s) => format!("{s:?}"),
        Value::Int(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Unit => "()".into(),
        Value::List(items) => format!("list[{}]", items.len()),
        Value::Struct(f) => format!("struct[{}]", f.len()),
        Value::Message(_) => "<message>".into(),
        Value::Err(e) => format!("err({e})"),
        Value::Path(p) => format!("{p:?}"),
        Value::EditProposal(_) => "<edit_proposal>".into(),
    };
    truncate(&raw, 60)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

async fn eval_node<'a>(node: &'a Node, env: &'a Env, ctx: &'a EvalCtx<'a>) -> Value {
    if ctx.flow_cancel.is_cancelled() {
        return Value::Err(RuntimeError::Cancelled("flow cancelled by user".into()));
    }
    match node {
        Node::ToolCall { path, args } => dispatch_tool_call(path, args, Vec::new(), env, ctx).await,
        Node::Fanout { items, collect } => match collect {
            atman_dsl::ast::FanoutCollect::All => {
                let parent_id = ctx.current_node_id.clone();
                let branch_ctxs: Vec<EvalCtx<'a>> = (0..items.len())
                    .map(|i| {
                        let branch_id = match &parent_id {
                            Some(p) => format!("{p}.branch[{i}]"),
                            None => format!("branch[{i}]"),
                        };
                        if let (Some(sink), Some(run_id)) = (ctx.events, ctx.flow_run_id.clone()) {
                            sink.emit(crate::event::Event::FlowNodeStart {
                                seq: 0,
                                run_id: run_id.clone(),
                                node_id: branch_id.clone(),
                                kind: crate::nodegraph::NodeKind::UserConfirm,
                                label: format!("branch[{i}]"),
                                parent_node_id: parent_id.clone(),
                                ts: chrono::Utc::now(),
                            });
                            if let Some(session) = ctx.session {
                                let _ = session.stream_tx().send(
                                    crate::stream::StreamFrame::FlowNodeStart {
                                        run_id: run_id.0.to_string(),
                                        node_id: branch_id.clone(),
                                        kind: crate::nodegraph::NodeKind::UserConfirm,
                                        label: format!("branch[{i}]"),
                                        parent_node_id: parent_id.clone(),
                                    },
                                );
                            }
                        }
                        ctx.with_node(branch_id)
                    })
                    .collect();
                let futs = items
                    .iter()
                    .zip(branch_ctxs.iter())
                    .map(|(item, bctx)| eval_expr(item, env, bctx));
                let results: Vec<Value> = futures::future::join_all(futs).await;
                for (bctx, v) in branch_ctxs.iter().zip(results.iter()) {
                    if let (Some(sink), Some(run_id), Some(bid)) =
                        (ctx.events, ctx.flow_run_id.clone(), &bctx.current_node_id)
                    {
                        let status = if v.is_err() {
                            crate::event::FlowNodeStatus::Err
                        } else {
                            crate::event::FlowNodeStatus::Ok
                        };
                        sink.emit(crate::event::Event::FlowNodeEnd {
                            seq: 0,
                            run_id: run_id.clone(),
                            node_id: bid.clone(),
                            status: status.clone(),
                            output_preview: None,
                            ts: chrono::Utc::now(),
                        });
                        if let Some(session) = ctx.session {
                            let _ =
                                session
                                    .stream_tx()
                                    .send(crate::stream::StreamFrame::FlowNodeEnd {
                                        run_id: run_id.0.to_string(),
                                        node_id: bid.clone(),
                                        status,
                                        output_preview: None,
                                        parent_node_id: parent_id.clone(),
                                    });
                        }
                    }
                }
                for v in &results {
                    if let Value::Err(e) = v {
                        return Value::Err(e.clone());
                    }
                }
                Value::List(results)
            }
            atman_dsl::ast::FanoutCollect::First => Value::Err(RuntimeError::ToolFailed(
                "fanout collect: first not yet implemented".into(),
            )),
        },
        Node::Llm { kwargs } => {
            let mut model: Option<String> = None;
            let mut prompt: Option<String> = None;
            let mut messages_override: Option<Vec<crate::message::Message>> = None;
            let mut system: Option<String> = None;
            let mut input: Value = Value::Unit;
            let mut retry_count: u32 = 0;
            let mut retry_kinds: Option<std::collections::HashSet<crate::error::ErrorKind>> = None;
            let mut cache_prompt = false;
            let mut context_budget: Option<u64> = None;
            let mut fallback_expr: Option<&Expr> = None;
            let mut tool_specs: Vec<crate::tool::ToolSpec> = Vec::new();
            for (k, v) in kwargs {
                match k.name.as_str() {
                    "schema" => continue,
                    "fallback" => {
                        fallback_expr = Some(v);
                        continue;
                    }
                    "retry_classified" => {
                        let idents = match parse_error_kind_list(v) {
                            Ok(k) => k,
                            Err(msg) => return Value::Err(RuntimeError::ToolFailed(msg)),
                        };
                        retry_kinds = Some(idents);
                        continue;
                    }
                    "tools" => {
                        match resolve_tool_specs(v, ctx.tools) {
                            Ok(specs) => tool_specs = specs,
                            Err(msg) => return Value::Err(RuntimeError::ToolFailed(msg)),
                        }
                        continue;
                    }
                    _ => {}
                }
                let val = eval_expr(v, env, ctx).await;
                if val.is_err() {
                    return val;
                }
                match k.name.as_str() {
                    "model" => match val {
                        Value::Str(s) => model = Some(s),
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "string".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "prompt" => match val {
                        Value::Str(s) => prompt = Some(s),
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "string or @\"path\"".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "messages" => match val {
                        Value::List(items) => {
                            let mut msgs = Vec::with_capacity(items.len());
                            for item in items {
                                match item {
                                    Value::Message(m) => msgs.push(m),
                                    other => {
                                        return Value::Err(RuntimeError::TypeMismatch {
                                            expected: "message".into(),
                                            actual: other.kind_name().into(),
                                        });
                                    }
                                }
                            }
                            messages_override = Some(msgs);
                        }
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "list of message".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "system" => match val {
                        Value::Str(s) => system = Some(s),
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "string (system prompt)".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "input" => input = val,
                    "retry" => match val {
                        Value::Int(n) if n >= 0 => retry_count = n as u32,
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "non-negative int".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "cache" => match val {
                        Value::Bool(b) => cache_prompt = b,
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "bool".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    "context_budget" => match val {
                        Value::Int(n) if n > 0 => context_budget = Some(n as u64),
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "positive int".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    },
                    _ => {}
                }
            }
            let Some(model) = model else {
                return Value::Err(RuntimeError::MissingArg("llm.model".into()));
            };
            if messages_override.is_some() && prompt.is_some() {
                return Value::Err(RuntimeError::ToolFailed(
                    "llm node: cannot specify both `messages:` and `prompt:` (pick one)".into(),
                ));
            }
            let Some(provider) = ctx.providers.resolve(&model) else {
                return Value::Err(RuntimeError::ToolFailed(format!(
                    "no provider registered for model `{model}`"
                )));
            };
            let turn_id = ctx
                .turn_id
                .clone()
                .unwrap_or_else(crate::event::TurnId::now);
            let (mut final_messages, prompt_for_budget) = if let Some(msgs) = messages_override {
                let budget_text = msgs.last().map(|m| m.text_concat()).unwrap_or_default();
                (msgs, budget_text)
            } else {
                let Some(mut prompt_text) = prompt else {
                    return Value::Err(RuntimeError::MissingArg(
                        "llm node: either `prompt:` or `messages:` required".into(),
                    ));
                };
                if let Some(budget) = context_budget {
                    let (truncated, stat) = truncate_prompt_to_budget_tracked(prompt_text, budget);
                    prompt_text = truncated;
                    if let (Some(sink), Some(stat)) = (ctx.events, stat) {
                        sink.emit(crate::event::Event::ContextTruncated {
                            seq: 0,
                            turn_id: Some(turn_id.clone()),
                            flow_run_id: ctx.flow_run_id.clone(),
                            original_chars: stat.original_chars as u64,
                            result_chars: stat.result_chars as u64,
                            dropped_chars: stat.dropped_chars as u64,
                            budget_tokens: stat.budget_tokens,
                            ts: chrono::Utc::now(),
                        });
                    }
                }
                let user_msg =
                    crate::message::Message::user_text(turn_id.clone(), prompt_text.clone());
                (vec![user_msg], prompt_text)
            };
            if let Some(session) = ctx.session
                && let Some(l3_or_l2) = session.peek_pending_l2_or_higher(&turn_id)
                && matches!(l3_or_l2.level, crate::injection::InjectionLevel::L3Redirect)
                && let Some(target) = &l3_or_l2.redirect_target
            {
                session.mark_injection_consumed(&l3_or_l2.id);
                return Value::Err(RuntimeError::Redirect(target.clone()));
            }
            if let Some(session) = ctx.session {
                let injections = session.drain_injections(&turn_id);
                let renderable: Vec<crate::injection::Injection> = injections
                    .into_iter()
                    .filter(|i| {
                        matches!(
                            i.level,
                            crate::injection::InjectionLevel::L1Nudge
                                | crate::injection::InjectionLevel::L2CourseCorrect
                        )
                    })
                    .collect();
                if !renderable.is_empty() {
                    let rendered = render_injections(&renderable);
                    final_messages.push(crate::message::Message::user_text(
                        turn_id.clone(),
                        rendered,
                    ));
                }
            }
            let prompt = prompt_for_budget;
            let mut rewrite_used = false;
            if let Some(safety) = ctx.safety
                && safety.enabled
            {
                let scan_text = final_messages
                    .last()
                    .map(|m| m.text_concat())
                    .unwrap_or_else(|| prompt.clone());
                let verdict = match safety.classifier.scan(&scan_text).await {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("[atman] safety scan skipped: {e}");
                        crate::safety::ScanVerdict::Pass
                    }
                };
                if !verdict.is_pass()
                    && let Some(sink) = ctx.events
                {
                    let action = match (&verdict, safety.mode) {
                        (crate::safety::ScanVerdict::Deny(_), crate::safety::SafetyMode::Deny) => {
                            "blocked"
                        }
                        _ => "warned",
                    };
                    for category in verdict.categories() {
                        sink.emit(crate::event::Event::ContentFilterHit {
                            seq: 0,
                            turn_id: Some(turn_id.clone()),
                            flow_run_id: ctx.flow_run_id.clone(),
                            provider: safety.classifier.kind().to_string(),
                            model: model.clone(),
                            category: category.clone(),
                            action: action.to_string(),
                            ts: chrono::Utc::now(),
                        });
                    }
                }
                if verdict.is_deny() && safety.mode == crate::safety::SafetyMode::Deny {
                    let cats = verdict.categories().join(", ");
                    return Value::Err(RuntimeError::ToolFailed(format!(
                        "safety: content_filter blocked prompt (categories: {cats})"
                    )));
                }
            }
            if let Some(session) = ctx.session
                && let Some(goal) = session.goal()
            {
                let prefix = format!("[session goal]\n{goal}\n[/session goal]");
                system = Some(match system.take() {
                    Some(existing) if !existing.is_empty() => format!("{prefix}\n\n{existing}"),
                    _ => prefix,
                });
            }
            let mut last_err: Option<RuntimeError> = None;
            let retry_kinds_ref = retry_kinds.as_ref();
            for attempt in 0..=retry_count {
                let req = crate::provider::LlmRequest {
                    model: model.clone(),
                    messages: final_messages.clone(),
                    system: system.clone(),
                    input: input.clone(),
                    schema: None,
                    cache_prompt,
                    tools: tool_specs.clone(),
                };
                let start = std::time::Instant::now();
                let outcome = call_and_maybe_stream(provider.as_ref(), req, ctx.session).await;
                let elapsed_ms = start.elapsed().as_millis() as u64;
                let usage = match &outcome {
                    Ok(am) => crate::provider::TokenUsage {
                        input: am
                            .token_usage
                            .input
                            .max(crate::provider::estimate_tokens(&prompt)),
                        cached_input: am.token_usage.cached_input,
                        output: am
                            .token_usage
                            .output
                            .max(crate::provider::estimate_tokens(&am.text_concat())),
                        cache_write: am.token_usage.cache_write,
                    },
                    Err(_) => crate::provider::TokenUsage {
                        input: crate::provider::estimate_tokens(&prompt),
                        ..Default::default()
                    },
                };
                let status = match &outcome {
                    Ok(_) => crate::event::LlmCallStatus::Ok,
                    Err(e) => crate::event::LlmCallStatus::Errored {
                        message: e.to_string(),
                    },
                };
                if let Some(sink) = ctx.events {
                    sink.emit(crate::event::Event::LlmCall {
                        seq: 0,
                        model: model.clone(),
                        provider: provider.name().to_string(),
                        usage: usage.clone(),
                        wallclock_ms: elapsed_ms,
                        status,
                        ts: chrono::Utc::now(),
                    });
                }
                if let Some(session) = ctx.session {
                    session.record_llm_call(&model, usage.input + usage.cached_input, usage.output);
                }
                match outcome {
                    Ok(am) => {
                        if let Some(session) = ctx.session {
                            session.append_message(am.message.clone(), ctx.flow_run_id.clone());
                            crate::compaction::maybe_auto_compact(session, &model);
                        }
                        return crate::provider::assistant_message_to_value(&am);
                    }
                    Err(e) => {
                        if !rewrite_used
                            && let Some(safety) = ctx.safety
                            && safety.enabled
                            && safety.auto_rewrite
                            && matches!(e.kind(), crate::error::ErrorKind::ContentFilter)
                        {
                            rewrite_used = true;
                            if let Some(last) = final_messages.last_mut()
                                && let Some(part) = last.parts.iter_mut().find_map(|p| match p {
                                    crate::message::MessagePart::Text { text } => Some(text),
                                    _ => None,
                                })
                            {
                                *part = format!(
                                    "Please rewrite the following in a neutral, safety-compliant way and answer it:\n{part}"
                                );
                            }
                            if let Some(sink) = ctx.events {
                                sink.emit(crate::event::Event::ContentFilterHit {
                                    seq: 0,
                                    turn_id: Some(turn_id.clone()),
                                    flow_run_id: ctx.flow_run_id.clone(),
                                    provider: provider.name().to_string(),
                                    model: model.clone(),
                                    category: "auto_rewrite".to_string(),
                                    action: "rewritten".to_string(),
                                    ts: chrono::Utc::now(),
                                });
                            }
                            last_err = Some(e);
                            continue;
                        }
                        if let Some(allowed) = retry_kinds_ref
                            && attempt < retry_count
                            && !allowed.contains(&e.kind())
                        {
                            last_err = Some(e);
                            break;
                        }
                        last_err = Some(e);
                    }
                }
            }
            if let Some(fb) = fallback_expr {
                return eval_expr(fb, env, ctx).await;
            }
            Value::Err(last_err.unwrap_or(RuntimeError::ToolFailed("llm failed".into())))
        }
        Node::UserConfirm { msg } => {
            let v = eval_expr(msg, env, ctx).await;
            if v.is_err() {
                return v;
            }
            Value::Bool(true)
        }
        Node::FixUntilTestPasses { kwargs } => eval_fix_until_test_passes(kwargs, env, ctx).await,
        Node::Message { role, args } => eval_message_node(*role, args, env, ctx).await,
        Node::Subflow { name, args } => {
            let Some(target) = ctx.flows.get(&name.name) else {
                return Value::Err(RuntimeError::UndefinedTool(format!(
                    "subflow({})",
                    name.name
                )));
            };
            let mut bindings = Vec::with_capacity(args.len());
            for (i, arg) in args.iter().enumerate() {
                let (param_name, value) = match arg {
                    Arg::Positional(e) => {
                        let Some((pname, _)) = target.params.get(i) else {
                            return Value::Err(RuntimeError::MissingArg(format!(
                                "subflow({}): too many positional args",
                                name.name
                            )));
                        };
                        let v = eval_expr(e, env, ctx).await;
                        (pname.name.clone(), v)
                    }
                    Arg::Named { name: n, value } => {
                        let v = eval_expr(value, env, ctx).await;
                        (n.name.clone(), v)
                    }
                };
                if value.is_err() {
                    return value;
                }
                bindings.push((param_name, value));
            }
            let mut sub_env = Env::new();
            for (n, v) in bindings {
                sub_env.bind(n, v);
            }
            let sub_run_id = crate::event::FlowRunId::now();
            if let Some(sink) = ctx.events {
                sink.emit(crate::event::Event::FlowStart {
                    seq: 0,
                    run_id: sub_run_id.clone(),
                    flow_name: name.name.clone(),
                    parent_run_id: ctx.flow_run_id.clone(),
                    parent_node_id: ctx.current_node_id.clone(),
                    ts: chrono::Utc::now(),
                });
            }
            if let Some(session) = ctx.session {
                let _ = session
                    .stream_tx()
                    .send(crate::stream::StreamFrame::FlowStart {
                        run_id: sub_run_id.0.to_string(),
                        flow_name: name.name.clone(),
                        parent_run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                        parent_node_id: ctx.current_node_id.clone(),
                    });
            }
            let sub_ctx = EvalCtx {
                flow_run_id: Some(sub_run_id.clone()),
                current_node_id: None,
                ..ctx.clone()
            };
            let outcome = crate::exec::exec_stmts(&target.body, &mut sub_env, &sub_ctx).await;
            let (result, status, ok) = match outcome {
                crate::exec::StmtOutcome::Return(v) => (v, crate::event::FlowStatus::Ok, true),
                crate::exec::StmtOutcome::Err(e) => (
                    Value::Err(e.clone()),
                    crate::event::FlowStatus::Errored {
                        message: format!("{e}"),
                    },
                    false,
                ),
                crate::exec::StmtOutcome::Continue => {
                    (Value::Unit, crate::event::FlowStatus::Ok, true)
                }
            };
            if let Some(sink) = ctx.events {
                sink.emit(crate::event::Event::FlowEnd {
                    seq: 0,
                    run_id: sub_run_id.clone(),
                    flow_name: name.name.clone(),
                    status,
                    ts: chrono::Utc::now(),
                });
            }
            if let Some(session) = ctx.session {
                let _ = session
                    .stream_tx()
                    .send(crate::stream::StreamFrame::FlowDone {
                        run_id: sub_run_id.0.to_string(),
                        flow_name: name.name.clone(),
                        ok,
                    });
            }
            result
        }
    }
}

fn tool_name(path: &[atman_dsl::ast::Ident]) -> String {
    let parts: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
    parts.join(".")
}

async fn eval_fix_until_test_passes<'a>(
    kwargs: &'a atman_dsl::ast::Kwargs,
    env: &'a Env,
    ctx: &'a EvalCtx<'a>,
) -> Value {
    let mut edit_flow_expr: Option<&Expr> = None;
    let mut test_expr: Option<&Expr> = None;
    let mut on_giveup_expr: Option<&Expr> = None;
    let mut max_iters: u32 = 5;
    let mut target_path: Option<std::path::PathBuf> = None;

    for (k, v) in kwargs {
        match k.name.as_str() {
            "edit_flow" => edit_flow_expr = Some(v),
            "test" => test_expr = Some(v),
            "on_giveup" => on_giveup_expr = Some(v),
            "max_iters" => match eval_expr(v, env, ctx).await {
                Value::Int(n) if n > 0 => max_iters = n as u32,
                other => {
                    return Value::Err(RuntimeError::TypeMismatch {
                        expected: "positive int (max_iters)".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            "target" => match eval_expr(v, env, ctx).await {
                Value::Path(p) => target_path = Some(p),
                Value::Str(s) => target_path = Some(std::path::PathBuf::from(s)),
                Value::Unit => {}
                other => {
                    return Value::Err(RuntimeError::TypeMismatch {
                        expected: "path (target)".into(),
                        actual: other.kind_name().into(),
                    });
                }
            },
            _ => {}
        }
    }

    let Some(edit_flow_expr) = edit_flow_expr else {
        return Value::Err(RuntimeError::MissingArg(
            "fix_until_test_passes.edit_flow".into(),
        ));
    };
    let Some(test_expr) = test_expr else {
        return Value::Err(RuntimeError::MissingArg(
            "fix_until_test_passes.test".into(),
        ));
    };

    let pristine: Option<String> = match &target_path {
        Some(p) => match tokio::fs::read_to_string(p).await {
            Ok(s) => Some(s),
            Err(e) => {
                return Value::Err(RuntimeError::ToolFailed(format!(
                    "fix_until_test_passes: cannot read target {}: {e}",
                    p.display()
                )));
            }
        },
        None => None,
    };

    let mut prev_fail = String::new();
    let mut last_test_result: Option<Value> = None;

    for iter in 0..max_iters {
        let mut loop_env = env.clone();
        loop_env.bind("iter", Value::Int(iter as i64));
        loop_env.bind("prev_fail", Value::Str(prev_fail.clone()));

        let edit_v = eval_expr(edit_flow_expr, &loop_env, ctx).await;
        if edit_v.is_err() {
            return edit_v;
        }
        loop_env.bind("last_edit", edit_v);

        let test_v = eval_expr(test_expr, &loop_env, ctx).await;
        if test_v.is_err() {
            return test_v;
        }
        let exit = test_v.field("exit").and_then(|v| match v {
            Value::Int(n) => Some(*n),
            _ => None,
        });
        last_test_result = Some(test_v.clone());
        if let Some(0) = exit {
            return Value::Struct(vec![
                ("status".into(), Value::Str("passed".into())),
                ("iters".into(), Value::Int((iter + 1) as i64)),
                ("test".into(), test_v),
            ]);
        }
        let stderr_tail = test_v
            .field("stderr_tail")
            .and_then(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let stdout_tail = test_v
            .field("stdout_tail")
            .and_then(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default();
        prev_fail = format!(
            "iter {iter} exit={:?}\n--- stderr ---\n{stderr_tail}\n--- stdout ---\n{stdout_tail}",
            exit
        );

        if let (Some(target), Some(pristine)) = (&target_path, &pristine)
            && let Err(e) = tokio::fs::write(target, pristine.as_bytes()).await
        {
            return Value::Err(RuntimeError::ToolFailed(format!(
                "fix_until_test_passes: revert failed on {}: {e}",
                target.display()
            )));
        }
    }

    if let Some(giveup) = on_giveup_expr {
        let mut giveup_env = env.clone();
        giveup_env.bind("iters", Value::Int(max_iters as i64));
        giveup_env.bind("prev_fail", Value::Str(prev_fail));
        return eval_expr(giveup, &giveup_env, ctx).await;
    }

    Value::Struct(vec![
        ("status".into(), Value::Str("gave_up".into())),
        ("iters".into(), Value::Int(max_iters as i64)),
        ("last_test".into(), last_test_result.unwrap_or(Value::Unit)),
    ])
}

async fn eval_message_node<'a>(
    ast_role: atman_dsl::ast::MessageRole,
    args: &'a [Arg],
    env: &'a Env,
    ctx: &'a EvalCtx<'a>,
) -> Value {
    use crate::message::{ImageData, ImageSource, Message, MessagePart, MessageRole};

    let role = match ast_role {
        atman_dsl::ast::MessageRole::User => MessageRole::User,
        atman_dsl::ast::MessageRole::Assistant => MessageRole::Assistant,
        atman_dsl::ast::MessageRole::System => MessageRole::System,
        atman_dsl::ast::MessageRole::Tool => MessageRole::Tool,
    };
    let turn_id = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);

    let mut positional = Vec::new();
    let mut named: Vec<(String, Value)> = Vec::new();
    let mut attachment_paths_raw: Option<Vec<std::path::PathBuf>> = None;
    for arg in args {
        match arg {
            Arg::Positional(e) => {
                let v = eval_expr(e, env, ctx).await;
                if v.is_err() {
                    return v;
                }
                positional.push(v);
            }
            Arg::Named { name, value } => {
                if name.name == "attachments" {
                    if let Expr::List(items) = value {
                        let mut collected = Vec::with_capacity(items.len());
                        let mut all_fileref = true;
                        for it in items {
                            if let Expr::FileRef(f) = it {
                                collected.push(std::path::PathBuf::from(&f.path));
                            } else {
                                all_fileref = false;
                                break;
                            }
                        }
                        if all_fileref {
                            attachment_paths_raw = Some(collected);
                            continue;
                        }
                    }
                }
                let v = eval_expr(value, env, ctx).await;
                if v.is_err() {
                    return v;
                }
                named.push((name.name.clone(), v));
            }
        }
    }
    let take_named = |k: &str, named: &mut Vec<(String, Value)>| -> Option<Value> {
        let pos = named.iter().position(|(n, _)| n == k)?;
        Some(named.remove(pos).1)
    };

    if role == MessageRole::Tool {
        let tool_use_id = match positional.first().or(take_named("id", &mut named).as_ref()) {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => {
                return Value::Err(RuntimeError::TypeMismatch {
                    expected: "string (tool_use_id)".into(),
                    actual: other.kind_name().into(),
                });
            }
            None => {
                return Value::Err(RuntimeError::MissingArg("tool_result: id".into()));
            }
        };
        let content = match positional
            .get(1)
            .or(take_named("content", &mut named).as_ref())
        {
            Some(Value::Str(s)) => s.clone(),
            Some(other) => {
                return Value::Err(RuntimeError::TypeMismatch {
                    expected: "string (content)".into(),
                    actual: other.kind_name().into(),
                });
            }
            None => {
                return Value::Err(RuntimeError::MissingArg("tool_result: content".into()));
            }
        };
        let is_error = match take_named("is_error", &mut named) {
            Some(Value::Bool(b)) => b,
            Some(other) => {
                return Value::Err(RuntimeError::TypeMismatch {
                    expected: "bool (is_error)".into(),
                    actual: other.kind_name().into(),
                });
            }
            None => false,
        };
        return Value::Message(Message {
            role,
            parts: vec![MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            }],
            turn_id,
        });
    }

    let text = match positional.first() {
        Some(Value::Str(s)) => Some(s.clone()),
        Some(other) => {
            return Value::Err(RuntimeError::TypeMismatch {
                expected: "string (message text)".into(),
                actual: other.kind_name().into(),
            });
        }
        None => None,
    };
    let attachment_paths: Vec<std::path::PathBuf> = if let Some(raw) = attachment_paths_raw {
        raw
    } else {
        match take_named("attachments", &mut named) {
            Some(Value::List(items)) => {
                let mut ps = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        Value::Path(p) => ps.push(p),
                        Value::Str(s) => ps.push(std::path::PathBuf::from(s)),
                        other => {
                            return Value::Err(RuntimeError::TypeMismatch {
                                expected: "path (attachment)".into(),
                                actual: other.kind_name().into(),
                            });
                        }
                    }
                }
                ps
            }
            Some(other) => {
                return Value::Err(RuntimeError::TypeMismatch {
                    expected: "list of path".into(),
                    actual: other.kind_name().into(),
                });
            }
            None => Vec::new(),
        }
    };

    let mut parts: Vec<MessagePart> = attachment_paths
        .into_iter()
        .map(|path| {
            let media_type = guess_image_mime(&path).unwrap_or_else(|| "image/png".to_string());
            MessagePart::Image {
                source: ImageSource {
                    media_type,
                    data: ImageData::Path { path },
                },
            }
        })
        .collect();
    if let Some(t) = text {
        parts.push(MessagePart::Text { text: t });
    }

    Value::Message(Message {
        role,
        parts,
        turn_id,
    })
}

fn render_injections(injections: &[crate::injection::Injection]) -> String {
    use crate::injection::InjectionLevel;
    let mut out = String::from(
        "The user sent the following steering message(s) while you were working. \
         Apply them to your next step if still relevant.\n\n",
    );
    for inj in injections {
        let tag = match inj.level {
            InjectionLevel::L2CourseCorrect => "user_correction",
            _ => "user_nudge",
        };
        out.push_str(&format!(
            "<{tag} id=\"{}\" ts=\"{}\">\n{}\n</{tag}>\n",
            inj.id.0,
            inj.created_at.to_rfc3339(),
            inj.text
        ));
    }
    out
}

fn guess_image_mime(path: &std::path::Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        }
        .to_string(),
    )
}

fn contract_allows_shell(contract: Option<&atman_dsl::ast::Contract>) -> bool {
    let Some(c) = contract else { return false };
    for block in &c.blocks {
        if block.name.name != "capabilities" {
            continue;
        }
        for (k, v) in &block.kwargs {
            if k.name != "shell" {
                continue;
            }
            if let atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Bool(true)) = v {
                return true;
            }
        }
    }
    false
}

pub struct TruncationStat {
    pub original_chars: usize,
    pub result_chars: usize,
    pub dropped_chars: usize,
    pub budget_tokens: u64,
}

fn resolve_tool_specs(
    expr: &Expr,
    tools: &crate::tool::ToolRegistry,
) -> Result<Vec<crate::tool::ToolSpec>, String> {
    let items = match expr {
        Expr::List(items) => items,
        _ => {
            return Err(
                "llm.tools: expected a list of tool references like [fs.read, bash.exec]".into(),
            );
        }
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let name = match tool_ref_name(item) {
            Some(n) => n,
            None => {
                return Err(format!(
                    "llm.tools: item is not a tool reference (want ident or ident.method): {item:?}"
                ));
            }
        };
        let tool = tools
            .get(&name)
            .ok_or_else(|| format!("llm.tools: unknown tool `{name}`"))?;
        out.push(crate::tool::tool_spec(tool.as_ref()));
    }
    Ok(out)
}

fn tool_ref_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(id) => Some(id.name.clone()),
        Expr::Member { base, field } => {
            let base = tool_ref_name(base)?;
            Some(format!("{base}.{}", field.name))
        }
        _ => None,
    }
}

fn parse_error_kind_list(
    expr: &Expr,
) -> Result<std::collections::HashSet<crate::error::ErrorKind>, String> {
    let items = match expr {
        Expr::List(items) => items,
        _ => {
            return Err(
                "retry_classified: expected a list literal like [timeout, rate_limit]".into(),
            );
        }
    };
    let mut out = std::collections::HashSet::new();
    for item in items {
        let name = match item {
            Expr::Ident(id) => id.name.clone(),
            Expr::Literal(atman_dsl::ast::Literal::Str(s)) => s.clone(),
            _ => {
                return Err(
                    "retry_classified: each item must be an identifier or string kind name".into(),
                );
            }
        };
        match crate::error::ErrorKind::from_name(&name) {
            Some(k) => {
                out.insert(k);
            }
            None => return Err(format!("retry_classified: unknown error kind `{name}`")),
        }
    }
    Ok(out)
}

pub fn truncate_prompt_to_budget(prompt: String, budget_tokens: u64) -> String {
    truncate_prompt_to_budget_tracked(prompt, budget_tokens).0
}

pub fn truncate_prompt_to_budget_tracked(
    prompt: String,
    budget_tokens: u64,
) -> (String, Option<TruncationStat>) {
    let budget_chars = budget_tokens.saturating_mul(4) as usize;
    if prompt.len() <= budget_chars {
        return (prompt, None);
    }
    let head_chars = budget_chars * 4 / 10;
    let tail_chars = budget_chars * 4 / 10;
    if head_chars + tail_chars >= prompt.len() {
        return (prompt, None);
    }
    let original_chars = prompt.len();
    let head_end = char_boundary(&prompt, head_chars, false);
    let tail_start = char_boundary(&prompt, prompt.len().saturating_sub(tail_chars), true);
    let head = &prompt[..head_end];
    let tail = &prompt[tail_start..];
    let dropped = original_chars - head.len() - tail.len();
    let result = format!("{head}\n\n[... truncated {dropped} chars ...]\n\n{tail}");
    let stat = TruncationStat {
        original_chars,
        result_chars: result.len(),
        dropped_chars: dropped,
        budget_tokens,
    };
    (result, Some(stat))
}

fn char_boundary(s: &str, target: usize, round_up: bool) -> usize {
    let mut idx = target.min(s.len());
    while idx > 0 && idx < s.len() && !s.is_char_boundary(idx) {
        if round_up {
            idx += 1;
        } else {
            idx -= 1;
        }
    }
    idx
}

// Bare primitive names inside `schema: { valid: bool, ... }` parse as tool calls; treat as Unit.
fn is_type_annotation(path: &[atman_dsl::ast::Ident]) -> bool {
    if path.len() != 1 {
        return false;
    }
    matches!(
        path[0].name.as_str(),
        "bool" | "int" | "float" | "string" | "path" | "bytes" | "duration"
    )
}

fn eval_literal(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
    }
}

fn eval_binop(op: BinOp, l: &Value, r: &Value) -> Value {
    match op {
        BinOp::Eq => Value::Bool(value_eq(l, r)),
        BinOp::Ne => Value::Bool(!value_eq(l, r)),
        BinOp::Lt => value_cmp(l, r, |a, b| a < b, |a, b| a < b, |a, b| a < b),
        BinOp::Le => value_cmp(l, r, |a, b| a <= b, |a, b| a <= b, |a, b| a <= b),
        BinOp::Gt => value_cmp(l, r, |a, b| a > b, |a, b| a > b, |a, b| a > b),
        BinOp::Ge => value_cmp(l, r, |a, b| a >= b, |a, b| a >= b, |a, b| a >= b),
        BinOp::And => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a && *b),
            _ => type_mismatch("bool && bool", l, r),
        },
        BinOp::Or => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a || *b),
            _ => type_mismatch("bool || bool", l, r),
        },
        BinOp::Add => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Str(a), Value::Str(b)) => Value::Str(format!("{a}{b}")),
            (Value::Str(a), Value::Path(b)) => Value::Str(format!("{a}{}", b.display())),
            (Value::Path(a), Value::Str(b)) => Value::Str(format!("{}{b}", a.display())),
            _ => type_mismatch(
                "int+int | float+float | string+string | string+path | path+string",
                l,
                r,
            ),
        },
        BinOp::Sub => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a - b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            _ => type_mismatch("int-int | float-float", l, r),
        },
        BinOp::Mul => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a * b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            _ => type_mismatch("int*int | float*float", l, r),
        },
        BinOp::Div => match (l, r) {
            (Value::Int(_), Value::Int(0)) => {
                Value::Err(RuntimeError::ToolFailed("integer div by zero".into()))
            }
            (Value::Int(a), Value::Int(b)) => Value::Int(a / b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            _ => type_mismatch("int/int | float/float", l, r),
        },
        BinOp::Mod => match (l, r) {
            (Value::Int(_), Value::Int(0)) => {
                Value::Err(RuntimeError::ToolFailed("integer mod by zero".into()))
            }
            (Value::Int(a), Value::Int(b)) => Value::Int(a % b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a % b),
            _ => type_mismatch("int%int | float%float", l, r),
        },
    }
}

fn eval_unop(op: UnOp, v: &Value) -> Value {
    match op {
        UnOp::Not => match v {
            Value::Bool(b) => Value::Bool(!b),
            other => Value::Err(RuntimeError::TypeMismatch {
                expected: "bool".into(),
                actual: other.kind_name().into(),
            }),
        },
        UnOp::Neg => match v {
            Value::Int(n) => Value::Int(-n),
            Value::Float(n) => Value::Float(-n),
            other => Value::Err(RuntimeError::TypeMismatch {
                expected: "int or float".into(),
                actual: other.kind_name().into(),
            }),
        },
    }
}

fn value_eq(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Path(a), Value::Path(b)) => a == b,
        _ => false,
    }
}

fn value_cmp(
    l: &Value,
    r: &Value,
    int_cmp: fn(i64, i64) -> bool,
    float_cmp: fn(f64, f64) -> bool,
    str_cmp: fn(&str, &str) -> bool,
) -> Value {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Value::Bool(int_cmp(*a, *b)),
        (Value::Float(a), Value::Float(b)) => Value::Bool(float_cmp(*a, *b)),
        (Value::Str(a), Value::Str(b)) => Value::Bool(str_cmp(a, b)),
        _ => type_mismatch("comparable pair", l, r),
    }
}

fn type_mismatch(expected: &str, l: &Value, r: &Value) -> Value {
    Value::Err(RuntimeError::TypeMismatch {
        expected: expected.into(),
        actual: format!("{} vs {}", l.kind_name(), r.kind_name()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    async fn eval_snippet(expr_src: &str) -> Value {
        let src = format!("flow t() {{\n    return {expr_src}\n}}\n");
        let file = parse_file(&src).expect("parse test snippet");
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };
        let stmt = &file.flows[0].body[0];
        if let atman_dsl::ast::Stmt::Return { value } = stmt {
            eval_expr(value, &Env::new(), &ctx).await
        } else {
            panic!("expected return statement");
        }
    }

    #[tokio::test]
    async fn literals_evaluate() {
        assert!(matches!(eval_snippet("42").await, Value::Int(42)));
        assert!(matches!(eval_snippet("true").await, Value::Bool(true)));
        assert!(matches!(
            eval_snippet(r#""hello""#).await,
            Value::Str(s) if s == "hello"
        ));
    }

    #[tokio::test]
    async fn undefined_ident_yields_err_value() {
        assert!(matches!(
            eval_snippet("missing").await,
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[tokio::test]
    async fn binary_arithmetic_and_comparison() {
        assert!(matches!(eval_snippet("1 == 1").await, Value::Bool(true)));
        assert!(matches!(eval_snippet("2 < 3").await, Value::Bool(true)));
        assert!(matches!(
            eval_snippet(r#""a" + "b""#).await,
            Value::Str(s) if s == "ab"
        ));
    }

    #[tokio::test]
    async fn type_mismatch_bubbles_up() {
        assert!(matches!(
            eval_snippet(r#"1 + "x""#).await,
            Value::Err(RuntimeError::TypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn err_short_circuits_binary() {
        assert!(matches!(
            eval_snippet("missing == 1").await,
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[tokio::test]
    async fn list_evaluates_all_items() {
        let v = eval_snippet("[1, 2, 3]").await;
        if let Value::List(items) = v {
            assert_eq!(items.len(), 3);
            assert!(matches!(items[2], Value::Int(3)));
        } else {
            panic!("expected list");
        }
    }

    #[tokio::test]
    async fn struct_literal_evaluates_fields_in_order() {
        let v = eval_snippet(r#"{ severity: "critical", count: 3 }"#).await;
        if let Value::Struct(fields) = v {
            assert_eq!(fields[0].0, "severity");
            assert_eq!(fields[1].0, "count");
        } else {
            panic!("expected struct");
        }
    }

    #[tokio::test]
    async fn undefined_tool_returns_undefined_tool_err() {
        let src = r#"flow t() { return fs.readnope("/tmp") }"#;
        let file = parse_file(src).unwrap();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            assert!(matches!(
                v,
                Value::Err(RuntimeError::UndefinedTool(name)) if name == "fs.readnope"
            ));
        }
    }

    #[tokio::test]
    async fn fanout_all_gathers_results_in_order() {
        use crate::tools::fs::FsRead;
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let pa = dir.path().join("a.txt");
        let pb = dir.path().join("b.txt");
        tokio::fs::write(&pa, b"AAA").await.unwrap();
        tokio::fs::write(&pb, b"BBB").await.unwrap();

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(FsRead));
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };

        let mut env = Env::new();
        env.bind("a", Value::Path(pa));
        env.bind("b", Value::Path(pb));

        let src = r#"flow t() { return fanout [ fs.read(a), fs.read(b) ] collect: all }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &env, &ctx).await;
            if let Value::List(items) = v {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0], Value::Str(s) if s == "AAA"));
                assert!(matches!(&items[1], Value::Str(s) if s == "BBB"));
            } else {
                panic!("expected list");
            }
        }
    }

    #[tokio::test]
    async fn fanout_all_short_circuits_on_err() {
        let src = r#"flow t() { return fanout [ 1, missing, 3 ] collect: all }"#;
        let file = parse_file(src).unwrap();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            assert!(matches!(
                v,
                Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
            ));
        }
    }

    #[tokio::test]
    async fn llm_node_dispatches_to_mock_provider() {
        use crate::providers::mock::MockProvider;
        use std::sync::Arc;

        let mut providers = crate::provider::ProviderRegistry::new();
        providers.register(Arc::new(MockProvider::new("mock").with_model(
            "claude-opus-4.7",
            Value::Struct(vec![("severity".into(), Value::Str("info".into()))]),
        )));
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };

        let src = r#"flow t() {
    return llm {
        model: "claude-opus-4.7"
        prompt: "review please"
        input: 1
    }
}
"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            if let Value::Struct(fields) = v {
                assert_eq!(fields[0].0, "severity");
                assert!(matches!(&fields[0].1, Value::Str(s) if s == "info"));
            } else {
                panic!("expected struct");
            }
        }
    }

    #[tokio::test]
    async fn llm_missing_model_reports_missing_arg() {
        let providers = crate::provider::ProviderRegistry::new();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };
        let src = r#"flow t() { return llm { prompt: "hi" } }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            assert!(matches!(
                v,
                Value::Err(RuntimeError::MissingArg(name)) if name == "llm.model"
            ));
        }
    }

    #[tokio::test]
    async fn user_confirm_stub_returns_true() {
        let providers = crate::provider::ProviderRegistry::new();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };
        let src = r#"flow t() { return user_confirm("proceed?") }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            assert!(matches!(
                eval_expr(value, &Env::new(), &ctx).await,
                Value::Bool(true)
            ));
        }
    }

    #[tokio::test]
    async fn subflow_calls_target_flow_with_positional_args() {
        let src = r#"flow child(n: Int) -> Int {
    return n + 100
}

flow parent(x: Int) -> Int {
    y = subflow(child, x)
    return y + 1
}
"#;
        let file = parse_file(src).unwrap();
        let flows_map: std::collections::HashMap<_, _> = file
            .flows
            .iter()
            .map(|f| (f.name.name.clone(), f.clone()))
            .collect();
        let parent = &file.flows[1];
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let out = crate::exec::exec_flow_with_siblings(
            parent,
            vec![("x".into(), Value::Int(5))],
            &tools,
            &tool_ctx,
            &providers,
            &flows_map,
            None,
            None,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(106)));
    }

    #[tokio::test]
    async fn subflow_missing_target_reports_undefined_tool() {
        let src = r#"flow parent() -> Int {
    return subflow(nope, 1)
}
"#;
        let file = parse_file(src).unwrap();
        let flows: std::collections::HashMap<_, _> = file
            .flows
            .iter()
            .map(|f| (f.name.name.clone(), f.clone()))
            .collect();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let err = crate::exec::exec_flow_with_siblings(
            &file.flows[0],
            vec![],
            &tools,
            &tool_ctx,
            &providers,
            &flows,
            None,
            None,
            None,
            None,
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::UndefinedTool(name) if name.contains("nope")));
    }

    #[tokio::test]
    async fn tool_call_dispatches_via_registry() {
        use crate::tools::fs::FsRead;
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hi.txt");
        tokio::fs::write(&path, b"hello runtime").await.unwrap();

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(FsRead));
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: None,
            turn_id: None,
            flow_run_id: None,
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: None,
        };

        let mut env = Env::new();
        env.bind("p", Value::Path(path));

        let src = r#"flow t() { return fs.read(p) }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &env, &ctx).await;
            assert!(matches!(v, Value::Str(s) if s == "hello runtime"));
        }
    }

    #[tokio::test]
    async fn fanout_emits_branch_start_end_events_with_parent_linkage() {
        let src = r#"flow t() { return fanout [1, 2, 3] collect: all }"#;
        let file = parse_file(src).unwrap();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let events = crate::event::EventSink::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
            providers: &providers,
            flows: &flows,
            contract: None,
            events: Some(&events),
            turn_id: None,
            flow_run_id: Some(crate::event::FlowRunId::now()),
            session: None,
            flow_cancel: tokio_util::sync::CancellationToken::new(),
            safety: None,
            current_node_id: Some("stmt_1".into()),
        };
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let _ = eval_expr(value, &Env::new(), &ctx).await;
        }
        let snap = events.snapshot();
        let starts: Vec<_> = snap
            .iter()
            .filter_map(|e| match e {
                crate::event::Event::FlowNodeStart {
                    node_id,
                    parent_node_id,
                    ..
                } => Some((node_id.clone(), parent_node_id.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(starts.len(), 3);
        assert_eq!(starts[0].0, "stmt_1.branch[0]");
        assert_eq!(starts[1].0, "stmt_1.branch[1]");
        assert_eq!(starts[2].0, "stmt_1.branch[2]");
        assert!(starts.iter().all(|(_, p)| p.as_deref() == Some("stmt_1")));
        let ends = snap
            .iter()
            .filter(|e| matches!(e, crate::event::Event::FlowNodeEnd { .. }))
            .count();
        assert_eq!(ends, 3);
    }
}
