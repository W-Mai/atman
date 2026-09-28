use crate::value::AtmanPayload;
use std::path::PathBuf;

use atman_rt::ast::{Expr, FlowDecl, Node, Stmt};
use atman_rt::{FlowCall, PatternBindError, Preflight, StatementHost, Vm, VmCallError, VmHost};

use crate::atman_host::AtmanHost;
use crate::error::RuntimeError;
use crate::tool::{ToolCtx, ToolRegistry};
use crate::value::Value;

type StmtOutcome = atman_rt::StatementOutcome<Value, RuntimeError>;

#[derive(Clone)]
struct AtmanStatementAdapter<'a> {
    ctx: AtmanHost<'a>,
}

struct ChildFlowGuard {
    run_id: crate::event::FlowRunId,
    started: Option<atman_rt::StartedFlow<crate::event::FlowRunId>>,
    lifecycle: Option<crate::tools::agent_ctrl::FlowLifecycleGuard>,
    _blocked: crate::tools::agent_ctrl::DescendantBlockGuard,
    events: Option<crate::event::EventSink>,
    stream_tx: Option<tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
}

impl ChildFlowGuard {
    fn finish(&mut self, status: crate::event::FlowStatus, ok: bool) {
        let Some(started) = self.started.take() else {
            return;
        };
        let cancelled = matches!(status, crate::event::FlowStatus::Cancelled);
        let end = started.finish(status);
        self.lifecycle.take();
        if let Some(sink) = &self.events {
            sink.emit(end.clone().into());
        }
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::FlowDone {
                run_id: self.run_id.0.to_string(),
                flow_name: end.flow_name,
                ok,
                cancelled,
                suicide: false,
            });
        }
    }
}

impl Drop for ChildFlowGuard {
    fn drop(&mut self) {
        self.finish(crate::event::FlowStatus::Cancelled, false);
    }
}

impl VmHost for AtmanStatementAdapter<'_> {
    type ChildGuard = ChildFlowGuard;

    fn call_error(&self, error: VmCallError) -> RuntimeError {
        match error {
            VmCallError::MissingEntry(name) => RuntimeError::UndefinedTool(name),
            VmCallError::MissingFlow(name) => {
                RuntimeError::UndefinedTool(format!("subflow({name})"))
            }
            VmCallError::TooManyPositional(name) => {
                RuntimeError::MissingArg(format!("subflow({name}): too many positional args"))
            }
            VmCallError::CallDepthExceeded(name) => {
                RuntimeError::ToolFailed(format!("subflow({name}): maximum call depth exceeded"))
            }
            VmCallError::ControlFlowEscaped(name) => RuntimeError::ToolFailed(format!(
                "subflow({name}): break or continue escaped the flow"
            )),
        }
    }

    fn enter_child(&self, call: &FlowCall<'_>) -> Result<(Self, Self::ChildGuard), RuntimeError> {
        let ctx = &self.ctx;
        let registry = ctx.tool_ctx.flow_registry.clone().ok_or_else(|| {
            RuntimeError::ToolFailed("subflow: trusted flow registry is unavailable".into())
        })?;
        let parent_run_id = ctx
            .tool_ctx
            .flow_identity
            .as_ref()
            .map(|identity| identity.run_id.clone())
            .ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "subflow: trusted parent flow identity is unavailable".into(),
                )
            })?;
        let run_id = crate::event::FlowRunId::now();
        let allows_shell = crate::flow_authority::contract_allows_shell(call.contract);
        let child_identity = registry.register_child(
            &parent_run_id,
            run_id.clone(),
            crate::flow_authority::InvocationKind::InlineSubflow,
            allows_shell,
            crate::flow_authority::ChildWorkspaceAuthority::Inherit,
        )?;
        let lifecycle = registry.lifecycle_guard(&run_id);
        let blocked = registry.block_on_descendant(&parent_run_id, &run_id)?;
        let (started, start) = atman_rt::FlowLifecycle::new(atman_rt::FlowStartFact {
            run_id: run_id.clone(),
            flow_name: call.display_name.to_string(),
            parent_run_id: Some(parent_run_id),
            parent_node_id: Some(call.parent_node_id.to_string()),
            spawned: false,
        })
        .start();
        if let Some(sink) = ctx.events {
            sink.emit(start.clone().into());
        }
        let stream_tx = ctx
            .session_runtime
            .as_ref()
            .map(|session| session.stream_tx())
            .or_else(|| ctx.tool_ctx.stream_tx.clone());
        if let Some(tx) = &stream_tx {
            let _ = tx.send(start.into());
        }

        let mut child_tool_ctx = ctx.tool_ctx.as_ref().clone();
        child_tool_ctx.flow_run_id = Some(run_id.clone());
        child_tool_ctx.flow_identity = Some(child_identity);
        let source_dir = ctx
            .linked_program
            .and_then(|program| program.source_dir(call.target))
            .map(std::path::Path::to_path_buf)
            .or_else(|| ctx.source_dir.clone());
        let child_ctx = AtmanHost {
            tool_ctx: std::borrow::Cow::Owned(child_tool_ctx),
            allows_shell,
            flow_run_id: Some(run_id.clone()),
            current_node_id: None,
            current_module: Some(call.target.module),
            source_dir,
            ..ctx.clone()
        };
        Ok((
            Self { ctx: child_ctx },
            ChildFlowGuard {
                run_id,
                started: Some(started),
                lifecycle: Some(lifecycle),
                _blocked: blocked,
                events: ctx.events.cloned(),
                stream_tx,
            },
        ))
    }

    fn exit_child(&self, _call: &FlowCall<'_>, outcome: &StmtOutcome, mut guard: Self::ChildGuard) {
        let status = crate::event::FlowStatus::from(atman_rt::classify_outcome(outcome, |error| {
            matches!(error, RuntimeError::Cancelled(_))
        }));
        let ok = !matches!(outcome, StmtOutcome::Err(_));
        guard.finish(status, ok);
    }
}

