use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use atman_rt::{
    AuthorizationDelegate, CancellationDelegate, ControlDelegate, EffectDelegate, EvalError,
    ExpressionEffect, FlowCall, FlowDelegate, FlowOutcome, HostFuture, ObserverDelegate, Preflight,
    Source, SourceResolver, StatementOutcome, ToolRouter, Value, Vm, VmContext, VmDelegate,
    VmDelegates, VmEffect, VmEvent, VmNode, VmStatus,
};

struct NoSources;

impl SourceResolver for NoSources {
    type Error = &'static str;

    fn resolve(&self, _importer_id: &str, _specifier: &str) -> Result<Source, Self::Error> {
        Err("source unavailable")
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..32 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("delegate test did not complete");
}

#[derive(Clone)]
struct Approval {
    trace: Arc<Mutex<Vec<String>>>,
    denied: Option<&'static str>,
}

impl AuthorizationDelegate<(), EvalError> for Approval {
    type Permit = ();

    fn authorize<'a>(
        &'a self,
        effect: &'a ExpressionEffect<(), EvalError>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, EvalError>> {
        Box::pin(async move {
            let ExpressionEffect::ToolCall { name, .. } = effect else {
                return Ok(());
            };
            self.trace.lock().unwrap().push(format!("authorize:{name}"));
            if self.denied == Some(name.as_str()) {
                Err(EvalError::MissingArgument(format!("denied:{name}")))
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Clone)]
struct Observer {
    trace: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<VmEvent>>>,
}

impl ObserverDelegate for Observer {
    fn on_event(&self, event: VmEvent) {
        let label = match &event {
            VmEvent::AuthorizationRequested {
                effect: VmEffect::ToolCall { name },
                ..
            } => Some(format!("authorization.request:{name}")),
            VmEvent::AuthorizationResolved {
                effect: VmEffect::ToolCall { name },
                status,
                ..
            } => Some(format!("authorization.{status:?}:{name}")),
            VmEvent::EffectStarted {
                effect: VmEffect::ToolCall { name },
                ..
            } => Some(format!("effect.start:{name}")),
            VmEvent::EffectEnded {
                effect: VmEffect::ToolCall { name },
                status,
                ..
            } => Some(format!("effect.{status:?}:{name}")),
            _ => None,
        };
        if let Some(label) = label {
            self.trace.lock().unwrap().push(label);
        }
        self.events.lock().unwrap().push(event);
    }
}

#[derive(Clone)]
struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct CancellationSignal {
    cancelled: AtomicBool,
    waiters: Mutex<Vec<Waker>>,
}

impl CancellationSignal {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        for waiter in self.waiters.lock().unwrap().drain(..) {
            waiter.wake();
        }
    }
}

#[derive(Clone)]
struct WakeableCancellation {
    signal: Arc<CancellationSignal>,
}

impl CancellationDelegate<EvalError> for WakeableCancellation {
    fn cancellation_error(&self, _context: &VmContext) -> Option<EvalError> {
        self.signal
            .cancelled
            .load(Ordering::SeqCst)
            .then(|| EvalError::MissingArgument("cancelled".into()))
    }

    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, EvalError> {
        Box::pin(std::future::poll_fn(move |context| {
            if self.signal.cancelled.load(Ordering::SeqCst) {
                Poll::Ready(EvalError::MissingArgument("cancelled".into()))
            } else {
                let mut waiters = self.signal.waiters.lock().unwrap();
                if !waiters
                    .iter()
                    .any(|waiter| waiter.will_wake(context.waker()))
                {
                    waiters.push(context.waker().clone());
                }
                Poll::Pending
            }
        }))
    }

    fn is_cancellation(&self, error: &EvalError) -> bool {
        matches!(error, EvalError::MissingArgument(message) if message == "cancelled")
    }
}

#[derive(Default)]
struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Clone)]
struct RecordingFlows {
    terminals: Arc<Mutex<Vec<String>>>,
}

impl FlowDelegate<(), EvalError> for RecordingFlows {
    type Guard = String;

