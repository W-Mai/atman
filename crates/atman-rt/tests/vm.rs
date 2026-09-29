use std::{
    future::{Future, poll_fn},
    num::NonZeroUsize,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use atman_rt::{
    EvalError, ExpressionEffect, FlowCall, FlowDriveMode, FlowOutcome, HostFuture, Source,
    SourceResolver, StatementOutcome, Value, Vm, VmCallError, VmContext, VmDelegate, VmRunOptions,
    ast::LifecycleEvent,
};

type TestValue = Value<(), EvalError>;

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

fn run_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test host must complete synchronously"),
    }
}

fn run_until_ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..16 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("flow did not finish after all branches were polled");
}

struct TestSources;

impl SourceResolver for TestSources {
    type Error = &'static str;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source, Self::Error> {
        match (importer_id, specifier) {
            ("main.at", "lib.at") => Ok(Source::new(
                "lib.at",
                "flow plus_one(value: int) -> int { return value + 1 }\npub flow inc(value: int) -> int { return plus_one(value).await }",
            )),
            _ => Err("source unavailable"),
        }
    }
}

#[derive(Clone, Default)]
struct TestHost {
    events: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<AtomicBool>,
    modes: Arc<Mutex<Vec<FlowDriveMode>>>,
    second_started: Arc<AtomicBool>,
}

impl VmDelegate for TestHost {
    type Payload = ();
    type Error = EvalError;
    type Permit = ();
    type FlowGuard = ();

    fn authorize<'a>(
        &'a self,
        _effect: &'a ExpressionEffect<Self::Payload, Self::Error>,
        _context: &'a VmContext,
    ) -> HostFuture<'a, Result<Self::Permit, Self::Error>> {
        Box::pin(async { Ok(()) })
    }

    fn enter_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
    ) -> Result<(Self, Self::FlowGuard), Self::Error> {
        if let Some(call) = call {
            self.modes.lock().unwrap().push(call.mode);
            self.events.lock().unwrap().push(format!(
                "enter:{}:{}:{}:{}",
                call.target.name,
                call.source_id,
                call.parent_node_id.unwrap_or("<none>"),
                call.contract.is_some()
            ));
        }
        Ok((self.clone(), ()))
    }

    fn exit_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        _guard: Self::FlowGuard,
    ) {
        if let Some(call) = call {
            self.events.lock().unwrap().push(format!(
                "exit:{}:{}",
                call.target.name,
                matches!(outcome, StatementOutcome::Return(Value::Int(_)))
            ));
        }
    }

    fn abort_flow(
        &self,
        call: Option<&FlowCall<'_>>,
        _context: &VmContext,
        _guard: Self::FlowGuard,
    ) {
        if let Some(call) = call {
            self.events
                .lock()
                .unwrap()
                .push(format!("exit:{}:false", call.target.name));
        }
    }

    fn cancellation_error(&self, _context: &VmContext) -> Option<Self::Error> {
        self.cancelled
            .load(Ordering::SeqCst)
            .then(|| EvalError::TypeMismatch {
                expected: "running flow".into(),
                actual: "cancelled".into(),
            })
    }

    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, Self::Error> {
        Box::pin(poll_fn(move |_| {
            if self.cancelled.load(Ordering::SeqCst) {
                Poll::Ready(EvalError::TypeMismatch {
                    expected: "running flow".into(),
                    actual: "cancelled".into(),
                })
            } else {
                Poll::Pending
            }
        }))
    }

    fn preflight_tool(&self, name: &str, _context: &VmContext) -> Option<TestValue> {
        (name == "blocked").then(|| {
            Value::Err(EvalError::TypeMismatch {
                expected: "allowed tool".into(),
                actual: name.into(),
            })
        })
    }

    fn invoke<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
        _permit: Self::Permit,
        _context: &'a VmContext,
    ) -> HostFuture<'a, TestValue> {
        Box::pin(async move {
            match effect {
                ExpressionEffect::ToolCall { name, .. } if name == "foreign" => {
                    self.events.lock().unwrap().push("foreign".into());
                    Value::Int(3)
                }
                ExpressionEffect::ToolCall { name, .. } if name == "fail" => {
                    self.events.lock().unwrap().push("fail".into());
                    Value::Err(self.call_error(VmCallError::MissingFlow(name)))
                }
                ExpressionEffect::ToolCall { name, .. } if name == "flip" => {
                    self.events.lock().unwrap().push("flip".into());
                    self.cancelled.store(true, Ordering::SeqCst);
                    Value::Int(1)
                }
                ExpressionEffect::ToolCall { name, .. } if name == "blocking_first" => {
                    poll_fn(|_| {
                        if self.second_started.load(Ordering::SeqCst) {
                            Poll::Ready(Value::Int(1))
                        } else {
                            Poll::Pending
                        }
                    })
                    .await
                }
                ExpressionEffect::ToolCall { name, .. } if name == "unblock_second" => {
                    self.second_started.store(true, Ordering::SeqCst);
                    Value::Int(2)
                }
                ExpressionEffect::ToolCall { name, .. } if name == "pending_forever" => {
                    poll_fn(|_| Poll::<TestValue>::Pending).await
                }
                ExpressionEffect::Message { .. } => Value::Unit,
                _ => panic!("unexpected external effect"),
            }
        })
    }

    fn call_error(&self, error: VmCallError) -> Self::Error {
        EvalError::TypeMismatch {
            expected: "valid VM call".into(),
            actual: error.to_string(),
        }
    }

    fn error_status(&self, error: &Self::Error) -> atman_rt::VmStatus {
        match error {
            EvalError::TypeMismatch { actual, .. } if actual == "cancelled" => {
                atman_rt::VmStatus::Cancelled
            }
            _ => atman_rt::VmStatus::Err,
        }
    }
}

