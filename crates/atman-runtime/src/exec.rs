use std::collections::HashMap;

use atman_dsl::ast::{CmpOp, Expr, FlowDecl, Node, Stmt, WatchAction, WatchDecl, WatchEvent};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::eval::{EvalCtx, eval_expr};
use crate::event::NodeEvent;
use crate::provider::LlmRequest;
use crate::tool::{BoxFut, ToolCtx, ToolRegistry};
use crate::value::Value;

fn bind_pattern(
    pattern: &atman_dsl::ast::Pattern,
    value: Value,
    env: &mut Env,
) -> Result<(), RuntimeError> {
    use atman_dsl::ast::{Pattern, PatternFieldBinding};
    match pattern {
        Pattern::Ident(id) => {
            env.bind(id.name.clone(), value);
            Ok(())
        }
        Pattern::Struct { fields } => {
            let pairs = match value {
                Value::Struct(pairs) => pairs,
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "struct for destructuring bind".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            for field in fields {
                let Some((_, matched)) = pairs.iter().find(|(k, _)| k == &field.source.name) else {
                    return Err(RuntimeError::MissingArg(format!(
                        "destructure: struct has no field `{}`",
                        field.source.name
                    )));
                };
                match &field.binding {
                    PatternFieldBinding::Same => {
                        env.bind(field.source.name.clone(), matched.clone());
                    }
                    PatternFieldBinding::Rename(target) => {
                        env.bind(target.name.clone(), matched.clone());
                    }
                    PatternFieldBinding::Nested(inner) => {
                        bind_pattern(inner, matched.clone(), env)?;
                    }
                }
            }
            Ok(())
        }
    }
}

pub enum StmtOutcome {
    Continue,
    Return(Value),
    Err(RuntimeError),
}

pub fn exec_stmts<'a>(
    stmts: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
) -> BoxFut<'a, StmtOutcome> {
    exec_stmts_prefixed(stmts, env, ctx, String::new())
}

pub fn exec_stmts_prefixed<'a>(
    stmts: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
    prefix: String,
) -> BoxFut<'a, StmtOutcome> {
    Box::pin(async move {
        let watches = collect_watches(stmts);
        let parent_node_id = ctx.current_node_id.clone();
        for (i, stmt) in stmts.iter().enumerate() {
            let node_id = if prefix.is_empty() {
                format!("{i}")
            } else {
                format!("{prefix}.{i}")
            };
            emit_flow_node_start(ctx, &node_id, stmt, parent_node_id.as_deref());
            let stmt_ctx = ctx.with_node(&node_id);
            let (outcome, preview) = exec_stmt(stmt, env, &stmt_ctx, &watches).await;
            emit_flow_node_end(
                ctx,
                &node_id,
                &outcome,
                parent_node_id.as_deref(),
                preview.as_deref(),
            );
            match outcome {
                StmtOutcome::Continue => continue,
                other => return other,
            }
        }
        StmtOutcome::Continue
    })
}

fn emit_flow_node_start(
    ctx: &EvalCtx<'_>,
    node_id: &str,
    stmt: &Stmt,
    parent_node_id: Option<&str>,
) {
    let Some(session) = ctx.session else {
        return;
    };
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return;
    };
    let (kind, label) = stmt_to_node_kind_label(stmt);
    if let Some(sink) = ctx.events {
        sink.emit(crate::event::Event::FlowNodeStart {
            seq: 0,
            run_id: run_id.clone(),
            node_id: node_id.to_string(),
            kind: kind.clone(),
            label: label.clone(),
            parent_node_id: parent_node_id.map(String::from),
            ts: chrono::Utc::now(),
        });
    }
    let _ = session
        .stream_tx()
        .send(crate::stream::StreamFrame::FlowNodeStart {
            run_id: run_id.0.to_string(),
            node_id: node_id.to_string(),
            kind,
            label,
            parent_node_id: parent_node_id.map(String::from),
        });
}

