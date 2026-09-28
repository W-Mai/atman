use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, ExpressionEffect, FlowCall, FlowDriveMode, FlowOutcome, HostFuture, Source,
    SourceResolver, StatementOutcome, Value, Vm, VmCallError, VmEmbedding, ast::LifecycleEvent,
};

type TestValue = Value<(), EvalError>;

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

impl VmEmbedding for TestHost {
    type Payload = ();
    type Error = EvalError;

    fn cancellation_error(&self) -> Option<Self::Error> {
        self.cancelled
            .load(Ordering::SeqCst)
            .then(|| EvalError::TypeMismatch {
                expected: "running flow".into(),
                actual: "cancelled".into(),
            })
    }

    fn preflight_tool(&self, name: &str) -> Option<TestValue> {
        (name == "blocked").then(|| {
            Value::Err(EvalError::TypeMismatch {
                expected: "allowed tool".into(),
                actual: name.into(),
            })
        })
    }

    fn effect<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
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

    fn child(&self, call: &FlowCall<'_>) -> Result<Self, Self::Error> {
        self.modes.lock().unwrap().push(call.mode);
        self.events.lock().unwrap().push(format!(
            "enter:{}:{}:{}:{}",
            call.target.name,
            call.source_id,
            call.parent_node_id,
            call.contract.is_some()
        ));
        Ok(self.clone())
    }

    fn child_end(&self, call: &FlowCall<'_>, outcome: &FlowOutcome<Self::Payload, Self::Error>) {
        self.events.lock().unwrap().push(format!(
            "exit:{}:{}",
            call.target.name,
            matches!(outcome, StatementOutcome::Return(Value::Int(_)))
        ));
    }
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
    let outcomes = run_ready(vm.run_lifecycle_with(LifecycleEvent::TurnStart, host));
    assert_eq!(outcomes.len(), 3);
    assert!(matches!(outcomes[0], StatementOutcome::Continue));
    assert!(matches!(outcomes[1], StatementOutcome::Err(_)));
    assert!(matches!(outcomes[2], StatementOutcome::Continue));
    assert_eq!(*events.lock().unwrap(), ["foreign", "fail", "foreign"]);
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
