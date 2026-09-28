//! Product callbacks for the high-level `atman-rt` VM delegate contract.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use atman_rt::{
    ExpressionEffect, FlowCall, FlowDriveMode, FlowOutcome, HostFuture, PatternBindError,
    Preflight, ToolCallMode, VmCallError, VmContext, VmDelegate, VmEvent, VmNode, VmStatus,
};

use crate::atman_host::AtmanHost;
use crate::error::RuntimeError;
use crate::event::{Event, EventSink, FlowNodeStatus, FlowRunId, FlowStatus, TurnId};
use crate::injection::InjectionLevel;
use crate::provider::ProviderRegistry;
use crate::safety::SafetyConfig;
use crate::session::Session;
use crate::source_program::LinkedProgram;
use crate::stream::StreamFrame;
use crate::tool::{HistorySegment, ToolArgs, ToolCtx, ToolRegistry};
use crate::value::{AtmanPayload, Value};

#[derive(Clone)]
struct RunBinding {
    product_run_id: Option<FlowRunId>,
    tool_ctx: ToolCtx,
    allows_shell: bool,
    source_dir: Option<PathBuf>,
    session: Option<Arc<Session>>,
    turn_id: Option<TurnId>,
    flow_cancel: tokio_util::sync::CancellationToken,
}

struct DelegateState {
    tools: ToolRegistry,
    providers: ProviderRegistry,
    flows: HashMap<String, atman_rt::ast::FlowDecl>,
    program: LinkedProgram,
    events: Option<EventSink>,
    safety: Option<SafetyConfig>,
}

/// Product callbacks and the current flow's host context.
#[derive(Clone)]
pub(crate) struct AtmanVmDelegate {
    state: Arc<DelegateState>,
    binding: RunBinding,
}

/// A one-use authorization result paired with the effect passed to `invoke`.
pub(crate) struct AtmanEffectPermit {
    inner: AtmanEffectPermitInner,
}

enum AtmanEffectPermitInner {
    Direct,
    InvocationEnv,
    Tool(Box<crate::eval::AuthorizedToolInvocation>),
}

impl AtmanVmDelegate {
    /// Captures the root product context used to derive every child flow binding.
    pub(crate) fn from_host(host: &AtmanHost<'_>) -> Result<Self, RuntimeError> {
        let program = host.linked_program.cloned().ok_or_else(|| {
            RuntimeError::ToolFailed("high-level VM execution requires a linked program".into())
        })?;
        let root = RunBinding {
            product_run_id: host.flow_run_id.clone(),
            tool_ctx: host.tool_ctx.as_ref().clone(),
            allows_shell: host.allows_shell,
            source_dir: host.source_dir.clone(),
            session: host.session_runtime.clone(),
            turn_id: host.turn_id.clone(),
            flow_cancel: host.flow_cancel.clone(),
        };
        Ok(Self {
            state: Arc::new(DelegateState {
                tools: host.tools.clone(),
                providers: host.providers.clone(),
                flows: host.flows.clone(),
                program,
                events: host.events.cloned(),
                safety: host.safety.cloned(),
            }),
            binding: root,
        })
    }