    fn enter(
        &self,
        _call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<Self::Guard, EvalError> {
        Ok(context.flow.name.clone())
    }

    fn exit(
        &self,
        _call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        outcome: &FlowOutcome<(), EvalError>,
        guard: Self::Guard,
    ) {
        let status = match outcome {
            StatementOutcome::Err(EvalError::MissingArgument(message))
                if message == "cancelled" =>
            {
                "cancelled"
            }
            StatementOutcome::Err(_) => "error",
            _ => "ok",
        };
        self.terminals
            .lock()
            .unwrap()
            .push(format!("exit:{guard}:{status}"));
    }

    fn abort(&self, _call: Option<&FlowCall<'_>>, _context: &VmContext, guard: Self::Guard) {
        self.terminals
            .lock()
            .unwrap()
            .push(format!("abort:{guard}"));
    }
}

impl CancellationDelegate<EvalError> for Cancellation {
    fn cancellation_error(&self, _context: &VmContext) -> Option<EvalError> {
        self.cancelled
            .load(Ordering::SeqCst)
            .then(|| EvalError::MissingArgument("cancelled".into()))
    }

    fn is_cancellation(&self, error: &EvalError) -> bool {
        matches!(error, EvalError::MissingArgument(message) if message == "cancelled")
    }
}

fn position(trace: &[String], needle: &str) -> usize {
    trace
        .iter()
        .position(|entry| entry == needle)
        .unwrap_or_else(|| panic!("missing trace entry {needle}: {trace:?}"))
}

#[derive(Clone)]
struct PermitEffect {
    consumed: Arc<Mutex<Vec<usize>>>,
}

impl EffectDelegate for PermitEffect {
    type Payload = ();
    type Error = EvalError;
    type Permit = usize;

    fn invoke<'a>(
        &'a self,
        _effect: ExpressionEffect<Self::Payload, Self::Error>,
        permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        self.consumed.lock().unwrap().push(permit);
        Box::pin(async move { Value::Int(permit as i64) })
    }
}

#[derive(Clone, Copy)]
struct PermitApproval;

impl AuthorizationDelegate<(), EvalError> for PermitApproval {
    type Permit = usize;

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<(), EvalError>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, EvalError>> {
        Box::pin(async { Ok(41) })
    }
}

#[derive(Clone, Copy)]
struct PendingApproval;

impl AuthorizationDelegate<(), EvalError> for PendingApproval {
    type Permit = ();

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<(), EvalError>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, EvalError>> {
        Box::pin(core::future::pending())
    }
}

#[derive(Clone, Copy)]
struct StopControl;

impl ControlDelegate<EvalError> for StopControl {
    fn preflight_statement(&self, _node: &VmNode, _context: &VmContext) -> Preflight<EvalError> {
        Preflight::Stop(EvalError::MissingArgument("stopped by control".into()))
    }
}

#[derive(Clone, Copy)]
struct ErrorPreviewControl;

impl ControlDelegate<EvalError> for ErrorPreviewControl {
    fn error_preview(&self, _error: &EvalError) -> Option<String> {
        Some("control error".into())
    }
}

#[derive(Clone)]
struct GatedApproval {
    ready: Arc<AtomicBool>,
}

impl AuthorizationDelegate<(), EvalError> for GatedApproval {
    type Permit = ();

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<(), EvalError>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, EvalError>> {
        Box::pin(std::future::poll_fn(move |_| {
            self.ready
                .load(Ordering::SeqCst)
                .then_some(Ok(()))
                .map_or(Poll::Pending, Poll::Ready)
        }))
    }
}

#[derive(Clone)]
struct ScopedDelegate {
    scope: String,
    terminals: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone)]
struct ChildCancellationDelegate {
    scope: String,
    cancelled: Arc<AtomicBool>,
    terminals: Arc<Mutex<Vec<String>>>,
}