impl<'a> StatementHost for AtmanStatementAdapter<'a> {
    type Payload = AtmanPayload;
    type Error = RuntimeError;
    type ExprHost = AtmanHost<'a>;

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

    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>) {
        emit_flow_node_start(&self.ctx, node_id, stmt, parent_node_id);
    }

    fn expression_host(&self, node_id: &str) -> Self::ExprHost {
        self.ctx.with_node(node_id)
    }

    fn pattern_error(&self, error: PatternBindError) -> RuntimeError {
        match error {
            PatternBindError::NonStruct { actual } => RuntimeError::TypeMismatch {
                expected: "struct for destructuring bind".into(),
                actual,
            },
            PatternBindError::MissingField { name } => {
                RuntimeError::MissingArg(format!("destructure: struct has no field `{name}`"))
            }
        }
    }

    fn iteration_start(&mut self, iteration: u64, node_id: &str, parent_node_id: Option<&str>) {
        emit_flow_node_start_raw(
            &self.ctx,
            node_id,
            crate::nodegraph::NodeKind::Return,
            &format!("iteration {iteration}"),
            parent_node_id,
        );
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
        emit_flow_node_end(&self.ctx, node_id, outcome, parent_node_id, preview);
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
        emit_flow_node_end(&self.ctx, node_id, outcome, parent_node_id, preview);
    }
}

fn emit_flow_node_start(
    ctx: &AtmanHost<'_>,
    node_id: &str,
    stmt: &Stmt,
    parent_node_id: Option<&str>,
) {
    let (kind, label) = stmt_to_node_kind_label(stmt);
    emit_flow_node_start_raw(ctx, node_id, kind, &label, parent_node_id);
}