    /// Builds the product effect context for a VM callback.
    ///
    /// Effect and authorization facets use this method to resolve the same
    /// per-flow `ToolCtx`; they must not keep authorization state in a side map.
    #[doc(hidden)]
    fn host_for<'a>(&'a self, context: &VmContext) -> AtmanHost<'a> {
        let mut tool_ctx = self.binding.tool_ctx.clone();
        tool_ctx.current_node_id = context.node_id.clone();
        AtmanHost {
            tools: &self.state.tools,
            tool_ctx: std::borrow::Cow::Owned(tool_ctx),
            providers: &self.state.providers,
            flows: &self.state.flows,
            linked_program: Some(&self.state.program),
            current_module: Some(context.flow.module),
            allows_shell: self.binding.allows_shell,
            events: self.state.events.as_ref(),
            turn_id: self.binding.turn_id.clone(),
            flow_run_id: self.binding.product_run_id.clone(),
            session_runtime: self.binding.session.clone(),
            flow_cancel: self.binding.flow_cancel.clone(),
            safety: self.state.safety.as_ref(),
            current_node_id: context.node_id.clone(),
            source_dir: self.binding.source_dir.clone(),
        }
    }

    fn emit_node_start(
        &self,
        context: &VmContext,
        kind: crate::nodegraph::NodeKind,
        label: String,
    ) {
        let (Some(run_id), Some(node_id)) =
            (self.binding.product_run_id.clone(), context.node_id.clone())
        else {
            return;
        };
        if let Some(events) = &self.state.events {
            events.emit(Event::FlowNodeStart {
                run_id: run_id.clone(),
                node_id: node_id.clone(),
                kind: kind.clone(),
                label: label.clone(),
                parent_node_id: context.parent_node_id.clone(),
            });
        }
        if let Some(tx) = &self.binding.tool_ctx.stream_tx {
            let _ = tx.send(StreamFrame::FlowNodeStart {
                run_id: run_id.0.to_string(),
                node_id,
                kind,
                label,
                parent_node_id: context.parent_node_id.clone(),
            });
        }
    }

    fn emit_node_end(&self, context: &VmContext, status: VmStatus, preview: Option<String>) {
        let (Some(run_id), Some(node_id)) =
            (self.binding.product_run_id.clone(), context.node_id.clone())
        else {
            return;
        };
        let status = product_node_status(status);
        if let Some(events) = &self.state.events {
            events.emit(Event::FlowNodeEnd {
                run_id: run_id.clone(),
                node_id: node_id.clone(),
                status: status.clone(),
                output_preview: preview.clone(),
            });
        }
        if let Some(tx) = &self.binding.tool_ctx.stream_tx {
            let _ = tx.send(StreamFrame::FlowNodeEnd {
                run_id: run_id.0.to_string(),
                node_id,
                status,
                output_preview: preview,
                parent_node_id: context.parent_node_id.clone(),
            });
        }
    }

    fn enter_root(&self, context: &VmContext) -> (Self, AtmanVmFlowGuard) {
        let mut entered = self.clone();
        let binding = &mut entered.binding;
        binding.source_dir = self
            .state
            .program
            .source_dir(&context.flow)
            .map(std::path::Path::to_path_buf)
            .or_else(|| binding.source_dir.clone());
        (entered, AtmanVmFlowGuard { lifecycle: None })
    }

    fn enter_child(
        &self,
        call: &FlowCall<'_>,
        context: &VmContext,
    ) -> Result<(Self, AtmanVmFlowGuard), RuntimeError> {
        context.parent_run_id.ok_or_else(|| {
            RuntimeError::ToolFailed("child flow is missing its parent VM run id".into())
        })?;
        let parent = &self.binding;
        let registry = parent.tool_ctx.flow_registry.clone().ok_or_else(|| {
            RuntimeError::ToolFailed("flow call: trusted flow registry is unavailable".into())
        })?;
        let parent_run_id = parent
            .tool_ctx
            .flow_identity
            .as_ref()
            .map(|identity| identity.run_id.clone())
            .ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "flow call: trusted parent flow identity is unavailable".into(),
                )
            })?;
        let product_run_id = FlowRunId::now();
        let allows_shell = crate::flow_authority::contract_allows_shell(call.contract);
        let child_identity = registry.register_child(
            &parent_run_id,
            product_run_id.clone(),
            crate::flow_authority::InvocationKind::InlineSubflow,
            allows_shell,
            crate::flow_authority::ChildWorkspaceAuthority::Inherit,
        )?;
        let lifecycle = registry.lifecycle_guard(&product_run_id);
        let blocked = registry.block_on_descendant(&parent_run_id, &product_run_id)?;

        let (started, start) = atman_rt::FlowLifecycle::new(atman_rt::FlowStartFact {
            run_id: product_run_id.clone(),
            flow_name: call.display_name.to_string(),
            parent_run_id: Some(parent_run_id),
            parent_node_id: call.parent_node_id.map(String::from),
            spawned: matches!(call.mode, FlowDriveMode::Parallel),
        })
        .start();
        if let Some(events) = &self.state.events {
            events.emit(start.clone().into());
        }
        let stream_tx = parent
            .session
            .as_ref()
            .map(|session| session.stream_tx())
            .or_else(|| parent.tool_ctx.stream_tx.clone());
        if let Some(tx) = &stream_tx {
            let _ = tx.send(start.into());
        }

        let mut child_tool_ctx = parent.tool_ctx.clone();
        child_tool_ctx.flow_run_id = Some(product_run_id.clone());
        child_tool_ctx.flow_identity = Some(child_identity);
        let child_session = if matches!(call.mode, FlowDriveMode::Parallel) {
            let mut snapshot = parent
                .session
                .as_ref()
                .map(|session| session.messages().to_vec())
                .or_else(|| {
                    parent
                        .tool_ctx
                        .session_messages_handle
                        .as_ref()
                        .map(|messages| messages.lock().unwrap().clone())
                })
                .unwrap_or_default();
            crate::message::retain_complete_tool_pairs(&mut snapshot);
            child_tool_ctx.session_runtime = None;
            child_tool_ctx.deferred_input_session = None;
            child_tool_ctx.context_records_session = parent
                .session
                .clone()
                .or_else(|| parent.tool_ctx.context_records_session.clone());
            child_tool_ctx.session_messages = None;
            child_tool_ctx.agent_entry = None;
            child_tool_ctx.on_memory_recent = None;
            child_tool_ctx.history_segment = HistorySegment::Spawned;
            child_tool_ctx.session_messages_handle =
                Some(Arc::new(std::sync::Mutex::new(snapshot)));
            child_tool_ctx.compact_lock_handle = Some(Arc::new(tokio::sync::Mutex::new(())));
            child_tool_ctx.context_epoch_handle =
                Some(Arc::new(std::sync::atomic::AtomicU64::new(0)));
            child_tool_ctx.context_prefix_tracker = Some(Arc::new(std::sync::Mutex::new(
                crate::context_plan::ContextPrefixTracker::default(),
            )));
            child_tool_ctx.call_intent = None;
            None
        } else {
            parent.session.clone()
        };
        child_tool_ctx.current_node_id = None;
        if let Some(tx) = stream_tx.clone() {
            child_tool_ctx.stream_tx = Some(tx);
        }
        let source_dir = self
            .state
            .program
            .source_dir(call.target)
            .map(std::path::Path::to_path_buf)
            .or_else(|| parent.source_dir.clone());
        let child = Self {
            state: Arc::clone(&self.state),
            binding: RunBinding {
                product_run_id: Some(product_run_id.clone()),
                tool_ctx: child_tool_ctx,
                allows_shell,
                source_dir,
                session: child_session,
                turn_id: parent.turn_id.clone(),
                flow_cancel: parent.flow_cancel.clone(),
            },
        };

        Ok((
            child,
            AtmanVmFlowGuard {
                lifecycle: Some(ChildFlowLifecycle {
                    run_id: product_run_id,
                    started: Some(started),
                    lifecycle: Some(lifecycle),
                    _blocked: blocked,
                    events: self.state.events.clone(),
                    stream_tx,
                }),
            },
        ))
    }
}

