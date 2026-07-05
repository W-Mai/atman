use std::collections::HashMap;

use atman_dsl::ast::{CmpOp, Expr, FlowDecl, Node, Stmt, WatchAction, WatchDecl, WatchEvent};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::eval::{EvalCtx, eval_expr};
use crate::event::NodeEvent;
use crate::provider::LlmRequest;
use crate::tool::{BoxFut, ToolCtx, ToolRegistry};
use crate::value::Value;

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
    Box::pin(async move {
        let watches = collect_watches(stmts);
        for stmt in stmts {
            let outcome = exec_stmt(stmt, env, ctx, &watches).await;
            match outcome {
                StmtOutcome::Continue => continue,
                other => return other,
            }
        }
        StmtOutcome::Continue
    })
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
) -> BoxFut<'a, StmtOutcome> {
    Box::pin(async move {
        match stmt {
            Stmt::Bind { name, value } => {
                let v = if let Some(ws) = watches.get(&name.name) {
                    match eval_bind_with_watches(value, env, ctx, ws).await {
                        Ok(v) => v,
                        Err(e) => return StmtOutcome::Err(e),
                    }
                } else {
                    eval_expr(value, env, ctx).await
                };
                if let Value::Err(e) = v {
                    return StmtOutcome::Err(e);
                }
                env.bind(name.name.clone(), v);
                StmtOutcome::Continue
            }
            Stmt::When { cond, body } => {
                let c = eval_expr(cond, env, ctx).await;
                match c {
                    Value::Bool(true) => exec_stmts(body, env, ctx).await,
                    Value::Bool(false) => StmtOutcome::Continue,
                    Value::Err(e) => StmtOutcome::Err(e),
                    other => StmtOutcome::Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    }),
                }
            }
            Stmt::Return { value } => {
                let v = eval_expr(value, env, ctx).await;
                if let Value::Err(e) = v {
                    return StmtOutcome::Err(e);
                }
                StmtOutcome::Return(v)
            }
            Stmt::Expr(e) => {
                let v = eval_expr(e, env, ctx).await;
                if let Value::Err(err) = v {
                    return StmtOutcome::Err(err);
                }
                StmtOutcome::Continue
            }
            Stmt::Watch(_) => StmtOutcome::Continue,
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
    let user_msg = crate::provider::user_text_message(prompt.clone());
    let obs = provider.call_streaming(LlmRequest {
        model,
        messages: vec![user_msg],
        system: None,
        input,
        schema: None,
        cache_prompt,
        tools: Vec::new(),
    });
    let cancel = obs.cancel.clone();
    let mut events = obs.events;
    let output = obs.output;
    tokio::pin!(output);

    let mut state = StreamMonitor::new(&rules, ctx);
    let elapsed_active = rules.elapsed_ms_gt.is_some();
    let elapsed_deadline_ms = rules.elapsed_ms_gt.unwrap_or(u64::MAX / 2);
    let elapsed_sleep = tokio::time::sleep(tokio::time::Duration::from_millis(
        elapsed_deadline_ms.saturating_add(1),
    ));
    tokio::pin!(elapsed_sleep);
    let started = std::time::Instant::now();

    let final_result = loop {
        tokio::select! {
            biased;
            ev = events.recv() => {
                match ev {
                    Ok(NodeEvent::LlmChunk { text, cumulative_tokens }) => {
                        state.on_chunk(&text, cumulative_tokens, started, &rules, &cancel);
                    }
                    Ok(NodeEvent::LlmDone { total_tokens }) => {
                        state.on_done(total_tokens, started, &rules, &cancel);
                    }
                    Ok(_) => {}
                    Err(_) => {}
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
                state.on_chunk(&text, cumulative_tokens, started, &rules, &cancel);
            }
            NodeEvent::LlmDone { total_tokens } => {
                state.on_done(total_tokens, started, &rules, &cancel);
            }
            _ => {}
        }
    }
    let mut abort_reason = state.abort_reason;

    match final_result {
        _ if abort_reason.is_some() => Ok(Value::Err(RuntimeError::Aborted(
            abort_reason.take().unwrap_or_default(),
        ))),
        Ok(am) => Ok(crate::provider::assistant_message_to_value(&am)),
        Err(e) => Ok(Value::Err(e)),
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