fn emit_flow_node_start_raw(
    ctx: &AtmanHost<'_>,
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
    ctx: &AtmanHost<'_>,
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
        Expr::Node(Node::Fanout { source }) => (
            NodeKind::Fanout,
            match source.as_ref() {
                Expr::List(items) => format!("fanout ×{}", items.len()),
                _ => "fanout".into(),
            },
        ),
        Expr::Node(Node::Subflow { name, .. }) => (
            NodeKind::Subflow {
                name: name.display_name(),
            },
            format!("subflow({})", name.display_name()),
        ),
        _ => (NodeKind::Return, "expr".into()),
    }
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
    exec_flow_with_linked_siblings(
        flow,
        args,
        tools,
        tool_ctx,
        providers,
        flows,
        events,
        turn_id,
        flow_run_id,
        session,
        flow_cancel,
        safety,
        source_dir,
        None,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn exec_flow_with_linked_siblings(
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
    linked_program: Option<&crate::source_program::LinkedProgram>,
    current_module: Option<crate::source_program::ModuleId>,
) -> Result<Value, RuntimeError> {
    let synthesized_program = if linked_program.is_none() {
        let mut all_flows: Vec<_> = flows
            .values()
            .filter(|sibling| sibling.name.name != flow.name.name)
            .cloned()
            .collect();
        all_flows.sort_by(|left, right| left.name.name.cmp(&right.name.name));
        all_flows.push(flow.clone());
        let file = atman_rt::ast::File {
            flows: all_flows,
            ..Default::default()
        };
        Some(
            crate::source_program::link_inline(file)
                .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?,
        )
    } else {
        None
    };
    let linked_program = linked_program
        .or(synthesized_program.as_ref())
        .expect("linked or synthesized program");
    let module = current_module.unwrap_or_else(|| linked_program.entry_module());
    let ctx = AtmanHost {
        tools,
        tool_ctx: std::borrow::Cow::Borrowed(tool_ctx),
        providers,
        flows,
        linked_program: Some(linked_program),
        current_module: Some(module),
        allows_shell: crate::flow_authority::contract_allows_shell(flow.contract.as_ref()),
        events,
        turn_id,
        flow_run_id,
        session_runtime: session,
        flow_cancel,
        safety,
        current_node_id: None,
        source_dir,
    };
    let host = AtmanStatementAdapter { ctx };
    let vm = Vm::from_shared(linked_program.shared_core());
    let raw_outcome = vm
        .run_flow(
            atman_rt::program::FlowId {
                module,
                name: flow.name.name.clone(),
            },
            args,
            host,
        )
        .await;
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
    use atman_rt::parse_file;

    #[test]
    fn dropped_child_guard_emits_cancelled_terminal_event_once() {
        let tools = ToolRegistry::new();
        let providers = crate::provider::ProviderRegistry::new();
        let flows = std::collections::HashMap::new();
        let events = crate::event::EventSink::new();
        let registry = std::sync::Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let parent_run_id = crate::event::FlowRunId::now();
        let identity = registry
            .register_root(
                "test-session".into(),
                parent_run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(
                    &crate::trust::TrustConfig::default(),
                    false,
                    None,
                ),
            )
            .unwrap();
        let mut tool_ctx = ToolCtx::new();
        tool_ctx.flow_run_id = Some(parent_run_id.clone());
        tool_ctx.flow_identity = Some(identity);
        tool_ctx.flow_registry = Some(registry);
        let host = AtmanStatementAdapter {
            ctx: AtmanHost {
                tools: &tools,
                tool_ctx: std::borrow::Cow::Borrowed(&tool_ctx),
                providers: &providers,
                flows: &flows,
                linked_program: None,
                current_module: None,
                allows_shell: false,
                events: Some(&events),
                turn_id: None,
                flow_run_id: Some(parent_run_id),
                session_runtime: None,
                flow_cancel: tokio_util::sync::CancellationToken::new(),
                safety: None,
                current_node_id: Some("parent.0".into()),
                source_dir: None,
            },
        };
        let target = atman_rt::program::FlowId {
            module: atman_rt::program::ModuleId(0),
            name: "child".into(),
        };
        let call = FlowCall {
            target: &target,
            display_name: "child",
            source_id: "entry:memory.at",
            contract: None,
            parent_node_id: "parent.0",
        };
        let (_, guard) = host.enter_child(&call).unwrap();
        drop(guard);
        let events = events.snapshot();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, crate::event::Event::FlowStart { .. }))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    crate::event::Event::FlowEnd {
                        status: crate::event::FlowStatus::Cancelled,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

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