/// Product resources held for the lifetime of one VM flow.
pub(crate) struct AtmanVmFlowGuard {
    lifecycle: Option<ChildFlowLifecycle>,
}

struct ChildFlowLifecycle {
    run_id: FlowRunId,
    started: Option<atman_rt::StartedFlow<FlowRunId>>,
    lifecycle: Option<crate::tools::agent_ctrl::FlowLifecycleGuard>,
    _blocked: crate::tools::agent_ctrl::DescendantBlockGuard,
    events: Option<EventSink>,
    stream_tx: Option<tokio::sync::broadcast::Sender<StreamFrame>>,
}

impl ChildFlowLifecycle {
    fn finish(mut self, status: FlowStatus, ok: bool) {
        let Some(started) = self.started.take() else {
            return;
        };
        let cancelled = matches!(status, FlowStatus::Cancelled);
        let end = started.finish(status);
        self.lifecycle.take();
        if let Some(events) = &self.events {
            events.emit(end.clone().into());
        }
        if let Some(tx) = &self.stream_tx {
            let _ = tx.send(StreamFrame::FlowDone {
                run_id: self.run_id.0.to_string(),
                flow_name: end.flow_name,
                ok,
                cancelled,
                suicide: false,
            });
        }
    }
}

impl VmDelegate for AtmanVmDelegate {
    type Payload = AtmanPayload;
    type Error = RuntimeError;
    type Permit = AtmanEffectPermit;
    type FlowGuard = AtmanVmFlowGuard;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        permit: Self::Permit,
        context: &'a VmContext,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move {
            match (effect, permit.inner) {
                (
                    ExpressionEffect::ToolCall {
                        name,
                        positional,
                        named,
                        ..
                    },
                    AtmanEffectPermitInner::InvocationEnv,
                ) if name == "env" => crate::eval::eval_invocation_env(
                    ToolArgs { positional, named },
                    &self.host_for(context),
                ),
                (ExpressionEffect::ToolCall { .. }, AtmanEffectPermitInner::Tool(permit)) => {
                    crate::eval::execute_tool_invocation(*permit).await
                }
                (direct, AtmanEffectPermitInner::Direct) => {
                    crate::eval::execute_direct_effect(direct, &self.host_for(context)).await
                }
                (ExpressionEffect::ToolCall { name, .. }, _) => {
                    Value::Err(RuntimeError::ToolFailed(format!(
                        "tool `{name}` is missing its matching authorization permit"
                    )))
                }
                (_, _) => Value::Err(RuntimeError::ToolFailed(
                    "effect received an authorization permit for a different operation".into(),
                )),
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        effect: &'a ExpressionEffect<AtmanPayload, RuntimeError>,
        context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, RuntimeError>> {
        Box::pin(async move {
            let ExpressionEffect::ToolCall {
                name,
                positional,
                named,
                watch_rules,
            } = effect
            else {
                return Ok(AtmanEffectPermit {
                    inner: AtmanEffectPermitInner::Direct,
                });
            };
            if name == "env" {
                return Ok(AtmanEffectPermit {
                    inner: AtmanEffectPermitInner::InvocationEnv,
                });
            }
            let permit = crate::eval::authorize_tool_invocation(
                name.clone(),
                ToolArgs {
                    positional: positional.clone(),
                    named: named.clone(),
                },
                watch_rules.clone(),
                &self.host_for(context),
            )
            .await?;
            Ok(AtmanEffectPermit {
                inner: AtmanEffectPermitInner::Tool(Box::new(permit)),
            })
        })
    }

    fn enter_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), RuntimeError> {
        match call {
            Some(call) => self.enter_child(call, context),
            None => Ok(self.enter_root(context)),
        }
    }

