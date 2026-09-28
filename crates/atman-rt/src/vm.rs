//! A linked Atman program and its flow execution entry points.

use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::fmt;

use crate::{
    Engine, ExpressionEffect, ExpressionHost, FlowArgs, FlowOutcome, HostFuture, HostValueOps,
    Preflight, StatementHost, StatementOutcome, Value, ValueError,
    ast::{Arg, Contract, FlowRef, LifecycleEvent, Stmt},
    engine::bind_evaluated_call_arguments,
    expr::EvaluatedArg,
    pattern::PatternBindError,
    program::{FlowId, LinkedProgram, ModuleId},
    route::RouteMatch,
};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmCallError {
    MissingEntry(String),
    MissingFlow(String),
    TooManyPositional(String),
    CallDepthExceeded(String),
    ControlFlowEscaped(String),
}

impl fmt::Display for VmCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEntry(name) => write!(f, "entry flow `{name}` does not exist"),
            Self::MissingFlow(name) => write!(f, "subflow `{name}` does not exist"),
            Self::TooManyPositional(name) => {
                write!(f, "subflow `{name}` received too many positional arguments")
            }
            Self::CallDepthExceeded(name) => {
                write!(f, "subflow `{name}` exceeded the maximum call depth")
            }
            Self::ControlFlowEscaped(name) => {
                write!(f, "break or continue escaped from flow `{name}`")
            }
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

/// The minimal interface for embedding an Atman VM in another application.
///
/// Effects contain evaluated values. Subflow calls are handled by the VM and
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
        let Some(flow) = self.program.flow(&id) else {
            return StatementOutcome::Err(host.call_error(VmCallError::MissingFlow(id.name)));
        };
        let mut engine = Engine::new(VmStatementHost {
            host,
            program: Arc::clone(&self.program),
            module: id.module,
            depth: 0,
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
        }
    }
}

impl<H: VmHost> VmExpressionHost<H> {
    fn call_subflow<'a>(
        &'a self,
        name: FlowRef,
        args: Vec<EvaluatedArg<H::Payload, H::Error>>,
    ) -> HostFuture<'a, Value<H::Payload, H::Error>> {
        Box::pin(async move {
            if let Some(error) = self.effect_host.cancellation_error() {
                return Value::Err(error);
            }
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
            let Some(source) = self.program.module(id.module) else {
                return Value::Err(self.host.call_error(VmCallError::MissingFlow(display_name)));
            };
            let call = FlowCall {
                target: &id,
                display_name: &display_name,
                source_id: &source.source_id,
                contract: flow.contract.as_ref(),
                parent_node_id: &self.node_id,
            };
            let (child_host, guard) = match self.host.enter_child(&call) {
                Ok(child) => child,
                Err(error) => return Value::Err(error),
            };
            let mut engine = Engine::new(VmStatementHost {
                host: child_host,
                program: Arc::clone(&self.program),
                module: id.module,
                depth: self.depth + 1,
            });
            let outcome = engine.run_flow(flow, bindings).await;
            let outcome = match self.effect_host.cancellation_error() {
                Some(error) => StatementOutcome::Err(error),
                None => outcome,
            };
            self.host.exit_child(&call, &outcome, guard);
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

    fn preflight_tool(&self, name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        self.effect_host.preflight_tool(name)
    }

    fn preflight_subflow(&self, name: &FlowRef, args: &[Arg]) -> Result<(), Self::Error> {
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
        }
    }

    fn fanout_branch_end(&self, index: usize, value: &Value<Self::Payload, Self::Error>) {
        self.effect_host.fanout_branch_end(index, value);
    }

    fn eval_subflow<'a>(
        &'a self,
        name: FlowRef,
        args: Vec<EvaluatedArg<Self::Payload, Self::Error>>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.call_subflow(name, args)
    }

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.effect_host.eval_external(effect)
    }
}
