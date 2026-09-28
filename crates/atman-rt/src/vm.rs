//! A linked Atman program and its flow execution entry points.

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{
    fmt,
    sync::atomic::{AtomicUsize, Ordering},
};

use crate::{
    AuthorizationDelegate, CancellationDelegate, ControlDelegate, EffectDelegate, Engine,
    ExecutionScope, ExpressionEffect, ExpressionHost, FanoutBranchStatus, FlowArgs, FlowDelegate,
    FlowOutcome, HostFuture, HostValueOps, NamedValues, ObserverDelegate, Preflight, StatementHost,
    StatementOutcome, ToolCallMode, Value, ValueError, VmContext, VmDelegates, VmEffect,
    VmEffectInvocation, VmEffectInvocationId, VmEvent, VmNode, VmRunId, VmStatus,
    ast::{Arg, Contract, FlowRef, LifecycleEvent, Stmt},
    engine::bind_evaluated_call_arguments,
    expr::{EvaluatedArg, MAX_ACTIVE_CALLS},
    pattern::PatternBindError,
    program::{FlowId, LinkedProgram, ModuleId},
    route::RouteMatch,
    value::{FlowFuture, ToolFuture},
    watch::WatchRules,
};

/// Selects the child context when a cold flow call is first driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowDriveMode {
    Inline,
    Parallel,
}

/// Information about a flow call that has already been resolved by the VM.
///
/// The host may use this metadata to establish product context and emit events.
/// It does not need to inspect syntax or execute the flow body.
pub struct FlowCall<'a> {
    pub target: &'a FlowId,
    pub display_name: &'a str,
    pub source_id: &'a str,
    pub contract: Option<&'a Contract>,
    pub parent_node_id: Option<&'a str>,
    pub mode: FlowDriveMode,
    pub branch_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmCallError {
    MissingEntry(String),
    MissingFlow(String),
    TooManyPositional(String),
    CallDepthExceeded(String),
    ControlFlowEscaped(String),
    InvalidFutureOwner,
    FutureBoundary,
    Cancelled,
}

impl fmt::Display for VmCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEntry(name) => write!(f, "entry flow `{name}` does not exist"),
            Self::MissingFlow(name) => write!(f, "flow `{name}` does not exist"),
            Self::TooManyPositional(name) => {
                write!(f, "flow `{name}` received too many positional arguments")
            }
            Self::CallDepthExceeded(name) => {
                write!(f, "flow `{name}` exceeded the maximum call depth")
            }
            Self::ControlFlowEscaped(name) => {
                write!(f, "break or continue escaped from flow `{name}`")
            }
            Self::InvalidFutureOwner => write!(f, "pending call belongs to another invocation"),
            Self::FutureBoundary => write!(f, "pending call cannot cross a flow or host boundary"),
            Self::Cancelled => write!(f, "flow call was cancelled"),
        }
    }
}

impl core::error::Error for VmCallError {}

/// A product host supplies effects, execution observations, and child-flow context.
/// Flow resolution, argument binding, and body execution are owned by the VM.
pub(crate) trait VmHost: StatementHost + Clone {
    type ChildGuard: Send;

    fn call_error(&self, error: VmCallError) -> Self::Error;

    fn cancellation_error(&self) -> Option<Self::Error>;

    fn cancelled(&self) -> HostFuture<'_, Self::Error>;

    fn context(&self) -> &VmContext;

    fn eval_external_from<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        origin: VmContext,
        execution: VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;

    fn enter_child(&self, call: &FlowCall<'_>) -> Result<(Self, Self::ChildGuard), Self::Error>;

    fn exit_child(
        &self,
        call: &FlowCall<'_>,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::ChildGuard,
    );

    fn abort_child(
        &self,
        call: &FlowCall<'_>,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::ChildGuard,
    ) {
        self.exit_child(call, outcome, guard);
    }
}

struct ChildExit<'a, H: VmHost> {
    parent_host: &'a H,
    child_host: H,
    call: FlowCall<'a>,
    guard: Option<H::ChildGuard>,
}

struct ParallelPermit(Arc<AtomicUsize>);

impl Drop for ParallelPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl<H: VmHost> ChildExit<'_, H> {
    fn finish(mut self, outcome: &FlowOutcome<H::Payload, H::Error>) {
        if let Some(guard) = self.guard.take() {
            self.parent_host.exit_child(&self.call, outcome, guard);
        }
    }
}

impl<H: VmHost> Drop for ChildExit<'_, H> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            if let Some(error) = self.child_host.cancellation_error() {
                let outcome = StatementOutcome::Err(error);
                self.parent_host.exit_child(&self.call, &outcome, guard);
            } else {
                let outcome =
                    StatementOutcome::Err(self.parent_host.call_error(VmCallError::Cancelled));
                self.parent_host.abort_child(&self.call, &outcome, guard);
            }
        }
    }
}

