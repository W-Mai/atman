use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, ExpressionEffect, Source, SourceResolver, StatementOutcome, ToolArgs,
    ToolRegisterError, ToolRouter, Value, Vm, VmEmbedding,
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
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test tool should complete synchronously"),
    }
}

#[test]
fn registered_tool_receives_evaluated_named_and_positional_arguments() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return add(2, b: 3) }"),
        &NoSources,
    )
    .expect("compile source");
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("add", |args| async move {
            Ok(Value::Int(args.int("a", 0)? + args.int("b", 1)?))
        })
        .expect("register tool");
    assert!(matches!(
        ready(vm.run("main", vec![], tools)),
        StatementOutcome::Return(Value::Int(5))
    ));
}

#[test]
fn unknown_tools_fail_before_arguments_run_and_at_dispatch() {
    let vm = Vm::compile(
        Source::new("main.at", "flow main() -> int { return missing(1 / 0) }"),
        &NoSources,
    )
    .expect("compile source");
    let tools = ToolRouter::<(), EvalError>::new();
    assert!(matches!(
        ready(vm.run("main", vec![], tools.clone())),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "missing"
    ));
    assert!(matches!(
        ready(tools.dispatch("missing", ToolArgs { positional: vec![], named: vec![] })),
        Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "missing"
    ));
}

#[test]
fn registration_rejects_reserved_and_duplicate_names_without_mutating_clones() {
    let mut tools = ToolRouter::<(), EvalError>::new();
    assert!(matches!(
        tools.register(" ", |_| async { Ok(Value::Unit) }),
        Err(ToolRegisterError::EmptyName)
    ));
    assert!(matches!(
        tools.register("list.map", |_| async { Ok(Value::Unit) }),
        Err(ToolRegisterError::ReservedName(_))
    ));
    tools
        .register("list", |_| async { Ok(Value::Unit) })
        .expect("bare list is not an intrinsic");
    let old = tools.clone();
    tools
        .register("ready", |_| async { Ok(Value::Int(1)) })
        .expect("register tool");
    assert!(!old.contains("ready"));
    assert!(matches!(
        tools.register("ready", |_| async { Ok(Value::Int(2)) }),
        Err(ToolRegisterError::DuplicateName(_))
    ));
    assert!(matches!(
        ready(tools.dispatch(
            "ready",
            ToolArgs {
                positional: vec![],
                named: vec![]
            }
        )),
        Value::Int(1)
    ));
}

#[test]
fn handler_errors_and_non_tool_effects_remain_explicit() {
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("fail", |_| async {
            Err(EvalError::MissingArgument("input".into()))
        })
        .expect("register tool");
    assert!(matches!(
        ready(tools.dispatch("fail", ToolArgs { positional: vec![], named: vec![] })),
        Value::Err(EvalError::MissingArgument(name)) if name == "input"
    ));
    assert!(matches!(
        ready(tools.effect(ExpressionEffect::FileRef("file.txt".into()))),
        Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "file reference"
    ));
}