    fn exit_flow(
        &self,
        _call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        outcome: &FlowOutcome<AtmanPayload, RuntimeError>,
        guard: Self::FlowGuard,
    ) {
        if let Some(lifecycle) = guard.lifecycle {
            let status = FlowStatus::from(atman_rt::classify_outcome(outcome, |error| {
                matches!(error, RuntimeError::Cancelled(_))
            }));
            lifecycle.finish(status, !matches!(outcome, FlowOutcome::Err(_)));
        }
    }

    fn abort_flow(
        &self,
        _call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        guard: Self::FlowGuard,
    ) {
        if let Some(lifecycle) = guard.lifecycle {
            lifecycle.finish(FlowStatus::Cancelled, false);
        }
    }

    fn cancellation_error(&self, _context: &VmContext) -> Option<RuntimeError> {
        self.binding
            .flow_cancel
            .is_cancelled()
            .then(|| RuntimeError::Cancelled("flow cancelled by user".into()))
    }

    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, RuntimeError> {
        let token = self.binding.flow_cancel.clone();
        Box::pin(async move {
            token.cancelled().await;
            RuntimeError::Cancelled("flow cancelled by user".into())
        })
    }

    fn preflight_tool(&self, name: &str, context: &VmContext) -> Option<Value> {
        match crate::eval::check_tool_call(name, &self.host_for(context)) {
            Ok(value) => value,
            Err(error) => Some(Value::Err(error)),
        }
    }

    fn tool_call_mode(&self, name: &str, _context: &VmContext) -> ToolCallMode {
        self.state
            .tools
            .get(name)
            .map(|tool| tool.call_mode())
            .unwrap_or(ToolCallMode::Immediate)
    }

