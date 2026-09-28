//! Composable delegates for embedding the VM.

use alloc::{
    boxed::Box,
    string::{String, ToString},
};

use crate::{
    ExpressionEffect, FlowCall, FlowOutcome, HostFuture, HostValueOps, ToolCallMode, Value,
    ValueError, VmNode,
    ast::MessageRole,
    engine::Preflight,
    pattern::PatternBindError,
    program::FlowId,
    vm::{FlowDriveMode, VmCallError},
};

/// VM-local identity of one root or child flow execution.
///
/// The identity is unique within a [`crate::Vm`] instance. Embedders may map it
/// to a durable product identity without exposing that product type to the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VmRunId(pub usize);

/// VM-owned execution context supplied to every delegate callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmContext {
    pub run_id: VmRunId,
    pub parent_run_id: Option<VmRunId>,
    pub source_id: String,
    pub flow: FlowId,
    pub caller_node_id: Option<String>,
    pub node_id: Option<String>,
    pub parent_node_id: Option<String>,
    pub drive_mode: FlowDriveMode,
    pub branch_index: Option<usize>,
}

/// Portable metadata for an evaluated external effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmEffect {
    FileRef { path: String },
    ToolCall { name: String },
    Confirm,
    Message { role: MessageRole },
    Call { name: String },
    FixSnapshot,
    FixRestore,
}

impl<P, E> From<&ExpressionEffect<P, E>> for VmEffect {
    fn from(effect: &ExpressionEffect<P, E>) -> Self {
        match effect {
            ExpressionEffect::FileRef(path) => Self::FileRef { path: path.clone() },
            ExpressionEffect::ToolCall { name, .. } => Self::ToolCall { name: name.clone() },
            ExpressionEffect::Confirm(_) => Self::Confirm,
            ExpressionEffect::Message { role, .. } => Self::Message { role: *role },
            ExpressionEffect::Call { name, .. } => Self::Call { name: name.clone() },
            ExpressionEffect::FixSnapshot { .. } => Self::FixSnapshot,
            ExpressionEffect::FixRestore { .. } => Self::FixRestore,
        }
    }
}

/// Portable terminal state for VM observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    Ok,
    Err,
    Cancelled,
}

/// An owned VM lifecycle observation that can be retained by the observer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmEvent {
    FlowStarted {
        context: VmContext,
    },
    FlowEnded {
        context: VmContext,
        status: VmStatus,
        error: Option<String>,
    },
    NodeStarted {
        context: VmContext,
        node: VmNode,
    },
    NodeEnded {
        context: VmContext,
        status: VmStatus,
        preview: Option<String>,
    },
    IterationStarted {
        context: VmContext,
        iteration: u64,
    },
    IterationEnded {
        context: VmContext,
        status: VmStatus,
        preview: Option<String>,
    },
    FanoutBranchStarted {
        context: VmContext,
    },
    FanoutBranchEnded {
        context: VmContext,
        status: VmStatus,
    },
    AuthorizationRequested {
        context: VmContext,
        effect: VmEffect,
    },
    AuthorizationResolved {
        context: VmContext,
        effect: VmEffect,
        status: VmStatus,
    },
    EffectStarted {
        context: VmContext,
        effect: VmEffect,
    },
    EffectEnded {
        context: VmContext,
        effect: VmEffect,
        status: VmStatus,
        preview: Option<String>,
    },
    CancellationObserved {
        context: VmContext,
    },
}

impl VmEvent {
    pub fn context(&self) -> &VmContext {
        match self {
            Self::FlowStarted { context }
            | Self::FlowEnded { context, .. }
            | Self::NodeStarted { context, .. }
            | Self::NodeEnded { context, .. }
            | Self::IterationStarted { context, .. }
            | Self::IterationEnded { context, .. }
            | Self::FanoutBranchStarted { context }
            | Self::FanoutBranchEnded { context, .. }
            | Self::AuthorizationRequested { context, .. }
            | Self::AuthorizationResolved { context, .. }
            | Self::EffectStarted { context, .. }
            | Self::EffectEnded { context, .. }
            | Self::CancellationObserved { context } => context,
        }
    }
}

/// Dispatches evaluated external effects and consumes authorization permits.
pub trait EffectDelegate: Clone + Send + Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;
    type Permit: Send;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        permit: Self::Permit,
        context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;

    /// Returns a value to reject or short-circuit a tool before its arguments run.
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

    fn preview(
        &self,
        _value: &Value<Self::Payload, Self::Error>,
        _context: &VmContext,
    ) -> Option<String> {
        None
    }

    fn error_status(&self, _error: &Self::Error) -> VmStatus {
        VmStatus::Err
    }
}

/// Authorizes one evaluated effect and returns a one-use invocation permit.
pub trait AuthorizationDelegate<P, E>: Clone + Send + Sync {
    type Permit: Send;

    fn authorize<'a>(
        &'a self,
        effect: &'a ExpressionEffect<P, E>,
        context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, E>>;
}

/// Receives owned VM lifecycle events.
pub trait ObserverDelegate: Clone + Send + Sync {
    fn on_event(&self, event: VmEvent);
}

/// Reports cancellation at VM checkpoints.
pub trait CancellationDelegate<E>: Clone + Send + Sync {
    fn cancellation_error(&self, context: &VmContext) -> Option<E>;

    /// Resolves when pending VM work must be interrupted.
    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, E> {
        Box::pin(async {
            loop {
                core::future::pending::<()>().await;
            }
        })
    }

    fn is_cancellation(&self, error: &E) -> bool;
}