impl VmDelegate for ChildCancellationDelegate {
    type Payload = ();
    type Error = EvalError;
    type Permit = ();
    type FlowGuard = String;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        _permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        Box::pin(async move {
            if matches!(effect, ExpressionEffect::ToolCall { ref name, .. } if name == "cancel_child")
            {
                self.cancelled.store(true, Ordering::SeqCst);
                Value::Int(1)
            } else {
                Value::Unit
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<Self::Payload, Self::Error>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, Self::Error>> {
        Box::pin(async { Ok(()) })
    }

    fn enter_flow(
        &self,
        _call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), Self::Error> {
        let scope = context.flow.name.clone();
        Ok((
            Self {
                scope: scope.clone(),
                cancelled: Arc::clone(&self.cancelled),
                terminals: Arc::clone(&self.terminals),
            },
            scope,
        ))
    }

    fn exit_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::FlowGuard,
    ) {
        if call.is_some() {
            self.terminals.lock().unwrap().push(format!(
                "exit:{guard}:{}",
                matches!(outcome, StatementOutcome::Err(_))
            ));
        }
    }

    fn abort_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        guard: Self::FlowGuard,
    ) {
        if call.is_some() {
            self.terminals
                .lock()
                .unwrap()
                .push(format!("abort:{guard}"));
        }
    }

    fn cancellation_error(&self, _context: &VmContext) -> Option<Self::Error> {
        (self.scope == "child" && self.cancelled.load(Ordering::SeqCst))
            .then(|| EvalError::MissingArgument("child cancelled".into()))
    }

    fn error_status(&self, error: &Self::Error) -> VmStatus {
        if matches!(error, EvalError::MissingArgument(message) if message == "child cancelled") {
            VmStatus::Cancelled
        } else {
            VmStatus::Err
        }
    }
}

impl VmDelegate for ScopedDelegate {
    type Payload = ();
    type Error = EvalError;
    type Permit = ();
    type FlowGuard = String;

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        _permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        if matches!(effect, ExpressionEffect::ToolCall { ref name, .. } if name == "wait") {
            Box::pin(core::future::pending())
        } else {
            Box::pin(async { Value::Unit })
        }
    }

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<Self::Payload, Self::Error>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, Self::Error>> {
        Box::pin(async { Ok(()) })
    }

    fn enter_flow(
        &self,
        _call: Option<&FlowCall<'_>>,
        context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), Self::Error> {
        let scope = context.flow.name.clone();
        Ok((
            Self {
                scope: scope.clone(),
                terminals: Arc::clone(&self.terminals),
            },
            scope,
        ))
    }

    fn exit_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        _outcome: &FlowOutcome<Self::Payload, Self::Error>,
        guard: Self::FlowGuard,
    ) {
        if call.is_some() {
            self.terminals
                .lock()
                .unwrap()
                .push(format!("exit:{}:{guard}", self.scope));
        }
    }

    fn abort_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        guard: Self::FlowGuard,
    ) {
        if call.is_some() {
            self.terminals
                .lock()
                .unwrap()
                .push(format!("abort:{}:{guard}", self.scope));
        }
    }
}

#[derive(Clone)]
struct PreviewEffect {
    contexts: Arc<Mutex<Vec<VmContext>>>,
}

impl EffectDelegate for PreviewEffect {
    type Payload = ();
    type Error = EvalError;
    type Permit = ();

    fn invoke<'a>(
        &'a self,
        _effect: ExpressionEffect<Self::Payload, Self::Error>,
        _permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        Box::pin(async { Value::Unit })
    }

    fn preview(
        &self,
        _value: &Value<Self::Payload, Self::Error>,
        context: &VmContext,
    ) -> Option<String> {
        self.contexts.lock().unwrap().push(context.clone());
        Some("value".into())
    }
}

#[test]
fn composed_delegates_receive_context_and_drive_effect_lifecycle() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child() -> int { return child_tool(value: 4).await }

flow main() -> int {
    unused_pending = unused(value: 1)
    child_pending = child()
    tool_pending = parallel_tool(value: 2)
    values = fanout [child_pending, tool_pending]
    return values[0] + values[1]
}
"#,
        ),
        &NoSources,
    )
    .expect("compile source");

    let trace = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<(), EvalError>::new();
    for name in ["unused", "child_tool", "parallel_tool"] {
        let trace = Arc::clone(&trace);
        let calls = Arc::clone(&calls);
        tools
            .register(name, move |args| {
                let trace = Arc::clone(&trace);
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    trace.lock().unwrap().push(format!("invoke:{name}"));
                    Ok(Value::Int(args.int("value", 0)?))
                }
            })
            .expect("register tool");
    }
    let delegates = VmDelegates::new(tools)
        .with_authorization(Approval {
            trace: Arc::clone(&trace),
            denied: None,
        })
        .with_observer(Observer {
            trace: Arc::clone(&trace),
            events: Arc::clone(&events),
        });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Return(Value::Int(6))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let trace = trace.lock().unwrap();
    assert!(!trace.iter().any(|entry| entry.contains("unused")));
    for name in ["child_tool", "parallel_tool"] {
        let requested = position(&trace, &format!("authorization.request:{name}"));
        let authorized = position(&trace, &format!("authorize:{name}"));
        let resolved = position(&trace, &format!("authorization.Ok:{name}"));
        let started = position(&trace, &format!("effect.start:{name}"));
        let invoked = position(&trace, &format!("invoke:{name}"));
        let ended = position(&trace, &format!("effect.Ok:{name}"));
        assert!(requested < authorized);
        assert!(authorized < resolved);
        assert!(resolved < started);
        assert!(started < invoked);
        assert!(invoked < ended);
    }
    drop(trace);

    let events = events.lock().unwrap();
    assert!(
        matches!(events.first(), Some(VmEvent::FlowStarted { context }) if context.flow.name == "main")
    );
    assert!(
        matches!(events.last(), Some(VmEvent::FlowEnded { context, status: VmStatus::Ok, .. }) if context.flow.name == "main")
    );
    let main_context = events
        .iter()
        .find_map(|event| match event {
            VmEvent::FlowStarted { context } if context.flow.name == "main" => Some(context),
            _ => None,
        })
        .unwrap();
    let child_context = events
        .iter()
        .find_map(|event| match event {
            VmEvent::FlowStarted { context } if context.flow.name == "child" => Some(context),
            _ => None,
        })
        .unwrap();
    assert_ne!(main_context.run_id, child_context.run_id);
    assert_eq!(child_context.parent_run_id, Some(main_context.run_id));
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::FlowStarted { context }
            if context.flow.name == "child"
                && context.drive_mode == atman_rt::FlowDriveMode::Parallel
                && context.branch_index == Some(0)
                && context.caller_node_id.as_deref() == Some("3.branch[0]")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::EffectStarted {
            context,
            effect: VmEffect::ToolCall { name },
        } if name == "parallel_tool"
            && context.drive_mode == atman_rt::FlowDriveMode::Parallel
            && context.branch_index == Some(1)
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, VmEvent::FanoutBranchStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, VmEvent::FanoutBranchEnded { .. }))
            .count(),
        2
    );
}

