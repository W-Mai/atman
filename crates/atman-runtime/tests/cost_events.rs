mod common;

use atman_dsl::parse::parse_file;
use atman_runtime::event::LlmCallStatus;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Event, Value};

#[tokio::test]
async fn llm_call_event_records_wallclock_and_tokens() {
    let registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("mock-model", "mock", 8_192, None),
        common::model_for_provider("flaky", "flaky", 8_192, None),
    ]))
    .await;
    let src = r#"flow t() -> string {
    return llm.call(
        model: "mock-model",
        prompt: "hello world",
        cache: true,
    )
}
"#;
    let file = parse_file(src).unwrap();
    let runtime = common::TestRuntime::new(
        registry,
        MockProvider::new("mock").with_model("mock-model", Value::Str("response text".into())),
    );
    runtime.executor.run(&file, "t", vec![]).await.unwrap();

    let events = runtime.executor.events.snapshot();
    let call_event = events
        .iter()
        .find(|e| matches!(e, Event::LlmCall { .. }))
        .expect("expected LlmCall event");
    match call_event {
        Event::LlmCall {
            model,
            provider,
            context_plan_id,
            context_tokens,
            usage_source,
            context_call_purpose,
            context_call_identity,
            context_cache,
            assistant_tool_batch_width,
            usage,
            status,
            ..
        } => {
            assert_eq!(model, "mock-model");
            assert_eq!(provider, "mock");
            assert!(context_plan_id.is_some());
            let tokens = context_tokens.as_ref().expect("context token lanes");
            assert!(tokens.messages > 0);
            assert!(usage_source.is_some());
            assert_eq!(
                *context_call_purpose,
                Some(atman_runtime::ContextCallPurpose::General)
            );
            assert_eq!(
                context_call_identity
                    .as_ref()
                    .map(|identity| identity.scope),
                Some(atman_runtime::ContextCallScope::Root)
            );
            let cache = context_cache.as_ref().expect("context cache observation");
            assert_eq!(
                cache.reset_reason,
                Some(atman_runtime::ContextCacheResetReason::ColdStart)
            );
            assert!(cache.wire_prefix_bytes > 0);
            assert_eq!(cache.common_prefix_bytes, 0);
            assert_eq!(*assistant_tool_batch_width, Some(0));
            assert!(matches!(status, LlmCallStatus::Ok));
            assert!(usage.input > 0);
            assert!(usage.output > 0);
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn llm_call_event_records_retry_attempts() {
    let registry = common::ModelRegistryGuard::acquire(common::config([
        common::model_for_provider("mock-model", "mock", 8_192, None),
        common::model_for_provider("flaky", "flaky", 8_192, None),
    ]))
    .await;
    let src = r#"flow t() -> string {
    return llm.call(
        model: "flaky",
        prompt: "hi",
        retry: 2,
    )
}
"#;
    let file = parse_file(src).unwrap();
    let runtime = common::TestRuntime::new(
        registry,
        MockProvider::new("flaky").with_fallback(Value::Str("unreachable".into())),
    );
    common::register_provider(
        &runtime.executor,
        MockProvider::new("stable").with_model("stable", Value::Str("stable-ok".into())),
    );
    let out = runtime.executor.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s == "unreachable"));

    let events = runtime.executor.events.snapshot();
    let call_count = events
        .iter()
        .filter(|e| matches!(e, Event::LlmCall { .. }))
        .count();
    assert!(call_count >= 1);
}