/// The high-level interface between the VM and an embedding application.
///
/// The VM owns language control flow. Delegates receive evaluated external
/// effects, authorization requests, cancellation checks, and typed lifecycle
/// events with VM-owned execution context.
pub trait VmDelegate: Clone + Send + Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;
    type Permit: Send;
    type FlowGuard: Send;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        permit: Self::Permit,
        context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;

    fn authorize<'a>(
        &'a self,
        effect: &'a ExpressionEffect<Self::Payload, Self::Error>,
        context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, Self::Error>>;

    fn enter_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), Self::Error>;

    fn exit_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::FlowGuard,
    );

    fn abort_flow(&self, call: Option<&FlowCall<'_>>, context: &VmContext, guard: Self::FlowGuard);

    fn cancellation_error(&self, _context: &VmContext) -> Option<Self::Error> {
        None
    }

    /// Resolves when pending authorization, effects, or flow execution must stop.
    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, Self::Error> {
        Box::pin(async {
            loop {
                core::future::pending::<()>().await;
            }
        })
    }

    /// Reject a tool before its arguments run. Authorization runs after evaluation.
    fn preflight_tool(
        &self,
        _name: &str,
        _context: &VmContext,
    ) -> Option<Value<Self::Payload, Self::Error>> {
        None
    }

    fn tool_call_mode(&self, _name: &str, _context: &VmContext) -> ToolCallMode {
        ToolCallMode::Immediate
    }

    fn preflight_statement(&self, _node: &VmNode, _context: &VmContext) -> Preflight<Self::Error> {
        Preflight::Continue
    }

    fn call_error(&self, error: VmCallError) -> Self::Error {
        Self::Error::type_mismatch("valid Atman call", error.to_string())
    }

    fn node_preview(
        &self,
        _value: &Value<Self::Payload, Self::Error>,
        _context: &VmContext,
    ) -> Option<String> {
        None
    }

    fn effect_input_preview(
        &self,
        _effect: &ExpressionEffect<Self::Payload, Self::Error>,
        _origin: &VmContext,
        _execution: &VmContext,
    ) -> Option<String> {
        None
    }

    fn effect_result_preview(
        &self,
        _value: &Value<Self::Payload, Self::Error>,
        _invocation: &VmEffectInvocation,
    ) -> Option<String> {
        None
    }

    fn error_status(&self, _error: &Self::Error) -> VmStatus {
        VmStatus::Err
    }

    fn error_preview(&self, _error: &Self::Error) -> Option<String> {
        None
    }

    fn undefined_var(&self, name: String) -> Self::Error {
        Self::Error::type_mismatch("bound variable", name)
    }

    fn undefined_field(&self, name: String) -> Self::Error {
        Self::Error::type_mismatch("existing field", name)
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        Self::Error::type_mismatch("matching pattern", format!("{error:?}"))
    }

    fn on_event(&self, _event: VmEvent) {}
}

impl<D, A, O, C, F, L> VmDelegate for VmDelegates<D, A, O, C, F, L>
where
    D: EffectDelegate,
    A: AuthorizationDelegate<D::Payload, D::Error, Permit = D::Permit>,
    O: ObserverDelegate,
    C: CancellationDelegate<D::Error>,
    F: FlowDelegate<D::Payload, D::Error>,
    L: ControlDelegate<D::Error>,
{
    type Payload = D::Payload;
    type Error = D::Error;
    type Permit = D::Permit;
    type FlowGuard = F::Guard;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        permit: Self::Permit,
        context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.effect.invoke(effect, permit, context)
    }

    fn authorize<'a>(
        &'a self,
        effect: &'a ExpressionEffect<Self::Payload, Self::Error>,
        context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, Self::Error>> {
        self.authorization.authorize(effect, context)
    }

    fn enter_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), Self::Error> {
        self.flows
            .enter(call, context)
            .map(|guard| (self.clone(), guard))
    }

    fn exit_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::FlowGuard,
    ) {
        self.flows.exit(call, context, outcome, guard);
    }

    fn abort_flow(&self, call: Option<&FlowCall<'_>>, context: &VmContext, guard: Self::FlowGuard) {
        self.flows.abort(call, context, guard);
    }

    fn cancellation_error(&self, context: &VmContext) -> Option<Self::Error> {
        self.cancellation.cancellation_error(context)
    }

    fn cancelled<'a>(&'a self, context: &'a VmContext) -> HostFuture<'a, Self::Error> {
        self.cancellation.cancelled(context)
    }

    fn preflight_tool(
        &self,
        name: &str,
        context: &VmContext,
    ) -> Option<Value<Self::Payload, Self::Error>> {
        self.effect.preflight_tool(name, context)
    }

    fn tool_call_mode(&self, name: &str, context: &VmContext) -> ToolCallMode {
        self.effect.tool_call_mode(name, context)
    }

    fn preflight_statement(&self, node: &VmNode, context: &VmContext) -> Preflight<Self::Error> {
        self.control.preflight_statement(node, context)
    }

    fn call_error(&self, error: VmCallError) -> Self::Error {
        self.control.call_error(error)
    }

    fn node_preview(
        &self,
        value: &Value<Self::Payload, Self::Error>,
        context: &VmContext,
    ) -> Option<String> {
        self.effect.node_preview(value, context)
    }

    fn effect_input_preview(
        &self,
        effect: &ExpressionEffect<Self::Payload, Self::Error>,
        origin: &VmContext,
        execution: &VmContext,
    ) -> Option<String> {
        self.effect.effect_input_preview(effect, origin, execution)
    }

    fn effect_result_preview(
        &self,
        value: &Value<Self::Payload, Self::Error>,
        invocation: &VmEffectInvocation,
    ) -> Option<String> {
        self.effect.effect_result_preview(value, invocation)
    }

    fn error_status(&self, error: &Self::Error) -> VmStatus {
        if self.cancellation.is_cancellation(error) {
            VmStatus::Cancelled
        } else {
            self.effect.error_status(error)
        }
    }

    fn error_preview(&self, error: &Self::Error) -> Option<String> {
        self.control.error_preview(error)
    }

    fn undefined_var(&self, name: String) -> Self::Error {
        self.control.undefined_var(name)
    }

    fn undefined_field(&self, name: String) -> Self::Error {
        self.control.undefined_field(name)
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        self.control.pattern_error(error)
    }

    fn on_event(&self, event: VmEvent) {
        self.observer.on_event(event);
    }
}

fn outcome_status<H: VmDelegate>(
    delegate: &H,
    outcome: &FlowOutcome<H::Payload, H::Error>,
) -> VmStatus {
    match outcome {
        StatementOutcome::Err(error) => delegate.error_status(error),
        _ => VmStatus::Ok,
    }
}

fn outcome_error<H: VmDelegate>(
    delegate: &H,
    outcome: &FlowOutcome<H::Payload, H::Error>,
) -> Option<String> {
    match outcome {
        StatementOutcome::Err(error) => delegate.error_preview(error),
        _ => None,
    }
}

impl VmContext {
    fn for_node(&self, node_id: &str, parent_node_id: Option<&str>) -> Self {
        let mut context = self.clone();
        context.node_id = Some(node_id.into());
        context.parent_node_id = parent_node_id.map(String::from);
        context
    }

