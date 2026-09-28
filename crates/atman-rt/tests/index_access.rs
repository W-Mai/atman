use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, Source, SourceResolver, StatementOutcome, ToolRouter, Value, Vm, VmDelegates,
};

type TestValue = Value<(), EvalError>;

struct NoSources;

impl SourceResolver for NoSources {
    type Error = &'static str;

    fn resolve(&self, _importer_id: &str, _specifier: &str) -> Result<Source, Self::Error> {
        Err("source unavailable")
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("index tests must complete synchronously"),
    }
}

fn run(source: &str) -> StatementOutcome<TestValue, EvalError> {
    let vm = Vm::compile(Source::new("main.at", source), &NoSources).expect("compile source");
    ready(vm.run(
        "main",
        vec![],
        VmDelegates::new(ToolRouter::<(), EvalError>::new()),
    ))
}

#[test]
fn index_selects_values_and_can_return_a_cold_flow_future() {
    let outcome = run(r#"
flow child() -> int { return 9 }

flow main() -> int {
    pending = [child()]
    return pending[0].await
}
"#);
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(9))));
}

#[test]
fn list_get_uses_the_same_index_contract() {
    assert!(matches!(
        run("flow main() -> int { return list.get(items: [10, 20], index: 1) }"),
        StatementOutcome::Return(Value::Int(20))
    ));
    assert!(matches!(
        run("flow main() -> int { return list.get([10], -1) }"),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "list index within bounds" && actual == "index -1 for length 1"
    ));
}

#[test]
fn index_expression_can_call_a_flow() {
    let outcome = run(r#"
flow choose() -> int { return 1 }

flow main() -> int {
    values = [10, 20]
    return values[choose().await]
}
"#);
    assert!(matches!(outcome, StatementOutcome::Return(Value::Int(20))));
}

#[test]
fn negative_and_large_indexes_are_explicit_errors() {
    assert!(matches!(
        run("flow main() -> int { return [1][-1] }"),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "list index within bounds" && actual == "index -1 for length 1"
    ));
    assert!(matches!(
        run("flow main() -> int { return [1][1] }"),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "list index within bounds" && actual == "index 1 for length 1"
    ));
}

#[test]
fn index_requires_a_list_and_integer() {
    assert!(matches!(
        run("flow main() -> int { return 1[0] }"),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "list" && actual == "int"
    ));
    assert!(matches!(
        run("flow main() -> int { return [1][\"zero\"] }"),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "int" && actual == "string"
    ));
}
