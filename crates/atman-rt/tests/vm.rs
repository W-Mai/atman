use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, ExpressionEffect, FlowCall, FlowOutcome, HostFuture, Source, SourceResolver,
    StatementOutcome, Value, Vm, VmCallError, VmEmbedding, ast::LifecycleEvent,
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

struct TestSources;

impl SourceResolver for TestSources {
    type Error = &'static str;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source, Self::Error> {
        match (importer_id, specifier) {
            ("main.at", "lib.at") => Ok(Source::new(
                "lib.at",
                "flow plus_one(value: int) -> int { return value + 1 }\npub flow inc(value: int) -> int { return subflow(plus_one, value) }",
            )),
            _ => Err("source unavailable"),
        }
    }
}

#[derive(Clone, Default)]
struct TestHost {
    events: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<AtomicBool>,
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
fn excess_subflow_argument_does_not_evaluate_its_effect() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow child() -> int { return 1 }\nflow main() -> int { return subflow(child, foreign()) }",
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
fn vm_compiles_routes_and_runs_imported_subflows_with_only_external_effects_in_host() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            r#"
use "lib.at"::inc as increment
route "go" { flow: main }
flow main(input: int) -> int {
    next = subflow(increment, input)
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
            "flow child(value: int) -> int { return value }\nflow main() -> int { return subflow(child, flip()) }",
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
            "flow child() -> int { return flip() }\nflow main() -> int { return subflow(child) }",
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