    fn for_branch(&self, index: usize) -> Self {
        let mut context = self.clone();
        context.parent_node_id = self.node_id.clone();
        context.node_id = self
            .node_id
            .as_ref()
            .map(|node_id| format!("{node_id}.branch[{index}]"));
        context.drive_mode = FlowDriveMode::Parallel;
        context.branch_index = Some(index);
        context
    }
}

struct DelegateAdapter<H: VmDelegate> {
    delegate: H,
    context: VmContext,
    run_ids: Arc<AtomicUsize>,
    effect_invocation_ids: Arc<AtomicUsize>,
}

impl<H: VmDelegate> Clone for DelegateAdapter<H> {
    fn clone(&self) -> Self {
        Self {
            delegate: self.delegate.clone(),
            context: self.context.clone(),
            run_ids: Arc::clone(&self.run_ids),
            effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
        }
    }
}

impl<H: VmDelegate> DelegateAdapter<H> {
    fn next_run_id(&self) -> VmRunId {
        VmRunId(self.run_ids.fetch_add(1, Ordering::Relaxed))
    }

    fn cancellation_error(&self) -> Option<H::Error> {
        observe_cancellation(&self.delegate, &self.context)
    }

    fn eval_external_attempt<'a>(
        &'a self,
        effect: ExpressionEffect<H::Payload, H::Error>,
        origin: VmContext,
        execution: VmContext,
    ) -> HostFuture<'a, Value<H::Payload, H::Error>> {
        let delegate = self.delegate.clone();
        let effect_invocation_ids = Arc::clone(&self.effect_invocation_ids);
        Box::pin(async move {
            if let Some(error) = observe_cancellation(&delegate, &execution) {
                return Value::Err(error);
            }
            let invocation = VmEffectInvocation {
                id: VmEffectInvocationId(effect_invocation_ids.fetch_add(1, Ordering::Relaxed)),
                effect: VmEffect::from(&effect),
                input_preview: delegate.effect_input_preview(&effect, &origin, &execution),
                origin,
                execution: execution.clone(),
            };
            let authorization = AuthorizationExit::start(delegate.clone(), invocation.clone());
            let permit = match crate::race_cancel(
                delegate.authorize(&effect, &execution),
                wait_for_cancellation(&delegate, &execution),
            )
            .await
            {
                Ok(permit) => {
                    if let Some(error) = observe_cancellation(&delegate, &execution) {
                        authorization.finish(VmStatus::Cancelled);
                        return Value::Err(error);
                    }
                    authorization.finish(VmStatus::Ok);
                    permit
                }
                Err(error) => {
                    let status = delegate.error_status(&error);
                    authorization.finish(status);
                    return Value::Err(error);
                }
            };
            let effect_exit = EffectExit::start(delegate.clone(), invocation.clone());
            let value = match crate::race_cancel(
                async { Ok::<_, H::Error>(delegate.invoke(effect, permit, &execution).await) },
                wait_for_cancellation(&delegate, &execution),
            )
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    effect_exit.finish(VmStatus::Cancelled, None);
                    return Value::Err(error);
                }
            };
            if let Some(error) = observe_cancellation(&delegate, &execution) {
                effect_exit.finish(VmStatus::Cancelled, None);
                return Value::Err(error);
            }
            let status = value_status(&delegate, &value);
            let result_preview = delegate.effect_result_preview(&value, &invocation);
            effect_exit.finish(status, result_preview);
            value
        })
    }
}

fn observe_cancellation<H: VmDelegate>(delegate: &H, context: &VmContext) -> Option<H::Error> {
    let error = delegate.cancellation_error(context);
    if error.is_some() {
        delegate.on_event(VmEvent::CancellationObserved {
            context: context.clone(),
        });
    }
    error
}

async fn wait_for_cancellation<H: VmDelegate>(delegate: &H, context: &VmContext) -> H::Error {
    let error = delegate.cancelled(context).await;
    delegate.on_event(VmEvent::CancellationObserved {
        context: context.clone(),
    });
    error
}

struct AuthorizationExit<H: VmDelegate> {
    delegate: H,
    invocation: VmEffectInvocation,
    active: bool,
}

impl<H: VmDelegate> AuthorizationExit<H> {
    fn start(delegate: H, invocation: VmEffectInvocation) -> Self {
        delegate.on_event(VmEvent::AuthorizationRequested {
            invocation: invocation.clone(),
        });
        Self {
            delegate,
            invocation,
            active: true,
        }
    }

    fn finish(mut self, status: VmStatus) {
        self.active = false;
        self.delegate.on_event(VmEvent::AuthorizationResolved {
            invocation: self.invocation.clone(),
            status,
        });
    }
}

impl<H: VmDelegate> Drop for AuthorizationExit<H> {
    fn drop(&mut self) {
        if self.active {
            self.delegate.on_event(VmEvent::AuthorizationResolved {
                invocation: self.invocation.clone(),
                status: VmStatus::Cancelled,
            });
        }
    }
}

struct EffectExit<H: VmDelegate> {
    delegate: H,
    invocation: VmEffectInvocation,
    active: bool,
}

impl<H: VmDelegate> EffectExit<H> {
    fn start(delegate: H, invocation: VmEffectInvocation) -> Self {
        delegate.on_event(VmEvent::EffectStarted {
            invocation: invocation.clone(),
        });
        Self {
            delegate,
            invocation,
            active: true,
        }
    }

    fn finish(mut self, status: VmStatus, result_preview: Option<String>) {
        self.active = false;
        self.delegate.on_event(VmEvent::EffectEnded {
            invocation: self.invocation.clone(),
            status,
            result_preview,
        });
    }
}

impl<H: VmDelegate> Drop for EffectExit<H> {
    fn drop(&mut self) {
        if self.active {
            self.delegate.on_event(VmEvent::EffectEnded {
                invocation: self.invocation.clone(),
                status: VmStatus::Cancelled,
                result_preview: None,
            });
        }
    }
}