fn value_preview(v: &Value) -> Option<String> {
    let raw = match v {
        Value::Str(s) => s.clone(),
        Value::Message(m) => m.text_concat(),
        Value::Path(p) => p.display().to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Unit => return None,
        Value::Err(e) => format!("err: {e}"),
        Value::List(items) => format!("list[{}]", items.len()),
        Value::Struct(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::EditProposal(_) => "<edit proposal>".into(),
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(4000).collect())
    }
}

fn emit_flow_node_end(
    ctx: &EvalCtx<'_>,
    node_id: &str,
    outcome: &StmtOutcome,
    parent_node_id: Option<&str>,
    output_preview: Option<&str>,
) {
    let Some(session) = ctx.session else {
        return;
    };
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return;
    };
    let status = match outcome {
        StmtOutcome::Err(_) => crate::event::FlowNodeStatus::Err,
        _ => crate::event::FlowNodeStatus::Ok,
    };
    let preview_owned = output_preview.map(String::from);
    if let Some(sink) = ctx.events {
        sink.emit(crate::event::Event::FlowNodeEnd {
            seq: 0,
            run_id: run_id.clone(),
            node_id: node_id.to_string(),
            status: status.clone(),
            output_preview: preview_owned.clone(),
            ts: chrono::Utc::now(),
        });
    }
    let _ = session
        .stream_tx()
        .send(crate::stream::StreamFrame::FlowNodeEnd {
            run_id: run_id.0.to_string(),
            node_id: node_id.to_string(),
            status,
            output_preview: preview_owned,
            parent_node_id: parent_node_id.map(String::from),
        });
}

fn stmt_to_node_kind_label(stmt: &Stmt) -> (crate::nodegraph::NodeKind, String) {
    use crate::nodegraph::NodeKind;
    match stmt {
        Stmt::Bind { value, .. } | Stmt::Expr(value) => expr_to_node_kind_label(value),
        Stmt::Return { .. } => (NodeKind::Return, "return".into()),
        Stmt::When { .. } => (
            NodeKind::When {
                condition_preview: "when".into(),
            },
            "when …".into(),
        ),
        Stmt::Watch(_) => (NodeKind::Return, "watch".into()),
    }
}

fn expr_to_node_kind_label(expr: &Expr) -> (crate::nodegraph::NodeKind, String) {
    use crate::nodegraph::NodeKind;
    match expr {
        Expr::Node(Node::Llm { .. }) => (NodeKind::Llm { model: None }, "llm".into()),
        Expr::Node(Node::ToolCall { path, .. }) => {
            let p = path
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>()
                .join(".");
            (NodeKind::ToolCall { path: p.clone() }, format!("⟶ {p}"))
        }
        Expr::Node(Node::Fanout { items, collect }) => (
            NodeKind::Fanout {
                collect: (*collect).into(),
            },
            format!("fanout ×{}", items.len()),
        ),
        Expr::Node(Node::Subflow { name, .. }) => (
            NodeKind::Subflow {
                name: name.name.clone(),
            },
            format!("subflow({})", name.name),
        ),
        _ => (NodeKind::Return, "expr".into()),
    }
}

fn collect_watches(stmts: &[Stmt]) -> HashMap<String, Vec<&WatchDecl>> {
    let mut out: HashMap<String, Vec<&WatchDecl>> = HashMap::new();
    for stmt in stmts {
        if let Stmt::Watch(w) = stmt {
            out.entry(w.target.name.clone()).or_default().push(w);
        }
    }
    out
}

