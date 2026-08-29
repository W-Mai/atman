use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::auth_store::{ProviderKind, StoredProvider};
use crate::config_hub::{AuthModelCacheCommit, ConfigError, ConfigHub};
use crate::model_registry::{
    CatalogDelta, CatalogError, PreparedProviderCatalog, ProviderDescriptor,
    commit_prepared_provider_catalog, prepare_provider_catalog, provider_catalog_namespace,
    remove_provider_catalog, shortest_unique_provider_id,
};
use crate::provider::{
    ModelDiscoveryError, Provider, ProviderRegistry, ReasoningWireProfile, WeakProviderRegistry,
};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProviderLifecycleError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Discovery(#[from] ModelDiscoveryError),
    #[error("auth provider `{id}` does not exist")]
    ProviderNotFound { id: String },
    #[error("auth provider `{id}` is disabled")]
    ProviderDisabled { id: String },
    #[error("auth provider `{id}` has no live provider")]
    LiveProviderMissing { id: String },
    #[error("live provider name `{actual}` does not match auth provider id `{expected}`")]
    LiveProviderNameMismatch { expected: String, actual: String },
    #[error("auth provider `{id}` changed while the operation was running")]
    Stale { id: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderStateChange {
    pub auth_changed: bool,
    pub live_changed: bool,
    pub catalog_changed: bool,
}

#[derive(Clone)]
pub struct ProviderLifecycle {
    hub: ConfigHub,
    providers: ProviderRegistry,
    state: Arc<Mutex<LifecycleState>>,
}

#[derive(Default)]
struct LifecycleState {
    generations: HashMap<String, u64>,
    operation_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    live_providers: HashMap<String, Arc<dyn Provider>>,
    registries: Vec<WeakProviderRegistry>,
}

static LIFECYCLE_COORDINATORS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<LifecycleState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone)]
struct ProviderIdentity {
    id: String,
    name: String,
    kind: ProviderKind,
    enabled: bool,
}

impl ProviderIdentity {
    fn new(provider: &StoredProvider) -> Self {
        Self {
            id: provider.id.clone(),
            name: provider.name.clone(),
            kind: provider.kind.clone(),
            enabled: provider.enabled,
        }
    }

    fn matches(&self, provider: &StoredProvider) -> bool {
        self.id == provider.id
            && self.name == provider.name
            && self.kind == provider.kind
            && self.enabled == provider.enabled
    }
}

impl ProviderLifecycle {
    pub fn new(hub: ConfigHub, providers: ProviderRegistry) -> Self {
        let state = shared_lifecycle_state(&hub);
        {
            let mut shared = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut registered = false;
            shared.registries.retain(|registry| {
                let Some(registry) = registry.upgrade() else {
                    return false;
                };
                registered |= registry.shares_storage_with(&providers);
                true
            });
            if !registered {
                for (provider_id, provider) in &shared.live_providers {
                    drop(providers.register_named(provider_id.clone(), provider.clone()));
                }
                shared.registries.push(providers.downgrade());
            }
        }
        Self {
            hub,
            providers,
            state,
        }
    }

    pub fn config_hub(&self) -> &ConfigHub {
        &self.hub
    }

    pub fn provider_registry(&self) -> &ProviderRegistry {
        &self.providers
    }

