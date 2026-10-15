use atman_rt::parse_file;
type FlowRunId = atman_rt::RunId<atman_runtime::event::AtmanUuid>;
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::stream::StreamFrame;
use atman_runtime::task_registry::{TaskFilter, TaskRegistry};
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use atman_runtime::tools::agent_ctrl::FlowRegistry;
use atman_runtime::tools::memory_stubs::RuleFetch;
use atman_runtime::{Event, Executor, FlowStatus, tools};
type Value = atman_rt::Value<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

mod common;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct ProbeTool {
    name: &'static str,
    tier: Tier,
    calls: Arc<AtomicUsize>,
}

fn instance_binding() -> atman_runtime::InstanceInvocation {
    let address = atman_runtime::flow_store::FlowRef {
        session_id: uuid::Uuid::new_v4().into(),
        instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
    };
    let mut state = atman_rt::FlowInstance::prepared(address);
    state.activate().unwrap();
    let lease = state
        .begin_turn(atman_rt::TurnId::now(), FlowRunId::now())
        .unwrap();
    let mut authority = EffectiveAuthority::root(&Default::default(), false, None);
    authority.permission_management = false;
    atman_runtime::InstanceInvocation::new(lease, authority, Default::default())
}

#[tokio::test]
async fn instance_execution_uses_explicit_authority_and_inline_calls_keep_instance_identity() {
    let executor = Executor::new();
    let binding = instance_binding();
    let context = Arc::new(atman_runtime::Session::open_instance_context(
        binding.lease.flow.clone(),
    ));
    let file = parse_file("flow helper(input: int) -> int { return input * 2 } flow main(input: int) -> int { return helper(input).await }").unwrap();
    let result = executor
        .run_with_invocation(
            &file,
            "main",
            vec![("input".into(), Value::Int(4))],
            atman_runtime::RootInvocation {
                session: Some(context.clone()),
                instance: Some(binding.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, Value::Int(8)));
    let identity = context
        .flow_registry
        .lookup_run(&binding.lease.run)
        .unwrap();
    assert_eq!(identity.instance.as_ref(), Some(&binding.lease.flow));
    assert_eq!(
        identity.invocation,
        atman_runtime::flow_authority::InvocationKind::Instance
    );
    assert_eq!(identity.effective_authority, binding.authority);
    assert!(!identity.effective_authority.permission_management);
    for event in executor.events.snapshot() {
        if let Event::FlowStart {
            run_id,
            parent_run_id: Some(_),
            ..
        } = event
        {
            let child = context.flow_registry.lookup_run(&run_id).unwrap();
            assert_eq!(child.instance.as_ref(), Some(&binding.lease.flow));
            assert!(!child.effective_authority.permission_management);
        }
    }
    assert_eq!(
        context.id().to_string(),
        binding.lease.flow.session_id.to_string()
    );
    assert!(
        context
            .flow_registry
            .lookup(&binding.lease.flow.instance_id.to_string())
            .is_ok()
    );
    assert!(context.flow_registry.lookup("root").is_err());
    assert!(
        executor
            .run_with_invocation(
                &file,
                "main",
                vec![("input".into(), Value::Int(4))],
                atman_runtime::RootInvocation {
                    session: Some(context),
                    instance: Some(binding),
                    ..Default::default()
                }
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("already used")
    );
}

#[tokio::test]
async fn instance_grants_cancellation_and_context_identity_are_enforced_before_effects() {
    let executor = Executor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    executor.tools.register(Arc::new(ProbeTool {
        name: "probe",
        tier: Tier::Zero,
        calls: calls.clone(),
    }));
    let file = parse_file("flow main() -> int { return probe() }").unwrap();
    let binding = instance_binding();
    let context = Arc::new(atman_runtime::Session::open_instance_context(
        binding.lease.flow.clone(),
    ));
    let mut restricted = binding.clone();
    restricted.authority.allowed_tiers[0] = false;
    let result = executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                session: Some(context.clone()),
                instance: Some(restricted),
                ..Default::default()
            },
        )
        .await;
    assert!(result.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let cancelled = instance_binding();
    cancelled.cancellation.cancel();
    let result = executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(cancelled),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(atman_runtime::RuntimeError::Cancelled(_))
    ));
    for invocation in [
        atman_runtime::RootInvocation {
            session: Some(context.clone()),
            ..Default::default()
        },
        atman_runtime::RootInvocation {
            session: Some(context.clone()),
            instance: Some(instance_binding()),
            ..Default::default()
        },
        atman_runtime::RootInvocation {
            turn_id: Some(atman_rt::TurnId::now()),
            instance: Some(instance_binding()),
            ..Default::default()
        },
    ] {
        assert!(
            executor
                .run_with_invocation(&file, "main", vec![], invocation)
                .await
                .is_err()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let other = Arc::new(atman_runtime::Session::open_instance_context(
        instance_binding().lease.flow,
    ));
    assert!(!Arc::ptr_eq(
        &context.messages_handle(),
        &other.messages_handle()
    ));
    assert!(!Arc::ptr_eq(&context.goal_control, &other.goal_control));
    assert!(!Arc::ptr_eq(&context.watch_hub, &other.watch_hub));
}

impl Tool for ProbeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn tier(&self) -> Tier {
        self.tier
    }

    fn requires_call_intent(&self) -> bool {
        false
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(Value::Int(1)) })
    }
}

