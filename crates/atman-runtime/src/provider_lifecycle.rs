use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use crate::auth_store::{ProviderKind, StoredProvider};
use crate::config_hub::{
    AuthModelCacheCommit, AuthModelCacheUpdate, AuthProviderInsertCommit,
    AuthProviderRuntimeCommit, AuthProviderRuntimeState, ConfigError, ConfigHub,
    ProviderConfigUpdate, ProviderConfigWriteMode,
};
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderLifecycleOutcome {
    pub state: ProviderStateChange,
    pub catalog: Option<CatalogDelta>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderReconcileOutcome {
    pub providers: Vec<(String, ProviderStateChange)>,
}

/// Result of a conditional provider catalog refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderCatalogRefreshOutcome {
    /// The persisted catalog is already current.
    NotNeeded,
    /// Another task in this process is refreshing the same provider.
    AlreadyInFlight,
    /// The in-memory catalog was updated from discovery or a newer persisted cache.
    CatalogUpdated(CatalogDelta),
}

#[derive(Clone)]
pub struct ProviderLifecycle {
    hub: ConfigHub,
    providers: ProviderRegistry,
    state: Arc<Mutex<LifecycleState>>,
    config_state: Arc<Mutex<ConfigProviderLifecycleState>>,
}

#[derive(Clone)]
pub(crate) struct ProviderLifecycleOwner {
    hub: ConfigHub,
    state: Arc<Mutex<LifecycleState>>,
    config_state: Arc<Mutex<ConfigProviderLifecycleState>>,
}

#[derive(Default)]
struct LifecycleState {
    generations: HashMap<String, u64>,
    operation_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    refresh_locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    live_providers: HashMap<String, LiveProviderEntry>,
    registries: Vec<WeakProviderRegistry>,
}

#[derive(Default)]
struct ConfigProviderLifecycleState {
    config_providers: HashMap<String, crate::model_registry::ProviderEntry>,
    registries: Vec<WeakProviderRegistry>,
}

#[derive(Clone)]
struct LiveProviderEntry {
    kind: ProviderKind,
    provider: Arc<dyn Provider>,
}

struct LiveProviderRegistration {
    changed: bool,
    replaced: Vec<Arc<dyn Provider>>,
}

struct LiveProviderRemoval {
    changed: bool,
    removed: Vec<Arc<dyn Provider>>,
}

struct ProviderOperationFence {
    operation: Option<tokio::sync::OwnedMutexGuard<()>>,
    providers: Vec<Arc<dyn Provider>>,
}

struct ProviderRefreshAttempt {
    generation: u64,
    runtime: AuthProviderRuntimeState,
    live_provider: Arc<dyn Provider>,
}

enum ProviderRefreshPreparation {
    Hydrated { delta: CatalogDelta, changed: bool },
    Discover(Box<ProviderRefreshAttempt>),
}

type ProviderRefreshPreparationResult = (
    Result<ProviderRefreshPreparation, ProviderLifecycleError>,
    Vec<Arc<dyn Provider>>,
);

type ProviderRefreshAttemptResult = (
    Result<ProviderRefreshAttempt, ProviderLifecycleError>,
    Vec<Arc<dyn Provider>>,
);

type ProviderCatalogHydrationResult = (
    Result<(CatalogDelta, bool), ProviderLifecycleError>,
    Vec<Arc<dyn Provider>>,
);

impl ProviderOperationFence {
    fn new(operation: tokio::sync::OwnedMutexGuard<()>) -> Self {
        Self {
            operation: Some(operation),
            providers: Vec::new(),
        }
    }

    fn protect(&mut self, provider: Arc<dyn Provider>) {
        self.providers.push(provider);
    }

    fn extend(&mut self, providers: Vec<Arc<dyn Provider>>) {
        self.providers.extend(providers);
    }
}

impl Drop for ProviderOperationFence {
    fn drop(&mut self) {
        drop(self.operation.take());
        self.providers.clear();
    }
}

static LIFECYCLE_COORDINATORS: LazyLock<Mutex<HashMap<PathBuf, Weak<Mutex<LifecycleState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static CONFIG_PROVIDER_COORDINATORS: LazyLock<
    Mutex<HashMap<PathBuf, Weak<Mutex<ConfigProviderLifecycleState>>>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

impl ProviderLifecycle {
    pub fn new(hub: ConfigHub, providers: ProviderRegistry) -> Self {
        let (lifecycle, replaced) = Self::new_deferred(hub, providers);
        drop(replaced);
        lifecycle
    }