#[test]
fn metered_run_reports_executed_language_operations() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { value = 1 + 2 return value }",
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::Int(3))
    ));
    assert_eq!(execution.operations(), 6);
    assert_eq!(execution.cooperative_yields(), 0);
}

#[test]
fn source_yield_self_wakes_and_resumes_with_the_next_statement() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { yield return foreign() }"),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let future = vm.run("main", vec![], host);
    let mut future = pin!(future);
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert!(events.lock().unwrap().is_empty());

    let Poll::Ready(outcome) = future.as_mut().poll(&mut context) else {
        panic!("flow must resume after the source yield");
    };
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(3))));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert_eq!(*events.lock().unwrap(), ["foreign"]);
}

#[test]
fn source_yield_resumes_nested_when_loop_before_return() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { when true { loop { yield break } } return 7 }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let future = vm.run("main", vec![], TestHost::default());
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    let Poll::Ready(outcome) = future.as_mut().poll(&mut context) else {
        panic!("nested control flow must resume after the source yield");
    };
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(7))));
}

#[test]
fn fanout_children_resume_after_source_yield_in_source_order() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child(value: int) -> int { yield return value }\nflow main() -> [int] { return fanout [child(1), child(2)] }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let future = vm.run("main", vec![], TestHost::default());
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    let Poll::Ready(outcome) = future.as_mut().poll(&mut context) else {
        panic!("fanout children must resume after their source yields");
    };
    assert!(matches!(
        outcome,
        StatementOutcome::Return(Value::List(values))
            if matches!(values.as_slice(), [Value::Int(1), Value::Int(2)])
    ));
}

#[test]
fn source_yield_counts_as_one_operation_without_an_interval_epoch() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { yield return foreign() }"),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_until_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::Int(3))
    ));
    assert_eq!(execution.operations(), 3);
    assert_eq!(execution.cooperative_yields(), 0);
}

#[test]
fn source_yield_and_interval_produce_independent_consecutive_pending_polls() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() { 1 yield }"),
        &TestSources,
    )
    .expect("compile source");
    let future = vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure().with_yield_interval(NonZeroUsize::new(2).unwrap()),
    );
    let mut future = pin!(future);
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 2);

    let Poll::Ready(execution) = future.as_mut().poll(&mut context) else {
        panic!("flow must finish after the interval and source yields");
    };
    assert!(matches!(execution.output(), StatementOutcome::Continue));
    assert_eq!(execution.operations(), 3);
    assert_eq!(execution.cooperative_yields(), 1);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 2);
}

