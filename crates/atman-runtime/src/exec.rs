use crate::value::AtmanPayload;
use std::{collections::HashMap, path::PathBuf};

use atman_rt::ast::{Arg, CmpOp, Expr, FlowDecl, Node, Stmt, WatchAction, WatchDecl, WatchEvent};
use atman_rt::{
    Engine, HostFuture, LoopExit, LoopHost, PatternBindError, Preflight, StatementExecution,
    StatementHost, bind_pattern, run_loop,
};

use crate::error::RuntimeError;
use crate::eval::{EvalCtx, eval_expr};
use crate::streaming::{WarnRule, WatchRules};
use crate::tool::{BoxFut, Tool, ToolArgs, ToolCtx, ToolRegistry};
use crate::value::Value;

type Env = atman_rt::Env<Value>;

type StmtOutcome = atman_rt::StatementOutcome<Value, RuntimeError>;

pub fn exec_stmts<'a>(
    stmts: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
) -> BoxFut<'a, atman_rt::StatementOutcome<Value, RuntimeError>> {
    exec_stmts_prefixed(stmts, env, ctx, String::new())
}

pub fn exec_stmts_prefixed<'a>(
    stmts: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
    prefix: String,
) -> BoxFut<'a, atman_rt::StatementOutcome<Value, RuntimeError>> {
    Box::pin(async move {
        let watches = collect_watches(stmts);
        let parent_node_id = ctx.current_node_id.clone();
        let host = AtmanStatementHost { env, ctx, watches };
        Engine::new(host)
            .run_statements(stmts, &prefix, parent_node_id.as_deref())
            .await
    })
}

struct AtmanStatementHost<'a> {
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
    watches: HashMap<String, Vec<&'a WatchDecl>>,
}

struct AtmanLoopHost<'a> {
    body: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
}

impl LoopHost for AtmanLoopHost<'_> {
    type Value = Value;
    type Error = RuntimeError;

    fn iteration_start(&mut self, iteration: u64, node_id: &str, parent_node_id: Option<&str>) {
        emit_flow_node_start_raw(
            self.ctx,
            node_id,
            crate::nodegraph::NodeKind::Return,
            &format!("iteration {iteration}"),
            parent_node_id,
        );
    }

    fn execute_iteration<'a>(&'a mut self, node_id: &'a str) -> HostFuture<'a, StmtOutcome> {
        Box::pin(async move {
            let iter_ctx = self.ctx.with_node(node_id);
            exec_stmts_prefixed(self.body, self.env, &iter_ctx, node_id.to_string()).await
        })
    }

    fn iteration_end(
        &mut self,
        node_id: &str,
        outcome: &StmtOutcome,
        parent_node_id: Option<&str>,
    ) {
        let preview = match outcome {
            StmtOutcome::LoopBreak => Some("break"),
            StmtOutcome::LoopContinue => Some("continue"),
            _ => None,
        };
        emit_flow_node_end(self.ctx, node_id, outcome, parent_node_id, preview);
    }
}

