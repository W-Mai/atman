use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use atman_rt::{EvalError, ToolArgs, ToolRegisterError, Value};

#[atman_rt::tools]
mod host_tools {
    #[tool(name = "math.double")]
    pub async fn double(value: i64) -> i64 {
        value * 2
    }

    #[tool]
    fn add_optional(value: Option<i64>, numbers: Vec<i64>) -> Vec<i64> {
        numbers
            .into_iter()
            .map(|number| number + value.unwrap_or(0))
            .collect()
    }

    #[tool]
    fn describe(flag: bool, label: String, factor: f64, _marker: ()) -> String {
        format!("{flag}:{label}:{factor}")
    }

    #[tool]
    fn checked(value: i64) -> Result<i64, atman_rt::EvalError> {
        if value < 0 {
            Err(atman_rt::EvalError::MissingArgument("nonnegative".into()))
        } else {
            Ok(value)
        }
    }

    #[tool]
    fn maybe(flag: bool) -> Option<String> {
        flag.then_some("present".to_owned())
    }

    pub fn helper() -> i64 {
        42
    }
}

#[atman_rt::tools]
mod duplicate_tools {
    #[tool(name = "same")]
    fn first() {}

    #[tool(name = "same")]
    fn second() {}
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test tool should complete synchronously"),
    }
}

fn args(
    positional: Vec<Value<(), EvalError>>,
    named: Vec<(&str, Value<(), EvalError>)>,
) -> ToolArgs<(), EvalError> {
    ToolArgs {
        positional,
        named: named
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    }
}

#[test]
fn generated_router_binds_async_and_sync_functions() {
    let tools = host_tools::router::<(), EvalError>().expect("register generated tools");
    assert!(tools.contains("math.double"));
    assert!(!tools.contains("helper"));
    assert_eq!(host_tools::helper(), 42);
    assert!(matches!(
        ready(tools.dispatch("math.double", args(vec![Value::Int(6)], vec![]))),
        Value::Int(12)
    ));
    assert!(matches!(
        ready(tools.dispatch(
            "math.double",
            args(vec![Value::Int(6)], vec![("value", Value::Int(9))]),
        )),
        Value::Int(18)
    ));
    assert!(matches!(
        ready(tools.dispatch("checked", args(vec![Value::Int(-1)], vec![]))),
        Value::Err(EvalError::MissingArgument(name)) if name == "nonnegative"
    ));
}

#[test]
fn generated_router_decodes_optional_lists_and_basic_types() {
    let tools = host_tools::router::<(), EvalError>().expect("register generated tools");
    let call = |named| ready(tools.dispatch("add_optional", args(vec![], named)));
    assert!(matches!(
        call(vec![("numbers", Value::List(vec![Value::Int(2), Value::Int(3)]))]),
        Value::List(values) if matches!(&values[..], [Value::Int(2), Value::Int(3)])
    ));
    assert!(matches!(
        call(vec![
            ("value", Value::Int(4)),
            ("numbers", Value::List(vec![Value::Int(2)])),
        ]),
        Value::List(values) if matches!(&values[..], [Value::Int(6)])
    ));
    assert!(matches!(
        call(vec![
            ("value", Value::Unit),
            ("numbers", Value::List(vec![Value::Int(2)])),
        ]),
        Value::List(values) if matches!(&values[..], [Value::Int(2)])
    ));
    assert!(matches!(
        call(vec![("numbers", Value::List(vec![Value::Str("bad".into())]))]),
        Value::Err(EvalError::TypeMismatch { expected, actual })
            if expected == "int" && actual == "string"
    ));
    assert!(matches!(
        ready(tools.dispatch("add_optional", args(vec![], vec![]))),
        Value::Err(EvalError::MissingArgument(name)) if name == "numbers"
    ));

    assert!(matches!(
        ready(tools.dispatch(
            "describe",
            args(vec![Value::Bool(true), Value::Str("x".into()), Value::Float(1.5), Value::Unit], vec![]),
        )),
        Value::Str(text) if text == "true:x:1.5"
    ));
    assert!(matches!(
        ready(tools.dispatch("maybe", args(vec![Value::Bool(false)], vec![]))),
        Value::Unit
    ));
    assert!(matches!(
        ready(tools.dispatch("maybe", args(vec![Value::Bool(true)], vec![]))),
        Value::Str(text) if text == "present"
    ));
}

#[test]
fn registration_errors_remain_visible() {
    assert!(matches!(
        duplicate_tools::router::<(), EvalError>(),
        Err(ToolRegisterError::DuplicateName(name)) if name == "same"
    ));
}