#[test]
fn operation_limit_is_preserved_across_a_source_yield() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { yield return foreign() }"),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let future = vm.run_with_options(
        "main",
        vec![],
        host,
        VmRunOptions::limited(NonZeroUsize::new(1).unwrap()),
    );
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    let Poll::Ready(execution) = future.as_mut().poll(&mut context) else {
        panic!("operation limit must stop the statement after the source yield");
    };

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 1 exceeded"
    ));
    assert_eq!(execution.operations(), 1);
    assert_eq!(execution.cooperative_yields(), 0);
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn cancellation_wins_when_a_source_yield_resumes() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { yield return foreign() }"),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let cancelled = Arc::clone(&host.cancelled);
    let events = Arc::clone(&host.events);
    let future = vm.run("main", vec![], host);
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    cancelled.store(true, Ordering::SeqCst);
    let Poll::Ready(outcome) = future.as_mut().poll(&mut context) else {
        panic!("cancellation must win when the source yield resumes");
    };

    assert!(matches!(
        outcome,
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "cancelled"
    ));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn explicit_yield_interval_returns_pending_between_operation_quanta() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { value = 1 + 2 return value }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let future = vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure().with_yield_interval(NonZeroUsize::new(2).unwrap()),
    );
    let mut future = pin!(future);
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 2);
    let Poll::Ready(execution) = future.as_mut().poll(&mut context) else {
        panic!("flow must finish after both cooperative yields");
    };

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::Int(3))
    ));
    assert_eq!(execution.operations(), 6);
    assert_eq!(execution.cooperative_yields(), 2);
}

#[test]
fn explicit_yield_interval_cooperates_inside_an_empty_loop() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() { loop {} }"),
        &TestSources,
    )
    .expect("compile source");
    let future = vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::limited(NonZeroUsize::new(5).unwrap())
            .with_yield_interval(NonZeroUsize::new(2).unwrap()),
    );
    let mut future = pin!(future);
    let wakes = Arc::new(WakeCounter::default());
    let waker = Waker::from(Arc::clone(&wakes));
    let mut context = Context::from_waker(&waker);

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    let Poll::Ready(execution) = future.as_mut().poll(&mut context) else {
        panic!("operation limit must stop the loop after both cooperative yields");
    };

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 5 exceeded"
    ));
    assert_eq!(execution.operations(), 5);
    assert_eq!(execution.cooperative_yields(), 2);
    assert_eq!(wakes.0.load(Ordering::SeqCst), 2);
}

#[test]
fn cancellation_is_observed_after_a_cooperative_yield() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() { loop {} }"),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let cancelled = Arc::clone(&host.cancelled);
    let future = vm.run_with_options(
        "main",
        vec![],
        host,
        VmRunOptions::measure().with_yield_interval(NonZeroUsize::new(2).unwrap()),
    );
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    cancelled.store(true, Ordering::SeqCst);
    let Poll::Ready(execution) = future.as_mut().poll(&mut context) else {
        panic!("cancellation must win when the executor polls the VM again");
    };

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "cancelled"
    ));
    assert_eq!(execution.operations(), 2);
    assert_eq!(execution.cooperative_yields(), 1);
}

#[test]
fn explicit_operation_limit_stops_before_the_next_expression() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { value = 1 + 2 return value }",
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::limited(NonZeroUsize::new(5).unwrap()),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 5 exceeded"
    ));
    assert_eq!(execution.operations(), 5);
}

#[test]
fn explicit_operation_limit_stops_an_empty_loop() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() { loop {} }"),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::limited(NonZeroUsize::new(4).unwrap()),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 4 exceeded"
    ));
    assert_eq!(execution.operations(), 4);
}

#[test]
fn child_flows_share_the_parent_operation_meter() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return 1 }\nflow main() -> int { return child().await }",
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::Int(1))
    ));
    assert_eq!(execution.operations(), 5);
}

#[test]
fn static_fanout_counts_its_list_and_branch_expressions() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> [int] { return fanout [1, 2] }"),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_until_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::List(values))
            if matches!(&values[..], [Value::Int(1), Value::Int(2)])
    ));
    assert_eq!(execution.operations(), 5);
}

