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
    EffectDelegate, EvalError, ExpressionEffect, FlowDriveMode, Source, SourceResolver,
    StatementOutcome, ToolArgs, ToolRegisterError, ToolRouter, Value, Vm, VmContext, VmDelegates,
    VmRunId,
    binding::Factory,
    catalog::{ToolSpec, TypeSpec},
    program::{FlowId, ModuleId},
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

fn context() -> VmContext {
    VmContext {
        run_id: VmRunId(1),
        parent_run_id: None,
        source_id: "main.at".into(),
        flow: FlowId {
            module: ModuleId(0),
            name: "main".into(),
        },
        caller_node_id: None,
        node_id: None,
        parent_node_id: None,
        drive_mode: FlowDriveMode::Inline,
        branch_index: None,
    }
}

fn tool_spec(name: &str, mode: atman_rt::ToolCallMode) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        namespace: "test".into(),
        description: "test tool".into(),
        mode,
        params: vec![],
        result: TypeSpec::Unit,
    }
}

struct TestFactory {
    names: &'static [&'static str],
    calls: Arc<AtomicUsize>,
}

impl<P, E> Factory<P, E> for TestFactory
where
    P: Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    fn build(
        self,
        _resources: Arc<atman_rt::resource::ResourceRegistry>,
    ) -> Result<ToolRouter<P, E>, ToolRegisterError> {
        let mut router = ToolRouter::new();
        for name in self.names {
            let calls = Arc::clone(&self.calls);
            router.register_sync(*name, move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Value::Unit)
            })?;
        }
        Ok(router)
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
        ready(vm.run("main", vec![], VmDelegates::new(tools))),
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
        ready(cold.run("main", vec![], VmDelegates::new(tools.clone()))),
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
        ready(awaited.run("main", vec![], VmDelegates::new(tools.clone()))),
        StatementOutcome::Return(Value::Int(8))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let escaped = Vm::compile(
        Source::new("main.at", "flow main() -> int { return delayed(4) }"),
        &NoSources,
    )
    .expect("compile escaped call");
    assert!(matches!(
        ready(escaped.run("main", vec![], VmDelegates::new(tools))),
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
    let mut future = pin!(vm.run("main", vec![], VmDelegates::new(tools)));
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
        ready(vm.run("main", vec![], VmDelegates::new(tools.clone()))),
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
    assert!(matches!(
        tools.register("list.first", |_| async { Ok(Value::Unit) }),
        Err(ToolRegisterError::ReservedName(_))
    ));
    assert!(matches!(
        tools.register("head", |_| async { Ok(Value::Unit) }),
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
        ready(tools.invoke(
            ExpressionEffect::FileRef("file.txt".into()),
            (),
            &context()
        )),
        Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "file reference"
    ));
}

#[test]
fn catalog_projects_manual_and_spec_registrations_from_the_same_entries() {
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register_sync("manual", |_| Ok(Value::Unit))
        .expect("register manual tool");
    tools
        .register_with_spec(
            tool_spec("generated", atman_rt::ToolCallMode::Deferred),
            |_| async { Ok(Value::Unit) },
        )
        .expect("register generated tool");

    let catalog = tools.catalog();
    let generated = catalog.lookup("generated").expect("generated entry");
    assert_eq!(generated.name, "generated");
    assert_eq!(generated.mode, atman_rt::ToolCallMode::Deferred);
    assert_eq!(generated.spec.as_ref().unwrap().name, "generated");
    assert!(catalog.lookup("manual").unwrap().spec.is_none());

    assert!(matches!(
        tools.register_with_spec(
            tool_spec("wrong", atman_rt::ToolCallMode::Immediate),
            |_| async { Ok(Value::Unit) }
        ),
        Err(ToolRegisterError::ModeMismatch {
            name,
            expected: atman_rt::ToolCallMode::Deferred,
            actual: atman_rt::ToolCallMode::Immediate,
        }) if name == "wrong"
    ));
    assert!(!tools.contains("wrong"));
}

#[test]
fn mount_is_atomic_and_preserves_copy_on_write_snapshots() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register_sync("taken", |_| Ok(Value::Int(1)))
        .expect("register target tool");
    let old = tools.clone();

    assert!(matches!(
        tools.mount(TestFactory {
            names: &["added", "taken"],
            calls: Arc::clone(&calls),
        }),
        Err(ToolRegisterError::DuplicateName(name)) if name == "taken"
    ));
    assert_eq!(tools.catalog(), old.catalog());
    assert!(!tools.contains("added"));

    tools
        .mount(TestFactory {
            names: &["mounted"],
            calls: Arc::clone(&calls),
        })
        .expect("mount binding with P and E inferred from the target router");
    assert!(!old.contains("mounted"));
    let mounted_clone = tools.clone();
    assert!(matches!(
        ready(tools.dispatch(
            "mounted",
            ToolArgs {
                positional: vec![],
                named: vec![],
            }
        )),
        Value::Unit
    ));
    assert!(matches!(
        ready(mounted_clone.dispatch(
            "mounted",
            ToolArgs {
                positional: vec![],
                named: vec![],
            }
        )),
        Value::Unit
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