impl StatementHost for AtmanStatementHost<'_> {
    type Payload = AtmanPayload;
    type Error = RuntimeError;

    fn preflight(&mut self, _stmt: &Stmt, _node_id: &str) -> Preflight<Self::Error> {
        let Some(session) = self.ctx.session_runtime.as_ref() else {
            return Preflight::Continue;
        };
        let Some(turn_id) = self.ctx.turn_id.as_ref() else {
            return Preflight::Continue;
        };
        let Some(inj) = session.peek_pending_l2_or_higher(turn_id) else {
            return Preflight::Continue;
        };
        match inj.level {
            crate::injection::InjectionLevel::L4HardStop => {
                session.mark_injection_consumed(&inj.id);
                Preflight::StopAfterNode {
                    error: RuntimeError::Cancelled("hard stop from user".into()),
                    preview: "cancelled: hard stop".into(),
                }
            }
            crate::injection::InjectionLevel::L3Redirect => {
                if let Some(target) = inj.redirect_target.clone() {
                    session.mark_injection_consumed(&inj.id);
                    Preflight::Stop(RuntimeError::Redirect(target))
                } else {
                    Preflight::Continue
                }
            }
            _ => Preflight::Continue,
        }
    }

    fn bind_parameter(&mut self, name: String, value: Value) {
        self.env.bind(name, value);
    }

    fn evaluate_default<'a>(&'a mut self, expr: &'a Expr) -> HostFuture<'a, Value> {
        Box::pin(async move { eval_expr(expr, self.env, self.ctx).await })
    }

    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>) {
        emit_flow_node_start(self.ctx, node_id, stmt, parent_node_id);
    }

    fn evaluate<'a>(&'a mut self, expr: &'a Expr, node_id: &'a str) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let stmt_ctx = self.ctx.with_node(node_id);
            eval_expr(expr, self.env, &stmt_ctx).await
        })
    }

    fn bind<'a>(
        &'a mut self,
        pattern: &'a atman_rt::ast::Pattern,
        expr: &'a Expr,
        node_id: &'a str,
    ) -> HostFuture<'a, StatementExecution<Value, RuntimeError>> {
        Box::pin(async move {
            let stmt_ctx = self.ctx.with_node(node_id);
            let watch_target = pattern.as_single_ident().map(|id| id.name.clone());
            let value = if let Some(target) = watch_target.as_ref()
                && let Some(watches) = self.watches.get(target)
            {
                match eval_bind_with_watches(expr, self.env, &stmt_ctx, watches).await {
                    Ok(value) => value,
                    Err(error) => return (StmtOutcome::Err(error), None),
                }
            } else {
                eval_expr(expr, self.env, &stmt_ctx).await
            };
            if let Value::Err(error) = value {
                return (StmtOutcome::Err(error), None);
            }
            let preview = value_preview(&value);
            if let Err(error) = bind_pattern(pattern, value, self.env) {
                let error = match error {
                    PatternBindError::NonStruct { actual } => RuntimeError::TypeMismatch {
                        expected: "struct for destructuring bind".into(),
                        actual,
                    },
                    PatternBindError::MissingField { name } => RuntimeError::MissingArg(format!(
                        "destructure: struct has no field `{name}`"
                    )),
                };
                return (StmtOutcome::Err(error), None);
            }
            (StmtOutcome::Continue, preview)
        })
    }

    fn run_body<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, StmtOutcome> {
        Box::pin(async move {
            let stmt_ctx = self.ctx.with_node(node_id);
            exec_stmts_prefixed(body, self.env, &stmt_ctx, node_id.to_string()).await
        })
    }

    fn run_loop<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, StatementExecution<Value, RuntimeError>> {
        Box::pin(async move {
            let stmt_ctx = self.ctx.with_node(node_id);
            let mut host = AtmanLoopHost {
                body,
                env: self.env,
                ctx: &stmt_ctx,
            };
            match run_loop(&mut host, Some(node_id)).await {
                LoopExit::Break => (StmtOutcome::Continue, Some("loop end".into())),
                LoopExit::Interrupted(outcome) => (outcome, Some("loop interrupted".into())),
            }
        })
    }

    fn preview(&self, value: &Value) -> Option<String> {
        value_preview(value)
    }

    fn node_end(
        &mut self,
        node_id: &str,
        outcome: &StmtOutcome,
        parent_node_id: Option<&str>,
        preview: Option<&str>,
    ) {
        emit_flow_node_end(self.ctx, node_id, outcome, parent_node_id, preview);
    }
}

fn emit_flow_node_start(
    ctx: &EvalCtx<'_>,
    node_id: &str,
    stmt: &Stmt,
    parent_node_id: Option<&str>,
) {
    let (kind, label) = stmt_to_node_kind_label(stmt);
    emit_flow_node_start_raw(ctx, node_id, kind, &label, parent_node_id);
}

fn emit_flow_node_start_raw(
    ctx: &EvalCtx<'_>,
    node_id: &str,
    kind: crate::nodegraph::NodeKind,
    label: &str,
    parent_node_id: Option<&str>,
) {
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return;
    };
    if let Some(sink) = ctx.events {
        sink.emit(crate::event::Event::FlowNodeStart {
            run_id: run_id.clone(),
            node_id: node_id.to_string(),
            kind: kind.clone(),
            label: label.to_string(),
            parent_node_id: parent_node_id.map(String::from),
        });
    }
    if let Some(tx) = ctx.tool_ctx.stream_tx.clone() {
        let _ = tx.send(crate::stream::StreamFrame::FlowNodeStart {
            run_id: run_id.0.to_string(),
            node_id: node_id.to_string(),
            kind,
            label: label.to_string(),
            parent_node_id: parent_node_id.map(String::from),
        });
    }
}

