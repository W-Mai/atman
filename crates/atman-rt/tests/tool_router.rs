use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use atman_rt::{
    EvalError, ExpressionEffect, Source, SourceResolver, StatementOutcome, ToolArgs,
    ToolRegisterError, ToolRouter, Value, Vm, VmEmbedding,
};
use futures::task::AtomicWaker;

struct WakeFlag(AtomicBool);

impl Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

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
        .register_sync("add", |args| {
            Ok(Value::Int(args.int("a", 0)? + args.int("b", 1)?))
        })
        .expect("register tool");
    assert!(matches!(
        ready(vm.run("main", vec![], tools)),
        StatementOutcome::Return(Value::Int(5))
    ));
}

#[test]
fn async_tool_is_cold_until_await_and_reuses_its_result() {
    let mut tools = ToolRouter::<(), EvalError>::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    tools
        .register("delayed", move |args| {
            let observed = Arc::clone(&observed);
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Int(args.int("value", 0)?))
            }
        })
        .expect("register async tool");

    let cold = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { pending = delayed(4); return 0 }",
        ),
        &NoSources,
    )
    .expect("compile cold call");
    assert!(matches!(
        ready(cold.run("main", vec![], tools.clone())),
        StatementOutcome::Return(Value::Int(0))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    let awaited = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> int { pending = delayed(4); first = pending.await; second = pending.await; return first + second }",
        ),
        &NoSources,
    )
    .expect("compile awaited call");
    assert!(matches!(
        ready(awaited.run("main", vec![], tools.clone())),
        StatementOutcome::Return(Value::Int(8))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let escaped = Vm::compile(
        Source::new("main.at", "flow main() -> int { return delayed(4) }"),
        &NoSources,
    )
    .expect("compile escaped call");
    assert!(matches!(
        ready(escaped.run("main", vec![], tools)),
        StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "pending call"
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn fanout_starts_async_tools_concurrently() {
    let vm = Vm::compile(
        Source::new(
            "main.at",
            "flow main() -> [int] { return fanout [delayed(1), delayed(2)] }",
        ),
        &NoSources,
    )
    .expect("compile fanout");
    let started = Arc::new(AtomicUsize::new(0));
    let waiting = Arc::new(AtomicWaker::new());
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("delayed", move |args| {
            let started = Arc::clone(&started);
            let waiting = Arc::clone(&waiting);
            async move {
                if started.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                    waiting.wake();
                }
                std::future::poll_fn(|cx| {
                    if started.load(Ordering::SeqCst) == 2 {
                        return Poll::Ready(Ok(Value::Int(
                            args.int("value", 0).expect("integer argument"),
                        )));
                    }
                    waiting.register(cx.waker());
                    if started.load(Ordering::SeqCst) == 2 {
                        waiting.wake();
                    }
                    Poll::Pending
                })
                .await
            }
        })
        .expect("register async tool");
    let mut future = pin!(vm.run("main", vec![], tools));
    let wake_flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = Waker::from(Arc::clone(&wake_flag));
    let mut context = Context::from_waker(&waker);
    let outcome = match future.as_mut().poll(&mut context) {
        Poll::Ready(outcome) => outcome,
        Poll::Pending => {
            assert!(
                wake_flag.0.swap(false, Ordering::SeqCst),
                "pending fanout must wake its executor when another branch starts"
            );
            match future.as_mut().poll(&mut context) {
                Poll::Ready(outcome) => outcome,
                Poll::Pending => panic!("woken fanout must complete on the next poll"),
            }
        }
    };
    assert!(matches!(
        outcome,
        StatementOutcome::Return(Value::List(values))
            if matches!(&values[..], [Value::Int(1), Value::Int(2)])
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