fn value_status<H: VmDelegate>(delegate: &H, value: &Value<H::Payload, H::Error>) -> VmStatus {
    match value {
        Value::Err(error) => delegate.error_status(error),
        _ => VmStatus::Ok,
    }
}

impl<H: VmDelegate> ExpressionHost for DelegateAdapter<H> {
    type Payload = H::Payload;
    type Error = H::Error;

    fn undefined_var(&self, name: String) -> Self::Error {
        self.delegate.undefined_var(name)
    }

    fn undefined_field(&self, name: String) -> Self::Error {
        self.delegate.undefined_field(name)
    }

    fn cancellation_error(&self) -> Option<Self::Error> {
        self.cancellation_error()
    }

    fn await_drive_mode(&self) -> FlowDriveMode {
        self.context.drive_mode
    }

    fn fanout_branch_start(&self, index: usize) {
        self.delegate.on_event(VmEvent::FanoutBranchStarted {
            context: self.context.for_branch(index),
        });
    }

    fn branch_host(&self, index: usize) -> Self {
        Self {
            delegate: self.delegate.clone(),
            context: self.context.for_branch(index),
            run_ids: Arc::clone(&self.run_ids),
            effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
        }
    }

    fn fanout_branch_end(&self, index: usize, status: FanoutBranchStatus) {
        self.delegate.on_event(VmEvent::FanoutBranchEnded {
            context: self.context.for_branch(index),
            status: match status {
                FanoutBranchStatus::Ok => VmStatus::Ok,
                FanoutBranchStatus::Err => VmStatus::Err,
                FanoutBranchStatus::Cancelled => VmStatus::Cancelled,
            },
        });
    }

    fn fanout_error_status(&self, error: &Self::Error) -> FanoutBranchStatus {
        match self.delegate.error_status(error) {
            VmStatus::Cancelled => FanoutBranchStatus::Cancelled,
            VmStatus::Ok => FanoutBranchStatus::Ok,
            VmStatus::Err => FanoutBranchStatus::Err,
        }
    }

    fn preflight_tool(&self, name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        self.delegate.preflight_tool(name, &self.context)
    }

    fn tool_call_mode(&self, name: &str) -> ToolCallMode {
        self.delegate.tool_call_mode(name, &self.context)
    }

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        let context = self.context.clone();
        self.eval_external_attempt(effect, context.clone(), context)
    }
}

enum DelegateScopeKind {
    Node,
    Iteration,
}

struct DelegateExecutionScope<H: VmDelegate> {
    delegate: H,
    context: VmContext,
    kind: DelegateScopeKind,
}

impl<H: VmDelegate> ExecutionScope<Value<H::Payload, H::Error>, H::Error>
    for DelegateExecutionScope<H>
{
    fn finish(self, outcome: &FlowOutcome<H::Payload, H::Error>, preview: Option<&str>) {
        let status = outcome_status(&self.delegate, outcome);
        match self.kind {
            DelegateScopeKind::Node => self.delegate.on_event(VmEvent::NodeEnded {
                context: self.context,
                status,
                preview: preview.map(String::from),
            }),
            DelegateScopeKind::Iteration => {
                self.delegate.on_event(VmEvent::IterationEnded {
                    context: self.context,
                    status,
                    preview: preview.map(String::from),
                });
            }
        }
    }

    fn abort(self) {
        match self.kind {
            DelegateScopeKind::Node => self.delegate.on_event(VmEvent::NodeEnded {
                context: self.context,
                status: VmStatus::Cancelled,
                preview: None,
            }),
            DelegateScopeKind::Iteration => {
                self.delegate.on_event(VmEvent::IterationEnded {
                    context: self.context,
                    status: VmStatus::Cancelled,
                    preview: None,
                });
            }
        }
    }
}

impl<H: VmDelegate> StatementHost for DelegateAdapter<H> {
    type Payload = H::Payload;
    type Error = H::Error;
    type ExprHost = Self;
    type NodeScope = DelegateExecutionScope<H>;
    type IterationScope = DelegateExecutionScope<H>;

