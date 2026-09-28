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
    Engine, ExpressionEffect, ExpressionHost, FlowArgs, FlowOutcome, HostFuture, HostValueOps,
    NamedValues, Preflight, StatementHost, StatementOutcome, ToolCallMode, Value, ValueError,
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
    pub parent_node_id: &'a str,
    pub mode: FlowDriveMode,
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
pub trait VmHost: StatementHost + Clone {
    type ChildGuard: Send;

    fn call_error(&self, error: VmCallError) -> Self::Error;

    fn enter_child(&self, call: &FlowCall<'_>) -> Result<(Self, Self::ChildGuard), Self::Error>;

    fn exit_child(
        &self,
        call: &FlowCall<'_>,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::ChildGuard,
    );
}

struct ChildExit<'a, H: VmHost> {
    host: &'a H,
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
            self.host.exit_child(&self.call, outcome, guard);
        }
    }
}

impl<H: VmHost> Drop for ChildExit<'_, H> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            let outcome = StatementOutcome::Err(self.host.call_error(VmCallError::Cancelled));
            self.host.exit_child(&self.call, &outcome, guard);
        }
    }
}

/// The minimal interface for embedding an Atman VM in another application.
///
/// Effects contain evaluated values. Flow calls are handled by the VM and
/// never reach `effect`. All other methods have defaults so a host can begin
/// with one effect dispatcher and add context or observation as needed.
pub trait VmEmbedding: Clone + Send + Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;

    fn effect<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;

    fn cancellation_error(&self) -> Option<Self::Error> {
        None
    }

    /// Reject a tool before its arguments run. Recheck authorization in `effect`.
    fn preflight_tool(&self, _name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        None
    }

    fn tool_call_mode(&self, _name: &str) -> ToolCallMode {
        ToolCallMode::Immediate
    }

    fn call_error(&self, error: VmCallError) -> Self::Error {
        Self::Error::type_mismatch("valid Atman call", error.to_string())
    }

    fn node_start(&self, _node_id: &str, _parent_node_id: Option<&str>) {}

    fn node_end(
        &self,
        _node_id: &str,
        _outcome: &FlowOutcome<Self::Payload, Self::Error>,
        _parent_node_id: Option<&str>,
        _preview: Option<&str>,
    ) {
    }

    fn preview(&self, _value: &Value<Self::Payload, Self::Error>) -> Option<String> {
        None
    }

    fn child(&self, _call: &FlowCall<'_>) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }

    fn child_end(&self, _call: &FlowCall<'_>, _outcome: &FlowOutcome<Self::Payload, Self::Error>) {}
}

struct EmbeddingAdapter<H: VmEmbedding> {
    host: H,
}

impl<H: VmEmbedding> Clone for EmbeddingAdapter<H> {
    fn clone(&self) -> Self {
        Self {
            host: self.host.clone(),
        }
    }
}

impl<H: VmEmbedding> ExpressionHost for EmbeddingAdapter<H> {
    type Payload = H::Payload;
    type Error = H::Error;

    fn undefined_var(&self, name: String) -> Self::Error {
        Self::Error::type_mismatch("bound variable", name)
    }

    fn undefined_field(&self, name: String) -> Self::Error {
        Self::Error::type_mismatch("existing field", name)
    }

    fn cancellation_error(&self) -> Option<Self::Error> {
        self.host.cancellation_error()
    }

    fn preflight_tool(&self, name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        self.host.preflight_tool(name)
    }

    fn tool_call_mode(&self, name: &str) -> ToolCallMode {
        self.host.tool_call_mode(name)
    }

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.host.effect(effect)
    }
}

impl<H: VmEmbedding> StatementHost for EmbeddingAdapter<H> {
    type Payload = H::Payload;
    type Error = H::Error;
    type ExprHost = Self;

    fn preflight(&mut self, _stmt: &Stmt, _node_id: &str) -> Preflight<Self::Error> {
        match self.host.cancellation_error() {
            Some(error) => Preflight::Stop(error),
            None => Preflight::Continue,
        }
    }

    fn node_start(&mut self, _stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>) {
        self.host.node_start(node_id, parent_node_id);
    }

    fn expression_host(&self, _node_id: &str) -> Self::ExprHost {
        self.clone()
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        Self::Error::type_mismatch("matching pattern", format!("{error:?}"))
    }

    fn preview(&self, value: &Value<Self::Payload, Self::Error>) -> Option<String> {
        self.host.preview(value)
    }

    fn node_end(
        &mut self,
        node_id: &str,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        parent_node_id: Option<&str>,
        preview: Option<&str>,
    ) {
        self.host
            .node_end(node_id, outcome, parent_node_id, preview);
    }
}

impl<H: VmEmbedding> VmHost for EmbeddingAdapter<H> {
    type ChildGuard = ();

    fn call_error(&self, error: VmCallError) -> Self::Error {
        self.host.call_error(error)
    }

    fn enter_child(&self, call: &FlowCall<'_>) -> Result<(Self, Self::ChildGuard), Self::Error> {
        self.host.child(call).map(|host| (Self { host }, ()))
    }

    fn exit_child(
        &self,
        call: &FlowCall<'_>,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        _guard: Self::ChildGuard,
    ) {
        self.host.child_end(call, outcome);
    }
}

/// A complete linked program. With `syntax`, it also loads and parses source text.
#[derive(Clone)]
pub struct Vm {
    program: Arc<LinkedProgram>,
}