#[test]
fn fanout_branches_share_one_cooperative_yield_epoch() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> [int] { return fanout [1, 2, 3] }",
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_until_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure().with_yield_interval(NonZeroUsize::new(3).unwrap()),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::List(values))
            if matches!(&values[..], [Value::Int(1), Value::Int(2), Value::Int(3)])
    ));
    assert_eq!(execution.operations(), 6);
    assert_eq!(execution.cooperative_yields(), 1);
}

#[test]
fn short_fanout_branch_progresses_beside_an_infinite_branch() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow spin() { loop {} }\nflow short() -> int { return 7 }\nflow main() { fanout [spin(), short()] }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let future = vm.run_with_options(
        "main",
        vec![],
        host,
        VmRunOptions::measure().with_yield_interval(NonZeroUsize::new(1).unwrap()),
    );
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());

    for _ in 0..32 {
        assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event.starts_with("exit:short:true"))
        {
            return;
        }
    }
    panic!("short branch was starved by the infinite branch");
}

#[test]
fn fanout_branches_share_one_explicit_operation_limit() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child(value: int) -> int { return value }\nflow main() -> [int] { return fanout [child(1), child(2)] }",
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_until_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::limited(NonZeroUsize::new(10).unwrap()),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 10 exceeded"
    ));
    assert_eq!(execution.operations(), 10);
}

#[test]
fn message_attachment_fast_path_counts_source_expressions() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"flow main() { return user_msg("describe", attachments: [@"pic.png", @"other.jpg"]) }"#,
        ),
        &TestSources,
    )
    .expect("compile source");

    let execution = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert!(matches!(
        execution.output(),
        StatementOutcome::Return(Value::Unit)
    ));
    assert_eq!(execution.operations(), 6);
}

#[test]
fn operation_meter_resets_for_each_explicit_run() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return 1 }"),
        &TestSources,
    )
    .expect("compile source");

    let first = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));
    let second = run_ready(vm.run_with_options(
        "main",
        vec![],
        TestHost::default(),
        VmRunOptions::measure(),
    ));

    assert_eq!(first.operations(), 2);
    assert_eq!(second.operations(), 2);
}

#[test]
fn denied_tool_does_not_evaluate_its_arguments() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { return blocked(foreign()) }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Err(_)));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn excess_flow_argument_does_not_evaluate_its_effect() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return 1 }\nflow main() -> int { return child(foreign()).await }",
        ),
        &TestSources,
    )
    .expect("compile source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Err(_)));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn vm_compiles_routes_and_runs_imported_flows_with_only_external_effects_in_host() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
use "lib.at"::inc as increment
route "go" { flow: main }
flow main(input: int) -> int {
    next = increment(input).await
    return next + foreign()
}
"#,
        ),
        &TestSources,
    )
    .expect("compile source closure");
    let route = vm.route("go 42").expect("route input");
    assert_eq!(route.command, "main");
    assert_eq!(route.args, "42");

    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![("input".into(), Value::Int(4))], host));
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(8))));
    assert_eq!(
        *events.lock().unwrap(),
        [
            "enter:inc:lib.at:0:false",
            "enter:plus_one:lib.at:0:false",
            "exit:plus_one:true",
            "exit:inc:true",
            "foreign"
        ]
    );
}

#[test]
fn vm_enforces_types_at_root_and_driven_child_boundaries() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
flow child(value: int) -> int { return value }
flow broken() -> int { return "wrong" }
flow main(input: int) -> int { return input }
flow call_bad_argument() -> int { return child(value: "wrong").await }
flow call_bad_return() -> int { return broken().await }
"#,
        ),
        &TestSources,
    )
    .expect("compile typed flows");

    assert!(matches!(
        run_ready(vm.run(
            "main",
            vec![("input".into(), Value::Str("wrong".into()))],
            TestHost::default(),
        )),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "parameter `input`: int"
                && actual == "parameter `input`: string"
    ));
    assert!(matches!(
        run_ready(vm.run("main", vec![], TestHost::default())),
        StatementOutcome::Err(EvalError::MissingArgument(name)) if name == "input"
    ));
    assert!(matches!(
        run_ready(vm.run("call_bad_argument", vec![], TestHost::default())),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "parameter `value`: int"
                && actual == "parameter `value`: string"
    ));
    assert!(matches!(
        run_ready(vm.run("call_bad_return", vec![], TestHost::default())),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "return value: int" && actual == "return value: string"
    ));
}