    fn preflight(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Preflight<Self::Error> {
        let context = self.context.for_node(node_id, parent_node_id);
        if let Some(error) = self.delegate.cancellation_error(&context) {
            self.delegate.on_event(VmEvent::CancellationObserved {
                context: context.clone(),
            });
            return Preflight::Stop(error);
        }
        self.delegate
            .preflight_statement(&VmNode::from(stmt), &context)
    }

    fn node_start(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::NodeScope {
        let context = self.context.for_node(node_id, parent_node_id);
        self.delegate.on_event(VmEvent::NodeStarted {
            context: context.clone(),
            node: VmNode::from(stmt),
        });
        DelegateExecutionScope {
            delegate: self.delegate.clone(),
            context,
            kind: DelegateScopeKind::Node,
        }
    }

    fn expression_host(
        &self,
        node_id: Option<&str>,
        parent_node_id: Option<&str>,
    ) -> Self::ExprHost {
        let context = match node_id {
            Some(node_id) => self.context.for_node(node_id, parent_node_id),
            None => self.context.clone(),
        };
        Self {
            delegate: self.delegate.clone(),
            context,
            run_ids: Arc::clone(&self.run_ids),
            effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
        }
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        self.delegate.pattern_error(error)
    }

    fn preview(
        &self,
        value: &Value<Self::Payload, Self::Error>,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Option<String> {
        self.delegate
            .node_preview(value, &self.context.for_node(node_id, parent_node_id))
    }

    fn iteration_start(
        &mut self,
        iteration: u64,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::IterationScope {
        let context = self.context.for_node(node_id, parent_node_id);
        self.delegate.on_event(VmEvent::IterationStarted {
            context: context.clone(),
            iteration,
        });
        DelegateExecutionScope {
            delegate: self.delegate.clone(),
            context,
            kind: DelegateScopeKind::Iteration,
        }
    }
}

struct DelegateChild<H: VmDelegate> {
    delegate: H,
    context: VmContext,
    flow_guard: H::FlowGuard,
}

impl<H: VmDelegate> VmHost for DelegateAdapter<H> {
    type ChildGuard = DelegateChild<H>;

    fn call_error(&self, error: VmCallError) -> Self::Error {
        self.delegate.call_error(error)
    }

    fn cancellation_error(&self) -> Option<Self::Error> {
        DelegateAdapter::cancellation_error(self)
    }

    fn cancelled(&self) -> HostFuture<'_, Self::Error> {
        Box::pin(wait_for_cancellation(&self.delegate, &self.context))
    }

    fn context(&self) -> &VmContext {
        &self.context
    }

    fn eval_external_from<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        origin: VmContext,
        execution: VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.eval_external_attempt(effect, origin, execution)
    }

    fn enter_child(&self, call: &FlowCall<'_>) -> Result<(Self, Self::ChildGuard), Self::Error> {
        let context = VmContext {
            run_id: self.next_run_id(),
            parent_run_id: Some(self.context.run_id),
            source_id: call.source_id.into(),
            flow: call.target.clone(),
            caller_node_id: call.parent_node_id.map(String::from),
            node_id: None,
            parent_node_id: None,
            drive_mode: call.mode,
            branch_index: call.branch_index,
        };
        let (delegate, flow_guard) = self.delegate.enter_flow(Some(call), &context)?;
        delegate.on_event(VmEvent::FlowStarted {
            context: context.clone(),
        });
        Ok((
            Self {
                delegate: delegate.clone(),
                context: context.clone(),
                run_ids: Arc::clone(&self.run_ids),
                effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
            },
            DelegateChild {
                delegate,
                context,
                flow_guard,
            },
        ))
    }

    fn exit_child(
        &self,
        call: &FlowCall<'_>,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::ChildGuard,
    ) {
        let DelegateChild {
            delegate,
            context,
            flow_guard,
        } = guard;
        let status = outcome_status(&delegate, outcome);
        let error = outcome_error(&delegate, outcome);
        delegate.exit_flow(Some(call), &context, outcome, flow_guard);
        delegate.on_event(VmEvent::FlowEnded {
            context,
            status,
            error,
        });
    }

    fn abort_child(
        &self,
        call: &FlowCall<'_>,
        _outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::ChildGuard,
    ) {
        let DelegateChild {
            delegate,
            context,
            flow_guard,
        } = guard;
        delegate.abort_flow(Some(call), &context, flow_guard);
        delegate.on_event(VmEvent::FlowEnded {
            context,
            status: VmStatus::Cancelled,
            error: None,
        });
    }
}

struct DelegateFlowExit<H: VmDelegate> {
    delegate: H,
    context: VmContext,
    guard: Option<H::FlowGuard>,
}

impl<H: VmDelegate> DelegateFlowExit<H> {
    fn start(delegate: H, context: VmContext, guard: H::FlowGuard) -> Self {
        delegate.on_event(VmEvent::FlowStarted {
            context: context.clone(),
        });
        Self {
            delegate,
            context,
            guard: Some(guard),
        }
    }

    fn finish(mut self, outcome: &FlowOutcome<H::Payload, H::Error>) {
        let guard = self.guard.take().expect("flow guard must be present");
        let status = outcome_status(&self.delegate, outcome);
        let error = outcome_error(&self.delegate, outcome);
        self.delegate.exit_flow(None, &self.context, outcome, guard);
        self.delegate.on_event(VmEvent::FlowEnded {
            context: self.context.clone(),
            status,
            error,
        });
    }
}

impl<H: VmDelegate> Drop for DelegateFlowExit<H> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            self.delegate.abort_flow(None, &self.context, guard);
            self.delegate.on_event(VmEvent::FlowEnded {
                context: self.context.clone(),
                status: VmStatus::Cancelled,
                error: None,
            });
        }
    }
}

/// A complete linked program. With `syntax`, it also loads and parses source text.
#[derive(Clone)]
pub struct Vm {
    program: Arc<LinkedProgram>,
    run_ids: Arc<AtomicUsize>,
    effect_invocation_ids: Arc<AtomicUsize>,
}

impl Vm {
    const MAX_CALL_DEPTH: usize = 128;

    pub fn new(program: LinkedProgram) -> Self {
        Self::from_shared(Arc::new(program))
    }

    pub fn from_shared(program: Arc<LinkedProgram>) -> Self {
        Self {
            program,
            run_ids: Arc::new(AtomicUsize::new(1)),
            effect_invocation_ids: Arc::new(AtomicUsize::new(1)),
        }
    }

    fn next_run_id(&self) -> VmRunId {
        VmRunId(self.run_ids.fetch_add(1, Ordering::Relaxed))
    }

    #[cfg(feature = "syntax")]
    pub fn compile<R: crate::SourceResolver>(
        entry: crate::Source,
        resolver: &R,
    ) -> Result<Self, crate::program::CompileError> {
        LinkedProgram::compile(entry, resolver).map(Self::new)
    }

    pub fn program(&self) -> &LinkedProgram {
        &self.program
    }

    pub fn route(&self, input: &str) -> Option<RouteMatch> {
        self.program.route(input)
    }

    /// Runs one entry flow through a high-level delegate contract.
    pub async fn run<H: VmDelegate>(
        &self,
        name: &str,
        args: FlowArgs<H::Payload, H::Error>,
        delegate: H,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let Some(id) = self.program.entry_flow(name) else {
            return StatementOutcome::Err(
                delegate.call_error(VmCallError::MissingEntry(name.into())),
            );
        };
        self.run_flow(id, args, delegate).await
    }

