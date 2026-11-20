use atman_runtime::{
    config_hub::ConfigHub,
    event_log::journal::Journal,
    flow_runtime::host::{RuntimeHost, SessionHandle},
    flow_store::{SessionConfiguration, SessionDescriptor},
    provider::{ReasoningEffort, ReasoningSelection},
    tool::Tier,
    trust::{EscalationPolicy, PolicyAction, TrustMode},
    user_input::{
        preference_authority::{PreferenceOutcome, PreferenceRejection, read_preference_outcome},
        preferences::{ComposerReasoning, PreferenceEdit},
    },
};
use std::path::Path;
use uuid::Uuid;

async fn create(host: &RuntimeHost, data: &Path, config: &ConfigHub) -> SessionHandle {
    host.create_session(
        data,
        SessionDescriptor {
            id: Uuid::now_v7(),
            title: "test".into(),
            project: None,
            configuration: SessionConfiguration::Atman(config.source()),
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn explicit_input_preferences_survive_restart_and_other_fields_follow_latest_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = ConfigHub::from_config_dir(temp.path().join("config"));
    let host = RuntimeHost::default();
    let a = create(&host, &data, &config).await;
    let b = create(&host, &data, &config).await;
    a.set_input_preference(PreferenceEdit::TrustMode(TrustMode::Eager))
        .await
        .unwrap();
    a.set_input_preference(PreferenceEdit::EagerTier {
        tier: Tier::Two,
        action: None,
    })
    .await
    .unwrap();
    b.set_input_preference(PreferenceEdit::TrustMode(TrustMode::Calm))
        .await
        .unwrap();
    b.set_input_preference(PreferenceEdit::Escalation(EscalationPolicy::Allow))
        .await
        .unwrap();
    b.set_input_preference(PreferenceEdit::EagerTier {
        tier: Tier::Two,
        action: Some(PolicyAction::Auto),
    })
    .await
    .unwrap();
    let high = ComposerReasoning::Selection(ReasoningSelection::Effort {
        effort: ReasoningEffort::High,
        execution_mode: None,
    });
    b.set_input_preference(PreferenceEdit::Reasoning(high.clone()))
        .await
        .unwrap();
    assert_eq!(
        a.subscribe_input_preferences().borrow().trust.escalation,
        EscalationPolicy::Ask
    );
    let old = a.id();
    drop(a);
    drop(b);
    host.shutdown().await.unwrap();

    let host = RuntimeHost::default();
    let a = host.open_session(&data, old).await.unwrap();
    a.refresh_input_preferences().await.unwrap();
    let observed = a.subscribe_input_preferences();
    let settings = observed.borrow().clone();
    assert_eq!(settings.trust.mode, TrustMode::Eager);
    assert_eq!(settings.trust.escalation, EscalationPolicy::Allow);
    assert_eq!(settings.trust.tiers.eager.tier2, None);
    assert_eq!(settings.reasoning, high);

    let new = create(&host, &data, &config).await;
    new.refresh_input_preferences().await.unwrap();
    let fresh = new.subscribe_input_preferences().borrow().clone();
    assert_eq!(fresh.trust.mode, TrustMode::Calm);
    assert_eq!(fresh.trust.tiers.eager.tier2, Some(PolicyAction::Auto));
    assert_eq!(fresh.reasoning, high);

    let other = ConfigHub::from_config_dir(temp.path().join("other"));
    let separate = create(&host, &data, &other).await;
    separate.refresh_input_preferences().await.unwrap();
    let isolated = separate.subscribe_input_preferences().borrow().clone();
    assert_eq!(isolated.reasoning, ComposerReasoning::ModelDefault);
    assert_eq!(isolated.trust.mode, TrustMode::Steady);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn interrupted_preference_saves_recover_only_matching_global_receipts() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = ConfigHub::from_config_dir(temp.path().join("config"));
    let host = RuntimeHost::default();
    let accepted = create(&host, &data, &config).await.id();
    let interrupted = create(&host, &data, &config).await.id();
    host.shutdown().await.unwrap();

    let receipt = Uuid::now_v7();
    for (session, request) in [(accepted, receipt), (interrupted, Uuid::now_v7())] {
        let path = data
            .join("runtime-v1/sessions")
            .join(session.to_string())
            .join("events.jsonl");
        let (mut journal, _) =
            Journal::open::<serde_json::Value>(&path, session, |_, _| Ok(())).unwrap();
        journal
            .commit(&[serde_json::json!({
                "type": "input_preferences_prepared",
                "data": { "request": request, "preferences": { "mode": "eager" } },
            })])
            .unwrap();
    }
    std::fs::create_dir_all(config.config_dir()).unwrap();
    let path = config.config_dir().join("config.toml");
    // A newer global default can already exist by the time the old owner is reopened.
    std::fs::write(
        &path,
        format!("[trust]\nmode = 'calm'\n[__atman_input_preference_receipts]\n{receipt} = true\n"),
    )
    .unwrap();

    let host = RuntimeHost::default();
    for (session, mode) in [(accepted, TrustMode::Eager), (interrupted, TrustMode::Calm)] {
        let session = host.open_session(&data, session).await.unwrap();
        session.refresh_input_preferences().await.unwrap();
        assert_eq!(
            session.subscribe_input_preferences().borrow().trust.mode,
            mode
        );
    }
    assert_eq!(config.trust_config().unwrap().mode, TrustMode::Calm);
    assert!(
        !std::fs::read_to_string(path)
            .unwrap()
            .contains(&receipt.to_string())
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_client_request_id_cannot_settle_a_different_sessions_prepared_write() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = ConfigHub::from_config_dir(temp.path().join("config"));
    let host = RuntimeHost::default();
    let sessions = [
        create(&host, &data, &config).await.id(),
        create(&host, &data, &config).await.id(),
    ];
    host.shutdown().await.unwrap();

    let client = Uuid::new_v4();
    let transactions = [Uuid::new_v4(), Uuid::new_v4()];
    let mut flows = Vec::new();
    for (session, transaction) in sessions.into_iter().zip(transactions) {
        let flow = atman_runtime::flow_store::FlowRef {
            session_id: session.into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let path = data
            .join("runtime-v1/sessions")
            .join(session.to_string())
            .join("events.jsonl");
        let (mut journal, _) =
            Journal::open::<serde_json::Value>(&path, session, |_, _| Ok(())).unwrap();
        journal
            .commit(&[serde_json::json!({
                "type": "input_preferences_prepared",
                "data": {
                    "request": transaction,
                    "preferences": { "mode": "eager" },
                    "intent": [client, {
                        "flow": flow.instance_id,
                        "expected": "recorded-source-revision",
                        "edit": { "trust_mode": "eager" },
                    }, Uuid::new_v4()],
                },
            })])
            .unwrap();
        drop(journal);
        assert_eq!(
            read_preference_outcome(data.clone(), flow.clone(), client)
                .await
                .unwrap(),
            PreferenceOutcome::Pending
        );
        flows.push(flow);
    }
    std::fs::create_dir_all(config.config_dir()).unwrap();
    let path = config.config_dir().join("config.toml");
    std::fs::write(
        &path,
        format!(
            "[trust]\nmode = 'calm'\n[__atman_input_preference_receipts]\n{} = true\n",
            transactions[0]
        ),
    )
    .unwrap();

    let host = RuntimeHost::default();
    let expected = [
        (TrustMode::Eager, PreferenceOutcome::Applied),
        (
            TrustMode::Calm,
            PreferenceOutcome::Rejected(PreferenceRejection::Storage),
        ),
    ];
    for (flow, (mode, outcome)) in flows.into_iter().zip(expected) {
        let handle = host.open_session(&data, flow.session_id.0).await.unwrap();
        handle.refresh_input_preferences().await.unwrap();
        assert_eq!(
            handle.subscribe_input_preferences().borrow().trust.mode,
            mode
        );
        assert_eq!(
            handle.input_preference_outcome(&flow.instance_id, client),
            outcome
        );
    }
    assert_eq!(config.trust_config().unwrap().mode, TrustMode::Calm);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_global_model_update_restores_the_session_override() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = ConfigHub::from_config_dir(temp.path().join("config"));
    let host = RuntimeHost::default();
    let session = create(&host, &data, &config).await;
    let initial = session.snapshot().await.unwrap().catalog.input_preferences;
    let path = config.config_dir().join("config.toml");
    std::fs::create_dir_all(config.config_dir()).unwrap();
    // Make the global edit fail after the Session writes its candidate.
    std::fs::write(&path, "[models.smart]\nmodel = 'conflict'\n").unwrap();
    assert!(
        session
            .set_input_preference(PreferenceEdit::Model("other".into()))
            .await
            .is_err()
    );
    assert_eq!(
        session.snapshot().await.unwrap().catalog.input_preferences,
        initial
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "[models.smart]\nmodel = 'conflict'\n"
    );
    host.shutdown().await.unwrap();
}
