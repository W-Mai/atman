use atman_runtime::{
    ContextCallIdentity, ContextCallPurpose, ContextCallScope, ContextPlanId, ContextUsageKey,
    Session, TokenUsage,
};

#[tokio::test]
async fn cumulative_input_tokens_accumulates_across_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    assert_eq!(session.cumulative_input_tokens(), 0);
    session.record_llm_call("claude-opus-4.7", 500, 100, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 500);
    session.record_llm_call("claude-opus-4.7", 250, 40, 0, 0, None, None);
    assert_eq!(session.cumulative_input_tokens(), 750);
    assert_eq!(session.last_model(), "claude-opus-4.7");
    session.reset_input_tokens_to(120);
    assert_eq!(session.cumulative_input_tokens(), 120);
}

#[tokio::test]
async fn scoped_plan_usage_does_not_replace_the_root_model_window() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let session_id = session.id().to_string();
    let root_identity = ContextCallIdentity {
        scope: ContextCallScope::Root,
        session_id: Some(session_id.clone()),
        flow_run_id: None,
    };
    let child_identity = ContextCallIdentity {
        scope: ContextCallScope::Child,
        session_id: Some(session_id),
        flow_run_id: Some(atman_runtime::FlowRunId::now()),
    };
    let root_key = ContextUsageKey {
        provider: "root-provider".into(),
        model: "root-model".into(),
        call_purpose: ContextCallPurpose::General,
        call_identity: root_identity.clone(),
    };
    let child_key = ContextUsageKey {
        provider: "child-provider".into(),
        model: "child-model".into(),
        call_purpose: ContextCallPurpose::Classification,
        call_identity: child_identity.clone(),
    };

    let root_plan = ContextPlanId::now();
    session.record_context_plan_call(
        Some(&session.context()),
        "root-provider",
        "root-model",
        Some(root_plan.clone()),
        ContextCallPurpose::General,
        root_identity,
        &TokenUsage {
            input: 40,
            cached_input: 60,
            cache_write: 20,
            ..Default::default()
        },
        None,
        None,
    );
    session.record_context_plan_call(
        Some(&session.context()),
        "child-provider",
        "child-model",
        Some(ContextPlanId::now()),
        ContextCallPurpose::Classification,
        child_identity,
        &TokenUsage {
            input: 1_000,
            ..Default::default()
        },
        None,
        None,
    );

    assert_eq!(session.last_input_tokens(), 120);
    assert_eq!(session.last_model(), "root-model");
    let snap = session.subscribe_context().borrow().clone();
    assert_eq!(snap.provider, "root-provider");
    assert_eq!(snap.usage_buckets.len(), 2);
    let primary = snap.primary_usage().expect("root usage bucket");
    assert_eq!(primary.tokens_in, 120);
    assert_eq!(primary.cache_read, 60);
    assert_eq!(primary.cache_write, 20);
    assert_eq!(primary.calls, 1);
    assert_eq!(
        snap.usage_buckets
            .iter()
            .find(|bucket| bucket.call_scope == ContextCallScope::Child)
            .expect("child usage bucket")
            .tokens_in,
        1_000
    );
    assert_eq!(
        session.last_context_usage(&root_key).unwrap().plan_id,
        root_plan
    );
    assert_eq!(
        session
            .last_context_usage(&child_key)
            .unwrap()
            .window_input_tokens(),
        1_000
    );

    let head = session.subscribe_context().borrow().clone();
    for purpose in [
        ContextCallPurpose::General,
        ContextCallPurpose::Classification,
    ] {
        let other = atman_runtime::context_state::ContextState::new(Vec::new(), None);
        let plan = ContextPlanId::now();
        let key = ContextUsageKey {
            provider: "other-provider".into(),
            model: "other-model".into(),
            call_purpose: purpose,
            call_identity: root_key.call_identity.clone(),
        };
        session.record_context_plan_call(
            Some(&other),
            &key.provider,
            &key.model,
            Some(plan.clone()),
            purpose,
            key.call_identity.clone(),
            &TokenUsage {
                input: 200,
                output: 50,
                ..Default::default()
            },
            Some(99),
            Some(12.0),
        );
        assert_eq!(other.last_usage(&key).unwrap().plan_id, plan);
        assert!(session.last_context_usage(&key).is_none());
        assert_eq!(
            session.last_context_usage(&root_key).unwrap().plan_id,
            root_plan
        );
    }
    let updated = session.subscribe_context().borrow().clone();
    assert_eq!(session.last_input_tokens(), 120);
    assert_eq!(updated.model, head.model);
    assert_eq!(updated.provider, head.provider);
    assert_eq!(updated.window_tokens, head.window_tokens);
    assert_eq!(updated.last_ttft_ms, head.last_ttft_ms);
    assert_eq!(updated.last_tokens_per_sec, head.last_tokens_per_sec);
    assert_eq!(updated.tokens_in, head.tokens_in + 400);
    assert_eq!(updated.tokens_out, head.tokens_out + 100);
    assert_eq!(updated.primary_usage().unwrap().calls, 1);
}