    /// Runs one resolved flow through the same high-level delegate contract.
    pub async fn run_flow<H: VmDelegate>(
        &self,
        id: FlowId,
        args: FlowArgs<H::Payload, H::Error>,
        delegate: H,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let Some(source) = self.program.module(id.module) else {
            return StatementOutcome::Err(delegate.call_error(VmCallError::MissingFlow(id.name)));
        };
        let context = VmContext {
            run_id: self.next_run_id(),
            parent_run_id: None,
            source_id: source.source_id.clone(),
            flow: id.clone(),
            caller_node_id: None,
            node_id: None,
            parent_node_id: None,
            drive_mode: FlowDriveMode::Inline,
            branch_index: None,
        };
        let (delegate, guard) = match delegate.enter_flow(None, &context) {
            Ok(entered) => entered,
            Err(error) => return StatementOutcome::Err(error),
        };
        let exit = DelegateFlowExit::start(delegate.clone(), context.clone(), guard);
        let adapter = DelegateAdapter {
            delegate,
            context,
            run_ids: Arc::clone(&self.run_ids),
            effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
        };
        let outcome = match adapter.cancellation_error() {
            Some(error) => StatementOutcome::Err(error),
            None => {
                let run_adapter = adapter.clone();
                match crate::race_cancel(
                    async {
                        Ok::<_, H::Error>(self.run_flow_internal(id, args, run_adapter).await)
                    },
                    adapter.cancelled(),
                )
                .await
                {
                    Err(error) => StatementOutcome::Err(error),
                    Ok(outcome) => match adapter.cancellation_error() {
                        Some(error) => StatementOutcome::Err(error),
                        None => outcome,
                    },
                }
            }
        };
        exit.finish(&outcome);
        outcome
    }

    async fn run_flow_internal<H: VmHost>(
        &self,
        id: FlowId,
        args: FlowArgs<H::Payload, H::Error>,
        host: H,
    ) -> FlowOutcome<H::Payload, H::Error> {
        if args.iter().any(|(_, value)| value.contains_pending_call()) {
            return StatementOutcome::Err(host.call_error(VmCallError::FutureBoundary));
        }
        let Some(flow) = self.program.flow(&id) else {
            return StatementOutcome::Err(host.call_error(VmCallError::MissingFlow(id.name)));
        };
        let mut engine = Engine::new(VmStatementHost {
            host,
            program: Arc::clone(&self.program),
            module: id.module,
            depth: 0,
            owner: Arc::new(()),
            parallel_active: Arc::new(AtomicUsize::new(0)),
        });
        engine.run_flow(flow, args).await
    }

    /// Executes matching lifecycle bodies in declaration order.
    pub async fn run_lifecycle<H: VmDelegate>(
        &self,
        event: LifecycleEvent,
        delegate: H,
    ) -> Vec<FlowOutcome<H::Payload, H::Error>> {
        let module = self.program.entry_module();
        let source_id = self
            .program
            .module(module)
            .map(|source| source.source_id.clone())
            .unwrap_or_default();
        let mut outcomes = Vec::new();
        for flow in self.program.lifecycle_flows(event) {
            let context = VmContext {
                run_id: self.next_run_id(),
                parent_run_id: None,
                source_id: source_id.clone(),
                flow: FlowId {
                    module,
                    name: flow.name.name.clone(),
                },
                caller_node_id: None,
                node_id: None,
                parent_node_id: None,
                drive_mode: FlowDriveMode::Inline,
                branch_index: None,
            };
            let (flow_delegate, guard) = match delegate.enter_flow(None, &context) {
                Ok(entered) => entered,
                Err(error) => {
                    outcomes.push(StatementOutcome::Err(error));
                    continue;
                }
            };
            let exit = DelegateFlowExit::start(flow_delegate.clone(), context.clone(), guard);
            let adapter = DelegateAdapter {
                delegate: flow_delegate,
                context,
                run_ids: Arc::clone(&self.run_ids),
                effect_invocation_ids: Arc::clone(&self.effect_invocation_ids),
            };
            let mut engine = Engine::new(VmStatementHost {
                host: adapter.clone(),
                program: Arc::clone(&self.program),
                module,
                depth: 0,
                owner: Arc::new(()),
                parallel_active: Arc::new(AtomicUsize::new(0)),
            });
            let outcome = match adapter.cancellation_error() {
                Some(error) => StatementOutcome::Err(error),
                None => {
                    match crate::race_cancel(
                        async { Ok::<_, H::Error>(engine.run_flow(&flow, Vec::new()).await) },
                        adapter.cancelled(),
                    )
                    .await
                    {
                        Err(error) => StatementOutcome::Err(error),
                        Ok(outcome) => match adapter.cancellation_error() {
                            Some(error) => StatementOutcome::Err(error),
                            None => outcome,
                        },
                    }
                }
            };
            exit.finish(&outcome);
            outcomes.push(outcome);
        }
        outcomes
    }
}

struct VmStatementHost<H: VmHost> {
    host: H,
    program: Arc<LinkedProgram>,
    module: ModuleId,
    depth: usize,
    owner: Arc<()>,
    parallel_active: Arc<AtomicUsize>,
}

impl<H: VmHost> StatementHost for VmStatementHost<H> {
    type Payload = H::Payload;
    type Error = H::Error;
    type ExprHost = VmExpressionHost<H>;
    type NodeScope = H::NodeScope;
    type IterationScope = H::IterationScope;

