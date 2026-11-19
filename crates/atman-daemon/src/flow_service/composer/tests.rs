use super::*;

use atman_proto::{JsonRpcRequest, methods::FLOW_COMPOSER};
use atman_runtime::{
    config_hub::ConfigHub,
    flow_runtime::FlowWorker,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, SessionConfiguration,
        SessionDescriptor,
    },
    source_program::RevisionBundle,
};
use serde_json::json;
use uuid::Uuid;

fn query(flow: &FlowRef) -> JsonRpcRequest {
    JsonRpcRequest::new(
        1,
        FLOW_COMPOSER,
        json!({"flow": super::super::address(flow)}),
    )
}

async fn attached(state: &DaemonState, key: &str, mode: TrustMode) -> (FlowWorker, ConfigHub) {
    let config = state.data_dir().join(key);
    std::fs::create_dir_all(&config).unwrap();
    let hub = ConfigHub::from_config_dir(config);
    hub.set_trust_config(&TrustConfig {
        mode,
        theme: Theme::Weather,
        ..Default::default()
    })
    .unwrap();
    hub.models()
        .replace_config_text(&format!(
            r#"
[providers.vendor]
kind = "openai-compat"
api_key = "test-private-credential"
base_url = "https://gateway.example/v1"
reasoning_format = "reasoning-effort"

[models.{key}]
model = "fixture/{key}"
provider = "vendor"
reasoning = "high"
reasoning_efforts = ["low", "high"]

[alias.smart]
model = "{key}"
"#
        ))
        .unwrap();
    let host = state.runtime_host();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            state.data_dir(),
            SessionDescriptor {
                id,
                title: key.into(),
                project: None,
                configuration: SessionConfiguration::Atman(hub.source()),
            },
        )
        .await
        .unwrap();
    let worker = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: FlowInstanceId::now(),
            },
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() { }".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .await
        .unwrap();
    (worker, hub)
}

#[tokio::test]
async fn composer_reads_original_target_settings_without_opening_or_mutating_an_owner() {
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(directory.path().join("atman")));
    let missing = FlowRef {
        session_id: Uuid::now_v7().into(),
        instance_id: FlowInstanceId::now(),
    };
    let reply = crate::dispatch(state.clone(), query(&missing)).await;
    assert_eq!(reply.error.unwrap().code, JsonRpcError::APPLICATION_ERROR);
    assert!(state.session_service.get().is_none());
    assert!(!state.data_dir().exists());

    let (first, first_hub) = attached(&state, "source_alpha", TrustMode::Eager).await;
    let (second, second_hub) = attached(&state, "source_beta", TrustMode::Calm).await;
    let first_before = first.status().await.unwrap().cursor;
    let second_before = second.status().await.unwrap().cursor;
    let reply = crate::dispatch(state.clone(), query(first.address())).await;
    let partial: FlowComposerResponse = serde_json::from_value(reply.result.unwrap()).unwrap();
    assert_eq!(partial.model, ComposerModelState::Pending);
    assert!(first.context().model_registry().is_none());

    first
        .context()
        .bind_model_registry(first_hub.models().clone())
        .unwrap();
    second
        .context()
        .bind_model_registry(second_hub.models().clone())
        .unwrap();
    let reply = crate::dispatch(state.clone(), query(first.address())).await;
    let value = reply.result.unwrap();
    let encoded = value.to_string();
    assert!(!encoded.contains("test-private-credential"));
    assert!(!encoded.contains("gateway.example"));
    assert!(!encoded.contains(directory.path().to_string_lossy().as_ref()));
    let alpha: FlowComposerResponse = serde_json::from_value(value).unwrap();
    assert_eq!(alpha.flow, super::super::address(first.address()));
    assert_ne!(alpha.revision, partial.revision);
    assert_eq!(alpha.trust.mode, ComposerTrustMode::Eager);
    assert_eq!(
        alpha.trust.name,
        Theme::Weather.display(TrustMode::Eager).name
    );
    assert_eq!(
        alpha.trust.escalation.unwrap().policy,
        ComposerEscalationPolicy::Ask
    );
    let ComposerModelState::Ready {
        model,
        context_budget,
        reasoning,
        ..
    } = alpha.model
    else {
        panic!("source model unavailable");
    };
    assert_eq!(model, "source_alpha");
    assert_eq!(context_budget, None);
    assert!(reasoning.available);
    assert_eq!(reasoning.requested, None);
    assert_eq!(
        reasoning.effective,
        Some(ComposerReasoning::Effort {
            effort: "high".into(),
            execution_mode: None
        })
    );

    let user = json!({"flow": super::super::address(second.address()), "reasoning": {"type": "effort", "effort": "low"}});
    let reply = crate::dispatch(state.clone(), JsonRpcRequest::new(1, FLOW_COMPOSER, user)).await;
    let beta: FlowComposerResponse = serde_json::from_value(reply.result.unwrap()).unwrap();
    assert_ne!(alpha.revision, beta.revision);
    assert_eq!(beta.trust.mode, ComposerTrustMode::Calm);
    assert_eq!(beta.trust.escalation, None);
    let ComposerModelState::Ready {
        model, reasoning, ..
    } = beta.model
    else {
        panic!("second source model unavailable");
    };
    assert_eq!(model, "source_beta");
    assert_eq!(reasoning.requested, reasoning.effective);
    assert_ne!(reasoning.requested, Some(reasoning.configured));
    assert_eq!(first.status().await.unwrap().cursor, first_before);
    assert_eq!(second.status().await.unwrap().cursor, second_before);

    let reply = crate::dispatch_as(state.clone(), query(first.address()), "foreign").await;
    assert_eq!(reply.error.unwrap().code, JsonRpcError::APPLICATION_ERROR);
    state.shutdown().await.unwrap();
}