    pub(crate) fn new_deferred(
        hub: ConfigHub,
        providers: ProviderRegistry,
    ) -> (Self, Vec<Arc<dyn Provider>>) {
        let state = shared_lifecycle_state(&hub);
        let config_state = shared_config_provider_state(&hub);
        let mut replaced = Vec::new();
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
                for (provider_id, live) in &shared.live_providers {
                    replaced.extend(
                        providers.register_named(provider_id.clone(), live.provider.clone()),
                    );
                }
                shared.registries.push(providers.downgrade());
            }
        }
        {
            let mut shared = config_state
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
                let mut config_providers = shared
                    .config_providers
                    .iter()
                    .map(|(name, entry)| (name.clone(), entry.clone()))
                    .collect::<Vec<_>>();
                config_providers.sort_by(|left, right| left.0.cmp(&right.0));
                for (name, entry) in config_providers {
                    let (_, previous) = crate::config_provider::reconcile_config_provider_deferred(
                        &providers, &name, &entry,
                    );
                    replaced.extend(previous);
                }
                shared.registries.push(providers.downgrade());
            }
        }
        (
            Self {
                hub,
                providers,
                state,
                config_state,
            },
            replaced,
        )
    }

    pub fn config_hub(&self) -> &ConfigHub {
        &self.hub
    }

    pub fn provider_registry(&self) -> &ProviderRegistry {
        &self.providers
    }

    pub(crate) fn owner(&self) -> ProviderLifecycleOwner {
        ProviderLifecycleOwner {
            hub: self.hub.clone(),
            state: self.state.clone(),
            config_state: self.config_state.clone(),
        }
    }

    pub(crate) fn from_owner(owner: ProviderLifecycleOwner, providers: ProviderRegistry) -> Self {
        Self {
            hub: owner.hub,
            providers,
            state: owner.state,
            config_state: owner.config_state,
        }
    }

    /// Reload config-backed providers and reconcile every attached live registry.
    pub fn reload_config_providers(&self) -> Result<(), ProviderLifecycleError> {
        reload_config_providers_for_hub(&self.hub)?;
        Ok(())
    }

    /// Create one config-backed provider and reconcile every attached live registry.
    pub fn create_config_provider(
        &self,
        update: ProviderConfigUpdate<'_>,
    ) -> Result<(), ProviderLifecycleError> {
        self.commit_config_provider(update, ProviderConfigWriteMode::Create)
    }

    /// Update one config-backed provider and reconcile every attached live registry.
    pub fn update_config_provider(
        &self,
        update: ProviderConfigUpdate<'_>,
    ) -> Result<(), ProviderLifecycleError> {
        self.commit_config_provider(update, ProviderConfigWriteMode::Update)
    }

    fn commit_config_provider(
        &self,
        update: ProviderConfigUpdate<'_>,
        mode: ProviderConfigWriteMode,
    ) -> Result<(), ProviderLifecycleError> {
        mutate_config_provider_for_hub(&self.hub, update, mode)?;
        Ok(())
    }

    pub async fn install_provider(
        &self,
        provider_record: StoredProvider,
        live_provider: Arc<dyn Provider>,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        self.validate_new_provider(&provider_record, &live_provider)?;
        let provider_id = provider_record.id.clone();
        let operation_lock = self.operation_lock(&provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        fence.protect(live_provider.clone());
        let generation = self.generation(&provider_id);
        let result = match live_provider.try_discover_models().await {
            Ok(models) => {
                self.commit_new_provider(provider_record, live_provider, &models, generation)
            }
            Err(error) => Err(error.into()),
        };
        match result {
            Ok((delta, replaced)) => {
                fence.extend(replaced);
                drop(fence);
                Ok(delta)
            }
            Err(error) => {
                drop(fence);
                Err(error)
            }
        }
    }

    pub async fn install_pre_discovered_provider(
        &self,
        provider_record: StoredProvider,
        live_provider: Arc<dyn Provider>,
        models: Vec<crate::provider::DiscoveredModelDetails>,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        self.validate_new_provider(&provider_record, &live_provider)?;
        let provider_id = provider_record.id.clone();
        let operation_lock = self.operation_lock(&provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        fence.protect(live_provider.clone());
        let generation = self.generation(&provider_id);
        let result = self.commit_new_provider(provider_record, live_provider, &models, generation);
        match result {
            Ok((delta, replaced)) => {
                fence.extend(replaced);
                drop(fence);
                Ok(delta)
            }
            Err(error) => {
                drop(fence);
                Err(error)
            }
        }
    }

    fn commit_new_provider(
        &self,
        provider_record: StoredProvider,
        live_provider: Arc<dyn Provider>,
        models: &[crate::provider::DiscoveredModelDetails],
        generation: u64,
    ) -> Result<(CatalogDelta, Vec<Arc<dyn Provider>>), ProviderLifecycleError> {
        let provider_id = provider_record.id.clone();
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
        let mut expected_provider_ids = store
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        expected_provider_ids.sort();
        store.providers.push(provider_record.clone());
        let descriptor = self.provider_descriptor(&provider_record, &store.providers)?;
        let prepared = prepare_provider_catalog(descriptor, models)?;
        let namespace = prepared.namespace().to_string();
        let provider_kind = provider_record.kind.clone();
        let (commit, applied) = self
            .hub
            .add_auth_provider_with_model_cache_details_if_provider_ids_and_then(
                provider_record,
                Some(&expected_provider_ids),
                &namespace,
                chrono::Utc::now().timestamp(),
                models,
                || {
                    let registration = Self::register_live_provider(
                        &mut state,
                        &provider_id,
                        provider_kind,
                        live_provider,
                    );
                    (
                        commit_prepared_provider_catalog(prepared),
                        registration.replaced,
                    )
                },
            )?;
        match (commit, applied) {
            (AuthProviderInsertCommit::Inserted, Some((delta, replaced_providers))) => {
                Self::bump_generation(&mut state, &provider_id);
                drop(state);
                Ok((delta, replaced_providers))
            }
            (AuthProviderInsertCommit::Changed, _) => {
                drop(state);
                Err(ProviderLifecycleError::Stale { id: provider_id })
            }
            (AuthProviderInsertCommit::Inserted, None) => {
                unreachable!("provider insertion callback was not run")
            }
        }
    }

    /// Return all in-memory live providers that must be reconciled with storage.
    pub fn catalog_refresh_plan(&self) -> Vec<String> {
        let state = self.lock_state();
        let mut plan = state.live_providers.keys().cloned().collect::<Vec<_>>();
        plan.sort();
        plan
    }

    pub async fn refresh_models(
        &self,
        provider_id: &str,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        let refresh_lock = self.refresh_lock(provider_id);
        let _refresh = refresh_lock.lock_owned().await;
        match self.run_catalog_refresh(provider_id, false).await? {
            ProviderCatalogRefreshOutcome::CatalogUpdated(delta) => Ok(delta),
            ProviderCatalogRefreshOutcome::NotNeeded
            | ProviderCatalogRefreshOutcome::AlreadyInFlight => {
                unreachable!("forced catalog refresh did not run")
            }
        }
    }

    /// Refresh one stale catalog without waiting behind an equivalent in-process refresh.
    pub async fn refresh_models_if_stale(
        &self,
        provider_id: &str,
    ) -> Result<ProviderCatalogRefreshOutcome, ProviderLifecycleError> {
        let refresh_lock = self.refresh_lock(provider_id);
        let Ok(_refresh) = refresh_lock.try_lock_owned() else {
            return Ok(ProviderCatalogRefreshOutcome::AlreadyInFlight);
        };
        self.run_catalog_refresh(provider_id, true).await
    }

    async fn run_catalog_refresh(
        &self,
        provider_id: &str,
        only_if_stale: bool,
    ) -> Result<ProviderCatalogRefreshOutcome, ProviderLifecycleError> {
        let operation_lock = self.operation_lock(provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        let (preparation, deferred) = self.prepare_catalog_refresh(
            provider_id,
            chrono::Utc::now().timestamp(),
            only_if_stale,
            None,
            None,
        );
        fence.extend(deferred);
        let attempt = match preparation? {
            ProviderRefreshPreparation::Hydrated { delta, changed } => {
                drop(fence);
                return Ok(if changed {
                    ProviderCatalogRefreshOutcome::CatalogUpdated(delta)
                } else {
                    ProviderCatalogRefreshOutcome::NotNeeded
                });
            }
            ProviderRefreshPreparation::Discover(attempt) => attempt,
        };
        drop(fence);

        let discovery = attempt.live_provider.try_discover_models().await;
        let operation_lock = self.operation_lock(provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        fence.protect(attempt.live_provider.clone());
        let (result, deferred) = match discovery {
            Ok(models) => self.finish_catalog_refresh(provider_id, &attempt, &models),
            Err(error) => {
                self.reconcile_refresh_failure(provider_id, &attempt, error.into(), Vec::new())
            }
        };
        fence.extend(deferred);
        drop(fence);
        result.map(ProviderCatalogRefreshOutcome::CatalogUpdated)
    }

    fn prepare_catalog_refresh(
        &self,
        provider_id: &str,
        now: i64,
        hydrate_if_fresh: bool,
        expected_live_provider: Option<&Arc<dyn Provider>>,
        expected_catalog_snapshot: Option<&crate::auth_store::AuthProviderCatalogSnapshot>,
    ) -> ProviderRefreshPreparationResult {
        for _ in 0..3 {
            let (attempt, mut deferred) = self.load_catalog_refresh_attempt(provider_id, now);
            let attempt = match attempt {
                Ok(attempt) => attempt,
                Err(error) => return (Err(error), deferred),
            };
            if expected_live_provider
                .is_some_and(|expected| !Arc::ptr_eq(expected, &attempt.live_provider))
            {
                return (
                    Err(ProviderLifecycleError::Stale {
                        id: provider_id.to_string(),
                    }),
                    deferred,
                );
            }
            if !hydrate_if_fresh
                || attempt.runtime.model_cache_freshness
                    != crate::auth_store::ModelCacheFreshness::Fresh
                || expected_catalog_snapshot
                    .is_some_and(|expected| expected == &attempt.runtime.catalog_snapshot)
            {
                return (
                    Ok(ProviderRefreshPreparation::Discover(Box::new(attempt))),
                    deferred,
                );
            }
            let (hydrated, hydrate_deferred) =
                self.hydrate_cached_catalog_if_current(provider_id, &attempt);
            deferred.extend(hydrate_deferred);
            match hydrated {
                Ok((delta, changed)) => {
                    return (
                        Ok(ProviderRefreshPreparation::Hydrated { delta, changed }),
                        deferred,
                    );
                }
                Err(ProviderLifecycleError::Stale { .. }) => continue,
                Err(error) => return (Err(error), deferred),
            }
        }
        let (authoritative, deferred) = self.load_catalog_refresh_attempt(provider_id, now);
        match authoritative {
            Ok(attempt)
                if expected_live_provider
                    .is_none_or(|expected| Arc::ptr_eq(expected, &attempt.live_provider)) =>
            {
                (
                    Err(ProviderLifecycleError::Stale {
                        id: provider_id.to_string(),
                    }),
                    deferred,
                )
            }
            Ok(_) => (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                deferred,
            ),
            Err(error) => (Err(error), deferred),
        }
    }

    fn load_catalog_refresh_attempt(
        &self,
        provider_id: &str,
        now: i64,
    ) -> ProviderRefreshAttemptResult {
        let mut state = self.lock_state();
        let runtime = self
            .hub
            .load_or_create_auth_provider_runtime_state_at(provider_id, now);
        let runtime = match runtime {
            Ok(Some(runtime)) => runtime,
            Ok(None) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (
                    Err(ProviderLifecycleError::ProviderNotFound {
                        id: provider_id.to_string(),
                    }),
                    removed,
                );
            }
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (Err(error.into()), removed);
            }
        };
        if !runtime.provider.enabled {
            Self::bump_generation(&mut state, provider_id);
            let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
            drop(state);
            return (
                Err(ProviderLifecycleError::ProviderDisabled {
                    id: provider_id.to_string(),
                }),
                removed,
            );
        }
        let Some(live) = state.live_providers.get(provider_id) else {
            Self::bump_generation(&mut state, provider_id);
            let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
            drop(state);
            return (
                Err(ProviderLifecycleError::LiveProviderMissing {
                    id: provider_id.to_string(),
                }),
                removed,
            );
        };
        if live.kind != runtime.provider.kind {
            Self::bump_generation(&mut state, provider_id);
            let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
            drop(state);
            return (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                removed,
            );
        }
        let attempt = ProviderRefreshAttempt {
            generation: Self::generation_in(&state, provider_id),
            runtime,
            live_provider: live.provider.clone(),
        };
        (Ok(attempt), Vec::new())
    }

    fn hydrate_cached_catalog_if_current(
        &self,
        provider_id: &str,
        attempt: &ProviderRefreshAttempt,
    ) -> ProviderCatalogHydrationResult {
        let mut state = self.lock_state();
        if let Err(error) = self.ensure_generation(&state, provider_id, attempt.generation) {
            return (Err(error), Vec::new());
        }
        let Some(current_live) = state.live_providers.get(provider_id) else {
            return (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                Vec::new(),
            );
        };
        if current_live.kind != attempt.runtime.provider.kind
            || !Arc::ptr_eq(&attempt.live_provider, &current_live.provider)
        {
            return (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                Vec::new(),
            );
        }
        let Some(models) = attempt.runtime.model_cache.as_deref() else {
            return (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                Vec::new(),
            );
        };
        let descriptor = match self.provider_descriptor_from_state(
            &attempt.runtime.provider,
            &attempt.runtime.provider_ids,
            attempt.runtime.model_namespace.as_deref(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => return (Err(error), Vec::new()),
        };
        let prepared = match prepare_provider_catalog(descriptor, models) {
            Ok(prepared) => prepared,
            Err(error) => return (Err(error.into()), Vec::new()),
        };
        let namespace = prepared.namespace().to_string();
        let revision = crate::model_registry::model_catalog_revision();
        let expected_provider_ids = attempt
            .runtime
            .model_namespace
            .is_none()
            .then_some(attempt.runtime.provider_ids.as_slice());
        let commit = self.hub.commit_auth_provider_runtime_if_current_and_then(
            &attempt.runtime.provider,
            &attempt.runtime.catalog_snapshot,
            false,
            Some(&namespace),
            expected_provider_ids,
            || {
                let delta = commit_prepared_provider_catalog(prepared);
                let changed = crate::model_registry::model_catalog_revision() != revision;
                (delta, changed)
            },
        );
        let (commit, hydrated) = match commit {
            Ok(result) => result,
            Err(error) => return (Err(error.into()), Vec::new()),
        };
        match commit {
            AuthProviderRuntimeCommit::Applied { .. } => {
                drop(state);
                (
                    Ok(hydrated.expect("catalog hydration callback was not run")),
                    Vec::new(),
                )
            }
            AuthProviderRuntimeCommit::Missing => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                (
                    Err(ProviderLifecycleError::ProviderNotFound {
                        id: provider_id.to_string(),
                    }),
                    removed,
                )
            }
            AuthProviderRuntimeCommit::Disabled => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                (
                    Err(ProviderLifecycleError::ProviderDisabled {
                        id: provider_id.to_string(),
                    }),
                    removed,
                )
            }
            AuthProviderRuntimeCommit::Changed => {
                drop(state);
                (
                    Err(ProviderLifecycleError::Stale {
                        id: provider_id.to_string(),
                    }),
                    Vec::new(),
                )
            }
        }
    }

    fn finish_catalog_refresh(
        &self,
        provider_id: &str,
        attempt: &ProviderRefreshAttempt,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> (
        Result<CatalogDelta, ProviderLifecycleError>,
        Vec<Arc<dyn Provider>>,
    ) {
        let ProviderRefreshAttempt {
            generation,
            runtime,
            live_provider,
        } = attempt;
        let deferred = Vec::new();
        let state = self.lock_state();
        if let Err(error) = self.ensure_generation(&state, provider_id, *generation) {
            drop(state);
            return self.finish_refresh_failure(
                provider_id,
                &runtime.provider.kind,
                *generation,
                live_provider,
                error,
                deferred,
            );
        }
        let Some(current_live) = state.live_providers.get(provider_id) else {
            drop(state);
            return self.finish_refresh_failure(
                provider_id,
                &runtime.provider.kind,
                *generation,
                live_provider,
                ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                },
                deferred,
            );
        };
        if current_live.kind != runtime.provider.kind
            || !Arc::ptr_eq(live_provider, &current_live.provider)
        {
            drop(state);
            return self.finish_refresh_failure(
                provider_id,
                &runtime.provider.kind,
                *generation,
                live_provider,
                ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                },
                deferred,
            );
        }

        let descriptor = match self.provider_descriptor_from_state(
            &runtime.provider,
            &runtime.provider_ids,
            runtime.model_namespace.as_deref(),
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => {
                drop(state);
                return self.finish_refresh_failure(
                    provider_id,
                    &runtime.provider.kind,
                    *generation,
                    live_provider,
                    error,
                    deferred,
                );
            }
        };
        let prepared = match prepare_provider_catalog(descriptor, models) {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(state);
                return self.finish_refresh_failure(
                    provider_id,
                    &runtime.provider.kind,
                    *generation,
                    live_provider,
                    error.into(),
                    deferred,
                );
            }
        };
        let expected_provider_ids = runtime
            .model_namespace
            .is_none()
            .then_some(runtime.provider_ids.as_slice());
        let result = self.commit_cache_and_catalog_if_current(
            &runtime.provider,
            &runtime.catalog_snapshot,
            expected_provider_ids,
            chrono::Utc::now().timestamp(),
            models,
            prepared,
        );
        drop(state);
        match result {
            Ok(delta) => (Ok(delta), deferred),
            Err(error @ ProviderLifecycleError::Stale { .. }) => {
                self.reconcile_refresh_failure(provider_id, attempt, error, deferred)
            }
            Err(error) => self.finish_refresh_failure(
                provider_id,
                &runtime.provider.kind,
                *generation,
                live_provider,
                error,
                deferred,
            ),
        }
    }

    fn reconcile_refresh_failure(
        &self,
        provider_id: &str,
        attempt: &ProviderRefreshAttempt,
        original: ProviderLifecycleError,
        mut deferred: Vec<Arc<dyn Provider>>,
    ) -> (
        Result<CatalogDelta, ProviderLifecycleError>,
        Vec<Arc<dyn Provider>>,
    ) {
        let (preparation, reconciled_deferred) = self.prepare_catalog_refresh(
            provider_id,
            chrono::Utc::now().timestamp(),
            true,
            Some(&attempt.live_provider),
            Some(&attempt.runtime.catalog_snapshot),
        );
        deferred.extend(reconciled_deferred);
        match preparation {
            Ok(ProviderRefreshPreparation::Hydrated { delta, .. }) => (Ok(delta), deferred),
            Ok(ProviderRefreshPreparation::Discover(_)) => self.finish_refresh_failure(
                provider_id,
                &attempt.runtime.provider.kind,
                attempt.generation,
                &attempt.live_provider,
                original,
                deferred,
            ),
            Err(error) => self.finish_refresh_failure(
                provider_id,
                &attempt.runtime.provider.kind,
                attempt.generation,
                &attempt.live_provider,
                error,
                deferred,
            ),
        }
    }

    fn finish_refresh_failure(
        &self,
        provider_id: &str,
        expected_kind: &ProviderKind,
        expected_generation: u64,
        expected_provider: &Arc<dyn Provider>,
        original: ProviderLifecycleError,
        mut deferred: Vec<Arc<dyn Provider>>,
    ) -> (
        Result<CatalogDelta, ProviderLifecycleError>,
        Vec<Arc<dyn Provider>>,
    ) {
        let catalog_identity_invalid = matches!(
            &original,
            ProviderLifecycleError::Catalog(
                CatalogError::NamespaceChanged { .. }
                    | CatalogError::NamespaceInUse { .. }
                    | CatalogError::NamespaceStore { .. }
            )
        );
        let mut state = self.lock_state();
        let authoritative = self
            .hub
            .load_or_create_auth_provider_runtime_state(provider_id);
        let error = match authoritative {
            Err(error) => error.into(),
            Ok(None) => ProviderLifecycleError::Stale {
                id: provider_id.to_string(),
            },
            Ok(Some(runtime))
                if !runtime.provider.enabled || runtime.provider.kind != *expected_kind =>
            {
                ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }
            }
            Ok(Some(_)) => {
                let same_generation =
                    Self::generation_in(&state, provider_id) == expected_generation;
                let current = state.live_providers.get(provider_id);
                let same_provider = current.is_some_and(|current| {
                    current.kind == *expected_kind
                        && Arc::ptr_eq(&current.provider, expected_provider)
                });
                if same_generation && same_provider && !catalog_identity_invalid {
                    drop(state);
                    return (Err(original), deferred);
                }
                if catalog_identity_invalid {
                    original
                } else if current.is_some_and(|current| current.kind == *expected_kind) {
                    drop(state);
                    return (
                        Err(ProviderLifecycleError::Stale {
                            id: provider_id.to_string(),
                        }),
                        deferred,
                    );
                } else {
                    ProviderLifecycleError::Stale {
                        id: provider_id.to_string(),
                    }
                }
            }
        };
        Self::bump_generation(&mut state, provider_id);
        let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
        deferred.extend(removed);
        drop(state);
        (Err(error), deferred)
    }

    pub async fn restore_provider(
        &self,
        provider_id: &str,
        expected_kind: ProviderKind,
        live_provider: Arc<dyn Provider>,
    ) -> Result<ProviderLifecycleOutcome, ProviderLifecycleError> {
        self.validate_live_name(provider_id, &live_provider)?;
        let operation_lock = self.operation_lock(provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        fence.protect(live_provider.clone());
        let (result, deferred) =
            self.commit_existing_provider_runtime(provider_id, expected_kind, live_provider, false);
        fence.extend(deferred);
        drop(fence);
        result
    }

    pub async fn enable_provider(
        &self,
        provider_id: &str,
        expected_kind: ProviderKind,
        live_provider: Arc<dyn Provider>,
    ) -> Result<ProviderLifecycleOutcome, ProviderLifecycleError> {
        self.validate_live_name(provider_id, &live_provider)?;
        let operation_lock = self.operation_lock(provider_id);
        let mut fence = ProviderOperationFence::new(operation_lock.lock_owned().await);
        fence.protect(live_provider.clone());
        let (result, deferred) =
            self.commit_existing_provider_runtime(provider_id, expected_kind, live_provider, true);
        fence.extend(deferred);
        drop(fence);
        result
    }

    pub fn reconcile_inactive_providers(
        &self,
    ) -> Result<ProviderReconcileOutcome, ProviderLifecycleError> {
        let mut state = self.lock_state();
        let removals = self
            .hub
            .with_auth_provider_runtime_descriptors(|providers| {
                let active = providers
                    .iter()
                    .filter(|provider| provider.enabled)
                    .map(|provider| (provider.id.as_str(), &provider.kind))
                    .collect::<HashMap<_, _>>();
                let mut inactive = state
                    .live_providers
                    .iter()
                    .filter(|(provider_id, live)| {
                        active
                            .get(provider_id.as_str())
                            .is_none_or(|kind| **kind != live.kind)
                    })
                    .map(|(provider_id, _)| provider_id.clone())
                    .collect::<BTreeSet<_>>();
                inactive.extend(
                    providers
                        .iter()
                        .filter(|provider| !provider.enabled)
                        .map(|provider| provider.id.clone()),
                );

                inactive
                    .into_iter()
                    .map(|provider_id| {
                        Self::bump_generation(&mut state, &provider_id);
                        let removal = Self::remove_live_provider(&mut state, &provider_id);
                        (provider_id, removal)
                    })
                    .collect::<Vec<_>>()
            });
        let removals = match removals {
            Ok(removals) => removals,
            Err(error) => {
                let provider_ids = state
                    .live_providers
                    .keys()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let mut removed_providers = Vec::new();
                for provider_id in provider_ids {
                    Self::bump_generation(&mut state, &provider_id);
                    let removal = Self::remove_live_provider(&mut state, &provider_id);
                    removed_providers.extend(removal.removed);
                    remove_provider_catalog(&provider_id);
                }
                drop(state);
                drop(removed_providers);
                return Err(error.into());
            }
        };
        let mut removed_providers = Vec::new();
        let providers = removals
            .into_iter()
            .map(|(provider_id, removal)| {
                removed_providers.extend(removal.removed);
                let change = ProviderStateChange {
                    auth_changed: false,
                    live_changed: removal.changed,
                    catalog_changed: remove_provider_catalog(&provider_id),
                };
                (provider_id, change)
            })
            .collect();
        drop(state);
        drop(removed_providers);
        Ok(ProviderReconcileOutcome { providers })
    }

    pub fn disable_provider(
        &self,
        provider_id: &str,
    ) -> Result<ProviderStateChange, ProviderLifecycleError> {
        let mut state = self.lock_state();
        let auth_changed = match self
            .hub
            .set_auth_provider_enabled_with_change(provider_id, false)
        {
            Ok(Some(auth_changed)) => auth_changed,
            Ok(None) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                drop(removed);
                return Err(ProviderLifecycleError::ProviderNotFound {
                    id: provider_id.to_string(),
                });
            }
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                drop(removed);
                return Err(error.into());
            }
        };
        Self::bump_generation(&mut state, provider_id);
        let (change, removed) =
            Self::remove_runtime_provider(&mut state, provider_id, auth_changed);
        drop(state);
        drop(removed);
        Ok(change)
    }

    pub fn remove_provider(
        &self,
        provider_id: &str,
    ) -> Result<ProviderStateChange, ProviderLifecycleError> {
        let mut state = self.lock_state();
        let auth_changed = match self.hub.remove_auth_provider(provider_id) {
            Ok(auth_changed) => auth_changed,
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                drop(removed);
                return Err(error.into());
            }
        };
        Self::bump_generation(&mut state, provider_id);
        let (change, removed) =
            Self::remove_runtime_provider(&mut state, provider_id, auth_changed);
        drop(state);
        drop(removed);
        Ok(change)
    }

    fn commit_existing_provider_runtime(
        &self,
        provider_id: &str,
        expected_kind: ProviderKind,
        live_provider: Arc<dyn Provider>,
        enable_if_disabled: bool,
    ) -> (
        Result<ProviderLifecycleOutcome, ProviderLifecycleError>,
        Vec<Arc<dyn Provider>>,
    ) {
        let mut state = self.lock_state();
        let runtime = self
            .hub
            .load_or_create_auth_provider_runtime_state(provider_id);
        let runtime = match runtime {
            Ok(Some(runtime)) => runtime,
            Ok(None) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (
                    Err(ProviderLifecycleError::ProviderNotFound {
                        id: provider_id.to_string(),
                    }),
                    removed,
                );
            }
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (Err(error.into()), removed);
            }
        };
        let provider = runtime.provider;
        if provider.kind != expected_kind {
            Self::bump_generation(&mut state, provider_id);
            let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
            drop(state);
            return (
                Err(ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                }),
                removed,
            );
        }
        if !provider.enabled && !enable_if_disabled {
            let validation = self.hub.commit_auth_provider_runtime_if_current_and_then(
                &provider,
                &runtime.catalog_snapshot,
                false,
                None,
                None,
                || (),
            );
            let error = match validation {
                Ok((AuthProviderRuntimeCommit::Missing, _)) => {
                    ProviderLifecycleError::ProviderNotFound {
                        id: provider_id.to_string(),
                    }
                }
                Ok((AuthProviderRuntimeCommit::Disabled, _)) => {
                    ProviderLifecycleError::ProviderDisabled {
                        id: provider_id.to_string(),
                    }
                }
                Ok((
                    AuthProviderRuntimeCommit::Changed | AuthProviderRuntimeCommit::Applied { .. },
                    _,
                )) => ProviderLifecycleError::Stale {
                    id: provider_id.to_string(),
                },
                Err(error) => error.into(),
            };
            Self::bump_generation(&mut state, provider_id);
            let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
            drop(state);
            return (Err(error), removed);
        }
        let prepared = match runtime.model_cache.as_deref() {
            Some(models) => self
                .provider_descriptor_from_state(
                    &provider,
                    &runtime.provider_ids,
                    runtime.model_namespace.as_deref(),
                )
                .and_then(|descriptor| {
                    prepare_provider_catalog(descriptor, models).map_err(Into::into)
                })
                .map(Some),
            None => Ok(None),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (Err(error), removed);
            }
        };
        let model_namespace = prepared
            .as_ref()
            .map(|prepared| prepared.namespace().to_string());
        let selected_provider = state
            .live_providers
            .get(provider_id)
            .filter(|current| current.kind == provider.kind)
            .map(|current| current.provider.clone())
            .unwrap_or_else(|| live_provider.clone());
        let catalog_revision = prepared
            .is_some()
            .then(crate::model_registry::model_catalog_revision);
        let expected_provider_ids = runtime
            .model_namespace
            .is_none()
            .then_some(runtime.provider_ids.as_slice());
        let commit = self.hub.commit_auth_provider_runtime_if_current_and_then(
            &provider,
            &runtime.catalog_snapshot,
            enable_if_disabled,
            model_namespace.as_deref(),
            expected_provider_ids,
            || {
                let registration = Self::register_live_provider(
                    &mut state,
                    provider_id,
                    provider.kind.clone(),
                    selected_provider,
                );
                let (catalog, catalog_changed) = match prepared {
                    Some(prepared) => {
                        let catalog = commit_prepared_provider_catalog(prepared);
                        let changed = catalog_revision.is_some_and(|revision| {
                            crate::model_registry::model_catalog_revision() != revision
                        });
                        (Some(catalog), changed)
                    }
                    None => (None, remove_provider_catalog(provider_id)),
                };
                (catalog, catalog_changed, registration)
            },
        );
        let (commit, applied) = match commit {
            Ok(commit) => commit,
            Err(error) => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                return (Err(error.into()), removed);
            }
        };

        match commit {
            AuthProviderRuntimeCommit::Applied { auth_changed } => {
                let (catalog, catalog_changed, registration) =
                    applied.expect("applied provider runtime callback was not run");
                if auth_changed || catalog_changed || registration.changed {
                    Self::bump_generation(&mut state, provider_id);
                }
                drop(state);
                (
                    Ok(ProviderLifecycleOutcome {
                        state: ProviderStateChange {
                            auth_changed,
                            live_changed: registration.changed,
                            catalog_changed,
                        },
                        catalog,
                    }),
                    registration.replaced,
                )
            }
            AuthProviderRuntimeCommit::Missing => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                (
                    Err(ProviderLifecycleError::ProviderNotFound {
                        id: provider_id.to_string(),
                    }),
                    removed,
                )
            }
            AuthProviderRuntimeCommit::Disabled => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                (
                    Err(ProviderLifecycleError::ProviderDisabled {
                        id: provider_id.to_string(),
                    }),
                    removed,
                )
            }
            AuthProviderRuntimeCommit::Changed => {
                Self::bump_generation(&mut state, provider_id);
                let (_, removed) = Self::remove_runtime_provider(&mut state, provider_id, false);
                drop(state);
                (
                    Err(ProviderLifecycleError::Stale {
                        id: provider_id.to_string(),
                    }),
                    removed,
                )
            }
        }
    }

    fn provider_descriptor(
        &self,
        provider: &StoredProvider,
        providers: &[StoredProvider],
    ) -> Result<ProviderDescriptor, ProviderLifecycleError> {
        let provider_ids = providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        let persisted_namespace = self.hub.load_auth_model_namespace(&provider.id)?;
        self.provider_descriptor_from_state(provider, &provider_ids, persisted_namespace.as_deref())
    }

    fn provider_descriptor_from_state(
        &self,
        provider: &StoredProvider,
        provider_ids: &[String],
        persisted_namespace: Option<&str>,
    ) -> Result<ProviderDescriptor, ProviderLifecycleError> {
        let in_memory_namespace = provider_catalog_namespace(&provider.id);
        if let (Some(current), Some(persisted)) = (&in_memory_namespace, persisted_namespace)
            && current != persisted
        {
            return Err(CatalogError::NamespaceChanged {
                provider_key: provider.id.clone(),
                current: current.clone(),
                requested: persisted.to_string(),
            }
            .into());
        }
        let namespace = in_memory_namespace
            .or_else(|| persisted_namespace.map(str::to_string))
            .unwrap_or_else(|| {
                let short_id = shortest_unique_provider_id(&provider.id, provider_ids);
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
        expected_provider_ids: Option<&[String]>,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
        prepared: PreparedProviderCatalog,
    ) -> Result<CatalogDelta, ProviderLifecycleError> {
        let namespace = prepared.namespace().to_string();
        let (commit, delta) = self
            .hub
            .update_auth_model_cache_details_if_enabled_and_then(
                AuthModelCacheUpdate {
                    expected: provider,
                    expected_catalog,
                    expected_provider_ids,
                    model_namespace: &namespace,
                    fetched_at,
                    models,
                },
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

    fn validate_new_provider(
        &self,
        provider: &StoredProvider,
        live_provider: &Arc<dyn Provider>,
    ) -> Result<(), ProviderLifecycleError> {
        self.validate_live_name(&provider.id, live_provider)?;
        if provider.enabled {
            return Ok(());
        }
        Err(ProviderLifecycleError::ProviderDisabled {
            id: provider.id.clone(),
        })
    }

    fn operation_lock(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lock_state()
            .operation_locks
            .entry(provider_id.to_string())
            .or_default()
            .clone()
    }

    fn refresh_lock(&self, provider_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.lock_state()
            .refresh_locks
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
        kind: ProviderKind,
        provider: Arc<dyn Provider>,
    ) -> LiveProviderRegistration {
        let mut changed = state.live_providers.get(provider_id).is_none_or(|current| {
            current.kind != kind || !Arc::ptr_eq(&current.provider, &provider)
        });
        let mut replaced = state
            .live_providers
            .insert(
                provider_id.to_string(),
                LiveProviderEntry {
                    kind,
                    provider: provider.clone(),
                },
            )
            .into_iter()
            .map(|live| live.provider)
            .collect::<Vec<_>>();
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            let previous = registry.register_named(provider_id.to_string(), provider.clone());
            changed |= previous
                .as_ref()
                .is_none_or(|previous| !Arc::ptr_eq(previous, &provider));
            replaced.extend(previous);
            true
        });
        LiveProviderRegistration { changed, replaced }
    }

    fn remove_live_provider(state: &mut LifecycleState, provider_id: &str) -> LiveProviderRemoval {
        let mut removed = state
            .live_providers
            .remove(provider_id)
            .map(|live| live.provider)
            .into_iter()
            .collect::<Vec<_>>();
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            removed.extend(registry.take_named(provider_id));
            true
        });
        LiveProviderRemoval {
            changed: !removed.is_empty(),
            removed,
        }
    }

    fn remove_runtime_provider(
        state: &mut LifecycleState,
        provider_id: &str,
        auth_changed: bool,
    ) -> (ProviderStateChange, Vec<Arc<dyn Provider>>) {
        let removal = Self::remove_live_provider(state, provider_id);
        (
            ProviderStateChange {
                auth_changed,
                live_changed: removal.changed,
                catalog_changed: remove_provider_catalog(provider_id),
            },
            removal.removed,
        )
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, LifecycleState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) fn reload_config_providers_for_hub(hub: &ConfigHub) -> Result<(), ConfigError> {
    let state = shared_config_provider_state(hub);
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut replaced = Vec::new();
    hub.reload_and_then(|snapshot| {
        let mut providers = snapshot.providers.into_iter().collect::<Vec<_>>();
        providers.sort_by(|left, right| left.0.cmp(&right.0));
        let current_names = providers
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>();
        let stale_names = state
            .config_providers
            .keys()
            .filter(|name| !current_names.contains(*name))
            .cloned()
            .collect::<Vec<_>>();
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            for name in &stale_names {
                replaced.extend(registry.take_named(&format!("config:{name}")));
            }
            for (name, entry) in &providers {
                let (_, previous) = crate::config_provider::reconcile_config_provider_deferred(
                    &registry, name, entry,
                );
                replaced.extend(previous);
            }
            true
        });
        state.config_providers = providers.into_iter().collect();
    })?;
    drop(state);
    drop(replaced);
    Ok(())
}