    pub async fn install_provider(
        &self,
        provider_record: StoredProvider,
        live_provider: Arc<dyn Provider>,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        self.validate_live_name(&provider_record.id, &live_provider)?;
        if !provider_record.enabled {
            return Err(ProviderLifecycleError::ProviderDisabled {
                id: provider_record.id,
            });
        }
        let provider_id = provider_record.id.clone();
        let operation_lock = self.operation_lock(&provider_id);
        let _operation = operation_lock.lock().await;
        let generation = self.generation(&provider_id);
        let models = live_provider.try_discover_models().await?;

        let mut state = self.lock_state();
        self.ensure_generation(&state, &provider_id, generation)?;
        let mut store = self.hub.load_auth()?;
        if store
            .providers
            .iter()
            .any(|provider| provider.id == provider_id)
        {
            return Err(ConfigError::Invalid(format!(
                "auth provider id {provider_id:?} already exists"
            ))
            .into());
        }
        store.providers.push(provider_record.clone());
        let descriptor = self.provider_descriptor(&provider_record, &store.providers)?;
        let prepared = prepare_provider_catalog(descriptor, &models)?;
        let namespace = prepared.namespace().to_string();
        let (delta, replaced_providers) = self
            .hub
            .add_auth_provider_with_model_cache_details_and_then(
                provider_record,
                &namespace,
                chrono::Utc::now().timestamp(),
                &models,
                || {
                    let replaced_providers =
                        Self::register_live_provider(&mut state, &provider_id, live_provider);
                    (
                        commit_prepared_provider_catalog(prepared),
                        replaced_providers,
                    )
                },
            )?;
        Self::bump_generation(&mut state, &provider_id);
        drop(state);
        drop(replaced_providers);
        Ok(delta)
    }

    pub async fn refresh_models(
        &self,
        provider_id: &str,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        let operation_lock = self.operation_lock(provider_id);
        let _operation = operation_lock.lock().await;
        let (generation, snapshot, catalog_snapshot, live_provider) = {
            let state = self.lock_state();
            let (provider, catalog_snapshot) = self
                .hub
                .load_or_create_auth_provider_catalog_state(provider_id)?
                .ok_or_else(|| ProviderLifecycleError::ProviderNotFound {
                    id: provider_id.to_string(),
                })?;
            let provider = Self::enabled_provider(std::slice::from_ref(&provider), provider_id)?;
            let live_provider =
                state
                    .live_providers
                    .get(provider_id)
                    .cloned()
                    .ok_or_else(|| ProviderLifecycleError::LiveProviderMissing {
                        id: provider_id.to_string(),
                    })?;
            (
                Self::generation_in(&state, provider_id),
                ProviderIdentity::new(provider),
                catalog_snapshot,
                live_provider,
            )
        };

        let models = live_provider.try_discover_models().await?;
        let state = self.lock_state();
        self.ensure_generation(&state, provider_id, generation)?;
        let store = self.hub.load_auth()?;
        let current = Self::enabled_provider(&store.providers, provider_id).map_err(|_| {
            ProviderLifecycleError::Stale {
                id: provider_id.to_string(),
            }
        })?;
        let current_live =
            state
                .live_providers
                .get(provider_id)
                .ok_or_else(|| ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                })?;
        if !snapshot.matches(current) || !Arc::ptr_eq(&live_provider, current_live) {
            return Err(ProviderLifecycleError::Stale {
                id: provider_id.to_string(),
            });
        }