#[test]
fn rejected_authorization_does_not_invoke_the_effect() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { return denied(value: 1).await }",
        ),
        &NoSources,
    )
    .expect("compile source");
    let trace = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<(), EvalError>::new();
    let called = Arc::clone(&calls);
    tools
        .register("denied", move |_| {
            let called = Arc::clone(&called);
            async move {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Int(1))
            }
        })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_authorization(Approval {
            trace: Arc::clone(&trace),
            denied: Some("denied"),
        })
        .with_observer(Observer {
            trace,
            events: Arc::clone(&events),
        });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Err(EvalError::MissingArgument(message)) if message == "denied:denied"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::AuthorizationResolved {
            effect: VmEffect::ToolCall { name },
            status: VmStatus::Err,
            ..
        } if name == "denied"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        VmEvent::EffectStarted {
            effect: VmEffect::ToolCall { name },
            ..
        } if name == "denied"
    )));
}

#[test]
fn cancellation_is_reported_through_the_delegate_boundary() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return 1 }"),
        &NoSources,
    )
    .expect("compile source");
    let events = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new())
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_cancellation(Cancellation {
            cancelled: Arc::new(AtomicBool::new(true)),
        });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Err(EvalError::MissingArgument(message)) if message == "cancelled"
    ));
    let events = events.lock().unwrap();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, VmEvent::CancellationObserved { .. }))
    );
    assert!(matches!(
        events.last(),
        Some(VmEvent::FlowEnded {
            status: VmStatus::Cancelled,
            ..
        })
    ));
}

#[test]
fn composed_control_delegate_drives_statement_preflight() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return 1 }"),
        &NoSources,
    )
    .unwrap();
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new()).with_control(StopControl);

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Err(EvalError::MissingArgument(message))
            if message == "stopped by control"
    ));
}

#[test]
fn control_delegate_formats_terminal_errors() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() { missing() }"),
        &NoSources,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new())
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_control(ErrorPreviewControl);

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Err(_)
    ));
    assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        VmEvent::FlowEnded {
            error: Some(error),
            ..
        } if error == "control error"
    )));
}

#[test]
fn authorization_permit_is_consumed_by_the_matching_effect() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return permitted() }"),
        &NoSources,
    )
    .unwrap();
    let consumed = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(PermitEffect {
        consumed: Arc::clone(&consumed),
    })
    .with_authorization(PermitApproval);

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Return(Value::Int(41))
    ));
    assert_eq!(*consumed.lock().unwrap(), [41]);
}

#[test]
fn dropping_pending_authorization_closes_every_open_scope_as_cancelled() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return wait().await }"),
        &NoSources,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("wait", |_| async { Ok(Value::Int(1)) })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_authorization(PendingApproval)
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(future);

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::AuthorizationResolved {
            status: VmStatus::Cancelled,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, VmEvent::EffectStarted { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::NodeEnded {
            status: VmStatus::Cancelled,
            ..
        }
    )));
    assert!(matches!(
        events.last(),
        Some(VmEvent::FlowEnded {
            status: VmStatus::Cancelled,
            ..
        })
    ));
}