#[tokio::test]
async fn call_costs_and_managed_window_observations_survive_replay_independently() {
    use atman_runtime::context_plan::{ContextCallPurpose as Purpose, ContextUsageKey};
    use atman_runtime::tool::{Tool, ToolArgs, ToolCtx, ToolRegistry};
    use atman_runtime::tools::{llm_call::LlmCallTool, llm_classify::LlmClassifyTool};
    use atman_runtime::{FlowRunId, Message, Session, TurnId};
    use std::sync::Arc;

    let _registry = common::ModelRegistryGuard::acquire(common::config(
        ["primary", "helper", "failure"]
            .map(|name| common::model_for_provider(name, "mock", 100_000, None)),
    ))
    .await;
    let directory = tempfile::tempdir().unwrap();
    let session = Arc::new(Session::open(directory.path()).unwrap());
    let turn_id = TurnId::now();
    session.begin_turn(Message::user_text(turn_id.clone(), "canonical task"));
    let registry = Arc::new(ToolRegistry::new());
    let providers = Arc::new(atman_runtime::provider::ProviderRegistry::new());
    providers.register(Arc::new(
        MockProvider::new("mock")
            .with_model("primary", Value::Str("primary reply".into()))
            .with_model("helper", Value::Str("yes".into())),
    ));
    let ctx = ToolCtx::new()
        .with_session_runtime(session.clone())
        .with_registry(registry.clone())
        .with_providers(providers)
        .with_anchors(Some(turn_id.clone()), Some(FlowRunId::now()), None);
    let mut primary = None;
    let mut first_prefix_bytes = 0;
    let mut explicit_prefix_bytes = None;
    let mut total_input = 0;
    let mut total_output = 0;

    let published = session.sink().published_seq();
    let invalid = LlmCallTool
        .call(
            ToolArgs {
                positional: Vec::new(),
                named: vec![
                    ("model".into(), Value::Str("primary".into())),
                    ("context".into(), Value::Str("session".into())),
                    ("messages".into(), Value::List(Vec::new())),
                ],
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(
        invalid
            .to_string()
            .contains("cannot specify both `messages:` and `context:`")
    );
    assert_eq!(session.sink().published_seq(), published);

    for (index, (model, mode, purpose)) in [
        ("primary", "session", Purpose::General),
        ("helper", "bare", Purpose::General),
        ("helper", "bare", Purpose::General),
        ("primary", "override", Purpose::General),
        ("helper", "session", Purpose::Classification),
        ("helper", "bare", Purpose::Classification),
        ("failure", "override", Purpose::General),
        ("failure", "bare", Purpose::General),
        ("primary", "session", Purpose::General),
    ]
    .into_iter()
    .enumerate()
    {
        let mut named = vec![
            ("model".into(), Value::Str(model.into())),
            ("cache".into(), Value::Bool(true)),
            (
                "context".into(),
                Value::Str(if mode == "session" { "session" } else { "none" }.into()),
            ),
        ];
        match mode {
            "bare" => named.push(("prompt".into(), Value::Str("separate input".into()))),
            "override" => named.push((
                "messages".into(),
                Value::List(vec![Value::Message(Message::user_text(
                    turn_id.clone(),
                    "explicit input",
                ))]),
            )),
            _ => {}
        }
        if purpose == Purpose::Classification && mode != "bare" {
            named.push(("prompt".into(), Value::Str("classify the input".into())));
        }
        let args = ToolArgs {
            positional: Vec::new(),
            named,
        };
        let tool: &dyn Tool = if purpose == Purpose::Classification {
            &LlmClassifyTool
        } else {
            &LlmCallTool
        };
        let result = tool.call(args, &ctx).await;
        assert_eq!(result.is_err(), model == "failure", "{mode}: {result:?}");

        let events = session.sink().snapshot_envelopes();
        let calls: Vec<_> = events
            .iter()
            .filter_map(|envelope| {
                if let Event::LlmCall { .. } = &envelope.event {
                    Some(&envelope.event)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(calls.len(), index + 1);
        let Event::LlmCall {
            managed_context,
            context_plan_id,
            context_call_identity,
            context_cache,
            usage,
            ..
        } = calls.last().unwrap()
        else {
            unreachable!()
        };
        assert_eq!(*managed_context, Some(mode == "session"));
        if model == "helper" && mode == "bare" && purpose == Purpose::General {
            let cache = context_cache.as_ref().unwrap();
            if let Some(previous) = explicit_prefix_bytes.replace(cache.wire_prefix_bytes) {
                assert_eq!(cache.reset_reason, None);
                assert_eq!(cache.common_prefix_bytes, previous);
            }
        }
        total_input += usage.prompt_input();
        total_output += usage.output;
        let key = ContextUsageKey {
            provider: "mock".into(),
            model: "primary".into(),
            call_purpose: Purpose::General,
            call_identity: context_call_identity.clone().unwrap(),
        };
        let current = session.last_context_usage(&key).unwrap();
        if model == "primary" && mode == "session" {
            assert_eq!(Some(&current.plan_id), context_plan_id.as_ref());
            if index == 0 {
                first_prefix_bytes = context_cache.as_ref().unwrap().wire_prefix_bytes;
            } else {
                let cache = context_cache.as_ref().unwrap();
                assert_eq!(cache.reset_reason, None);
                assert!(cache.common_prefix_bytes >= first_prefix_bytes);
            }
            primary = Some(current.plan_id.clone());
        }
        assert_eq!(Some(current.plan_id), primary);
        assert_eq!(session.last_input_tokens(), current.usage.prompt_input());
        let live = session.subscribe_context().borrow().clone();
        assert_eq!(live.model, "primary");
        assert_eq!(live.tokens_in, total_input);
        assert_eq!(live.tokens_out, total_output);
        assert_eq!(
            live.usage_buckets
                .iter()
                .map(|bucket| bucket.calls)
                .sum::<u64>(),
            index as u64 + 1
        );

        let durable = session.flush_writer().await.unwrap();
        assert!(durable.seq >= session.sink().published_seq());
        let replay = atman_runtime::event_log::replay::SessionReplay::from_path(
            &session.dir().join("events.jsonl"),
            None,
        )
        .unwrap()
        .context;
        assert_eq!(replay.model, live.model);
        assert_eq!(replay.provider, live.provider);
        assert_eq!(replay.tokens_in, live.tokens_in);
        assert_eq!(replay.tokens_out, live.tokens_out);
        assert_eq!(replay.usage_buckets, live.usage_buckets);
        assert_eq!(replay.last_ttft_ms, live.last_ttft_ms);
        assert_eq!(replay.last_tokens_per_sec, live.last_tokens_per_sec);
    }
    session.end_turn(&turn_id);
    let live = session.subscribe_context().borrow().clone();
    let id = session.id().to_string();
    assert!(session.flush_writer().await.unwrap().seq >= session.sink().published_seq());
    drop(ctx);
    drop(session);
    let restored = Session::open_existing(directory.path(), &id).unwrap();
    let restored = restored.subscribe_context().borrow().clone();
    assert_eq!(restored.model, live.model);
    assert_eq!(restored.provider, live.provider);
    assert_eq!(restored.tokens_in, live.tokens_in);
    assert_eq!(restored.tokens_out, live.tokens_out);
    assert_eq!(restored.usage_buckets, live.usage_buckets);
}