fn value_preview(v: &Value) -> Option<String> {
    let raw = match v {
        Value::Str(s) => s.clone(),
        Value::Host(AtmanPayload::Message(m)) => {
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
        Value::Host(AtmanPayload::Path(p)) => p.display().to_string(),
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
        Value::Host(AtmanPayload::EditProposal(_)) => "<edit proposal>".into(),
        Value::Lambda { .. } => "<lambda>".into(),
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
        StmtOutcome::LoopBreak => crate::event::FlowNodeStatus::Ok,
        StmtOutcome::LoopContinue => crate::event::FlowNodeStatus::Ok,
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
        Stmt::When { cond, .. } => {
            let preview = crate::nodegraph::format_expr_short(cond);
            (
                NodeKind::When {
                    condition_preview: preview.clone(),
                },
                format!("when {preview}"),
            )
        }
        Stmt::Watch(_) => (NodeKind::Return, "watch".into()),
        Stmt::Loop { .. } => (NodeKind::Loop, "loop".into()),
        Stmt::Break => (NodeKind::Return, "break".into()),
        Stmt::Continue => (NodeKind::Return, "continue".into()),
    }
}

fn expr_to_node_kind_label(expr: &Expr) -> (crate::nodegraph::NodeKind, String) {
    use crate::nodegraph::NodeKind;
    match expr {
        Expr::Node(Node::ToolCall { path, .. })
            if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
        {
            (NodeKind::Llm { model: None }, "llm.call".into())
        }
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

async fn eval_bind_with_watches(
    expr: &Expr,
    env: &mut Env,
    ctx: &EvalCtx<'_>,
    watches: &[&WatchDecl],
) -> Result<Value, RuntimeError> {
    let Expr::Node(Node::ToolCall { path, args }) = expr else {
        return Ok(eval_expr(expr, env, ctx).await);
    };
    if !(path.len() == 2 && path[0].name == "llm" && path[1].name == "call") {
        return Ok(eval_expr(expr, env, ctx).await);
    };

    let mut positional = Vec::new();
    let mut named = Vec::new();
    for arg in args {
        match arg {
            Arg::Positional(expr) => {
                let value = eval_expr(expr, env, ctx).await;
                if value.is_err() {
                    return Ok(value);
                }
                positional.push(value);
            }
            Arg::Named { name, value } => {
                let value = eval_expr(value, env, ctx).await;
                if value.is_err() {
                    return Ok(value);
                }
                named.push((name.name.clone(), value));
            }
        }
    }

    let registry = std::sync::Arc::new(ctx.tools.clone());
    let call_args = ToolArgs { positional, named };
    let mut tool_ctx = ctx
        .tool_ctx
        .clone()
        .with_anchors(
            ctx.turn_id.clone(),
            ctx.flow_run_id.clone(),
            ctx.events.map(|s| s.next_seq_peek()),
        )
        .with_registry(registry)
        .with_current_node(ctx.current_node_id.clone())
        .with_providers(std::sync::Arc::new(ctx.providers.clone()))
        .with_watch_rules(collect_watch_rules(watches));
    if let Some(sink) = ctx.events {
        tool_ctx = tool_ctx.with_events(sink.clone());
    }
    if let Some(session) = ctx.session_runtime.as_ref() {
        tool_ctx = tool_ctx
            .with_session_messages(session.messages_full())
            .with_session_messages_handle(session.messages_handle())
            .with_session_runtime(session.clone())
            .with_watch_hub(std::sync::Arc::clone(&session.watch_hub))
            .with_flow_registry(std::sync::Arc::clone(&session.flow_registry))
            .with_compact_lock_handle(session.compact_lock_handle());
    }
    if let Some(safety) = ctx.safety.cloned() {
        tool_ctx = tool_ctx.with_safety(safety);
    }
    if let Some(model) = &ctx.tool_ctx.current_model {
        tool_ctx = tool_ctx.with_current_model(model.clone());
    }
    if let Some(tx) = ctx.tool_ctx.stream_tx.clone() {
        tool_ctx = tool_ctx.with_stream_tx(tx);
    }

    let result = crate::tools::llm_call::LlmCallTool
        .call(call_args, &tool_ctx)
        .await;
    Ok(match result {
        Ok(v) => v,
        Err(e) => Value::Err(e),
    })
}

fn render_warn_msg(msg: &Option<Expr>, fallback: &str) -> String {
    match msg {
        Some(Expr::Literal(atman_rt::ast::Literal::Str(s))) => s.clone(),
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
    let host = AtmanStatementHost {
        env: &mut env,
        ctx: &ctx,
        watches: collect_watches(&flow.body),
    };
    let raw_outcome = Engine::new(host).run_flow(flow, args).await;
    match raw_outcome {
        StmtOutcome::Return(v) => Ok(v),
        StmtOutcome::Err(e) => Err(e),
        StmtOutcome::Continue => Ok(Value::Unit),
        StmtOutcome::LoopBreak => Err(RuntimeError::ToolFailed("break outside loop".into())),
        StmtOutcome::LoopContinue => Err(RuntimeError::ToolFailed("continue outside loop".into())),
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
    async fn flow_defaults_follow_parameter_order_and_explicit_args_win() {
        let source = r#"flow t(a: int = 7, b: int = a + 2) -> int {
    return b
}
"#;
        assert!(matches!(run(source, vec![]).await.unwrap(), Value::Int(9)));
        assert!(matches!(
            run(source, vec![("a".into(), Value::Int(3))])
                .await
                .unwrap(),
            Value::Int(5)
        ));
        assert!(matches!(
            run(source, vec![("b".into(), Value::Int(20))])
                .await
                .unwrap(),
            Value::Int(20)
        ));
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

    #[tokio::test]
    async fn loop_continues_then_breaks_without_skipping_following_statement() {
        let out = run(
            r#"flow t() -> Int {
    n = 0
    loop {
        n = n + 1
        when n == 2 {
            continue
        }
        when n == 4 {
            break
        }
    }
    return n
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(4)));
    }

    #[tokio::test]
    async fn return_inside_loop_exits_flow() {
        let out = run(
            r#"flow t() -> Int {
    loop {
        return 7
    }
    return 0
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(7)));
    }
}