#[test]
fn dropping_pending_effect_and_nested_effect_context_are_reported_by_the_vm() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { when true { return wait().await } return 0 }",
        ),
        &NoSources,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("wait", |_| async {
            core::future::pending::<Result<Value<(), EvalError>, EvalError>>().await
        })
        .unwrap();
    let delegates = VmDelegates::new(tools).with_observer(Observer {
        trace: Arc::new(Mutex::new(Vec::new())),
        events: Arc::clone(&events),
    });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));

    {
        let events = events.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            VmEvent::EffectStarted { context, .. }
                if context.node_id.as_deref() == Some("0.0")
                    && context.parent_node_id.as_deref() == Some("0")
        )));
    }
    drop(future);

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::EffectEnded {
            status: VmStatus::Cancelled,
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                VmEvent::NodeEnded {
                    status: VmStatus::Cancelled,
                    ..
                }
            ))
            .count(),
        2
    );
}

#[test]
fn cancelled_empty_root_flow_ends_cancelled() {
    let vm = Vm::compile(Source::new("main.at", "flow main() {}"), &NoSources).unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new())
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_cancellation(Cancellation {
            cancelled: Arc::new(AtomicBool::new(true)),
        });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Err(EvalError::MissingArgument(message)) if message == "cancelled"
    ));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::CancellationObserved { context } if context.node_id.is_none()
    )));
    assert!(matches!(
        events.last(),
        Some(VmEvent::FlowEnded {
            context,
            status: VmStatus::Cancelled,
            ..
        }) if context.flow.name == "main"
    ));
}

#[test]
fn cancellation_after_authorization_wait_skips_invocation() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return guarded().await }"),
        &NoSources,
    )
    .unwrap();
    let authorization_ready = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    let calls_for_tool = Arc::clone(&calls);
    tools
        .register("guarded", move |_| {
            let calls = Arc::clone(&calls_for_tool);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Int(1))
            }
        })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_authorization(GatedApproval {
            ready: Arc::clone(&authorization_ready),
        })
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_cancellation(Cancellation {
            cancelled: Arc::clone(&cancelled),
        });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));

    cancelled.store(true, Ordering::SeqCst);
    authorization_ready.store(true, Ordering::SeqCst);
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(StatementOutcome::Err(EvalError::MissingArgument(message)))
            if message == "cancelled"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::AuthorizationResolved {
            status: VmStatus::Cancelled,
            ..
        }
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, VmEvent::EffectStarted { .. }))
    );
}

#[test]
fn cancellation_future_wakes_pending_authorization_and_exits_the_flow() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return guarded().await }"),
        &NoSources,
    )
    .unwrap();
    let signal = Arc::new(CancellationSignal::default());
    let terminals = Arc::new(Mutex::new(Vec::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("guarded", |_| async { Ok(Value::Int(1)) })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_authorization(PendingApproval)
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_cancellation(WakeableCancellation {
            signal: Arc::clone(&signal),
        })
        .with_flows(RecordingFlows {
            terminals: Arc::clone(&terminals),
        });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    signal.cancel();
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(StatementOutcome::Err(EvalError::MissingArgument(message)))
            if message == "cancelled"
    ));
    assert_eq!(*terminals.lock().unwrap(), ["exit:main:cancelled"]);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::AuthorizationResolved {
            status: VmStatus::Cancelled,
            ..
        }
    )));
}

#[test]
fn cancellation_future_wakes_pending_effect() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return wait().await }"),
        &NoSources,
    )
    .unwrap();
    let signal = Arc::new(CancellationSignal::default());
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("wait", |_| async {
            core::future::pending::<Result<Value<(), EvalError>, EvalError>>().await
        })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_observer(Observer {
            trace: Arc::new(Mutex::new(Vec::new())),
            events: Arc::clone(&events),
        })
        .with_cancellation(WakeableCancellation {
            signal: Arc::clone(&signal),
        });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    signal.cancel();
    assert!(wakes.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(StatementOutcome::Err(EvalError::MissingArgument(message)))
            if message == "cancelled"
    ));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::EffectEnded {
            status: VmStatus::Cancelled,
            ..
        }
    )));
}