/// Supplies host-specific language errors and statement preflight decisions.
pub trait ControlDelegate<E: ValueError>: Clone + Send + Sync {
    fn preflight_statement(&self, _node: &VmNode, _context: &VmContext) -> Preflight<E> {
        Preflight::Continue
    }

    fn call_error(&self, error: VmCallError) -> E {
        E::type_mismatch("valid Atman call", error.to_string())
    }

    fn undefined_var(&self, name: String) -> E {
        E::type_mismatch("bound variable", name)
    }

    fn undefined_field(&self, name: String) -> E {
        E::type_mismatch("existing field", name)
    }

    fn pattern_error(&self, error: PatternBindError) -> E {
        E::type_mismatch("matching pattern", alloc::format!("{error:?}"))
    }

    fn error_preview(&self, _error: &E) -> Option<String> {
        None
    }
}

/// Establishes and tears down host state for each root or child flow.
pub trait FlowDelegate<P, E>: Clone + Send + Sync {
    type Guard: Send;

    fn enter(&self, call: Option<&FlowCall<'_>>, context: &VmContext) -> Result<Self::Guard, E>;

    fn exit(
        &self,
        call: Option<&FlowCall<'_>>,
        context: &VmContext,
        outcome: &FlowOutcome<P, E>,
        guard: Self::Guard,
    );

    /// Tears down a flow whose driving future was dropped before an outcome existed.
    fn abort(&self, call: Option<&FlowCall<'_>>, context: &VmContext, guard: Self::Guard) {
        let _ = (call, context, guard);
    }
}

/// Authorization policy that accepts every effect.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl<P, E> AuthorizationDelegate<P, E> for AllowAll {
    type Permit = ();

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<P, E>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, E>> {
        Box::pin(async { Ok(()) })
    }
}

/// Observer that discards every event.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopObserver;

impl ObserverDelegate for NoopObserver {
    fn on_event(&self, _event: VmEvent) {}
}

/// Cancellation policy that never interrupts execution.
#[derive(Debug, Clone, Copy, Default)]
pub struct NeverCancel;

impl<E> CancellationDelegate<E> for NeverCancel {
    fn cancellation_error(&self, _context: &VmContext) -> Option<E> {
        None
    }

    fn is_cancellation(&self, _error: &E) -> bool {
        false
    }
}

/// Default language error mapping for portable hosts.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultControl;

impl<E: ValueError> ControlDelegate<E> for DefaultControl {}

/// Flow scope policy that keeps no host state.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopFlows;

impl<P, E> FlowDelegate<P, E> for NoopFlows {
    type Guard = ();

    fn enter(&self, _call: Option<&FlowCall<'_>>, _context: &VmContext) -> Result<Self::Guard, E> {
        Ok(())
    }

    fn exit(
        &self,
        _call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        _outcome: &FlowOutcome<P, E>,
        _guard: Self::Guard,
    ) {
    }
}

/// A statically composed set of VM delegate facets.
#[derive(Debug, Clone)]
pub struct VmDelegates<
    D,
    A = AllowAll,
    O = NoopObserver,
    C = NeverCancel,
    F = NoopFlows,
    L = DefaultControl,
> {
    pub(crate) effect: D,
    pub(crate) authorization: A,
    pub(crate) observer: O,
    pub(crate) cancellation: C,
    pub(crate) flows: F,
    pub(crate) control: L,
}

impl<D> VmDelegates<D> {
    pub fn new(effect: D) -> Self {
        Self {
            effect,
            authorization: AllowAll,
            observer: NoopObserver,
            cancellation: NeverCancel,
            flows: NoopFlows,
            control: DefaultControl,
        }
    }
}

impl<D, A, O, C, F, L> VmDelegates<D, A, O, C, F, L> {
    pub fn with_authorization<N>(self, authorization: N) -> VmDelegates<D, N, O, C, F, L> {
        VmDelegates {
            effect: self.effect,
            authorization,
            observer: self.observer,
            cancellation: self.cancellation,
            flows: self.flows,
            control: self.control,
        }
    }

    pub fn with_observer<N>(self, observer: N) -> VmDelegates<D, A, N, C, F, L> {
        VmDelegates {
            effect: self.effect,
            authorization: self.authorization,
            observer,
            cancellation: self.cancellation,
            flows: self.flows,
            control: self.control,
        }
    }

    pub fn with_cancellation<N>(self, cancellation: N) -> VmDelegates<D, A, O, N, F, L> {
        VmDelegates {
            effect: self.effect,
            authorization: self.authorization,
            observer: self.observer,
            cancellation,
            flows: self.flows,
            control: self.control,
        }
    }

    pub fn with_flows<N>(self, flows: N) -> VmDelegates<D, A, O, C, N, L> {
        VmDelegates {
            effect: self.effect,
            authorization: self.authorization,
            observer: self.observer,
            cancellation: self.cancellation,
            flows,
            control: self.control,
        }
    }

    pub fn with_control<N>(self, control: N) -> VmDelegates<D, A, O, C, F, N> {
        VmDelegates {
            effect: self.effect,
            authorization: self.authorization,
            observer: self.observer,
            cancellation: self.cancellation,
            flows: self.flows,
            control,
        }
    }

    pub fn effect(&self) -> &D {
        &self.effect
    }

    pub fn authorization(&self) -> &A {
        &self.authorization
    }

    pub fn observer(&self) -> &O {
        &self.observer
    }

    pub fn cancellation(&self) -> &C {
        &self.cancellation
    }

    pub fn flows(&self) -> &F {
        &self.flows
    }

    pub fn control(&self) -> &L {
        &self.control
    }
}

impl<D> From<D> for VmDelegates<D> {
    fn from(effect: D) -> Self {
        Self::new(effect)
    }
}