pub(crate) fn upsert_config_provider_for_hub(
    hub: &ConfigHub,
    update: ProviderConfigUpdate<'_>,
) -> Result<(), ConfigError> {
    mutate_config_provider_for_hub(hub, update, ProviderConfigWriteMode::Upsert)
}

fn mutate_config_provider_for_hub(
    hub: &ConfigHub,
    update: ProviderConfigUpdate<'_>,
    mode: ProviderConfigWriteMode,
) -> Result<(), ConfigError> {
    let state = shared_config_provider_state(hub);
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut replaced = Vec::new();
    hub.write_provider_config_and_then(update, mode, |entry| {
        state.registries.retain(|registry| {
            let Some(registry) = registry.upgrade() else {
                return false;
            };
            let (_, previous) = crate::config_provider::reconcile_config_provider_deferred(
                &registry,
                &entry.name,
                entry,
            );
            replaced.extend(previous);
            true
        });
        state
            .config_providers
            .insert(entry.name.clone(), entry.clone());
    })?;
    drop(state);
    drop(replaced);
    Ok(())
}

fn wire_profile_for_kind(kind: &ProviderKind) -> ReasoningWireProfile {
    match kind {
        ProviderKind::Codex => ReasoningWireProfile::CodexResponses,
        ProviderKind::AnthropicOauth => ReasoningWireProfile::AnthropicMessages,
        ProviderKind::GitHubCopilot | ProviderKind::Custom => ReasoningWireProfile::Unknown,
    }
}