fn exec_stmt<'a>(
    stmt: &'a Stmt,
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
    watches: &'a HashMap<String, Vec<&'a WatchDecl>>,
) -> BoxFut<'a, (StmtOutcome, Option<String>)> {
    Box::pin(async move {
        match stmt {
            Stmt::Bind { name, value } => {
                let watch_target = name.as_single_ident().map(|id| id.name.clone());
                let v = if let Some(target) = watch_target.as_ref()
                    && let Some(ws) = watches.get(target)
                {
                    match eval_bind_with_watches(value, env, ctx, ws).await {
                        Ok(v) => v,
                        Err(e) => return (StmtOutcome::Err(e), None),
                    }
                } else {
                    eval_expr(value, env, ctx).await
                };
                if let Value::Err(e) = v {
                    return (StmtOutcome::Err(e), None);
                }
                let preview = value_preview(&v);
                if let Err(e) = bind_pattern(name, v, env) {
                    return (StmtOutcome::Err(e), None);
                }
                (StmtOutcome::Continue, preview)
            }
            Stmt::When { cond, body } => {
                let c = eval_expr(cond, env, ctx).await;
                match c {
                    Value::Bool(true) => (exec_stmts(body, env, ctx).await, Some("true".into())),
                    Value::Bool(false) => (StmtOutcome::Continue, Some("false".into())),
                    Value::Err(e) => (StmtOutcome::Err(e), None),
                    other => (
                        StmtOutcome::Err(RuntimeError::TypeMismatch {
                            expected: "bool".into(),
                            actual: other.kind_name().into(),
                        }),
                        None,
                    ),
                }
            }
            Stmt::Return { value } => {
                let v = eval_expr(value, env, ctx).await;
                if let Value::Err(e) = v {
                    return (StmtOutcome::Err(e), None);
                }
                let preview = value_preview(&v);
                (StmtOutcome::Return(v), preview)
            }
            Stmt::Expr(e) => {
                let v = eval_expr(e, env, ctx).await;
                if let Value::Err(err) = v {
                    return (StmtOutcome::Err(err), None);
                }
                let preview = value_preview(&v);
                (StmtOutcome::Continue, preview)
            }
            Stmt::Watch(_) => (StmtOutcome::Continue, None),
        }
    })
}