#[test]
fn shared_cancellation_exits_an_active_child_instead_of_aborting_it() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child() -> int { return wait().await }
flow main() -> int { return child().await }
"#,
        ),
        &NoSources,
    )
    .unwrap();
    let signal = Arc::new(CancellationSignal::default());
    let terminals = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("wait", |_| async {
            core::future::pending::<Result<Value<(), EvalError>, EvalError>>().await
        })
        .unwrap();
    let delegates = VmDelegates::new(tools)
        .with_cancellation(WakeableCancellation {
            signal: Arc::clone(&signal),
        })
        .with_flows(RecordingFlows {
            terminals: Arc::clone(&terminals),
        });
    let mut future = Box::pin(vm.run("main", vec![], delegates));
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    signal.cancel();
    assert!(matches!(
        future.as_mut().poll(&mut context),
        Poll::Ready(StatementOutcome::Err(EvalError::MissingArgument(message)))
            if message == "cancelled"
    ));
    let terminals = terminals.lock().unwrap();
    assert!(
        terminals
            .iter()
            .any(|event| event == "exit:child:cancelled")
    );
    assert!(terminals.iter().any(|event| event == "exit:main:cancelled"));
    assert!(!terminals.iter().any(|event| event.starts_with("abort:")));
}

#[test]
fn child_scoped_delegate_receives_exit_and_abort() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child_done() -> int { return 1 }
flow child_wait() -> int { return wait().await }
flow main_done() -> int { return child_done().await }
flow main_wait() -> int { return child_wait().await }
"#,
        ),
        &NoSources,
    )
    .unwrap();
    let terminals = Arc::new(Mutex::new(Vec::new()));
    let delegate = ScopedDelegate {
        scope: "unscoped".into(),
        terminals: Arc::clone(&terminals),
    };

    assert!(matches!(
        ready(vm.run("main_done", vec![], delegate.clone())),
        StatementOutcome::Return(Value::Int(1))
    ));
    let mut pending = Box::pin(vm.run("main_wait", vec![], delegate));
    assert!(matches!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(pending);

    let terminals = terminals.lock().unwrap();
    assert!(
        terminals
            .iter()
            .any(|event| event == "exit:child_done:child_done")
    );
    assert!(
        terminals
            .iter()
            .any(|event| event == "abort:child_wait:child_wait")
    );
}

#[test]
fn child_postflight_uses_the_child_delegate_context() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child() -> int { return cancel_child() }
flow main() -> int { return child().await }
"#,
        ),
        &NoSources,
    )
    .unwrap();
    let terminals = Arc::new(Mutex::new(Vec::new()));
    let delegate = ChildCancellationDelegate {
        scope: "unscoped".into(),
        cancelled: Arc::new(AtomicBool::new(false)),
        terminals: Arc::clone(&terminals),
    };

    assert!(matches!(
        ready(vm.run("main", vec![], delegate)),
        StatementOutcome::Err(EvalError::MissingArgument(message))
            if message == "child cancelled"
    ));
    assert_eq!(*terminals.lock().unwrap(), ["exit:child:true"]);
}

#[test]
fn iteration_terminal_events_keep_break_and_continue_previews() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow main() {
    index = 0
    loop {
        index = index + 1
        when index == 1 { continue }
        break
    }
}
"#,
        ),
        &NoSources,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new()).with_observer(Observer {
        trace: Arc::new(Mutex::new(Vec::new())),
        events: Arc::clone(&events),
    });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Continue
    ));
    let previews = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            VmEvent::IterationEnded { preview, .. } => preview.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(previews, ["continue", "break"]);
}

#[test]
fn default_argument_child_has_no_caller_node() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child() -> int { return 7 }
flow main(value: int = child().await) -> int { return value }
"#,
        ),
        &NoSources,
    )
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(ToolRouter::<(), EvalError>::new()).with_observer(Observer {
        trace: Arc::new(Mutex::new(Vec::new())),
        events: Arc::clone(&events),
    });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Return(Value::Int(7))
    ));
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        VmEvent::FlowStarted { context }
            if context.flow.name == "child" && context.caller_node_id.is_none()
    )));
}

#[test]
fn preview_receives_the_corresponding_node_context() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { value = 1 return value }"),
        &NoSources,
    )
    .unwrap();
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let delegates = VmDelegates::new(PreviewEffect {
        contexts: Arc::clone(&contexts),
    });

    assert!(matches!(
        ready(vm.run("main", vec![], delegates)),
        StatementOutcome::Return(Value::Int(1))
    ));
    let node_ids = contexts
        .lock()
        .unwrap()
        .iter()
        .map(|context| context.node_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(node_ids, [Some("0".into()), Some("1".into())]);
}
