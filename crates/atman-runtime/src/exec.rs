use std::{collections::HashMap, path::PathBuf};

use atman_dsl::ast::{CmpOp, Expr, FlowDecl, Node, Stmt, WatchAction, WatchDecl, WatchEvent};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::eval::{EvalCtx, eval_expr};
use crate::provider::LlmRequest;
use crate::streaming::{LlmStream, WarnRule, WatchRules};
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
            // Check for pending L4 stop / L3 redirect between statements.
            if let Some(session) = ctx.session_runtime.as_ref()
                && let Some(turn_id) = ctx.turn_id.as_ref()
            {
                if let Some(inj) = session.peek_pending_l2_or_higher(turn_id) {
                    match inj.level {
                        crate::injection::InjectionLevel::L4HardStop => {
                            session.mark_injection_consumed(&inj.id);
                            emit_flow_node_start(ctx, &node_id, stmt, parent_node_id.as_deref());
                            emit_flow_node_end(
                                ctx,
                                &node_id,
                                &StmtOutcome::Continue,
                                parent_node_id.as_deref(),
                                Some("cancelled: hard stop"),
                            );
                            return StmtOutcome::Err(RuntimeError::Cancelled(
                                "hard stop from user".into(),
                            ));
                        }
                        crate::injection::InjectionLevel::L3Redirect => {
                            if let Some(target) = inj.redirect_target.clone() {
                                session.mark_injection_consumed(&inj.id);
                                return StmtOutcome::Err(RuntimeError::Redirect(target));
                            }
                        }
                        _ => {}
                    }
                }
            }
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
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return;
    };
    let (kind, label) = stmt_to_node_kind_label(stmt);
    if let Some(sink) = ctx.events {
        sink.emit(crate::event::Event::FlowNodeStart {
            run_id: run_id.clone(),
            node_id: node_id.to_string(),
            kind: kind.clone(),
            label: label.clone(),
            parent_node_id: parent_node_id.map(String::from),
        });
    }
    if let Some(tx) = ctx.tool_ctx.stream_tx.clone() {
        let _ = tx.send(crate::stream::StreamFrame::FlowNodeStart {
            run_id: run_id.0.to_string(),
            node_id: node_id.to_string(),
            kind,
            label,
            parent_node_id: parent_node_id.map(String::from),
        });
    }
}

fn value_preview(v: &Value) -> Option<String> {
    let raw = match v {
        Value::Str(s) => s.clone(),
        Value::Message(m) => {
            let text = m.text_concat();
            let tool_uses: Vec<String> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    crate::message::MessagePart::ToolUse { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            match (text.trim().is_empty(), tool_uses.is_empty()) {
                (false, true) => text,
                (false, false) => format!("{}\n\n→ tool_uses: {}", text, tool_uses.join(", ")),
                (true, false) => format!("→ tool_uses: {}", tool_uses.join(", ")),
                (true, true) => return None,
            }
        }
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
            run_id: run_id.clone(),
            node_id: node_id.to_string(),
            status: status.clone(),
            output_preview: preview_owned.clone(),
        });
    }
    if let Some(tx) = ctx.tool_ctx.stream_tx.clone() {
        let _ = tx.send(crate::stream::StreamFrame::FlowNodeEnd {
            run_id: run_id.0.to_string(),
            node_id: node_id.to_string(),
            status,
            output_preview: preview_owned,
            parent_node_id: parent_node_id.map(String::from),
        });
    }
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
                let truthy = match c {
                    Value::Bool(b) => b,
                    Value::Unit => false,
                    Value::Err(e) => {
                        return (StmtOutcome::Err(e), None);
                    }
                    _ => true,
                };
                if truthy {
                    (exec_stmts(body, env, ctx).await, Some("true".into()))
                } else {
                    (StmtOutcome::Continue, Some("false".into()))
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
                turn_id: ctx.turn_id.clone(),
                flow_run_id: ctx.flow_run_id.clone(),
                original_chars: stat.original_chars as u64,
                result_chars: stat.result_chars as u64,
                dropped_chars: stat.dropped_chars as u64,
                budget_tokens: stat.budget_tokens,
            });
        }
    }
    let Some(provider) = ctx.providers.resolve(&model) else {
        return Ok(Value::Err(RuntimeError::ToolFailed(format!(
            "no provider registered for model `{model}`"
        ))));
    };

    let messages = vec![crate::provider::user_text_message(prompt.clone())];
    let req = LlmRequest {
        model: model.clone(),
        messages,
        system: None,
        input: input.clone(),
        schema: None,
        cache_prompt,
        tools: Vec::new(),
        thinking_enabled: false,
        stall_timeout_secs: 120,
    };
    let frame_tx = ctx
        .tool_ctx
        .agent_entry
        .as_ref()
        .map(|e| e.frame_tx.clone());
    let mut stream = LlmStream::new(provider.as_ref(), req)
        .with_stream_tx(ctx.tool_ctx.stream_tx.clone())
        .with_frame_tx(frame_tx)
        .with_watch_rules(collect_watch_rules(watches))
        .with_event_sink(ctx.events)
        .with_turn_id(ctx.turn_id.clone())
        .with_flow_run_id(ctx.flow_run_id.clone());
    if let Some(session) = ctx.session_runtime.as_ref() {
        stream = stream.with_session(session);
    }
    if let Some(entry) = ctx.tool_ctx.agent_entry.as_ref() {
        stream = stream.with_entry(entry);
    }
    match stream.run().await {
        Ok(am) => Ok(crate::provider::assistant_message_to_value(&am)),
        Err(e) => Ok(Value::Err(e)),
    }
}

fn render_warn_msg(msg: &Option<Expr>, fallback: &str) -> String {
    match msg {
        Some(Expr::Literal(atman_dsl::ast::Literal::Str(s))) => s.clone(),
        _ => fallback.to_string(),
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
    source_dir: Option<PathBuf>,
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
        source_dir,
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
    session: Option<std::sync::Arc<crate::session::Session>>,
    flow_cancel: tokio_util::sync::CancellationToken,
    safety: Option<&crate::safety::SafetyConfig>,
    source_dir: Option<PathBuf>,
) -> Result<Value, RuntimeError> {
    let ctx = EvalCtx {
        tools,
        tool_ctx,
        providers,
        flows,
        contract: flow.contract.as_ref(),
        events,
        turn_id,
        flow_run_id,
        session_runtime: session,
        flow_cancel,
        safety,
        current_node_id: None,
        source_dir,
    };
    let mut env = Env::new();
    let provided: std::collections::HashSet<String> = args.iter().map(|(n, _)| n.clone()).collect();
    for (name, value) in args {
        env.bind(name, value);
    }
    for p in &flow.params {
        if !provided.contains(&p.name.name) {
            if let Some(default) = &p.default {
                let val = crate::eval::eval_expr(default, &env, &ctx).await;
                env.bind(p.name.name.clone(), val);
            }
        }
    }
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
        exec_flow(&file.flows[0], args, &tools, &tool_ctx, &providers, None).await
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
    async fn when_cond_unit_is_falsy() {
        let out = run(
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
        .unwrap();
        assert!(matches!(out, Value::Int(1)));
    }

    #[tokio::test]
    async fn when_cond_struct_is_truthy() {
        let out = run(
            r#"flow t() -> Int {
    x = { a: 1 }
    when x {
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