        let descriptor = self.provider_descriptor(current, &store.providers)?;
        let prepared = prepare_provider_catalog(descriptor, &models)?;
        self.commit_cache_and_catalog_if_current(
            current,
            &catalog_snapshot,
            chrono::Utc::now().timestamp(),
            &models,
            prepared,
        )
    }

    pub fn disable_provider(
        &self,
        provider_id: &str,
    ) -> Result<ProviderStateChange, ProviderLifecycleError> {
        let mut state = self.lock_state();
        let Some(auth_changed) = self
            .hub
            .set_auth_provider_enabled_with_change(provider_id, false)?
        else {
            Self::bump_generation(&mut state, provider_id);
            Self::remove_live_provider(&mut state, provider_id);
            remove_provider_catalog(provider_id);
            return Err(ProviderLifecycleError::ProviderNotFound {
                id: provider_id.to_string(),
            });
        };
        Self::bump_generation(&mut state, provider_id);
        Ok(ProviderStateChange {
            auth_changed,
            live_changed: Self::remove_live_provider(&mut state, provider_id),
            catalog_changed: remove_provider_catalog(provider_id),
        })
    }

    pub fn remove_provider(
        &self,
        provider_id: &str,
    ) -> Result<ProviderStateChange, ProviderLifecycleError> {
        let mut state = self.lock_state();
        let auth_changed = self.hub.remove_auth_provider(provider_id)?;
        Self::bump_generation(&mut state, provider_id);
        Ok(ProviderStateChange {
            auth_changed,
            live_changed: Self::remove_live_provider(&mut state, provider_id),
            catalog_changed: remove_provider_catalog(provider_id),
        })
    }

    fn provider_descriptor(
        &self,
        provider: &StoredProvider,
        providers: &[StoredProvider],
    ) -> Result<ProviderDescriptor, ProviderLifecycleError> {
        let in_memory_namespace = provider_catalog_namespace(&provider.id);
        let persisted_namespace = self.hub.load_auth_model_namespace(&provider.id)?;
        if let (Some(current), Some(persisted)) = (&in_memory_namespace, &persisted_namespace)
            && current != persisted
        {
            return Err(CatalogError::NamespaceChanged {
                provider_key: provider.id.clone(),
                current: current.clone(),
                requested: persisted.clone(),
            }
            .into());
        }
        let namespace = in_memory_namespace
            .or(persisted_namespace)
            .unwrap_or_else(|| {
                let provider_ids = providers
                    .iter()
                    .map(|provider| provider.id.clone())
                    .collect::<Vec<_>>();
                let short_id = shortest_unique_provider_id(&provider.id, &provider_ids);
                format!("{short_id}@{}", provider.name)
            });
        Ok(ProviderDescriptor {
            provider_key: provider.id.clone(),
            provider_name: provider.name.clone(),
            namespace,
            wire_profile: wire_profile_for_kind(&provider.kind),
        })
    }

    fn commit_cache_and_catalog_if_current(
        &self,
        provider: &StoredProvider,
        expected_catalog: &crate::auth_store::AuthProviderCatalogSnapshot,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
        prepared: PreparedProviderCatalog,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        let namespace = prepared.namespace().to_string();
        let (commit, delta) = self
            .hub
            .update_auth_model_cache_details_if_enabled_and_then(
                provider,
                expected_catalog,
                &namespace,
                fetched_at,
                models,
                || commit_prepared_provider_catalog(prepared),
            )?;
        match (commit, delta) {
            (AuthModelCacheCommit::Updated, Some(delta)) => Ok(delta),
            (
                AuthModelCacheCommit::Missing
                | AuthModelCacheCommit::Disabled
                | AuthModelCacheCommit::Changed,
                _,
            ) => Err(ProviderLifecycleError::Stale {
                id: provider.id.clone(),
            }),
            (AuthModelCacheCommit::Updated, None) => unreachable!("updated callback was not run"),
        }
    }

    fn validate_live_name(
        &self,
        provider_id: &str,
        live_provider: &Arc<dyn Provider>,
    ) -> Result<(), ProviderLifecycleError> {
        if live_provider.name() == provider_id {
            return Ok(());
        }
        Err(ProviderLifecycleError::LiveProviderNameMismatch {
            expected: provider_id.to_string(),
            actual: live_provider.name().to_string(),
        })
    }

    fn enabled_provider<'a>(
        providers: &'a [StoredProvider],
        provider_id: &str,
    ) -> Result<&'a StoredProvider, ProviderLifecycleError> {
        let provider = providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .ok_or_else(|| ProviderLifecycleError::ProviderNotFound {
                id: provider_id.to_string(),
            })?;
        if !provider.enabled {
            return Err(ProviderLifecycleError::ProviderDisabled {
                id: provider_id.to_string(),
            });
        }
        Ok(provider)
    }

    fn operation_lock(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lock_state()
            .operation_locks
            .entry(provider_id.to_string())
            .or_default()
            .clone()
    }

    fn generation(&self, provider_id: &str) -> u64 {
        Self::generation_in(&self.lock_state(), provider_id)
    }

    fn generation_in(state: &LifecycleState, provider_id: &str) -> u64 {
        state.generations.get(provider_id).copied().unwrap_or(0)
    }

    fn ensure_generation(
        &self,
        state: &LifecycleState,
        provider_id: &str,
        expected: u64,
    ) -> Result<(), ProviderLifecycleError> {
        if Self::generation_in(state, provider_id) == expected {
            return Ok(());
        }
        Err(ProviderLifecycleError::Stale {
            id: provider_id.to_string(),
        })
    }

    fn bump_generation(state: &mut LifecycleState, provider_id: &str) {
        let generation = state
            .generations
            .entry(provider_id.to_string())
            .or_default();
        *generation = generation.wrapping_add(1);
    }

    fn register_live_provider(
        state: &mut LifecycleState,
        provider_id: &str,
        provider: Arc<dyn Provider>,
    ) -> Vec<Arc<dyn Provider>> {
        let mut replaced = state
            .live_providers
            .insert(provider_id.to_string(), provider.clone())
            .into_iter()
            .collect::<Vec<_>>();
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            replaced.extend(registry.register_named(provider_id.to_string(), provider.clone()));
            true
        });
        replaced
    }

    fn remove_live_provider(state: &mut LifecycleState, provider_id: &str) -> bool {
        let mut removed = state.live_providers.remove(provider_id).is_some();
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            removed |= registry.remove(provider_id);
            true
        });
        removed
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, LifecycleState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn wire_profile_for_kind(kind: &ProviderKind) -> ReasoningWireProfile {
    match kind {
        ProviderKind::Codex => ReasoningWireProfile::CodexResponses,
        ProviderKind::AnthropicOauth => ReasoningWireProfile::AnthropicMessages,
        ProviderKind::GitHubCopilot | ProviderKind::Custom => ReasoningWireProfile::Unknown,
    }
}

