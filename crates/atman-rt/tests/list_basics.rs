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
        Poll::Pending => panic!("portable list intrinsics must complete synchronously"),
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
fn list_basics_run_without_host_tools() {
    let outcome = run(r#"
flow main() -> [int] {
    values = list.concat(
        [list.first([1, 2]), list.last([1, 2])],
        list.tail([3, 4, 5]),
    )
    when list.is_empty(values) {
        return []
    }
    return list.concat(values, [list.len(values), len("你好")])
}
"#);

    assert!(matches!(
        outcome,
        StatementOutcome::Return(Value::List(values))
            if matches!(
                &values[..],
                [
                    Value::Int(1),
                    Value::Int(2),
                    Value::Int(4),
                    Value::Int(5),
                    Value::Int(4),
                    Value::Int(2),
                ]
            )
    ));
}

#[test]
fn published_bare_names_keep_their_behavior() {
    let outcome = run(r#"
flow main() -> [int] {
    when !is_empty("") {
        return []
    }
    return concat(left: [head([7, 8]), len("ab")], right: tail([9, 10]))
}
"#);

    assert!(matches!(
        outcome,
        StatementOutcome::Return(Value::List(values))
            if matches!(&values[..], [Value::Int(7), Value::Int(2), Value::Int(10)])
    ));
}

#[test]
fn namespaced_length_only_accepts_lists() {
    assert!(matches!(
        run(r#"flow main() -> int { return list.len("text") }"#),
        StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "list" && actual == "string"
    ));
}

#[test]
fn empty_list_errors_name_the_called_intrinsic() {
    assert!(matches!(
        run(r#"flow main() -> int { return head([]) }"#),
        StatementOutcome::Err(EvalError::EmptyList(name)) if name == "head"
    ));
    assert!(matches!(
        run(r#"flow main() -> int { return list.last([]) }"#),
        StatementOutcome::Err(EvalError::EmptyList(name)) if name == "list.last"
    ));
}