#[tokio::test]
async fn denied_or_missing_tool_does_not_run_argument_effects() {
    let ex = Executor::new();
    let calls = Arc::new(AtomicUsize::new(0));
    for (name, tier) in [("side.effect", Tier::Zero), ("shell.exec", Tier::Four)] {
        ex.tools.register(Arc::new(ProbeTool {
            name,
            tier,
            calls: Arc::clone(&calls),
        }));
    }
    for (outer, expected) in [("shell.exec", "Tier 4"), ("missing.tool", "undefined tool")] {
        let source = format!("flow t() -> int {{ return {outer}(side.effect()) }}");
        let file = parse_file(&source).unwrap();
        let error = ex.run(&file, "t", vec![]).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn executor_runs_flow_and_emits_start_end() {
    let src = r#"flow t(n: Int) -> Int {
    return n + 1
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

    let out = ex
        .run(&file, "t", vec![("n".into(), Value::Int(4))])
        .await
        .unwrap();
    assert!(matches!(out, Value::Int(5)));

    let events = ex.events.snapshot();
    assert!(matches!(events[0], Event::FlowStart { .. }));
    assert!(matches!(events[1], Event::FlowGraph { .. }));
    match events.last() {
        Some(Event::FlowEnd { status, .. }) => assert!(matches!(status, FlowStatus::Ok)),
        other => panic!("expected FlowEnd last, got {other:?}"),
    }
}

#[tokio::test]
async fn loop_iteration_terminal_previews_reach_events_and_stream() {
    let file = parse_file(
        r#"flow t() -> int {
    n = 0
    loop {
        n = n + 1
        when n == 1 {
            continue
        }
        when n == 2 {
            break
        }
    }
    return n
}
"#,
    )
    .unwrap();
    let executor = Executor::new();
    let events = executor.events.clone();
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let mut frames = session.stream_subscribe();
    let turn_id = atman_rt::TurnId::now();
    session.begin_turn(atman_runtime::message::Message::user_text(
        turn_id.clone(),
        "run loop",
    ));

    let result = executor
        .run_in_turn(&file, "t", vec![], Some(turn_id), Some(session))
        .await
        .unwrap();
    assert!(matches!(result, Value::Int(2)));

    let event_previews = events
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            Event::FlowNodeEnd {
                node_id,
                output_preview: Some(preview),
                ..
            } if node_id.contains(".iter[")
                && node_id.ends_with(']')
                && (preview == "continue" || preview == "break") =>
            {
                Some(preview)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(event_previews, ["continue", "break"]);

    let stream_previews = std::iter::from_fn(|| frames.try_recv().ok())
        .filter_map(|frame| match frame {
            StreamFrame::FlowNodeEnd {
                node_id,
                output_preview: Some(preview),
                ..
            } if node_id.contains(".iter[")
                && node_id.ends_with(']')
                && (preview == "continue" || preview == "break") =>
            {
                Some(preview)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(stream_previews, ["continue", "break"]);
}

#[tokio::test]
async fn root_registration_failure_does_not_leave_running_task() {
    let file = parse_file("flow t() -> Int { return 1 }").unwrap();
    let run_id = FlowRunId::now();
    let flow_registry = Arc::new(FlowRegistry::new());
    flow_registry
        .register_root(
            "existing-session".into(),
            run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let tasks = TaskRegistry::new();
    let mut ex = Executor::new();
    ex.tool_ctx = ex
        .tool_ctx
        .clone()
        .with_flow_registry(flow_registry)
        .with_task_registry(tasks.clone());

    let error = ex
        .run_in_turn_with_run_id(&file, "t", vec![], None, None, Some(run_id))
        .await
        .unwrap_err();

    assert!(error.to_string().contains("already registered"));
    assert!(tasks.list(&TaskFilter::running()).is_empty());
    assert!(tasks.list(&TaskFilter::all()).is_empty());
}

#[tokio::test]
async fn executor_reports_err_status_on_failure() {
    let src = r#"flow t() -> Int {
    return missing
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    assert!(matches!(
        err,
        atman_runtime::RuntimeError::UndefinedVar(name) if name == "missing"
    ));
    let events = ex.events.snapshot();
    assert!(matches!(
        events.last(),
        Some(Event::FlowEnd {
            status: FlowStatus::Errored { .. },
            ..
        })
    ));
}

#[tokio::test]
async fn executor_rule_fetch_returns_content() {
    let src = r#"flow t() -> string {
    return rule.fetch("comment-discipline")
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);

    let rule = RuleFetch::new();
    rule.insert("comment-discipline", "only write why-comments")
        .await;
    ex.tools.register(Arc::new(rule));

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "only write why-comments"));
}

#[tokio::test]
async fn executor_runs_review_flow_with_mock_provider() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "claude-opus-4.7",
            "mock",
            8_192,
            None,
        )]))
        .await;
    let src = r#"flow review_code(file: path) -> Review {
    gather = rule.fetch(query: "none")
    primary = llm.call(
        model: "claude-opus-4.7",
        prompt: "review please",
        input: gather,
    )
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let ex = Executor::new();
    tools::register_tier_zero(&ex.tools);
    ex.providers
        .register(Arc::new(MockProvider::new("mock").with_model(
            "claude-opus-4.7",
            Value::Struct(vec![
                ("severity".into(), Value::Str("info".into())),
                ("issues".into(), Value::List(vec![])),
            ]),
        )));

    let out = ex
        .run(
            &file,
            "review_code",
            vec![(
                "file".into(),
                Value::Host(atman_runtime::AtmanPayload::Path("src/main.rs".into())),
            )],
        )
        .await
        .unwrap();
    if let Value::Struct(fields) = out {
        assert_eq!(fields[0].0, "severity");
        assert!(matches!(&fields[0].1, Value::Str(s) if s == "info"));
    } else {
        panic!("expected struct");
    }
}