    fn preflight(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Preflight<Self::Error> {
        self.host.preflight(stmt, node_id, parent_node_id)
    }

    fn node_start(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::NodeScope {
        self.host.node_start(stmt, node_id, parent_node_id)
    }

    fn expression_host(
        &self,
        node_id: Option<&str>,
        parent_node_id: Option<&str>,
    ) -> Self::ExprHost {
        let effect_context = match node_id {
            Some(node_id) => self.host.context().for_node(node_id, parent_node_id),
            None => self.host.context().clone(),
        };
        VmExpressionHost {
            effect_host: self.host.expression_host(node_id, parent_node_id),
            host: self.host.clone(),
            program: Arc::clone(&self.program),
            module: self.module,
            depth: self.depth,
            node_id: node_id.map(String::from),
            owner: Arc::clone(&self.owner),
            parallel_active: Arc::clone(&self.parallel_active),
            await_mode: FlowDriveMode::Inline,
            branch_index: None,
            effect_context,
        }
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        self.host.pattern_error(error)
    }

    fn iteration_start(
        &mut self,
        iteration: u64,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::IterationScope {
        self.host
            .iteration_start(iteration, node_id, parent_node_id)
    }

    fn preview(
        &self,
        value: &Value<Self::Payload, Self::Error>,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Option<String> {
        self.host.preview(value, node_id, parent_node_id)
    }
}

struct VmExpressionHost<H: VmHost> {
    effect_host: H::ExprHost,
    host: H,
    program: Arc<LinkedProgram>,
    module: ModuleId,
    depth: usize,
    node_id: Option<String>,
    owner: Arc<()>,
    parallel_active: Arc<AtomicUsize>,
    await_mode: FlowDriveMode,
    branch_index: Option<usize>,
    effect_context: VmContext,
}

impl<H: VmHost> Clone for VmExpressionHost<H> {
    fn clone(&self) -> Self {
        Self {
            effect_host: self.effect_host.clone(),
            host: self.host.clone(),
            program: Arc::clone(&self.program),
            module: self.module,
            depth: self.depth,
            node_id: self.node_id.clone(),
            owner: Arc::clone(&self.owner),
            parallel_active: Arc::clone(&self.parallel_active),
            await_mode: self.await_mode,
            branch_index: self.branch_index,
            effect_context: self.effect_context.clone(),
        }
    }
}

impl<H: VmHost> VmExpressionHost<H> {
    fn acquire_parallel_permit(
        &self,
        mode: FlowDriveMode,
    ) -> Result<Option<ParallelPermit>, H::Error> {
        if mode == FlowDriveMode::Inline {
            return Ok(None);
        }
        loop {
            let active = self.parallel_active.load(Ordering::Acquire);
            if active >= MAX_ACTIVE_CALLS {
                return Err(H::Error::type_mismatch(
                    "fanout with at most 128 concurrent calls",
                    "active call limit exceeded".into(),
                ));
            }
            if self
                .parallel_active
                .compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Some(ParallelPermit(Arc::clone(&self.parallel_active))));
            }
        }
    }

    fn create_flow_future(
        &self,
        name: FlowRef,
        args: Vec<EvaluatedArg<H::Payload, H::Error>>,
    ) -> Value<H::Payload, H::Error> {
        let display_name = name.display_name();
        let Some(id) = self.program.resolve(self.module, &name) else {
            return Value::Err(self.host.call_error(VmCallError::MissingFlow(display_name)));
        };
        let Some(flow) = self.program.flow(&id) else {
            return Value::Err(self.host.call_error(VmCallError::MissingFlow(display_name)));
        };
        if self.depth >= Vm::MAX_CALL_DEPTH {
            return Value::Err(
                self.host
                    .call_error(VmCallError::CallDepthExceeded(display_name)),
            );
        }
        let bindings = match bind_evaluated_call_arguments(&flow.params, args) {
            Ok(bindings) => bindings,
            Err(crate::CallArgumentError::TooManyPositional) => {
                return Value::Err(
                    self.host
                        .call_error(VmCallError::TooManyPositional(display_name)),
                );
            }
            Err(crate::CallArgumentError::Evaluation(value)) => return value,
        };
        if bindings
            .iter()
            .any(|(_, value)| value.contains_pending_call())
        {
            return Value::Err(self.host.call_error(VmCallError::FutureBoundary));
        }
        Value::FlowFuture(Arc::new(FlowFuture::new(
            id,
            bindings,
            Arc::clone(&self.owner),
        )))
    }

    fn call_flow_future<'a>(
        &'a self,
        future: &'a FlowFuture<H::Payload, H::Error>,
        mode: FlowDriveMode,
    ) -> HostFuture<'a, Value<H::Payload, H::Error>> {
        Box::pin(async move {
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
            let id = &future.target;
            let display_name = id.name.clone();
            let Some(flow) = self.program.flow(id) else {
                return Value::Err(self.host.call_error(VmCallError::MissingFlow(display_name)));
            };
            if self.depth >= Vm::MAX_CALL_DEPTH {
                return Value::Err(
                    self.host
                        .call_error(VmCallError::CallDepthExceeded(display_name)),
                );
            }
            let Some(source) = self.program.module(id.module) else {
                return Value::Err(self.host.call_error(VmCallError::MissingFlow(display_name)));
            };
            let call = FlowCall {
                target: id,
                display_name: &display_name,
                source_id: &source.source_id,
                contract: flow.contract.as_ref(),
                parent_node_id: self.node_id.as_deref(),
                mode,
                branch_index: self.branch_index,
            };
            let (child_host, guard) = match self.host.enter_child(&call) {
                Ok(child) => child,
                Err(error) => return Value::Err(error),
            };
            let child_cancellation = child_host.clone();
            let exit = ChildExit {
                parent_host: &self.host,
                child_host: child_cancellation.clone(),
                call,
                guard: Some(guard),
            };
            let mut engine = Engine::new(VmStatementHost {
                host: child_host,
                program: Arc::clone(&self.program),
                module: id.module,
                depth: self.depth + 1,
                owner: Arc::new(()),
                parallel_active: Arc::clone(&self.parallel_active),
            });
            let outcome = match child_cancellation.cancellation_error() {
                Some(error) => StatementOutcome::Err(error),
                None => match crate::race_cancel(
                    async { Ok::<_, H::Error>(engine.run_flow(flow, future.args.clone()).await) },
                    child_cancellation.cancelled(),
                )
                .await
                {
                    Err(error) => StatementOutcome::Err(error),
                    Ok(outcome) => match child_cancellation.cancellation_error() {
                        Some(error) => StatementOutcome::Err(error),
                        None => outcome,
                    },
                },
            };
            exit.finish(&outcome);
            match outcome {
                StatementOutcome::Return(value) => value,
                StatementOutcome::Err(error) => Value::Err(error),
                StatementOutcome::Continue => Value::Unit,
                StatementOutcome::LoopBreak | StatementOutcome::LoopContinue => Value::Err(
                    self.host
                        .call_error(VmCallError::ControlFlowEscaped(display_name)),
                ),
            }
        })
    }
}