async fn eval_bind_with_watches(
    expr: &Expr,
    env: &mut Env,
    ctx: &EvalCtx<'_>,
    watches: &[&WatchDecl],
) -> Result<Value, RuntimeError> {
    if let Expr::Node(Node::ToolCall { path, .. }) = expr {
        let name: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
        if name.join(".") == "bash.exec" {
            return eval_bash_with_watches(expr, env, ctx, watches).await;
        }
    }
    let Expr::Node(Node::Llm { kwargs }) = expr else {
        return Ok(eval_expr(expr, env, ctx).await);
    };

    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut input = Value::Unit;
    let mut cache_prompt = false;
    let mut context_budget: Option<u64> = None;
    for (k, v) in kwargs {
        if k.name == "schema" || k.name == "fallback" || k.name == "retry" {
            continue;
        }
        let val = eval_expr(v, env, ctx).await;
        if val.is_err() {
            return Ok(val);
        }
        match k.name.as_str() {
            "model" => match val {
                Value::Str(s) => model = Some(s),
                other => {
                    return Ok(Value::Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "prompt" => match val {
                Value::Str(s) => prompt = Some(s),
                other => {
                    return Ok(Value::Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "input" => input = val,
            "cache" => match val {
                Value::Bool(b) => cache_prompt = b,
                other => {
                    return Ok(Value::Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            "context_budget" => match val {
                Value::Int(n) if n > 0 => context_budget = Some(n as u64),
                other => {
                    return Ok(Value::Err(RuntimeError::TypeMismatch {
                        expected: "positive int".into(),
                        actual: other.kind_name().into(),
                    }));
                }
            },
            _ => {}
        }
    }
    let Some(model) = model else {
        return Ok(Value::Err(RuntimeError::MissingArg("llm.model".into())));
    };
    let Some(mut prompt) = prompt else {
        return Ok(Value::Err(RuntimeError::MissingArg("llm.prompt".into())));
    };
    if let Some(budget) = context_budget {
        let (truncated, stat) = crate::eval::truncate_prompt_to_budget_tracked(prompt, budget);
        prompt = truncated;
        if let (Some(sink), Some(stat)) = (ctx.events, stat) {
            sink.emit(crate::event::Event::ContextTruncated {
                seq: 0,
                turn_id: ctx.turn_id.clone(),
                flow_run_id: ctx.flow_run_id.clone(),
                original_chars: stat.original_chars as u64,
                result_chars: stat.result_chars as u64,
                dropped_chars: stat.dropped_chars as u64,
                budget_tokens: stat.budget_tokens,
                ts: chrono::Utc::now(),
            });
        }
    }
    let Some(provider) = ctx.providers.resolve(&model) else {
        return Ok(Value::Err(RuntimeError::ToolFailed(format!(
            "no provider registered for model `{model}`"
        ))));
    };

    let rules = collect_watch_rules(watches);
    let mut restart_count = 0u32;
    let mut correction: Option<String> = None;
    let mut prior_partial: Option<String> = None;
    loop {
        let mut messages = Vec::with_capacity(3);
        if let Some(partial) = &prior_partial {
            messages.push(crate::message::Message::assistant_text(
                ctx.turn_id
                    .clone()
                    .unwrap_or_else(crate::event::TurnId::now),
                format!("[partial output before user correction]\n{partial}"),
            ));
        }
        if let Some(corr) = &correction {
            messages.push(crate::provider::user_text_message(format!(
                "<user_correction>{corr}</user_correction>\n\n{prompt}"
            )));
        } else {
            messages.push(crate::provider::user_text_message(prompt.clone()));
        }
        let req = LlmRequest {
            model: model.clone(),
            messages,
            system: None,
            input: input.clone(),
            schema: None,
            cache_prompt,
            tools: Vec::new(),
        };
        let outcome = run_streaming_once(provider.as_ref(), req, &rules, ctx).await;
        match outcome {
            StreamOutcome::L2Restart {
                correction_text,
                partial_output,
                partial_tokens,
            } if restart_count < 3 => {
                if let Some(sink) = ctx.events {
                    sink.emit(crate::event::Event::LlmPartialCall {
                        seq: 0,
                        turn_id: ctx.turn_id.clone(),
                        flow_run_id: ctx.flow_run_id.clone(),
                        model: model.clone(),
                        provider: provider.name().to_string(),
                        tokens_before_abort: partial_tokens,
                        restart_reason: "l2_course_correct".to_string(),
                        ts: chrono::Utc::now(),
                    });
                }
                restart_count += 1;
                correction = Some(correction_text);
                prior_partial = Some(partial_output);
                continue;
            }
            StreamOutcome::L2Restart { .. } => {
                return Ok(Value::Err(RuntimeError::ToolFailed(
                    "l2 restart exhausted 3x, giving up".into(),
                )));
            }
            StreamOutcome::Done(value) => return Ok(value),
        }
    }
}

enum StreamOutcome {
    Done(Value),
    L2Restart {
        correction_text: String,
        partial_output: String,
        partial_tokens: u64,
    },
}

async fn run_streaming_once<'a>(
    provider: &dyn crate::provider::Provider,
    req: LlmRequest,
    rules: &WatchRules,
    ctx: &EvalCtx<'a>,
) -> StreamOutcome {
    let mut inj_rx = ctx.session.map(|s| s.subscribe_injections());
    let stream_tx = ctx.session.map(|s| s.stream_tx());
    let model_name = req.model.clone();
    let obs = provider.call_streaming(req);
    let cancel = obs.cancel.clone();
    let mut events = obs.events;
    let output = obs.output;
    tokio::pin!(output);

    let mut state = StreamMonitor::new(rules, ctx);
    let elapsed_active = rules.elapsed_ms_gt.is_some();
    let elapsed_deadline_ms = rules.elapsed_ms_gt.unwrap_or(u64::MAX / 2);
    let elapsed_sleep = tokio::time::sleep(tokio::time::Duration::from_millis(
        elapsed_deadline_ms.saturating_add(1),
    ));
    tokio::pin!(elapsed_sleep);
    let started = std::time::Instant::now();

    let mut l2_correction: Option<String> = None;
    let final_result = loop {
        tokio::select! {
            biased;
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::LlmChunk { text, cumulative_tokens }) => {
                        if let Some(session) = ctx.session {
                            session.mark_streamed();
                        }
                        if let Some(tx) = &stream_tx {
                            let _ = tx.send(crate::stream::StreamFrame::LlmChunk {
                                text: text.clone(),
                                model: model_name.clone(),
                            });
                        }
                        state.on_chunk(&text, cumulative_tokens, started, rules, &cancel);
                    }
                    Ok(NodeEvent::LlmDone { total_tokens }) => {
                        if let Some(tx) = &stream_tx {
                            let _ = tx.send(crate::stream::StreamFrame::LlmDone { total_tokens });
                        }
                        state.on_done(total_tokens, started, rules, &cancel);
                    }
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
            inj_msg = poll_injection(&mut inj_rx), if inj_rx.is_some() && l2_correction.is_none() => {
                if let Some(inj) = inj_msg
                    && ctx.turn_id.as_ref().is_some_and(|t| &inj.turn_id == t)
                {
                    match inj.level {
                        crate::injection::InjectionLevel::L2CourseCorrect => {
                            cancel.cancel();
                            l2_correction = Some(inj.text.clone());
                        }
                        crate::injection::InjectionLevel::L4HardStop => {
                            cancel.cancel();
                            break Err(RuntimeError::Cancelled("hard stop from user".into()));
                        }
                        _ => {}
                    }
                }
            }
            _ = &mut elapsed_sleep, if elapsed_active && state.abort_reason.is_none() => {
                state.abort_reason = Some(format!("elapsed > {elapsed_deadline_ms}ms"));
                cancel.cancel();
                break Err(RuntimeError::Cancelled("elapsed".into()));
            }
            result = &mut output => break result,
        }
    };

    while let Ok(ev) = events.try_recv() {
        match ev {
            NodeEvent::LlmChunk {
                text,
                cumulative_tokens,
            } => {
                if let Some(session) = ctx.session {
                    session.mark_streamed();
                }
                if let Some(tx) = &stream_tx {
                    let _ = tx.send(crate::stream::StreamFrame::LlmChunk {
                        text: text.clone(),
                        model: model_name.clone(),
                    });
                }
                state.on_chunk(&text, cumulative_tokens, started, rules, &cancel);
            }
            NodeEvent::LlmDone { total_tokens } => {
                if let Some(tx) = &stream_tx {
                    let _ = tx.send(crate::stream::StreamFrame::LlmDone { total_tokens });
                }
                state.on_done(total_tokens, started, rules, &cancel);
            }
            _ => {}
        }
    }

    if let Some(correction) = l2_correction {
        return StreamOutcome::L2Restart {
            correction_text: correction,
            partial_output: state.text_captured.clone(),
            partial_tokens: state.tokens_seen,
        };
    }

    let mut abort_reason = state.abort_reason;
    match final_result {
        _ if abort_reason.is_some() => StreamOutcome::Done(Value::Err(RuntimeError::Aborted(
            abort_reason.take().unwrap_or_default(),
        ))),
        Ok(am) => StreamOutcome::Done(crate::provider::assistant_message_to_value(&am)),
        Err(e) => StreamOutcome::Done(Value::Err(e)),
    }
}

async fn eval_bash_with_watches(
    expr: &Expr,
    env: &mut Env,
    ctx: &EvalCtx<'_>,
    watches: &[&WatchDecl],
) -> Result<Value, RuntimeError> {
    let rules = collect_watch_rules(watches);
    let (tx, mut rx) = tokio::sync::broadcast::channel::<String>(1024);
    let cancel = ctx.tool_ctx.cancel.child_token();
    let mut child_ctx = ctx.tool_ctx.clone().with_stdout_broadcast(tx.clone());
    child_ctx.cancel = cancel.clone();

    let cancel_for_watch = cancel.clone();
    let patterns: Vec<String> = rules.token_matches.iter().map(|(p, _)| p.clone()).collect();
    let watcher = if !patterns.is_empty() {
        Some(tokio::spawn(async move {
            let mut window = String::new();
            loop {
                tokio::select! {
                    biased;
                    _ = cancel_for_watch.cancelled() => break None,
                    line = rx.recv() => {
                        let Ok(line) = line else { break None };
                        window.push_str(&line);
                        window.push('\n');
                        if window.len() > 4096 {
                            let drop = window.len() - 4096;
                            window.drain(..drop);
                        }
                        if let Some(p) = patterns.iter().find(|p| window.contains(p.as_str())) {
                            let hit = format!("token match: {p}");
                            cancel_for_watch.cancel();
                            break Some(hit);
                        }
                    }
                }
            }
        }))
    } else {
        None
    };

    let ctx_snap = child_ctx.clone();
    let new_ctx = EvalCtx {
        tools: ctx.tools,
        tool_ctx: &ctx_snap,
        providers: ctx.providers,
        flows: ctx.flows,
        contract: ctx.contract,
        events: ctx.events,
        turn_id: ctx.turn_id.clone(),
        flow_run_id: ctx.flow_run_id.clone(),
        session: ctx.session,
        flow_cancel: ctx.flow_cancel.clone(),
        safety: ctx.safety,
        current_node_id: ctx.current_node_id.clone(),
    };
    let value = eval_expr(expr, env, &new_ctx).await;
    drop(tx);
    drop(ctx_snap);

    let abort_reason = if let Some(handle) = watcher {
        cancel.cancel();
        handle.await.ok().flatten()
    } else {
        None
    };
    if let Some(reason) = abort_reason {
        return Ok(Value::Err(RuntimeError::Aborted(reason)));
    }
    if let Value::Err(RuntimeError::Cancelled(_)) = &value {
        return Ok(value);
    }
    Ok(value)
}

async fn poll_injection(
    rx: &mut Option<tokio::sync::broadcast::Receiver<crate::injection::Injection>>,
) -> Option<crate::injection::Injection> {
    let rx = rx.as_mut()?;
    loop {
        match rx.recv().await {
            Ok(inj) => return Some(inj),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return None,
        }
    }
}

#[derive(Default)]
struct WatchRules {
    token_matches: Vec<(String, String)>,
    tokens_gt: Option<u64>,
    elapsed_ms_gt: Option<u64>,
    warn_token: Vec<WarnRule>,
    warn_tokens_gt: Vec<(u64, WarnRule)>,
    warn_elapsed_ms_gt: Vec<(u64, WarnRule)>,
}

#[derive(Clone)]
struct WarnRule {
    target: String,
    message: String,
    pattern: String,
}

fn render_warn_msg(msg: &Option<Expr>, fallback: &str) -> String {
    match msg {
        Some(Expr::Literal(atman_dsl::ast::Literal::Str(s))) => s.clone(),
        _ => fallback.to_string(),
    }
}

struct StreamMonitor<'a> {
    window: String,
    text_captured: String,
    tokens_seen: u64,
    abort_reason: Option<String>,
    fired_warn_token: std::collections::HashSet<String>,
    fired_warn_tokens: std::collections::HashSet<u64>,
    fired_warn_elapsed: std::collections::HashSet<u64>,
    ctx: &'a EvalCtx<'a>,
}

impl<'a> StreamMonitor<'a> {
    fn new(_rules: &WatchRules, ctx: &'a EvalCtx<'a>) -> Self {
        Self {
            window: String::new(),
            text_captured: String::new(),
            tokens_seen: 0,
            abort_reason: None,
            fired_warn_token: Default::default(),
            fired_warn_tokens: Default::default(),
            fired_warn_elapsed: Default::default(),
            ctx,
        }
    }

    fn push_window(&mut self, text: &str) {
        self.window.push_str(text);
        if self.window.len() > 512 {
            let drop = self.window.len() - 512;
            self.window.drain(..drop);
        }
        self.text_captured.push_str(text);
    }

    fn emit_warn(&self, rule: &WarnRule, trigger: &str) {
        if let Some(sink) = self.ctx.events {
            sink.emit(crate::event::Event::WatchWarn {
                seq: 0,
                turn_id: self.ctx.turn_id.clone(),
                flow_run_id: self.ctx.flow_run_id.clone(),
                target: rule.target.clone(),
                trigger: trigger.to_string(),
                message: rule.message.clone(),
                ts: chrono::Utc::now(),
            });
        }
    }

    fn check_token_warns(&mut self, rules: &WatchRules) {
        for rule in &rules.warn_token {
            if !self.fired_warn_token.contains(&rule.pattern)
                && self.window.contains(rule.pattern.as_str())
            {
                self.fired_warn_token.insert(rule.pattern.clone());
                self.emit_warn(rule, &format!("token({})", rule.pattern));
            }
        }
    }

    fn check_tokens_consumed_warns(&mut self, rules: &WatchRules) {
        for (threshold, rule) in &rules.warn_tokens_gt {
            if !self.fired_warn_tokens.contains(threshold) && self.tokens_seen > *threshold {
                self.fired_warn_tokens.insert(*threshold);
                self.emit_warn(rule, &format!("tokens_consumed>{threshold}"));
            }
        }
    }

    fn check_elapsed_warns(&mut self, rules: &WatchRules, started: std::time::Instant) {
        let elapsed = started.elapsed().as_millis() as u64;
        for (threshold, rule) in &rules.warn_elapsed_ms_gt {
            if !self.fired_warn_elapsed.contains(threshold) && elapsed > *threshold {
                self.fired_warn_elapsed.insert(*threshold);
                self.emit_warn(rule, &format!("elapsed>{threshold}ms"));
            }
        }
    }

    fn on_chunk(
        &mut self,
        text: &str,
        cumulative_tokens: u64,
        started: std::time::Instant,
        rules: &WatchRules,
        cancel: &tokio_util::sync::CancellationToken,
    ) {
        self.tokens_seen = cumulative_tokens.max(self.tokens_seen);
        self.push_window(text);
        if self.abort_reason.is_none() {
            for (pat, reason) in &rules.token_matches {
                if self.window.contains(pat.as_str()) {
                    self.abort_reason = Some(reason.clone());
                    cancel.cancel();
                    break;
                }
            }
        }
        if self.abort_reason.is_none()
            && let Some(limit) = rules.tokens_gt
            && self.tokens_seen > limit
        {
            self.abort_reason = Some(format!("tokens_consumed > {limit}"));
            cancel.cancel();
        }
        self.check_token_warns(rules);
        self.check_tokens_consumed_warns(rules);
        self.check_elapsed_warns(rules, started);
    }

    fn on_done(
        &mut self,
        total_tokens: u64,
        started: std::time::Instant,
        rules: &WatchRules,
        _cancel: &tokio_util::sync::CancellationToken,
    ) {
        self.tokens_seen = total_tokens.max(self.tokens_seen);
        if self.abort_reason.is_none()
            && let Some(limit) = rules.tokens_gt
            && self.tokens_seen > limit
        {
            self.abort_reason = Some(format!("tokens_consumed > {limit}"));
        }
        self.check_tokens_consumed_warns(rules);
        self.check_elapsed_warns(rules, started);
    }
}

fn collect_watch_rules(watches: &[&WatchDecl]) -> WatchRules {
    let mut rules = WatchRules::default();
    for w in watches {
        for on in &w.on_blocks {
            let has_abort = on
                .actions
                .iter()
                .any(|a| matches!(a, WatchAction::Abort { .. }));
            let warn_msg_expr = on.actions.iter().find_map(|a| match a {
                WatchAction::Warn { msg } => Some(msg),
                _ => None,
            });
            if !has_abort && warn_msg_expr.is_none() {
                continue;
            }
            match &on.event {
                WatchEvent::Token { patterns } => {
                    for p in patterns {
                        if has_abort {
                            rules
                                .token_matches
                                .push((p.clone(), format!("token match: {p}")));
                        }
                        if let Some(msg_expr) = warn_msg_expr {
                            rules.warn_token.push(WarnRule {
                                target: w.target.name.clone(),
                                message: render_warn_msg(
                                    msg_expr,
                                    &format!("watch warn: token `{p}`"),
                                ),
                                pattern: p.clone(),
                            });
                        }
                    }
                }
                WatchEvent::TokensConsumed { cmp, value }
                    if matches!(cmp, CmpOp::Gt | CmpOp::Ge) =>
                {
                    let threshold = if matches!(cmp, CmpOp::Ge) {
                        value.saturating_sub(1)
                    } else {
                        *value
                    };
                    if has_abort {
                        rules.tokens_gt = Some(match rules.tokens_gt {
                            Some(existing) => existing.min(threshold),
                            None => threshold,
                        });
                    }
                    if let Some(msg_expr) = warn_msg_expr {
                        rules.warn_tokens_gt.push((
                            threshold,
                            WarnRule {
                                target: w.target.name.clone(),
                                message: render_warn_msg(
                                    msg_expr,
                                    &format!("watch warn: tokens_consumed > {threshold}"),
                                ),
                                pattern: format!("tokens_consumed>{threshold}"),
                            },
                        ));
                    }
                }
                WatchEvent::Elapsed { cmp, duration_ms }
                    if matches!(cmp, CmpOp::Gt | CmpOp::Ge) =>
                {
                    let threshold = if matches!(cmp, CmpOp::Ge) {
                        duration_ms.saturating_sub(1)
                    } else {
                        *duration_ms
                    };
                    if has_abort {
                        rules.elapsed_ms_gt = Some(match rules.elapsed_ms_gt {
                            Some(existing) => existing.min(threshold),
                            None => threshold,
                        });
                    }
                    if let Some(msg_expr) = warn_msg_expr {
                        rules.warn_elapsed_ms_gt.push((
                            threshold,
                            WarnRule {
                                target: w.target.name.clone(),
                                message: render_warn_msg(
                                    msg_expr,
                                    &format!("watch warn: elapsed > {threshold}ms"),
                                ),
                                pattern: format!("elapsed>{threshold}ms"),
                            },
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    rules
}

pub async fn exec_flow(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    providers: &crate::provider::ProviderRegistry,
) -> Result<Value, RuntimeError> {
    let flows = std::collections::HashMap::new();
    exec_flow_with_siblings(
        flow,
        args,
        tools,
        tool_ctx,
        providers,
        &flows,
        None,
        None,
        None,
        None,
        tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn exec_flow_with_siblings(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
    providers: &crate::provider::ProviderRegistry,
    flows: &std::collections::HashMap<String, FlowDecl>,
    events: Option<&crate::event::EventSink>,
    turn_id: Option<crate::event::TurnId>,
    flow_run_id: Option<crate::event::FlowRunId>,
    session: Option<&crate::session::Session>,
    flow_cancel: tokio_util::sync::CancellationToken,
    safety: Option<&crate::safety::SafetyConfig>,
) -> Result<Value, RuntimeError> {
    let mut env = Env::new();
    for (name, value) in args {
        env.bind(name, value);
    }
    let ctx = EvalCtx {
        tools,
        tool_ctx,
        providers,
        flows,
        contract: flow.contract.as_ref(),
        events,
        turn_id,
        flow_run_id,
        session,
        flow_cancel,
        safety,
        current_node_id: None,
    };
    match exec_stmts(&flow.body, &mut env, &ctx).await {
        StmtOutcome::Return(v) => Ok(v),
        StmtOutcome::Err(e) => Err(e),
        StmtOutcome::Continue => Ok(Value::Unit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    async fn run(src: &str, args: Vec<(String, Value)>) -> Result<Value, RuntimeError> {
        let file = parse_file(src).expect("parse test src");
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let providers = crate::provider::ProviderRegistry::new();
        exec_flow(&file.flows[0], args, &tools, &tool_ctx, &providers).await
    }

    #[tokio::test]
    async fn bind_and_return() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    y = x + 2
    return y
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(3)));
    }

    #[tokio::test]
    async fn when_true_executes_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 5
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(42)));
    }

    #[tokio::test]
    async fn when_false_skips_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(0)));
    }

    #[tokio::test]
    async fn err_in_bind_stops_flow() {
        let err = run(
            r#"flow t() -> Int {
    x = missing
    return 1
}
"#,
            vec![],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::UndefinedVar(n) if n == "missing"));
    }

    #[tokio::test]
    async fn flow_args_bind_before_body() {
        let out = run(
            r#"flow t() -> Int {
    return n + 1
}
"#,
            vec![("n".into(), Value::Int(4))],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(5)));
    }

    #[tokio::test]
    async fn when_cond_non_bool_is_type_error() {
        let err = run(
            r#"flow t() -> Int {
    when 1 {
        return 1
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::TypeMismatch { .. }));
    }

    #[tokio::test]
    async fn flow_falls_through_to_unit_without_return() {
        let out = run(
            r#"flow t() {
    x = 1
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Unit));
    }
}