fn shared_lifecycle_state(hub: &ConfigHub) -> Arc<Mutex<LifecycleState>> {
    let key = coordinator_key(hub.auth_path());
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

fn shared_config_provider_state(hub: &ConfigHub) -> Arc<Mutex<ConfigProviderLifecycleState>> {
    let key = coordinator_key(&hub.config_toml_path());
    let mut coordinators = CONFIG_PROVIDER_COORDINATORS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    coordinators.retain(|_, coordinator| coordinator.strong_count() > 0);
    if let Some(state) = coordinators.get(&key).and_then(Weak::upgrade) {
        return state;
    }
    let state = Arc::new(Mutex::new(ConfigProviderLifecycleState::default()));
    coordinators.insert(key, Arc::downgrade(&state));
    state
}

fn coordinator_key(path: &Path) -> PathBuf {
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use tokio::sync::oneshot;

    use super::*;
    use crate::error::RuntimeError;
    use crate::event::Observable;
    use crate::provider::{
        AssistantMessage, CapabilityKnowledge, DiscoveredModelDetails, LlmRequest,
        ModelCapabilities, ReasoningEffort,
    };
    use crate::tool::BoxFut;

    struct TestProvider {
        name: String,
        models: Arc<StdMutex<Vec<DiscoveredModelDetails>>>,
        block: Arc<StdMutex<Option<DiscoveryBlock>>>,
        fail: Arc<StdMutex<Option<ModelDiscoveryError>>>,
        before_discovery: Arc<StdMutex<Option<DiscoveryHook>>>,
        discovery_calls: Arc<AtomicUsize>,
        on_drop: StdMutex<Option<DiscoveryHook>>,
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
                discovery_calls: Arc::new(AtomicUsize::new(0)),
                on_drop: StdMutex::new(None),
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

        fn discovery_calls(&self) -> usize {
            self.discovery_calls.load(Ordering::SeqCst)
        }

        fn on_drop(&self, hook: impl FnOnce() + Send + 'static) {
            *self.on_drop.lock().unwrap() = Some(Box::new(hook));
        }
    }

    impl Drop for TestProvider {
        fn drop(&mut self) {
            if let Some(on_drop) = self.on_drop.lock().unwrap().take() {
                on_drop();
            }
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
            self.discovery_calls.fetch_add(1, Ordering::SeqCst);
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

    fn model_with_effort(slug: &str, effort: ReasoningEffort) -> DiscoveredModelDetails {
        DiscoveredModelDetails {
            slug: slug.into(),
            context_budget: Some(128_000),
            capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities {
                reasoning_efforts: vec![effort],
                ..Default::default()
            }),
        }
    }

    fn seed_cached_provider(
        lifecycle: &ProviderLifecycle,
        id: &str,
        enabled: bool,
        models: &[DiscoveredModelDetails],
    ) -> String {
        let mut record = provider_record(id);
        record.enabled = enabled;
        let namespace = format!("{id}@account");
        lifecycle
            .hub
            .add_auth_provider_with_model_cache_details(record, &namespace, 1, models)
            .unwrap();
        namespace
    }

    fn seed_legacy_cached_provider(lifecycle: &ProviderLifecycle, id: &str, enabled: bool) {
        let mut record = provider_record(id);
        record.enabled = enabled;
        record.model_cache = Some(crate::auth_store::ModelCache {
            fetched_at: 1,
            models: vec![crate::auth_store::CachedModel {
                slug: "cached".into(),
                context_budget: Some(128_000),
                thinking: true,
            }],
        });
        lifecycle.hub.add_auth_provider(record).unwrap();
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
    fn concurrent_config_provider_updates_leave_config_and_live_registries_in_sync() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                crate::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let registry = ProviderRegistry::new();
        let lifecycle = ProviderLifecycle::new(hub.clone(), registry.clone());
        lifecycle
            .create_config_provider(ProviderConfigUpdate {
                name: "gateway",
                kind: "openai-compat",
                api_key: Some("initial-key"),
                api_key_env: None,
                base_url: Some("https://gateway.example/v1"),
                max_tokens: Some(8_192),
                reasoning_format: None,
                enabled: true,
            })
            .unwrap();

        let peer_registry = ProviderRegistry::new();
        let _peer = ProviderLifecycle::new(hub.clone(), peer_registry.clone());
        let writers = 16;
        let barrier = Arc::new(std::sync::Barrier::new(writers));
        let handles = (0..writers)
            .map(|index| {
                let lifecycle = lifecycle.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let api_key = format!("key-{index}");
                    barrier.wait();
                    lifecycle
                        .update_config_provider(ProviderConfigUpdate {
                            name: "gateway",
                            kind: "openai-compat",
                            api_key: Some(&api_key),
                            api_key_env: None,
                            base_url: Some("https://gateway.example/v1"),
                            max_tokens: Some(8_192 + index as u32),
                            reasoning_format: None,
                            enabled: index % 2 == 0,
                        })
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            handle.join().unwrap();
        }
        let committed = hub.model_config().unwrap().unwrap().providers["gateway"].clone();
        let global = crate::model_registry::all_provider_entries()
            .into_iter()
            .find(|(name, _)| name == "gateway")
            .map(|(_, entry)| entry)
            .unwrap();
        assert_eq!(global.api_key, committed.api_key);
        assert_eq!(global.max_tokens, committed.max_tokens);
        assert_eq!(global.enabled, committed.enabled);
        let available = crate::config_provider::config_provider_availability(&committed)
            == crate::config_provider::ConfigProviderAvailability::Available;
        assert_eq!(registry.contains("config:gateway"), available);
        assert_eq!(peer_registry.contains("config:gateway"), available);
    }

    #[test]
    fn public_config_mutations_sync_existing_and_new_registries() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                crate::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let active = ProviderConfigUpdate {
            name: "gateway",
            kind: "openai-compat",
            api_key: Some("test-key"),
            api_key_env: None,
            base_url: Some("https://gateway.example/v1"),
            max_tokens: None,
            reasoning_format: None,
            enabled: true,
        };
        hub.upsert_provider(active).unwrap();
        let registry = ProviderRegistry::new();
        let lifecycle = ProviderLifecycle::new(hub.clone(), registry.clone());
        lifecycle.reload_config_providers().unwrap();
        assert!(registry.contains("config:gateway"));

        hub.upsert_provider(ProviderConfigUpdate {
            enabled: false,
            ..active
        })
        .unwrap();
        assert!(!registry.contains("config:gateway"));
        let before_attach_registry = ProviderRegistry::new();
        let _before_attach = ProviderLifecycle::new(hub.clone(), before_attach_registry.clone());
        assert!(!before_attach_registry.contains("config:gateway"));

        hub.upsert_provider(active).unwrap();
        assert!(registry.contains("config:gateway"));
        assert!(before_attach_registry.contains("config:gateway"));
        let after_attach_registry = ProviderRegistry::new();
        let _after_attach = ProviderLifecycle::new(hub.clone(), after_attach_registry.clone());
        assert!(after_attach_registry.contains("config:gateway"));

        std::fs::write(
            hub.config_toml_path(),
            "[providers.gateway]\nkind = \"openai-compat\"\napi_key = \"reloaded-key\"\nenabled = false\n",
        )
        .unwrap();
        hub.reload().unwrap();
        assert!(!registry.contains("config:gateway"));
        assert!(!before_attach_registry.contains("config:gateway"));
        assert!(!after_attach_registry.contains("config:gateway"));
        let reloaded_registry = ProviderRegistry::new();
        let _reloaded = ProviderLifecycle::new(hub.clone(), reloaded_registry.clone());
        assert!(!reloaded_registry.contains("config:gateway"));
    }

    #[test]
    fn public_config_mutations_fan_out_across_distinct_auth_stores() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                crate::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let dir = tempfile::tempdir().unwrap();
        let hub_a = ConfigHub::from_auth_path(dir.path().join("auth-a.json"));
        let hub_b = ConfigHub::from_auth_path(dir.path().join("auth-b.json"));
        let registry_a = ProviderRegistry::new();
        let registry_b = ProviderRegistry::new();
        let lifecycle_a = ProviderLifecycle::new(hub_a.clone(), registry_a.clone());
        let lifecycle_b = ProviderLifecycle::new(hub_b.clone(), registry_b.clone());

        assert!(!Arc::ptr_eq(&lifecycle_a.state, &lifecycle_b.state));
        assert!(Arc::ptr_eq(
            &lifecycle_a.config_state,
            &lifecycle_b.config_state
        ));

        let active = ProviderConfigUpdate {
            name: "gateway",
            kind: "openai-compat",
            api_key: Some("test-key"),
            api_key_env: None,
            base_url: Some("https://gateway.example/v1"),
            max_tokens: None,
            reasoning_format: None,
            enabled: true,
        };
        hub_a.upsert_provider(active).unwrap();
        assert!(registry_a.contains("config:gateway"));
        assert!(registry_b.contains("config:gateway"));

        hub_b
            .upsert_provider(ProviderConfigUpdate {
                enabled: false,
                ..active
            })
            .unwrap();
        assert!(!registry_a.contains("config:gateway"));
        assert!(!registry_b.contains("config:gateway"));

        let registry_c = ProviderRegistry::new();
        let lifecycle_c = ProviderLifecycle::new(hub_a.clone(), registry_c.clone());
        assert!(Arc::ptr_eq(
            &lifecycle_a.config_state,
            &lifecycle_c.config_state
        ));
        assert!(!registry_c.contains("config:gateway"));
    }

    #[test]
    fn public_config_reload_fans_out_across_distinct_auth_stores() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                crate::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let dir = tempfile::tempdir().unwrap();
        let hub_a = ConfigHub::from_auth_path(dir.path().join("auth-a.json"));
        let hub_b = ConfigHub::from_auth_path(dir.path().join("auth-b.json"));
        let registry_a = ProviderRegistry::new();
        let registry_b = ProviderRegistry::new();
        let _lifecycle_a = ProviderLifecycle::new(hub_a.clone(), registry_a.clone());
        let _lifecycle_b = ProviderLifecycle::new(hub_b.clone(), registry_b.clone());

        std::fs::write(
            hub_a.config_toml_path(),
            "[providers.gateway]\nkind = \"openai-compat\"\napi_key = \"first-key\"\nenabled = true\n",
        )
        .unwrap();
        hub_a.reload().unwrap();
        assert!(registry_a.contains("config:gateway"));
        assert!(registry_b.contains("config:gateway"));

        std::fs::write(
            hub_b.config_toml_path(),
            "[providers.gateway]\nkind = \"openai-compat\"\napi_key = \"second-key\"\nenabled = false\n",
        )
        .unwrap();
        hub_b.reload().unwrap();
        assert!(!registry_a.contains("config:gateway"));
        assert!(!registry_b.contains("config:gateway"));

        let projected = crate::model_registry::all_provider_entries()
            .into_iter()
            .find(|(name, _)| name == "gateway")
            .map(|(_, entry)| entry)
            .unwrap();
        assert_eq!(projected.api_key.as_deref(), Some("second-key"));
        assert_eq!(projected.enabled, Some(false));
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
    fn pre_discovered_install_does_not_run_discovery() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-pre-discovered");
            remove_provider_catalog("lifecycle-pre-discovered");
            provider.fail_once(ModelDiscoveryError::Transport("unused".into()));

            lifecycle
                .install_pre_discovered_provider(
                    provider_record("lifecycle-pre-discovered"),
                    provider.clone(),
                    vec![model("provided")],
                )
                .await
                .unwrap();

            assert_eq!(provider.discovery_calls(), 0);
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("lifecycle-pre-discovered")
                .unwrap()
                .unwrap();
            assert!(crate::model_registry::model_entry(&format!("{namespace}:provided")).is_some());
            assert!(matches!(
                lifecycle.refresh_models("lifecycle-pre-discovered").await,
                Err(ProviderLifecycleError::Discovery(
                    ModelDiscoveryError::Transport(message)
                )) if message == "unused"
            ));
            lifecycle
                .remove_provider("lifecycle-pre-discovered")
                .unwrap();
        });
    }

    #[test]
    fn cached_restore_is_offline_and_reuses_the_shared_provider() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-restore");
            remove_provider_catalog("lifecycle-restore");
            seed_legacy_cached_provider(&lifecycle, "lifecycle-restore", true);
            provider.fail_once(ModelDiscoveryError::Transport("offline".into()));

            let first = lifecycle
                .restore_provider("lifecycle-restore", ProviderKind::Codex, provider.clone())
                .await
                .unwrap();

            assert_eq!(provider.discovery_calls(), 0);
            assert!(first.state.auth_changed);
            assert!(first.state.live_changed);
            assert!(first.state.catalog_changed);
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("lifecycle-restore")
                .unwrap()
                .unwrap();
            assert!(crate::model_registry::model_entry(&format!("{namespace}:cached")).is_some());
            let expected: Arc<dyn Provider> = provider.clone();
            assert!(Arc::ptr_eq(
                &lifecycle.providers.get("lifecycle-restore").unwrap(),
                &expected
            ));

            let peer_registry = ProviderRegistry::new();
            let peer = ProviderLifecycle::new(lifecycle.hub.clone(), peer_registry.clone());
            let replacement = Arc::new(TestProvider::new(
                "lifecycle-restore",
                vec![model("replacement")],
            ));
            let before_revision = crate::model_registry::model_catalog_revision();
            let second = peer
                .restore_provider(
                    "lifecycle-restore",
                    ProviderKind::Codex,
                    replacement.clone(),
                )
                .await
                .unwrap();

            assert_eq!(replacement.discovery_calls(), 0);
            assert_eq!(second.state, ProviderStateChange::default());
            assert_eq!(
                crate::model_registry::model_catalog_revision(),
                before_revision
            );
            assert!(Arc::ptr_eq(
                &peer_registry.get("lifecycle-restore").unwrap(),
                &expected
            ));
            assert!(matches!(
                lifecycle.refresh_models("lifecycle-restore").await,
                Err(ProviderLifecycleError::Discovery(
                    ModelDiscoveryError::Transport(message)
                )) if message == "offline"
            ));
            assert_eq!(provider.discovery_calls(), 1);
            lifecycle.remove_provider("lifecycle-restore").unwrap();
        });
    }

    #[test]
    fn catalog_refresh_plan_contains_restored_live_providers() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-plan");
            remove_provider_catalog("catalog-refresh-plan");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-plan", true);
            lifecycle
                .hub
                .add_auth_provider(provider_record("catalog-refresh-no-live"))
                .unwrap();
            lifecycle
                .restore_provider(
                    "catalog-refresh-plan",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            assert_eq!(
                lifecycle.catalog_refresh_plan(),
                vec!["catalog-refresh-plan"]
            );
            provider.set_models(vec![model("fresh")]);
            lifecycle
                .refresh_models("catalog-refresh-plan")
                .await
                .unwrap();
            assert_eq!(
                lifecycle.catalog_refresh_plan(),
                vec!["catalog-refresh-plan"]
            );
            assert_eq!(
                lifecycle
                    .refresh_models_if_stale("catalog-refresh-plan")
                    .await
                    .unwrap(),
                ProviderCatalogRefreshOutcome::NotNeeded
            );
            assert_eq!(provider.discovery_calls(), 1);

            lifecycle.remove_provider("catalog-refresh-plan").unwrap();
            lifecycle
                .remove_provider("catalog-refresh-no-live")
                .unwrap();
        });
    }

    #[test]
    fn catalog_refresh_plan_revalidates_external_provider_mutations() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            for mutation in ["remove", "disable", "kind"] {
                let provider_id = format!("catalog-plan-{mutation}");
                let (_dir, lifecycle, provider) = fixture(&provider_id);
                remove_provider_catalog(&provider_id);
                seed_legacy_cached_provider(&lifecycle, &provider_id, true);
                lifecycle
                    .restore_provider(&provider_id, ProviderKind::Codex, provider)
                    .await
                    .unwrap();

                let external_hub = ConfigHub::from_config_dir(lifecycle.hub.config_dir());
                match mutation {
                    "remove" => {
                        assert!(external_hub.remove_auth_provider(&provider_id).unwrap());
                    }
                    "disable" => {
                        assert_eq!(
                            external_hub
                                .set_auth_provider_enabled_with_change(&provider_id, false)
                                .unwrap(),
                            Some(true)
                        );
                    }
                    "kind" => external_hub
                        .update_auth(|store| {
                            store.providers[0].kind = ProviderKind::AnthropicOauth;
                            Ok(())
                        })
                        .unwrap(),
                    _ => unreachable!(),
                }

                assert_eq!(lifecycle.catalog_refresh_plan(), vec![provider_id.clone()]);
                let result = lifecycle.refresh_models_if_stale(&provider_id).await;
                match mutation {
                    "remove" => assert!(matches!(
                        result,
                        Err(ProviderLifecycleError::ProviderNotFound { .. })
                    )),
                    "disable" => assert!(matches!(
                        result,
                        Err(ProviderLifecycleError::ProviderDisabled { .. })
                    )),
                    "kind" => assert!(matches!(result, Err(ProviderLifecycleError::Stale { .. }))),
                    _ => unreachable!(),
                }
                assert!(!lifecycle.providers.contains(&provider_id));
                assert!(provider_catalog_namespace(&provider_id).is_none());
                if mutation != "remove" {
                    lifecycle.remove_provider(&provider_id).unwrap();
                }
            }
        });
    }

    #[test]
    fn fresh_external_cache_is_hydrated_when_plan_observes_fresh_storage() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-external-cache");
            remove_provider_catalog("catalog-refresh-external-cache");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-external-cache", true);
            lifecycle
                .restore_provider(
                    "catalog-refresh-external-cache",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("catalog-refresh-external-cache")
                .unwrap()
                .unwrap();
            let external_hub = ConfigHub::from_config_dir(lifecycle.hub.config_dir());
            external_hub
                .update_auth_model_cache_details(
                    "catalog-refresh-external-cache",
                    &namespace,
                    chrono::Utc::now().timestamp(),
                    &[model_with_effort("cached", ReasoningEffort::High)],
                )
                .unwrap();
            assert_eq!(
                lifecycle.catalog_refresh_plan(),
                vec!["catalog-refresh-external-cache"]
            );
            let registry_key = format!("{namespace}:cached");
            assert!(
                crate::model_registry::model_entry(&registry_key)
                    .unwrap()
                    .reasoning_efforts
                    .is_empty()
            );

            assert!(matches!(
                lifecycle
                    .refresh_models_if_stale("catalog-refresh-external-cache")
                    .await
                    .unwrap(),
                ProviderCatalogRefreshOutcome::CatalogUpdated(delta)
                    if delta.updated == 1 && delta.total == 1
            ));
            assert_eq!(provider.discovery_calls(), 0);
            assert_eq!(
                crate::model_registry::model_entry(&registry_key)
                    .unwrap()
                    .reasoning_efforts,
                vec![ReasoningEffort::High]
            );

            lifecycle
                .remove_provider("catalog-refresh-external-cache")
                .unwrap();
        });
    }

    #[test]
    fn stale_network_commit_hydrates_the_external_cache_winner() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-external-winner");
            remove_provider_catalog("catalog-refresh-external-winner");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-external-winner", true);
            lifecycle
                .restore_provider(
                    "catalog-refresh-external-winner",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("catalog-refresh-external-winner")
                .unwrap()
                .unwrap();
            provider.set_models(vec![model_with_effort("loser", ReasoningEffort::Low)]);
            let (started, proceed) = provider.block_once();
            let background_lifecycle = lifecycle.clone();
            let background = tokio::spawn(async move {
                background_lifecycle
                    .refresh_models_if_stale("catalog-refresh-external-winner")
                    .await
            });
            started.await.unwrap();

            let external_hub = ConfigHub::from_config_dir(lifecycle.hub.config_dir());
            external_hub
                .update_auth_model_cache_details(
                    "catalog-refresh-external-winner",
                    &namespace,
                    chrono::Utc::now().timestamp(),
                    &[model_with_effort("winner", ReasoningEffort::High)],
                )
                .unwrap();
            proceed.send(()).unwrap();

            assert!(matches!(
                background.await.unwrap().unwrap(),
                ProviderCatalogRefreshOutcome::CatalogUpdated(delta)
                    if delta.added == 1 && delta.removed == 1 && delta.total == 1
            ));
            assert_eq!(provider.discovery_calls(), 1);
            assert!(crate::model_registry::model_entry(&format!("{namespace}:loser")).is_none());
            assert_eq!(
                crate::model_registry::model_entry(&format!("{namespace}:winner"))
                    .unwrap()
                    .reasoning_efforts,
                vec![ReasoningEffort::High]
            );
            assert_eq!(
                lifecycle.hub.load_auth().unwrap().providers[0]
                    .model_cache
                    .as_ref()
                    .unwrap()
                    .models[0]
                    .slug,
                "winner"
            );

            lifecycle
                .remove_provider("catalog-refresh-external-winner")
                .unwrap();
        });
    }

    #[test]
    fn failed_network_discovery_hydrates_the_external_cache_winner() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-error-winner");
            remove_provider_catalog("catalog-refresh-error-winner");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-error-winner", true);
            lifecycle
                .restore_provider(
                    "catalog-refresh-error-winner",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            let namespace = lifecycle
                .hub
                .load_auth_model_namespace("catalog-refresh-error-winner")
                .unwrap()
                .unwrap();
            provider.fail_once(ModelDiscoveryError::Transport("offline".into()));
            let (started, proceed) = provider.block_once();
            let background_lifecycle = lifecycle.clone();
            let background = tokio::spawn(async move {
                background_lifecycle
                    .refresh_models_if_stale("catalog-refresh-error-winner")
                    .await
            });
            started.await.unwrap();

            ConfigHub::from_config_dir(lifecycle.hub.config_dir())
                .update_auth_model_cache_details(
                    "catalog-refresh-error-winner",
                    &namespace,
                    chrono::Utc::now().timestamp(),
                    &[model_with_effort("winner", ReasoningEffort::High)],
                )
                .unwrap();
            proceed.send(()).unwrap();

            assert!(matches!(
                background.await.unwrap().unwrap(),
                ProviderCatalogRefreshOutcome::CatalogUpdated(delta)
                    if delta.added == 1 && delta.removed == 1 && delta.total == 1
            ));
            assert_eq!(provider.discovery_calls(), 1);
            assert_eq!(
                crate::model_registry::model_entry(&format!("{namespace}:winner"))
                    .unwrap()
                    .reasoning_efforts,
                vec![ReasoningEffort::High]
            );

            lifecycle
                .remove_provider("catalog-refresh-error-winner")
                .unwrap();
        });
    }

    #[test]
    fn cache_hydration_cas_remove_and_disable_prune_local_runtime() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            for (provider_id, remove) in [
                ("catalog-hydrate-removed", true),
                ("catalog-hydrate-disabled", false),
            ] {
                let (_dir, lifecycle, provider) = fixture(provider_id);
                remove_provider_catalog(provider_id);
                seed_cached_provider(&lifecycle, provider_id, true, &[model("cached")]);
                let expected_live: Arc<dyn Provider> = provider.clone();
                lifecycle
                    .restore_provider(provider_id, ProviderKind::Codex, provider)
                    .await
                    .unwrap();
                let (attempt, deferred) = lifecycle.load_catalog_refresh_attempt(provider_id, 1);
                assert!(deferred.is_empty());
                let attempt = attempt.unwrap();
                let external_hub = ConfigHub::from_config_dir(lifecycle.hub.config_dir());
                if remove {
                    external_hub.remove_auth_provider(provider_id).unwrap();
                } else {
                    external_hub
                        .set_auth_provider_enabled(provider_id, false)
                        .unwrap();
                }

                let (result, deferred) =
                    lifecycle.hydrate_cached_catalog_if_current(provider_id, &attempt);
                drop(deferred);
                if remove {
                    assert!(matches!(
                        result,
                        Err(ProviderLifecycleError::ProviderNotFound { .. })
                    ));
                } else {
                    assert!(matches!(result, Err(ProviderLifecycleError::Stale { .. })));
                    let (result, deferred) = lifecycle.prepare_catalog_refresh(
                        provider_id,
                        1,
                        true,
                        Some(&expected_live),
                        Some(&attempt.runtime.catalog_snapshot),
                    );
                    drop(deferred);
                    assert!(matches!(
                        result,
                        Err(ProviderLifecycleError::ProviderDisabled { .. })
                    ));
                }
                assert!(!lifecycle.providers.contains(provider_id));
                assert!(provider_catalog_namespace(provider_id).is_none());
            }
        });
    }

    #[test]
    fn cache_hydration_cas_kind_change_is_reconciled_fail_closed() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let provider_id = "catalog-hydrate-kind-change";
            let (_dir, lifecycle, provider) = fixture(provider_id);
            remove_provider_catalog(provider_id);
            seed_cached_provider(&lifecycle, provider_id, true, &[model("cached")]);
            lifecycle
                .restore_provider(provider_id, ProviderKind::Codex, provider.clone())
                .await
                .unwrap();
            let expected_live: Arc<dyn Provider> = provider;
            let (attempt, deferred) = lifecycle.load_catalog_refresh_attempt(provider_id, 1);
            assert!(deferred.is_empty());
            let attempt = attempt.unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store.providers[0].kind = ProviderKind::AnthropicOauth;
                    Ok(())
                })
                .unwrap();

            let (result, deferred) =
                lifecycle.hydrate_cached_catalog_if_current(provider_id, &attempt);
            assert!(deferred.is_empty());
            assert!(matches!(result, Err(ProviderLifecycleError::Stale { .. })));
            let (result, deferred) = lifecycle.prepare_catalog_refresh(
                provider_id,
                1,
                true,
                Some(&expected_live),
                Some(&attempt.runtime.catalog_snapshot),
            );
            drop(deferred);
            assert!(matches!(result, Err(ProviderLifecycleError::Stale { .. })));
            assert!(!lifecycle.providers.contains(provider_id));
            assert!(provider_catalog_namespace(provider_id).is_none());
            lifecycle.remove_provider(provider_id).unwrap();
        });
    }

    #[test]
    fn background_refresh_is_deduplicated_without_blocking_runtime_restore() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-background");
            remove_provider_catalog("catalog-refresh-background");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-background", true);
            lifecycle
                .restore_provider(
                    "catalog-refresh-background",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            provider.set_models(vec![model("fresh")]);
            let (started, proceed) = provider.block_once();
            let background_lifecycle = lifecycle.clone();
            let background = tokio::spawn(async move {
                background_lifecycle
                    .refresh_models_if_stale("catalog-refresh-background")
                    .await
            });
            started.await.unwrap();

            assert!(
                lifecycle
                    .operation_lock("catalog-refresh-background")
                    .try_lock()
                    .is_ok()
            );
            assert_eq!(
                lifecycle
                    .refresh_models_if_stale("catalog-refresh-background")
                    .await
                    .unwrap(),
                ProviderCatalogRefreshOutcome::AlreadyInFlight
            );
            let restored = lifecycle
                .restore_provider(
                    "catalog-refresh-background",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            assert_eq!(restored.state, ProviderStateChange::default());

            proceed.send(()).unwrap();
            assert!(matches!(
                background.await.unwrap().unwrap(),
                ProviderCatalogRefreshOutcome::CatalogUpdated(delta) if delta.total == 1
            ));
            assert_eq!(provider.discovery_calls(), 1);
            assert_eq!(
                lifecycle
                    .refresh_models_if_stale("catalog-refresh-background")
                    .await
                    .unwrap(),
                ProviderCatalogRefreshOutcome::NotNeeded
            );
            assert_eq!(provider.discovery_calls(), 1);

            lifecycle
                .remove_provider("catalog-refresh-background")
                .unwrap();
        });
    }

    #[test]
    fn concurrent_background_refresh_returns_already_in_flight() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("catalog-refresh-failure");
            remove_provider_catalog("catalog-refresh-failure");
            seed_legacy_cached_provider(&lifecycle, "catalog-refresh-failure", true);
            lifecycle
                .restore_provider(
                    "catalog-refresh-failure",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            provider.fail_once(ModelDiscoveryError::Transport("offline".into()));
            let (started, proceed) = provider.block_once();
            let background_lifecycle = lifecycle.clone();
            let background = tokio::spawn(async move {
                background_lifecycle
                    .refresh_models_if_stale("catalog-refresh-failure")
                    .await
            });
            started.await.unwrap();

            assert_eq!(
                lifecycle
                    .refresh_models_if_stale("catalog-refresh-failure")
                    .await
                    .unwrap(),
                ProviderCatalogRefreshOutcome::AlreadyInFlight
            );
            assert_eq!(provider.discovery_calls(), 1);
            proceed.send(()).unwrap();
            assert!(matches!(
                background.await.unwrap(),
                Err(ProviderLifecycleError::Discovery(
                    ModelDiscoveryError::Transport(message)
                )) if message == "offline"
            ));
            assert_eq!(provider.discovery_calls(), 1);

            lifecycle
                .remove_provider("catalog-refresh-failure")
                .unwrap();
        });
    }

    #[test]
    fn enabling_a_cached_provider_is_atomic_and_offline() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-enable");
            remove_provider_catalog("lifecycle-enable");
            let namespace =
                seed_cached_provider(&lifecycle, "lifecycle-enable", false, &[model("cached")]);
            provider.fail_once(ModelDiscoveryError::Transport("unused".into()));

            let outcome = lifecycle
                .enable_provider("lifecycle-enable", ProviderKind::Codex, provider.clone())
                .await
                .unwrap();

            assert_eq!(provider.discovery_calls(), 0);
            assert_eq!(
                outcome.state,
                ProviderStateChange {
                    auth_changed: true,
                    live_changed: true,
                    catalog_changed: true,
                }
            );
            assert!(lifecycle.hub.load_auth().unwrap().providers[0].enabled);
            assert!(lifecycle.providers.contains("lifecycle-enable"));
            assert!(crate::model_registry::model_entry(&format!("{namespace}:cached")).is_some());
            lifecycle.remove_provider("lifecycle-enable").unwrap();
        });
    }

    #[test]
    fn disabled_restore_ignores_an_invalid_cached_catalog() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-disabled-cache");
            remove_provider_catalog("lifecycle-disabled-cache");
            let mut record = provider_record("lifecycle-disabled-cache");
            record.enabled = false;
            record.model_cache = Some(crate::auth_store::ModelCache {
                fetched_at: 1,
                models: vec![
                    crate::auth_store::CachedModel {
                        slug: "duplicate".into(),
                        context_budget: None,
                        thinking: false,
                    },
                    crate::auth_store::CachedModel {
                        slug: "duplicate".into(),
                        context_budget: None,
                        thinking: false,
                    },
                ],
            });
            lifecycle.hub.add_auth_provider(record).unwrap();

            assert!(matches!(
                lifecycle
                    .restore_provider(
                        "lifecycle-disabled-cache",
                        ProviderKind::Codex,
                        provider.clone(),
                    )
                    .await,
                Err(ProviderLifecycleError::ProviderDisabled { id })
                    if id == "lifecycle-disabled-cache"
            ));
            assert_eq!(provider.discovery_calls(), 0);
            assert!(!lifecycle.providers.contains("lifecycle-disabled-cache"));
            assert!(provider_catalog_namespace("lifecycle-disabled-cache").is_none());
            lifecycle
                .remove_provider("lifecycle-disabled-cache")
                .unwrap();
        });
    }

    #[test]
    fn restoring_without_cache_removes_the_stale_catalog() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-empty-cache");
            remove_provider_catalog("lifecycle-empty-cache");
            lifecycle
                .install_provider(provider_record("lifecycle-empty-cache"), provider.clone())
                .await
                .unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store.providers[0].model_cache = None;
                    Ok(())
                })
                .unwrap();
            let replacement = Arc::new(TestProvider::new(
                "lifecycle-empty-cache",
                vec![model("unused")],
            ));

            let outcome = lifecycle
                .restore_provider(
                    "lifecycle-empty-cache",
                    ProviderKind::Codex,
                    replacement.clone(),
                )
                .await
                .unwrap();

            assert_eq!(provider.discovery_calls(), 1);
            assert_eq!(replacement.discovery_calls(), 0);
            assert!(!outcome.state.live_changed);
            assert!(outcome.state.catalog_changed);
            assert!(outcome.catalog.is_none());
            assert!(lifecycle.providers.contains("lifecycle-empty-cache"));
            assert!(provider_catalog_namespace("lifecycle-empty-cache").is_none());
            lifecycle.remove_provider("lifecycle-empty-cache").unwrap();
        });
    }

    #[test]
    fn reconcile_prunes_external_disable_remove_and_kind_change() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let lifecycle = ProviderLifecycle::new(
                ConfigHub::from_config_dir(dir.path()),
                ProviderRegistry::new(),
            );
            let ids = ["lifecycle-disabled", "lifecycle-removed", "lifecycle-kind"];
            for id in ids {
                remove_provider_catalog(id);
                lifecycle
                    .install_provider(
                        provider_record(id),
                        Arc::new(TestProvider::new(id, vec![model("cached")])),
                    )
                    .await
                    .unwrap();
            }
            let peer_registry = ProviderRegistry::new();
            let _peer = ProviderLifecycle::new(lifecycle.hub.clone(), peer_registry.clone());

            lifecycle
                .hub
                .set_auth_provider_enabled("lifecycle-disabled", false)
                .unwrap();
            lifecycle
                .hub
                .remove_auth_provider("lifecycle-removed")
                .unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store
                        .providers
                        .iter_mut()
                        .find(|provider| provider.id == "lifecycle-kind")
                        .unwrap()
                        .kind = ProviderKind::AnthropicOauth;
                    Ok(())
                })
                .unwrap();

            let outcome = lifecycle.reconcile_inactive_providers().unwrap();

            assert_eq!(outcome.providers.len(), 3);
            for id in ids {
                assert!(!lifecycle.providers.contains(id));
                assert!(!peer_registry.contains(id));
                assert!(provider_catalog_namespace(id).is_none());
            }
            let store = lifecycle.hub.load_auth().unwrap();
            assert_eq!(store.providers.len(), 2);
            assert!(
                !store
                    .providers
                    .iter()
                    .find(|provider| provider.id == "lifecycle-disabled")
                    .unwrap()
                    .enabled
            );
            assert_eq!(
                store
                    .providers
                    .iter()
                    .find(|provider| provider.id == "lifecycle-kind")
                    .unwrap()
                    .kind,
                ProviderKind::AnthropicOauth
            );
            lifecycle.remove_provider("lifecycle-disabled").unwrap();
            lifecycle.remove_provider("lifecycle-kind").unwrap();
        });
    }

    #[test]
    fn reconcile_auth_parse_error_prunes_live_runtime() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-reconcile-invalid");
            remove_provider_catalog("lifecycle-reconcile-invalid");
            lifecycle
                .install_provider(provider_record("lifecycle-reconcile-invalid"), provider)
                .await
                .unwrap();
            std::fs::write(lifecycle.hub.auth_path(), b"{").unwrap();

            assert!(matches!(
                lifecycle.reconcile_inactive_providers(),
                Err(ProviderLifecycleError::Config(_))
            ));
            assert!(!lifecycle.providers.contains("lifecycle-reconcile-invalid"));
            assert!(provider_catalog_namespace("lifecycle-reconcile-invalid").is_none());
        });
    }

    #[test]
    fn provider_mutation_auth_errors_fail_closed() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            for (id, remove) in [
                ("lifecycle-disable-invalid", false),
                ("lifecycle-remove-invalid", true),
            ] {
                let (_dir, lifecycle, provider) = fixture(id);
                remove_provider_catalog(id);
                lifecycle
                    .install_provider(provider_record(id), provider)
                    .await
                    .unwrap();
                std::fs::write(lifecycle.hub.auth_path(), b"{").unwrap();

                let result = if remove {
                    lifecycle.remove_provider(id)
                } else {
                    lifecycle.disable_provider(id)
                };
                assert!(matches!(result, Err(ProviderLifecycleError::Config(_))));
                assert!(!lifecycle.providers.contains(id));
                assert!(provider_catalog_namespace(id).is_none());
            }
        });
    }

    #[test]
    fn kind_change_after_construction_fails_closed() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-kind-race");
            remove_provider_catalog("lifecycle-kind-race");
            seed_cached_provider(&lifecycle, "lifecycle-kind-race", true, &[model("cached")]);
            lifecycle
                .restore_provider("lifecycle-kind-race", ProviderKind::Codex, provider)
                .await
                .unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store.providers[0].kind = ProviderKind::AnthropicOauth;
                    Ok(())
                })
                .unwrap();
            let stale_candidate = Arc::new(TestProvider::new(
                "lifecycle-kind-race",
                vec![model("unused")],
            ));

            assert!(matches!(
                lifecycle
                    .restore_provider(
                        "lifecycle-kind-race",
                        ProviderKind::Codex,
                        stale_candidate,
                    )
                    .await,
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-kind-race"
            ));
            assert!(!lifecycle.providers.contains("lifecycle-kind-race"));
            assert!(provider_catalog_namespace("lifecycle-kind-race").is_none());
            lifecycle.remove_provider("lifecycle-kind-race").unwrap();
        });
    }

    #[test]
    fn invalid_cached_catalog_fails_closed() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-invalid-cache");
            remove_provider_catalog("lifecycle-invalid-cache");
            seed_cached_provider(
                &lifecycle,
                "lifecycle-invalid-cache",
                true,
                &[model("cached")],
            );
            let operation_lock = lifecycle.operation_lock("lifecycle-invalid-cache");
            let dropped = Arc::new(AtomicBool::new(false));
            let dropped_from_callback = dropped.clone();
            provider.on_drop(move || {
                assert!(operation_lock.try_lock().is_ok());
                dropped_from_callback.store(true, Ordering::SeqCst);
            });
            lifecycle
                .restore_provider("lifecycle-invalid-cache", ProviderKind::Codex, provider)
                .await
                .unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store.providers[0].model_cache = Some(crate::auth_store::ModelCache {
                        fetched_at: 2,
                        models: vec![
                            crate::auth_store::CachedModel {
                                slug: "duplicate".into(),
                                context_budget: None,
                                thinking: false,
                            },
                            crate::auth_store::CachedModel {
                                slug: "duplicate".into(),
                                context_budget: None,
                                thinking: false,
                            },
                        ],
                    });
                    Ok(())
                })
                .unwrap();

            assert!(matches!(
                lifecycle
                    .restore_provider(
                        "lifecycle-invalid-cache",
                        ProviderKind::Codex,
                        Arc::new(TestProvider::new(
                            "lifecycle-invalid-cache",
                            vec![model("unused")],
                        )),
                    )
                    .await,
                Err(ProviderLifecycleError::Catalog(CatalogError::DuplicateModel { model }))
                    if model == "duplicate"
            ));
            assert!(!lifecycle.providers.contains("lifecycle-invalid-cache"));
            assert!(provider_catalog_namespace("lifecycle-invalid-cache").is_none());
            assert!(dropped.load(Ordering::SeqCst));
            lifecycle
                .remove_provider("lifecycle-invalid-cache")
                .unwrap();
        });
    }

    #[test]
    fn refresh_cancellation_releases_the_operation_before_provider_drop() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-cancel");
            remove_provider_catalog("lifecycle-refresh-cancel");
            lifecycle
                .install_provider(
                    provider_record("lifecycle-refresh-cancel"),
                    provider.clone(),
                )
                .await
                .unwrap();
            let operation_lock = lifecycle.operation_lock("lifecycle-refresh-cancel");
            let dropped = Arc::new(AtomicBool::new(false));
            let dropped_from_callback = dropped.clone();
            provider.on_drop(move || {
                assert!(operation_lock.try_lock().is_ok());
                dropped_from_callback.store(true, Ordering::SeqCst);
            });
            let (started, _proceed) = provider.block_once();
            drop(provider);
            let refresh_lifecycle = lifecycle.clone();
            let refresh = tokio::spawn(async move {
                refresh_lifecycle
                    .refresh_models("lifecycle-refresh-cancel")
                    .await
            });
            started.await.unwrap();
            lifecycle
                .disable_provider("lifecycle-refresh-cancel")
                .unwrap();

            refresh.abort();
            assert!(refresh.await.unwrap_err().is_cancelled());
            assert!(dropped.load(Ordering::SeqCst));
            lifecycle
                .remove_provider("lifecycle-refresh-cancel")
                .unwrap();
        });
    }

    #[test]
    fn provider_id_compare_and_swap_rejects_stale_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let mut target = provider_record("namespace-target");
        target.enabled = false;
        hub.add_auth_provider(target).unwrap();
        let runtime = hub
            .load_or_create_auth_provider_runtime_state("namespace-target")
            .unwrap()
            .unwrap();
        hub.add_auth_provider(provider_record("namespace-peer"))
            .unwrap();
        let callback_ran = AtomicBool::new(false);

        let (commit, callback) = hub
            .commit_auth_provider_runtime_if_current_and_then(
                &runtime.provider,
                &runtime.catalog_snapshot,
                true,
                Some("namespace-target@account"),
                Some(&runtime.provider_ids),
                || callback_ran.store(true, Ordering::SeqCst),
            )
            .unwrap();

        assert_eq!(commit, AuthProviderRuntimeCommit::Changed);
        assert!(callback.is_none());
        assert!(!callback_ran.load(Ordering::SeqCst));
        let stored = hub
            .load_auth()
            .unwrap()
            .providers
            .into_iter()
            .find(|provider| provider.id == "namespace-target")
            .unwrap();
        assert!(!stored.enabled);
        assert_eq!(
            hub.load_auth_model_namespace("namespace-target").unwrap(),
            None
        );
    }

    #[test]
    fn provider_insert_rejects_a_changed_provider_id_set() {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        let expected_provider_ids = Vec::new();
        hub.add_auth_provider(provider_record("insert-peer"))
            .unwrap();
        let callback_ran = AtomicBool::new(false);

        let (commit, callback) = hub
            .add_auth_provider_with_model_cache_details_if_provider_ids_and_then(
                provider_record("insert-target"),
                Some(&expected_provider_ids),
                "insert-target@account",
                1,
                &[model("cached")],
                || callback_ran.store(true, Ordering::SeqCst),
            )
            .unwrap();

        assert_eq!(commit, AuthProviderInsertCommit::Changed);
        assert!(callback.is_none());
        assert!(!callback_ran.load(Ordering::SeqCst));
        assert!(
            hub.load_auth()
                .unwrap()
                .providers
                .iter()
                .all(|provider| provider.id != "insert-target")
        );
    }

    #[test]
    fn provider_drop_runs_after_lifecycle_locks_are_released() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-drop");
            remove_provider_catalog("lifecycle-drop");
            lifecycle
                .install_provider(provider_record("lifecycle-drop"), provider.clone())
                .await
                .unwrap();
            let state = Arc::downgrade(&lifecycle.state);
            let registry = lifecycle.providers.clone();
            let hub = lifecycle.hub.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let dropped_from_callback = dropped.clone();
            provider.on_drop(move || {
                assert!(state.upgrade().unwrap().try_lock().is_ok());
                assert!(registry.get("lifecycle-drop").is_none());
                assert!(
                    !hub.set_auth_provider_enabled("lifecycle-drop", false)
                        .unwrap()
                );
                dropped_from_callback.store(true, Ordering::SeqCst);
            });
            drop(provider);

            lifecycle.remove_provider("lifecycle-drop").unwrap();

            assert!(dropped.load(Ordering::SeqCst));
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
    fn refresh_auth_parse_error_prunes_live_runtime() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-invalid-auth");
            remove_provider_catalog("lifecycle-refresh-invalid-auth");
            lifecycle
                .install_provider(
                    provider_record("lifecycle-refresh-invalid-auth"),
                    provider.clone(),
                )
                .await
                .unwrap();
            let auth_path = lifecycle.hub.auth_path().to_path_buf();
            provider.before_discovery_once(move || {
                std::fs::write(auth_path, b"{").unwrap();
            });

            assert!(matches!(
                lifecycle
                    .refresh_models("lifecycle-refresh-invalid-auth")
                    .await,
                Err(ProviderLifecycleError::Config(_))
            ));
            assert!(
                !lifecycle
                    .providers
                    .contains("lifecycle-refresh-invalid-auth")
            );
            assert!(provider_catalog_namespace("lifecycle-refresh-invalid-auth").is_none());
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
    fn external_cache_aba_hydrates_the_authoritative_catalog() {
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
            assert_eq!(
                refresh.await.unwrap().unwrap(),
                CatalogDelta {
                    total: 1,
                    ..Default::default()
                }
            );

            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "initial");
            assert!(crate::model_registry::model_entry(&format!("{namespace}:initial")).is_some());
            assert!(crate::model_registry::model_entry(&format!("{namespace}:slow")).is_none());
            lifecycle.remove_provider("lifecycle-cache-cas").unwrap();
        });
    }

    #[test]
    fn external_enable_aba_preserves_the_authoritative_cache() {
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

            assert_eq!(
                refresh.await.unwrap().unwrap(),
                CatalogDelta {
                    total: 1,
                    ..Default::default()
                }
            );
            let stored = lifecycle.hub.load_auth().unwrap().providers.remove(0);
            assert!(stored.enabled);
            assert_eq!(stored.model_cache.unwrap().models[0].slug, "initial");
            lifecycle.remove_provider("lifecycle-enable-aba").unwrap();
        });
    }

    #[test]
    fn external_kind_change_during_refresh_prunes_the_old_runtime() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-kind");
            remove_provider_catalog("lifecycle-refresh-kind");
            lifecycle
                .install_provider(provider_record("lifecycle-refresh-kind"), provider.clone())
                .await
                .unwrap();
            provider.set_models(vec![model("replacement")]);
            let (started, proceed) = provider.block_once();
            let refresh_lifecycle = lifecycle.clone();
            let refresh = tokio::spawn(async move {
                refresh_lifecycle
                    .refresh_models("lifecycle-refresh-kind")
                    .await
            });
            started.await.unwrap();
            lifecycle
                .hub
                .update_auth(|store| {
                    store.providers[0].kind = ProviderKind::AnthropicOauth;
                    Ok(())
                })
                .unwrap();
            proceed.send(()).unwrap();

            assert!(matches!(
                refresh.await.unwrap(),
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-refresh-kind"
            ));
            assert!(!lifecycle.providers.contains("lifecycle-refresh-kind"));
            assert!(provider_catalog_namespace("lifecycle-refresh-kind").is_none());
            lifecycle.remove_provider("lifecycle-refresh-kind").unwrap();
        });
    }

    #[test]
    fn discovery_failure_observes_an_external_disable() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-disabled");
            remove_provider_catalog("lifecycle-refresh-disabled");
            lifecycle
                .install_provider(
                    provider_record("lifecycle-refresh-disabled"),
                    provider.clone(),
                )
                .await
                .unwrap();
            let hub = lifecycle.hub.clone();
            provider.before_discovery_once(move || {
                assert!(
                    hub.set_auth_provider_enabled("lifecycle-refresh-disabled", false)
                        .unwrap()
                );
            });
            provider.fail_once(ModelDiscoveryError::Transport("offline".into()));

            assert!(matches!(
                lifecycle.refresh_models("lifecycle-refresh-disabled").await,
                Err(ProviderLifecycleError::Stale { id }) if id == "lifecycle-refresh-disabled"
            ));
            assert!(!lifecycle.providers.contains("lifecycle-refresh-disabled"));
            assert!(provider_catalog_namespace("lifecycle-refresh-disabled").is_none());
            lifecycle
                .remove_provider("lifecycle-refresh-disabled")
                .unwrap();
        });
    }

    #[test]
    fn namespace_mismatch_during_refresh_fails_closed() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-namespace");
            remove_provider_catalog("lifecycle-refresh-namespace");
            lifecycle
                .install_provider(
                    provider_record("lifecycle-refresh-namespace"),
                    provider.clone(),
                )
                .await
                .unwrap();
            remove_provider_catalog("lifecycle-refresh-namespace");
            crate::model_registry::replace_provider_catalog(
                ProviderDescriptor {
                    provider_key: "lifecycle-refresh-namespace".into(),
                    provider_name: "Account".into(),
                    namespace: "unexpected@account".into(),
                    wire_profile: ReasoningWireProfile::CodexResponses,
                },
                &[model("replacement")],
            )
            .unwrap();

            assert!(matches!(
                lifecycle
                    .refresh_models("lifecycle-refresh-namespace")
                    .await,
                Err(ProviderLifecycleError::Catalog(
                    CatalogError::NamespaceChanged { .. }
                ))
            ));
            assert!(!lifecycle.providers.contains("lifecycle-refresh-namespace"));
            assert!(provider_catalog_namespace("lifecycle-refresh-namespace").is_none());
            lifecycle
                .remove_provider("lifecycle-refresh-namespace")
                .unwrap();
        });
    }

    #[test]
    fn registry_key_conflict_during_refresh_preserves_last_good_catalog() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                crate::model_registry::set_provider_config(
                    crate::model_registry::ProviderConfig::default(),
                );
            }
        }

        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _config_reset = ConfigReset;
        test_runtime().block_on(async {
            let (_dir, lifecycle, provider) = fixture("lifecycle-refresh-registry-key");
            remove_provider_catalog("lifecycle-refresh-registry-key");
            lifecycle
                .hub
                .add_auth_provider_with_model_cache_details(
                    provider_record("lifecycle-refresh-registry-key"),
                    "stable",
                    1,
                    &[model("initial")],
                )
                .unwrap();
            lifecycle
                .restore_provider(
                    "lifecycle-refresh-registry-key",
                    ProviderKind::Codex,
                    provider.clone(),
                )
                .await
                .unwrap();
            let mut config = crate::model_registry::ProviderConfig::default();
            config.providers.insert(
                "stable".into(),
                crate::model_registry::ProviderEntry {
                    kind: "openai".into(),
                    base_url: Some("https://api.openai.com/v1".into()),
                    ..Default::default()
                },
            );
            crate::model_registry::set_provider_config(config);
            let before_revision = crate::model_registry::model_catalog_revision();
            provider.set_models(vec![model("gpt-4o")]);

            assert!(matches!(
                lifecycle
                    .refresh_models("lifecycle-refresh-registry-key")
                    .await,
                Err(ProviderLifecycleError::Catalog(
                    CatalogError::RegistryKeyInUse { .. }
                ))
            ));
            assert!(
                lifecycle
                    .providers
                    .contains("lifecycle-refresh-registry-key")
            );
            assert!(crate::model_registry::model_entry("stable:initial").is_some());
            assert_eq!(
                crate::model_registry::model_catalog_revision(),
                before_revision
            );
            lifecycle
                .remove_provider("lifecycle-refresh-registry-key")
                .unwrap();
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