#[test]
fn lifecycle_runs_all_matching_bodies_after_an_error() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
on turn.start { foreign() }
on turn.start { fail() }
on turn.start { foreign() }
flow main() -> int { return 0 }
"#,
        ),
        &TestSources,
    )
    .expect("compile lifecycle source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcomes = run_ready(vm.run_lifecycle(LifecycleEvent::TurnStart, host));
    assert_eq!(outcomes.len(), 3);
    assert!(matches!(outcomes[0], StatementOutcome::Continue));
    assert!(matches!(outcomes[1], StatementOutcome::Err(_)));
    assert!(matches!(outcomes[2], StatementOutcome::Continue));
    assert_eq!(*events.lock().unwrap(), ["foreign", "fail", "foreign"]);
}

#[test]
fn lifecycle_bodies_share_the_explicit_invocation_limit() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "on turn.start { 1 }\non turn.start { 2 }\nflow main() -> int { return 0 }",
        ),
        &TestSources,
    )
    .expect("compile lifecycle source");

    let execution = run_ready(vm.run_lifecycle_with_options(
        LifecycleEvent::TurnStart,
        TestHost::default(),
        VmRunOptions::limited(NonZeroUsize::new(3).unwrap()),
    ));

    assert!(matches!(execution.output()[0], StatementOutcome::Continue));
    assert!(matches!(
        &execution.output()[1],
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. })
            if actual == "operation limit of 3 exceeded"
    ));
    assert_eq!(execution.operations(), 3);
}

#[test]
fn cancellation_before_child_entry_wins_after_argument_effect() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child(value: int) -> int { return value }\nflow main() -> int { return child(flip()).await }",
        ),
        &TestSources,
    )
    .expect("compile cancellation source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Err(_)));
    assert_eq!(*events.lock().unwrap(), ["flip"]);
}

#[test]
fn cancellation_after_child_body_marks_child_terminal_as_error() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return flip() }\nflow main() -> int { return child().await }",
        ),
        &TestSources,
    )
    .expect("compile cancellation source");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Err(_)));
    assert_eq!(
        *events.lock().unwrap(),
        ["enter:child:main.at:0:false", "flip", "exit:child:false"]
    );
}

#[test]
fn cold_call_defers_body_and_default_arguments() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child(value: int = foreign()) -> int { return foreign() }\nflow main() -> int { pending = child(); return 9 }",
        ),
        &TestSources,
    )
    .expect("compile cold call");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(9))));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn repeated_inline_await_runs_one_child_and_reuses_result() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return foreign() }\nflow main() -> int { pending = child(); a = pending.await; b = pending.await; return a + b }",
        ),
        &TestSources,
    )
    .expect("compile repeated await");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let modes = Arc::clone(&host.modes);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(6))));
    assert_eq!(
        *events.lock().unwrap(),
        ["enter:child:main.at:1:false", "foreign", "exit:child:true"]
    );
    assert_eq!(*modes.lock().unwrap(), [FlowDriveMode::Inline]);
}

#[test]
fn fanout_drives_cold_calls_concurrently_and_keeps_source_order() {
    for body in [
        "pending = [first(), second()]; return fanout pending",
        "return fanout [first(), second()]",
        "return fanout [first(), second()] { |pending| pending }",
        "return fanout [first().await, second().await]",
        "return fanout [first().await + 0, second().await + 0]",
    ] {
        let vm = Vm::compile(
            Source::new(
                "main.at",
                format!("flow first() -> int {{ return blocking_first() }}\nflow second() -> int {{ return unblock_second() }}\nflow main() -> [int] {{ {body} }}"),
            ),
            &TestSources,
        )
        .expect("compile fanout");
        let host = TestHost::default();
        let modes = Arc::clone(&host.modes);
        let outcome = run_until_ready(vm.run("main", vec![], host));
        assert!(
            matches!(outcome, StatementOutcome::Return(Value::List(values)) if matches!(&values[..], [Value::Int(1), Value::Int(2)]))
        );
        assert_eq!(
            *modes.lock().unwrap(),
            [FlowDriveMode::Parallel, FlowDriveMode::Parallel]
        );
    }
}