    fn preflight_statement(&self, _node: &VmNode, _context: &VmContext) -> Preflight<RuntimeError> {
        let (Some(session), Some(turn_id)) = (&self.binding.session, &self.binding.turn_id) else {
            return Preflight::Continue;
        };
        let Some(injection) = session.peek_pending_l2_or_higher(turn_id) else {
            return Preflight::Continue;
        };
        match injection.level {
            InjectionLevel::L4HardStop => {
                session.mark_injection_consumed(&injection.id);
                Preflight::StopAfterNode {
                    error: RuntimeError::Cancelled("hard stop from user".into()),
                    preview: "cancelled: hard stop".into(),
                }
            }
            InjectionLevel::L3Redirect => match injection.redirect_target {
                Some(target) => {
                    session.mark_injection_consumed(&injection.id);
                    Preflight::Stop(RuntimeError::Redirect(target))
                }
                None => Preflight::Continue,
            },
            _ => Preflight::Continue,
        }
    }

    fn call_error(&self, error: VmCallError) -> RuntimeError {
        match error {
            VmCallError::MissingEntry(name) | VmCallError::MissingFlow(name) => {
                RuntimeError::UndefinedTool(name)
            }
            VmCallError::TooManyPositional(name) => {
                RuntimeError::MissingArg(format!("{name}(): too many positional args"))
            }
            VmCallError::CallDepthExceeded(name) => {
                RuntimeError::ToolFailed(format!("{name}(): maximum call depth exceeded"))
            }
            VmCallError::ControlFlowEscaped(name) => {
                RuntimeError::ToolFailed(format!("{name}(): break or continue escaped the flow"))
            }
            VmCallError::Cancelled => RuntimeError::Cancelled("flow call cancelled".into()),
            VmCallError::InvalidFutureOwner => {
                RuntimeError::ToolFailed("pending call belongs to another invocation".into())
            }
            VmCallError::FutureBoundary => {
                RuntimeError::ToolFailed("pending call cannot cross a flow or host boundary".into())
            }
        }
    }

    fn preview(&self, value: &Value, _context: &VmContext) -> Option<String> {
        crate::eval::value_preview(value)
    }

    fn error_status(&self, error: &RuntimeError) -> VmStatus {
        if matches!(error, RuntimeError::Cancelled(_)) {
            VmStatus::Cancelled
        } else {
            VmStatus::Err
        }
    }

    fn error_preview(&self, error: &RuntimeError) -> Option<String> {
        Some(error.to_string())
    }

    fn undefined_var(&self, name: String) -> RuntimeError {
        RuntimeError::UndefinedVar(name)
    }

    fn undefined_field(&self, name: String) -> RuntimeError {
        RuntimeError::UndefinedVar(name)
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

    fn on_event(&self, event: VmEvent) {
        match event {
            VmEvent::NodeStarted { context, node } => {
                let kind = crate::nodegraph::NodeKind::from(&node);
                self.emit_node_start(&context, kind, node.label);
            }
            VmEvent::NodeEnded {
                context,
                status,
                preview,
            } => self.emit_node_end(&context, status, preview),
            VmEvent::IterationStarted { context, iteration } => self.emit_node_start(
                &context,
                crate::nodegraph::NodeKind::Return,
                format!("iteration {iteration}"),
            ),
            VmEvent::IterationEnded {
                context,
                status,
                preview,
            } => self.emit_node_end(&context, status, preview),
            VmEvent::FanoutBranchStarted { context } => self.emit_node_start(
                &context,
                crate::nodegraph::NodeKind::UserConfirm,
                format!("branch[{}]", context.branch_index.unwrap_or_default()),
            ),
            VmEvent::FanoutBranchEnded { context, status } => {
                self.emit_node_end(&context, status, None);
            }
            VmEvent::FlowStarted { .. }
            | VmEvent::FlowEnded { .. }
            | VmEvent::AuthorizationRequested { .. }
            | VmEvent::AuthorizationResolved { .. }
            | VmEvent::EffectStarted { .. }
            | VmEvent::EffectEnded { .. }
            | VmEvent::CancellationObserved { .. } => {}
        }
    }
}

fn product_node_status(status: VmStatus) -> FlowNodeStatus {
    match status {
        VmStatus::Ok => FlowNodeStatus::Ok,
        VmStatus::Err => FlowNodeStatus::Err,
        VmStatus::Cancelled => FlowNodeStatus::Cancelled,
    }
}
