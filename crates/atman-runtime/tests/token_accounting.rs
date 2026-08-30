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
        "root-provider",
        "root-model",
        root_plan.clone(),
        ContextCallPurpose::General,
        root_identity,
        &TokenUsage {
            input: 40,
            cached_input: 60,
            ..Default::default()
        },
        None,
        None,
    );
    session.record_context_plan_call(
        "child-provider",
        "child-model",
        ContextPlanId::now(),
        ContextCallPurpose::Classification,
        child_identity,
        &TokenUsage {
            input: 1_000,
            ..Default::default()
        },
        None,
        None,
    );

    assert_eq!(session.last_input_tokens(), 100);
    assert_eq!(session.last_model(), "root-model");
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
}