fn shared_lifecycle_state(hub: &ConfigHub) -> Arc<Mutex<LifecycleState>> {
    let key = lifecycle_coordinator_key(hub);
    let mut coordinators = LIFECYCLE_COORDINATORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    coordinators.retain(|_, coordinator| coordinator.strong_count() > 0);
    if let Some(state) = coordinators.get(&key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(Mutex::new(LifecycleState::default()));
    coordinators.insert(key, Arc::downgrade(&state));
    state
}

fn lifecycle_coordinator_key(hub: &ConfigHub) -> PathBuf {
    let path = hub.auth_path();
    if let Some(parent) = path.parent()
        && let Ok(parent) = std::fs::canonicalize(parent)
        && let Some(file_name) = path.file_name()
    {
        return parent.join(file_name);
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use tokio::sync::oneshot;

    use super::*;
    use crate::error::RuntimeError;
    use crate::event::Observable;
    use crate::provider::{
        AssistantMessage, CapabilityKnowledge, DiscoveredModelDetails, LlmRequest,
        ModelCapabilities,
    };
    use crate::tool::BoxFut;

    struct TestProvider {
        name: String,
        models: Arc<StdMutex<Vec<DiscoveredModelDetails>>>,
        block: Arc<StdMutex<Option<DiscoveryBlock>>>,
        fail: Arc<StdMutex<Option<ModelDiscoveryError>>>,
        before_discovery: Arc<StdMutex<Option<DiscoveryHook>>>,
    }

    type DiscoveryHook = Box<dyn FnOnce() + Send>;

    struct DiscoveryBlock {
        started: oneshot::Sender<()>,
        proceed: oneshot::Receiver<()>,
    }

    impl TestProvider {
        fn new(name: &str, models: Vec<DiscoveredModelDetails>) -> Self {
            Self {
                name: name.into(),
                models: Arc::new(StdMutex::new(models)),
                block: Arc::new(StdMutex::new(None)),
                fail: Arc::new(StdMutex::new(None)),
                before_discovery: Arc::new(StdMutex::new(None)),
            }
        }

        fn set_models(&self, models: Vec<DiscoveredModelDetails>) {
            *self.models.lock().unwrap() = models;
        }

        fn fail_once(&self, error: ModelDiscoveryError) {
            *self.fail.lock().unwrap() = Some(error);
        }

        fn block_once(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
            let (started_tx, started_rx) = oneshot::channel();
            let (proceed_tx, proceed_rx) = oneshot::channel();
            *self.block.lock().unwrap() = Some(DiscoveryBlock {
                started: started_tx,
                proceed: proceed_rx,
            });
            (started_rx, proceed_tx)
        }

        fn before_discovery_once(&self, hook: impl FnOnce() + Send + 'static) {
            *self.before_discovery.lock().unwrap() = Some(Box::new(hook));
        }
    }

    impl Provider for TestProvider {
        fn name(&self) -> &str {
            &self.name
        }

        fn call<'a>(
            &'a self,
            _req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            Box::pin(async { unreachable!("not used by lifecycle tests") })
        }

        fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
            unreachable!("not used by lifecycle tests")
        }

        fn try_discover_models(
            &self,
        ) -> BoxFut<'static, Result<Vec<DiscoveredModelDetails>, ModelDiscoveryError>> {
            let models = self.models.lock().unwrap().clone();
            let block = self.block.lock().unwrap().take();
            let error = self.fail.lock().unwrap().take();
            let before_discovery = self.before_discovery.lock().unwrap().take();
            Box::pin(async move {
                if let Some(before_discovery) = before_discovery {
                    before_discovery();
                }
                if let Some(block) = block {
                    let _ = block.started.send(());
                    let _ = block.proceed.await;
                }
                if let Some(error) = error {
                    return Err(error);
                }
                Ok(models)
            })
        }
    }

    fn provider_record(id: &str) -> StoredProvider {
        StoredProvider {
            id: id.into(),
            name: "Account".into(),
            kind: ProviderKind::Codex,
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
            account: Some("account@example.com".into()),
            enabled: true,
            model_cache: None,
        }
    }

    fn model(slug: &str) -> DiscoveredModelDetails {
        DiscoveredModelDetails {
            slug: slug.into(),
            context_budget: Some(128_000),
            capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities::default()),
        }
    }

    fn fixture(id: &str) -> (tempfile::TempDir, ProviderLifecycle, Arc<TestProvider>) {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = ProviderLifecycle::new(
            ConfigHub::from_config_dir(dir.path()),
            ProviderRegistry::new(),
        );
        let provider = Arc::new(TestProvider::new(id, vec![model("initial")]));
        (dir, lifecycle, provider)
    }

    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn install_commits_auth_live_provider_and_catalog_together() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-install");
            remove_provider_catalog("lifecycle-install");

            let delta = lifecycle
                .install_provider(provider_record("lifecycle-install"), provider)
                .await
                .unwrap();

            assert_eq!(delta.total, 1);
            assert!(lifecycle.providers.contains("lifecycle-install"));
            assert_eq!(
                lifecycle.hub.load_auth().unwrap().providers[0].id,
                "lifecycle-install"
            );
            assert!(provider_catalog_namespace("lifecycle-install").is_some());
            lifecycle.remove_provider("lifecycle-install").unwrap();
        });
    }

    #[test]
    fn failed_refresh_preserves_cache_and_catalog_revision() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-failure");
            remove_provider_catalog("lifecycle-failure");
            lifecycle
                .install_provider(provider_record("lifecycle-failure"), provider.clone())
                .await
                .unwrap();
            let auth_path = lifecycle.hub.config_dir().join("auth.json");
            let before_auth = std::fs::read(&auth_path).unwrap();
            let before_revision = crate::model_registry::model_catalog_revision();
            provider.fail_once(ModelDiscoveryError::Transport("offline".into()));

            assert!(matches!(
                lifecycle.refresh_models("lifecycle-failure").await,
                Err(ProviderLifecycleError::Discovery(
                    ModelDiscoveryError::Transport(message)
                )) if message == "offline"
            ));
            assert_eq!(std::fs::read(auth_path).unwrap(), before_auth);
            assert_eq!(
                crate::model_registry::model_catalog_revision(),
                before_revision
            );
            lifecycle.remove_provider("lifecycle-failure").unwrap();
        });
    }

    #[test]
    fn disable_invalidates_in_flight_refresh_without_resurrection() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-race");
            remove_provider_catalog("lifecycle-race");
            lifecycle
                .install_provider(provider_record("lifecycle-race"), provider.clone())
                .await
                .unwrap();
            let peer_registry = ProviderRegistry::new();
            let peer = ProviderLifecycle::new(lifecycle.hub.clone(), peer_registry.clone());
            assert!(Arc::ptr_eq(&lifecycle.state, &peer.state));
            assert!(peer_registry.contains("lifecycle-race"));
            provider.set_models(vec![model("replacement")]);
            let (started, proceed) = provider.block_once();
            let refresh_lifecycle = lifecycle.clone();
            let refresh =
                tokio::spawn(
                    async move { refresh_lifecycle.refresh_models("lifecycle-race").await },
                );
            started.await.unwrap();

            peer.disable_provider("lifecycle-race").unwrap();
            proceed.send(()).unwrap();
            assert!(matches!(
                refresh.await.unwrap(),
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-race"
            ));
            assert!(!lifecycle.providers.contains("lifecycle-race"));
            assert!(!peer_registry.contains("lifecycle-race"));
            assert!(provider_catalog_namespace("lifecycle-race").is_none());
            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert!(!stored.enabled);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "initial");
            lifecycle.remove_provider("lifecycle-race").unwrap();
        });
    }

    #[test]
    fn independent_lifecycles_serialize_refreshes_for_the_same_auth_store() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-shared-gate");
            remove_provider_catalog("lifecycle-shared-gate");
            lifecycle
                .install_provider(provider_record("lifecycle-shared-gate"), provider.clone())
                .await
                .unwrap();
            let peer_registry = ProviderRegistry::new();
            let peer = ProviderLifecycle::new(lifecycle.hub.clone(), peer_registry.clone());
            assert!(Arc::ptr_eq(&lifecycle.state, &peer.state));
            assert!(peer_registry.contains("lifecycle-shared-gate"));

            provider.set_models(vec![model("slow")]);
            let (started, proceed) = provider.block_once();
            let slow_lifecycle = lifecycle.clone();
            let slow = tokio::spawn(async move {
                slow_lifecycle.refresh_models("lifecycle-shared-gate").await
            });
            started.await.unwrap();
            provider.set_models(vec![model("fresh")]);
            let fast =
                tokio::spawn(async move { peer.refresh_models("lifecycle-shared-gate").await });
            tokio::task::yield_now().await;
            assert!(!fast.is_finished());

            proceed.send(()).unwrap();
            slow.await.unwrap().unwrap();
            fast.await.unwrap().unwrap();

            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "fresh");
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("lifecycle-shared-gate")
                .unwrap()
                .unwrap();
            assert!(crate::model_registry::model_entry(&format!("{namespace}:fresh")).is_some());
            assert!(crate::model_registry::model_entry(&format!("{namespace}:slow")).is_none());
            lifecycle.remove_provider("lifecycle-shared-gate").unwrap();
        });
    }

    #[test]
    fn external_cache_aba_invalidates_an_in_flight_refresh() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-cache-cas");
            remove_provider_catalog("lifecycle-cache-cas");
            lifecycle
                .install_provider(provider_record("lifecycle-cache-cas"), provider.clone())
                .await
                .unwrap();
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("lifecycle-cache-cas")
                .unwrap()
                .unwrap();
            let initial_fetched_at = lifecycle.hub.load_auth().unwrap().providers[0]
                .model_cache
                .as_ref()
                .unwrap()
                .fetched_at;
            let initial = lifecycle
                .hub
                .load_auth_model_cache_details("lifecycle-cache-cas")
                .unwrap()
                .unwrap();
            provider.set_models(vec![model("slow")]);
            let (started, proceed) = provider.block_once();
            let refresh_lifecycle = lifecycle.clone();
            let refresh = tokio::spawn(async move {
                refresh_lifecycle
                    .refresh_models("lifecycle-cache-cas")
                    .await
            });
            started.await.unwrap();

            assert!(
                lifecycle
                    .hub
                    .update_auth_model_cache_details(
                        "lifecycle-cache-cas",
                        &namespace,
                        99,
                        &[model("external")],
                    )
                    .unwrap()
            );
            assert!(
                lifecycle
                    .hub
                    .update_auth_model_cache_details(
                        "lifecycle-cache-cas",
                        &namespace,
                        initial_fetched_at,
                        &initial,
                    )
                    .unwrap()
            );
            proceed.send(()).unwrap();
            assert!(matches!(
                refresh.await.unwrap(),
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-cache-cas"
            ));

            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "initial");
            assert!(crate::model_registry::model_entry(&format!("{namespace}:initial")).is_some());
            assert!(crate::model_registry::model_entry(&format!("{namespace}:slow")).is_none());
            lifecycle.remove_provider("lifecycle-cache-cas").unwrap();
        });
    }

    #[test]
    fn external_enable_aba_invalidates_an_in_flight_refresh() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-enable-aba");
            remove_provider_catalog("lifecycle-enable-aba");
            lifecycle
                .install_provider(provider_record("lifecycle-enable-aba"), provider.clone())
                .await
                .unwrap();
            provider.set_models(vec![model("slow")]);
            let (started, proceed) = provider.block_once();
            let refresh_lifecycle = lifecycle.clone();
            let refresh = tokio::spawn(async move {
                refresh_lifecycle
                    .refresh_models("lifecycle-enable-aba")
                    .await
            });
            started.await.unwrap();

            lifecycle
                .hub
                .set_auth_provider_enabled("lifecycle-enable-aba", false)
                .unwrap();
            lifecycle
                .hub
                .set_auth_provider_enabled("lifecycle-enable-aba", true)
                .unwrap();
            proceed.send(()).unwrap();

            assert!(matches!(
                refresh.await.unwrap(),
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-enable-aba"
            ));
            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert!(stored.enabled);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "initial");
            lifecycle.remove_provider("lifecycle-enable-aba").unwrap();
        });
    }

    #[test]
    fn credential_rotation_during_discovery_does_not_invalidate_catalog_refresh() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-token-refresh");
            remove_provider_catalog("lifecycle-token-refresh");
            lifecycle
                .install_provider(provider_record("lifecycle-token-refresh"), provider.clone())
                .await
                .unwrap();
            provider.set_models(vec![model("replacement")]);
            let hub = lifecycle.hub.clone();
            provider.before_discovery_once(move || {
                assert!(
                    hub.update_auth_tokens(
                        "lifecycle-token-refresh",
                        crate::config_hub::AuthTokenUpdate {
                            access_token: "rotated-access".into(),
                            refresh_token: Some("rotated-refresh".into()),
                            expires_at: 123,
                            account: Some("rotated@example.com".into()),
                        },
                    )
                    .unwrap()
                );
            });

            let delta = lifecycle
                .refresh_models("lifecycle-token-refresh")
                .await
                .unwrap();

            assert_eq!(delta.total, 1);
            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert_eq!(stored.access_token, "rotated-access");
            assert_eq!(stored.refresh_token.as_deref(), Some("rotated-refresh"));
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "replacement");
            lifecycle
                .remove_provider("lifecycle-token-refresh")
                .unwrap();
        });
    }
}