impl Vm {
    const MAX_CALL_DEPTH: usize = 128;

    pub fn new(program: LinkedProgram) -> Self {
        Self::from_shared(Arc::new(program))
    }

    pub fn from_shared(program: Arc<LinkedProgram>) -> Self {
        Self { program }
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

    /// Runs one entry flow with the minimal embedding interface.
    pub async fn run<H: VmEmbedding>(
        &self,
        name: &str,
        args: FlowArgs<H::Payload, H::Error>,
        host: H,
    ) -> FlowOutcome<H::Payload, H::Error> {
        self.run_entry(name, args, EmbeddingAdapter { host }).await
    }

    pub async fn run_entry<H: VmHost>(
        &self,
        name: &str,
        args: FlowArgs<H::Payload, H::Error>,
        host: H,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let Some(id) = self.program.entry_flow(name) else {
            return StatementOutcome::Err(host.call_error(VmCallError::MissingEntry(name.into())));
        };
        self.run_flow(id, args, host).await
    }

    pub async fn run_flow<H: VmHost>(
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
    pub async fn run_lifecycle<H: VmHost>(
        &self,
        event: LifecycleEvent,
        host: H,
    ) -> Vec<FlowOutcome<H::Payload, H::Error>> {
        let mut outcomes = Vec::new();
        for flow in self.program.lifecycle_flows(event) {
            let mut engine = Engine::new(VmStatementHost {
                host: host.clone(),
                program: Arc::clone(&self.program),
                module: self.program.entry_module(),
                depth: 0,
                owner: Arc::new(()),
                parallel_active: Arc::new(AtomicUsize::new(0)),
            });
            let outcome = engine.run_flow(&flow, Vec::new()).await;
            outcomes.push(outcome);
        }
        outcomes
    }

    pub async fn run_lifecycle_with<H: VmEmbedding>(
        &self,
        event: LifecycleEvent,
        host: H,
    ) -> Vec<FlowOutcome<H::Payload, H::Error>> {
        self.run_lifecycle(event, EmbeddingAdapter { host }).await
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

    fn preflight(&mut self, stmt: &Stmt, node_id: &str) -> Preflight<Self::Error> {
        self.host.preflight(stmt, node_id)
    }

    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>) {
        self.host.node_start(stmt, node_id, parent_node_id);
    }

    fn expression_host(&self, node_id: &str) -> Self::ExprHost {
        VmExpressionHost {
            effect_host: self.host.expression_host(node_id),
            host: self.host.clone(),
            program: Arc::clone(&self.program),
            module: self.module,
            depth: self.depth,
            node_id: node_id.into(),
            owner: Arc::clone(&self.owner),
            parallel_active: Arc::clone(&self.parallel_active),
            await_mode: FlowDriveMode::Inline,
        }
    }

    fn pattern_error(&self, error: PatternBindError) -> Self::Error {
        self.host.pattern_error(error)
    }

    fn iteration_start(&mut self, iteration: u64, node_id: &str, parent_node_id: Option<&str>) {
        self.host
            .iteration_start(iteration, node_id, parent_node_id);
    }

    fn iteration_end(
        &mut self,
        node_id: &str,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        parent_node_id: Option<&str>,
    ) {
        self.host.iteration_end(node_id, outcome, parent_node_id);
    }

    fn preview(&self, value: &Value<Self::Payload, Self::Error>) -> Option<String> {
        self.host.preview(value)
    }

    fn node_end(
        &mut self,
        node_id: &str,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        parent_node_id: Option<&str>,
        preview: Option<&str>,
    ) {
        self.host
            .node_end(node_id, outcome, parent_node_id, preview);
    }
}

struct VmExpressionHost<H: VmHost> {
    effect_host: H::ExprHost,
    host: H,
    program: Arc<LinkedProgram>,
    module: ModuleId,
    depth: usize,
    node_id: String,
    owner: Arc<()>,
    parallel_active: Arc<AtomicUsize>,
    await_mode: FlowDriveMode,
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
                parent_node_id: &self.node_id,
                mode,
            };
            let (child_host, guard) = match self.host.enter_child(&call) {
                Ok(child) => child,
                Err(error) => return Value::Err(error),
            };
            let exit = ChildExit {
                host: &self.host,
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
            let outcome = engine.run_flow(flow, future.args.clone()).await;
            let outcome = match self.effect_host.cancellation_error() {
                Some(error) => StatementOutcome::Err(error),
                None => outcome,
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
            node_id: format!("{}.branch[{index}]", self.node_id),
            owner: Arc::clone(&self.owner),
            parallel_active: Arc::clone(&self.parallel_active),
            await_mode: FlowDriveMode::Parallel,
        }
    }

    fn fanout_branch_end(&self, index: usize, value: &Value<Self::Payload, Self::Error>) {
        self.effect_host.fanout_branch_end(index, value);
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
            if let Some(value) = result.as_ref() {
                return value.clone();
            }
            let _permit = match self.acquire_parallel_permit(mode) {
                Ok(permit) => permit,
                Err(error) => return Value::Err(error),
            };
            let value = self
                .eval_external(ExpressionEffect::ToolCall {
                    name: future.name.clone(),
                    positional: future.positional.clone(),
                    named: future.named.clone(),
                    watch_rules: future.watch_rules.clone(),
                })
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
        self.effect_host.eval_external(effect)
    }
}