impl<H: VmHost> ExpressionHost for VmExpressionHost<H> {
    type Payload = H::Payload;
    type Error = H::Error;

    fn undefined_var(&self, name: String) -> Self::Error {
        self.effect_host.undefined_var(name)
    }

    fn undefined_field(&self, name: String) -> Self::Error {
        self.effect_host.undefined_field(name)
    }

    fn cancellation_error(&self) -> Option<Self::Error> {
        self.effect_host.cancellation_error()
    }

    fn await_drive_mode(&self) -> FlowDriveMode {
        self.await_mode
    }

    fn preflight_tool(&self, name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        self.effect_host.preflight_tool(name)
    }

    fn tool_call_mode(&self, name: &str) -> ToolCallMode {
        self.effect_host.tool_call_mode(name)
    }

    fn make_tool_future(
        &self,
        name: String,
        positional: Vec<Value<Self::Payload, Self::Error>>,
        named: NamedValues<Self::Payload, Self::Error>,
        watch_rules: Option<WatchRules>,
    ) -> Value<Self::Payload, Self::Error> {
        Value::ToolFuture(Arc::new(ToolFuture::new(
            name,
            positional,
            named,
            watch_rules,
            Some(Arc::clone(&self.owner)),
            Some(self.effect_context.clone()),
        )))
    }

    fn validate_tool_future(
        &self,
        future: &ToolFuture<Self::Payload, Self::Error>,
    ) -> Result<(), Self::Error> {
        future
            .belongs_to(&self.owner)
            .then_some(())
            .ok_or_else(|| self.host.call_error(VmCallError::InvalidFutureOwner))
    }

    fn preflight_flow(&self, name: &FlowRef, args: &[Arg]) -> Result<(), Self::Error> {
        let display_name = name.display_name();
        let id = self.program.resolve(self.module, name).ok_or_else(|| {
            self.host
                .call_error(VmCallError::MissingFlow(display_name.clone()))
        })?;
        let flow = self.program.flow(&id).ok_or_else(|| {
            self.host
                .call_error(VmCallError::MissingFlow(display_name.clone()))
        })?;
        if args
            .iter()
            .enumerate()
            .any(|(index, arg)| matches!(arg, Arg::Positional(_)) && index >= flow.params.len())
        {
            return Err(self
                .host
                .call_error(VmCallError::TooManyPositional(display_name)));
        }
        Ok(())
    }

    fn fanout_branch_start(&self, index: usize) {
        self.effect_host.fanout_branch_start(index);
    }

    fn branch_host(&self, index: usize) -> Self {
        Self {
            effect_host: self.effect_host.branch_host(index),
            host: self.host.clone(),
            program: Arc::clone(&self.program),
            module: self.module,
            depth: self.depth,
            node_id: Some(match self.node_id.as_deref() {
                Some(node_id) => format!("{node_id}.branch[{index}]"),
                None => format!("branch[{index}]"),
            }),
            owner: Arc::clone(&self.owner),
            parallel_active: Arc::clone(&self.parallel_active),
            await_mode: FlowDriveMode::Parallel,
            branch_index: Some(index),
            effect_context: self.effect_context.for_branch(index),
        }
    }

    fn fanout_branch_end(&self, index: usize, status: FanoutBranchStatus) {
        self.effect_host.fanout_branch_end(index, status);
    }

    fn fanout_error_status(&self, error: &Self::Error) -> FanoutBranchStatus {
        self.effect_host.fanout_error_status(error)
    }

    fn make_flow_future(
        &self,
        name: FlowRef,
        args: Vec<EvaluatedArg<Self::Payload, Self::Error>>,
    ) -> Value<Self::Payload, Self::Error> {
        self.create_flow_future(name, args)
    }

    fn validate_flow_future(
        &self,
        future: &FlowFuture<Self::Payload, Self::Error>,
    ) -> Result<(), Self::Error> {
        future
            .belongs_to(&self.owner)
            .then_some(())
            .ok_or_else(|| self.host.call_error(VmCallError::InvalidFutureOwner))
    }

    fn drive_flow_future<'a>(
        &'a self,
        future: &'a FlowFuture<Self::Payload, Self::Error>,
        mode: FlowDriveMode,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        Box::pin(async move {
            if let Err(error) = self.validate_flow_future(future) {
                return Value::Err(error);
            }
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
            let mut result = future.result.lock().await;
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
            if let Some(value) = result.as_ref() {
                return value.clone();
            }
            let _permit = match self.acquire_parallel_permit(mode) {
                Ok(permit) => permit,
                Err(error) => return Value::Err(error),
            };
            let value = self.call_flow_future(future, mode).await;
            *result = Some(value.clone());
            value
        })
    }

    fn drive_tool_future<'a>(
        &'a self,
        future: &'a ToolFuture<Self::Payload, Self::Error>,
        mode: FlowDriveMode,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        Box::pin(async move {
            if let Err(error) = self.validate_tool_future(future) {
                return Value::Err(error);
            }
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
            let mut result = future.result.lock().await;
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
            if let Some(value) = result.as_ref() {
                return value.clone();
            }
            let _permit = match self.acquire_parallel_permit(mode) {
                Ok(permit) => permit,
                Err(error) => return Value::Err(error),
            };
            let origin = future
                .audit_origin
                .clone()
                .unwrap_or_else(|| self.effect_context.clone());
            let value = self
                .host
                .eval_external_from(
                    ExpressionEffect::ToolCall {
                        name: future.name.clone(),
                        positional: future.positional.clone(),
                        named: future.named.clone(),
                        watch_rules: future.watch_rules.clone(),
                    },
                    origin,
                    self.effect_context.clone(),
                )
                .await;
            *result = Some(value.clone());
            value
        })
    }

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        if effect.contains_pending_call() {
            return Box::pin(async move {
                Value::Err(self.host.call_error(VmCallError::FutureBoundary))
            });
        }
        self.host.eval_external_from(
            effect,
            self.effect_context.clone(),
            self.effect_context.clone(),
        )
    }
}
