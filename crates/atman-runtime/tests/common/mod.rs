#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use atman_runtime::Executor;
use atman_runtime::model_registry::{ModelConfig, ModelEntry};
use atman_runtime::provider::Provider;
use atman_runtime::providers::mock::MockProvider;

static MODEL_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static SYNC_MODEL_REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct ModelRegistryGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

pub struct SyncModelRegistryGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl SyncModelRegistryGuard {
    pub fn acquire(config: ModelConfig) -> Self {
        let lock = SYNC_MODEL_REGISTRY_LOCK.lock().unwrap();
        assert!(
            atman_runtime::model_registry::all_model_entries()
                .into_iter()
                .all(|(_, entry)| !entry.discovered),
            "test model registry contains discovered entries that set_model_config cannot replace"
        );
        atman_runtime::model_registry::set_model_config(config);
        Self { _lock: lock }
    }

    pub fn mock(model: &str) -> Self {
        Self::acquire(config([mock_model(model)]))
    }
}

impl ModelRegistryGuard {
    pub async fn acquire(config: ModelConfig) -> Self {
        let lock = MODEL_REGISTRY_LOCK.lock().await;
        assert!(
            atman_runtime::model_registry::all_model_entries()
                .into_iter()
                .all(|(_, entry)| !entry.discovered),
            "test model registry contains discovered entries that set_model_config cannot replace"
        );
        atman_runtime::model_registry::set_model_config(config);
        Self { _lock: lock }
    }

    pub async fn mock(model: &str) -> Self {
        Self::acquire(config([mock_model(model)])).await
    }
}

pub fn mock_model(name: &str) -> (String, ModelEntry) {
    model(name, 8_192, None)
}

fn model(
    name: &str,
    context_budget: u64,
    compact_threshold_ratio: Option<f64>,
) -> (String, ModelEntry) {
    (
        name.to_string(),
        ModelEntry {
            model: name.to_string(),
            provider: Some(name.to_string()),
            context_budget: Some(context_budget),
            compact_threshold_ratio,
            ..Default::default()
        },
    )
}

pub fn model_for_provider(
    name: &str,
    provider: &str,
    context_budget: u64,
    compact_threshold_ratio: Option<f64>,
) -> (String, ModelEntry) {
    let (_, mut entry) = model(name, context_budget, compact_threshold_ratio);
    entry.provider = Some(provider.to_string());
    (name.to_string(), entry)
}

pub fn config<I>(models: I) -> ModelConfig
where
    I: IntoIterator<Item = (String, ModelEntry)>,
{
    ModelConfig {
        models: models.into_iter().collect::<HashMap<_, _>>(),
        providers: HashMap::new(),
        aliases: HashMap::new(),
    }
}

pub struct TestRuntime {
    pub executor: Executor,
    pub _registry: ModelRegistryGuard,
}

impl TestRuntime {
    pub fn new(registry: ModelRegistryGuard, provider: MockProvider) -> Self {
        let executor = Executor::new();
        executor.providers.register(Arc::new(provider));
        Self {
            executor,
            _registry: registry,
        }
    }
}

pub fn executor() -> Executor {
    Executor::new()
}

pub fn register_provider<P>(executor: &Executor, provider: P)
where
    P: Provider + 'static,
{
    executor.providers.register(Arc::new(provider));
}