#[test]
fn fanout_rejects_excess_distinct_futures_before_starting_any_child() {
    let calls = vec!["child()"; 129].join(", ");
    let vm = Vm::compile(
        Source::new(
            "main.at",
            format!("flow child() -> int {{ return foreign() }}\nflow main() -> [int] {{ pending = [{calls}]; return fanout pending }}"),
        ),
        &TestSources,
    )
    .expect("compile oversized fanout");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let outcome = run_ready(vm.run("main", vec![], host));
    assert!(
        matches!(outcome, StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "129 distinct futures")
    );
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn explicit_awaits_inside_fanout_share_the_parallel_activity_limit() {
    let calls = vec!["child().await"; 129].join(", ");
    let vm = Vm::compile(
        Source::new(
            "main.at",
            format!("flow child() -> int {{ return pending_forever() }}\nflow main() -> [int] {{ return fanout [{calls}] }}"),
        ),
        &TestSources,
    )
    .expect("compile awaited fanout");
    let host = TestHost::default();
    let modes = Arc::clone(&host.modes);
    let events = Arc::clone(&host.events);
    let mut future = Box::pin(vm.run("main", vec![], host));
    for _ in 0..3 {
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
    }
    assert_eq!(modes.lock().unwrap().len(), 128);
    assert!(
        modes
            .lock()
            .unwrap()
            .iter()
            .all(|mode| *mode == FlowDriveMode::Parallel)
    );
    drop(future);
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.starts_with("enter:child:"))
            .count(),
        128
    );
    assert_eq!(
        events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "exit:child:false")
            .count(),
        128
    );
}

#[test]
fn fanout_repeated_reference_runs_one_child_without_deadlock() {
    for count in [2, 129] {
        let references = vec!["pending"; count].join(", ");
        let vm = Vm::compile(
            Source::new(
                "main.at",
                format!("flow child() -> int {{ return foreign() }}\nflow main() -> [int] {{ pending = child(); return fanout [{references}] }}"),
            ),
            &TestSources,
        )
        .expect("compile repeated fanout reference");
        let host = TestHost::default();
        let events = Arc::clone(&host.events);
        let modes = Arc::clone(&host.modes);
        let outcome = run_until_ready(vm.run("main", vec![], host));
        assert!(
            matches!(outcome, StatementOutcome::Return(Value::List(values)) if values.len() == count && values.iter().all(|value| matches!(value, Value::Int(3))))
        );
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| *event == "foreign")
                .count(),
            1
        );
        assert_eq!(*modes.lock().unwrap(), [FlowDriveMode::Parallel]);
    }
}

#[test]
fn future_cannot_cross_return_or_external_effect_boundary() {
    for source in [
        "flow child() -> int { return foreign() }\nflow main() -> int { return child() }",
        "flow child() -> int { return foreign() }\nflow main() -> int { return foreign([child()]) }",
    ] {
        let vm = Vm::compile(Source::new("main.at", source), &TestSources)
            .expect("compile future boundary");
        let host = TestHost::default();
        let events = Arc::clone(&host.events);
        assert!(matches!(
            run_ready(vm.run("main", vec![], host)),
            StatementOutcome::Err(_)
        ));
        assert!(events.lock().unwrap().is_empty());
    }
}

#[test]
fn dropping_a_driven_child_emits_one_terminal_event() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return pending_forever() }\nflow main() -> int { pending = child(); return pending.await }",
        ),
        &TestSources,
    )
    .expect("compile pending child");
    let host = TestHost::default();
    let events = Arc::clone(&host.events);
    let mut future = Box::pin(vm.run("main", vec![], host));
    assert!(matches!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(future);
    assert_eq!(
        *events.lock().unwrap(),
        ["enter:child:main.at:1:false", "exit:child:false"]
    );
}
