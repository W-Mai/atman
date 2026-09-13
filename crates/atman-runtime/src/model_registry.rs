use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{LazyLock, RwLock};

use crate::auth_store::AuthStore;
use crate::provider::{
    CapabilityKnowledge, ImageDetail, InputModality, ModelCapabilities, ReasoningEffort,
    ReasoningExecutionMode, ReasoningSelection, ReasoningWireProfile,
};

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub name: String,
    pub context_budget: u64,
    pub compact_threshold_ratio: f64,
    pub reasoning: ReasoningSelection,
    pub capabilities: ModelCapabilities,
    pub image_detail: ImageDetail,
    pub max_output_tokens: Option<u32>,
}

pub const DEFAULT_CONFIG_PROVIDER_TYPE: &str = "openai-compat";

pub fn config_provider_types() -> Vec<&'static str> {
    let mut types = Vec::new();
    for preset in PROVIDER_PRESETS {
        if preset.provider_type == "codex" {
            continue;
        }
        if !types.contains(&preset.provider_type) {
            types.push(preset.provider_type);
        }
    }
    if types.is_empty() {
        types.push(DEFAULT_CONFIG_PROVIDER_TYPE);
    }
    types
}

#[derive(Debug, Clone, Default)]
pub struct ProviderEntry {
    pub name: String,
    pub kind: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub reasoning_format: Option<crate::providers::openai::OpenAiReasoningFormat>,
    pub prompt_cache_key: Option<bool>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelEntry {
    pub model: String,
    pub provider: Option<String>,
    pub context_budget: Option<u64>,
    pub compact_threshold_ratio: Option<f64>,
    pub thinking: Option<bool>,
    pub reasoning: Option<String>,
    pub reasoning_mode: Option<String>,
    pub reasoning_budget_tokens: Option<u32>,
    pub reasoning_efforts: Vec<ReasoningEffort>,
    pub default_reasoning_effort: Option<ReasoningEffort>,
    pub reasoning_modes: Vec<ReasoningExecutionMode>,
    pub default_reasoning_mode: Option<ReasoningExecutionMode>,
    pub input_modalities: Vec<InputModality>,
    pub image_detail: Option<ImageDetail>,
    pub max_tokens: Option<u32>,
    pub enabled: Option<bool>,
    #[allow(dead_code)]
    pub discovered: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AliasEntry {
    pub model: String,
}

#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    pub providers: HashMap<String, ProviderEntry>,
    pub models: HashMap<String, ModelEntry>,
    pub aliases: HashMap<String, AliasEntry>,
}

/// Backwards-compatible alias — ProviderConfig is the canonical name.
pub type ModelConfig = ProviderConfig;

#[derive(Debug, Clone, Default)]
struct CapabilityDeclarations {
    reasoning_efforts: bool,
    reasoning_modes: bool,
    input_modalities: bool,
}

impl CapabilityDeclarations {
    fn inferred(entry: &ModelEntry) -> Self {
        Self {
            reasoning_efforts: !entry.reasoning_efforts.is_empty(),
            reasoning_modes: !entry.reasoning_modes.is_empty(),
            input_modalities: !entry.input_modalities.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ConfigLayer {
    values: ProviderConfig,
    capabilities: HashMap<String, CapabilityDeclarations>,
}

impl ConfigLayer {
    fn inferred(values: ProviderConfig) -> Self {
        let capabilities = values
            .models
            .iter()
            .map(|(name, entry)| (name.clone(), CapabilityDeclarations::inferred(entry)))
            .collect();
        Self {
            values,
            capabilities,
        }
    }

    fn declarations(&self, name: &str) -> CapabilityDeclarations {
        self.capabilities.get(name).cloned().unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ModelIdentity {
    provider_key: String,
    api_model: String,
}

impl ModelIdentity {
    fn new(provider_key: impl Into<String>, api_model: impl Into<String>) -> Self {
        Self {
            provider_key: provider_key.into(),
            api_model: api_model.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct PresetModelEntry {
    registry_key: String,
    entry: ModelEntry,
}

#[derive(Debug, Clone)]
struct CatalogModelEntry {
    registry_key: String,
    api_model: String,
    context_budget: Option<u64>,
    capability_knowledge: CapabilityKnowledge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderDescriptor {
    pub provider_key: String,
    pub provider_name: String,
    pub namespace: String,
    pub wire_profile: ReasoningWireProfile,
}

#[derive(Debug, Clone)]
struct ProviderCatalog {
    descriptor: ProviderDescriptor,
    models: BTreeMap<String, CatalogModelEntry>,
}

#[derive(Debug, Default)]
struct RegistryState {
    config: ConfigLayer,
    preset_models: BTreeMap<ModelIdentity, PresetModelEntry>,
    catalogs: BTreeMap<String, ProviderCatalog>,
    legacy_models: BTreeMap<String, ModelEntry>,
    legacy_discovered_models: Vec<String>,
    catalog_revision: u64,
}

static REGISTRY_STATE: LazyLock<RwLock<RegistryState>> =
    LazyLock::new(|| RwLock::new(RegistryState::default()));
static CATALOG_REVISION: LazyLock<tokio::sync::watch::Sender<u64>> =
    LazyLock::new(|| tokio::sync::watch::channel(0).0);
static REGISTRY_TRANSACTION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serializes tests that mutate the global model registry.
///
/// This stays available in integration tests so they can avoid racing the
/// shared model registry state.
pub static MODEL_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set_discovered_models(models: Vec<String>) {
    REGISTRY_STATE.write().unwrap().legacy_discovered_models = models;
}

pub fn discovered_models() -> Vec<String> {
    let state = REGISTRY_STATE.read().unwrap();
    let mut models = state.legacy_discovered_models.clone();
    let mut seen: BTreeSet<String> = models.iter().cloned().collect();
    for catalog in state.catalogs.values() {
        for model in catalog.models.values() {
            if seen.insert(model.registry_key.clone()) {
                models.push(model.registry_key.clone());
            }
        }
    }
    models
}

/// Set the base model configuration (from config.toml).
/// Dynamic provider catalogs remain in their own layer.
pub fn set_provider_config(cfg: ProviderConfig) {
    if let Err(error) = install_config_layer(ConfigLayer::inferred(cfg)) {
        crate::notify!(error, "model config update rejected: {error}");
    }
}

/// Backwards-compatible alias for [set_provider_config].
pub fn set_model_config(cfg: ModelConfig) {
    set_provider_config(cfg);
}

/// Register additional model entries without clobbering existing ones.
pub fn register_model_entries(entries: Vec<(String, ModelEntry)>) {
    let mut state = REGISTRY_STATE.write().unwrap();
    for (name, entry) in entries {
        if state.config.values.models.contains_key(&name) || state.legacy_models.contains_key(&name)
        {
            continue;
        }
        if entry.discovered {
            state.legacy_models.insert(name, entry);
        } else {
            let declarations = CapabilityDeclarations::inferred(&entry);
            state.config.capabilities.insert(name.clone(), declarations);
            state.config.values.models.insert(name, entry);
        }
    }
}

/// Register additional provider entries without clobbering existing ones.
pub fn register_provider_entries(entries: Vec<(String, ProviderEntry)>) {
    let mut config = REGISTRY_STATE.read().unwrap().config.clone();
    for (name, entry) in entries {
        config.values.providers.entry(name).or_insert(entry);
    }
    if let Err(error) = install_config_layer(config) {
        crate::notify!(error, "provider config update rejected: {error}");
    }
}

/// Register a legacy catalog using `<provider_name>:<api_model>` keys.
pub fn register_discovered(
    _provider_id: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModel],
) {
    register_legacy_discovered_entries(provider_name, provider_name, models, true);
}

pub fn register_discovered_details(
    provider_id: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<CatalogDelta, CatalogError> {
    let prepared = prepare_discovered_details(provider_id, provider_name, models)?;
    Ok(commit_prepared_provider_catalog(prepared))
}

pub fn prepare_discovered_details(
    provider_id: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    let profile = reasoning_wire_profile_for_provider(provider_name);
    prepare_discovered_details_with_global_auth(provider_id, provider_name, profile, models)
}

pub fn register_discovered_for_provider(
    provider_key: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModel],
) {
    if models.is_empty() {
        return;
    }
    let provider_ids = AuthStore::load()
        .map(|auth| {
            auth.providers
                .into_iter()
                .map(|provider| provider.id)
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|_| vec![provider_key.to_string()]);
    let short_id = shortest_unique_provider_id(provider_key, &provider_ids);
    let model_namespace = format!("{short_id}@{provider_name}");
    register_legacy_discovered_entries(&model_namespace, provider_key, models, false);
}

fn register_legacy_discovered_entries(
    model_namespace: &str,
    provider_key: &str,
    models: &[crate::provider::DiscoveredModel],
    api_model_uses_registry_key: bool,
) {
    if models.is_empty() {
        return;
    }
    let entries: Vec<_> = models
        .iter()
        .map(|model| {
            let name = format!("{model_namespace}:{}", model.slug);
            (
                name.clone(),
                ModelEntry {
                    model: if api_model_uses_registry_key {
                        name.clone()
                    } else {
                        model.slug.clone()
                    },
                    provider: Some(provider_key.to_string()),
                    context_budget: model.context_budget,
                    thinking: Some(model.thinking),
                    discovered: true,
                    ..Default::default()
                },
            )
        })
        .collect();
    let names = entries.iter().map(|(name, _)| name.clone()).collect();
    register_model_entries(entries);
    set_discovered_models(names);
}

pub fn register_discovered_details_for_provider(
    provider_key: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<CatalogDelta, CatalogError> {
    let prepared = prepare_discovered_details_for_provider(provider_key, provider_name, models)?;
    Ok(commit_prepared_provider_catalog(prepared))
}

pub fn prepare_discovered_details_for_provider(
    provider_key: &str,
    provider_name: &str,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    prepare_discovered_details_with_global_auth(
        provider_key,
        provider_name,
        ReasoningWireProfile::CodexResponses,
        models,
    )
}

fn prepare_discovered_details_with_global_auth(
    provider_key: &str,
    provider_name: &str,
    wire_profile: ReasoningWireProfile,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    let auth = AuthStore::load().map_err(|error| CatalogError::NamespaceStore {
        message: error.to_string(),
    })?;
    let persisted_namespace = crate::auth_store::load_provider_model_namespace(provider_key)
        .map_err(|error| CatalogError::NamespaceStore {
            message: error.to_string(),
        })?;
    prepare_discovered_details_for_provider_with_auth(
        provider_key,
        provider_name,
        &auth,
        persisted_namespace.as_deref(),
        wire_profile,
        models,
    )
}

pub fn prepare_discovered_details_for_provider_with_auth(
    provider_key: &str,
    provider_name: &str,
    auth: &AuthStore,
    persisted_namespace: Option<&str>,
    wire_profile: ReasoningWireProfile,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    prepare_discovered_details_with_profile(
        provider_key,
        provider_name,
        wire_profile,
        auth,
        persisted_namespace,
        models,
    )
}

fn prepare_discovered_details_with_profile(
    provider_key: &str,
    provider_name: &str,
    wire_profile: ReasoningWireProfile,
    auth: &AuthStore,
    persisted_namespace: Option<&str>,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    let in_memory_namespace = REGISTRY_STATE
        .read()
        .unwrap()
        .catalogs
        .get(provider_key)
        .map(|catalog| catalog.descriptor.namespace.clone());
    if let (Some(current), Some(persisted)) = (&in_memory_namespace, persisted_namespace)
        && current != persisted
    {
        return Err(CatalogError::NamespaceChanged {
            provider_key: provider_key.to_string(),
            current: current.clone(),
            requested: persisted.to_string(),
        });
    }
    let model_namespace = if let Some(namespace) = in_memory_namespace {
        namespace
    } else if let Some(namespace) = persisted_namespace {
        namespace.to_string()
    } else {
        let provider_ids = auth
            .providers
            .iter()
            .map(|provider| provider.id.clone())
            .collect::<Vec<_>>();
        let short_id = shortest_unique_provider_id(provider_key, &provider_ids);
        format!("{short_id}@{provider_name}")
    };
    let descriptor = ProviderDescriptor {
        provider_key: provider_key.to_string(),
        provider_name: provider_name.to_string(),
        namespace: model_namespace,
        wire_profile,
    };
    prepare_provider_catalog(descriptor, models)
}

pub(crate) fn shortest_unique_provider_id(provider_id: &str, provider_ids: &[String]) -> String {
    let normalized: String = provider_id
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    let peers: Vec<(&str, String)> = provider_ids
        .iter()
        .map(|id| {
            (
                id.as_str(),
                id.chars().filter(|ch| ch.is_ascii_alphanumeric()).collect(),
            )
        })
        .collect();
    if normalized.is_empty() {
        return "provider".to_string();
    }
    if peers
        .iter()
        .any(|(id, peer)| *id != provider_id && peer == &normalized)
    {
        return provider_id.to_string();
    }
    let mut len = normalized.len().min(6);
    while len < normalized.len()
        && peers
            .iter()
            .filter(|(id, _)| *id != provider_id)
            .any(|(_, peer)| peer.starts_with(&normalized[..len]))
    {
        len += 1;
    }
    normalized[..len].to_string()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CatalogDelta {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CatalogError {
    #[error("provider catalog {field} must not be empty")]
    EmptyDescriptorField { field: &'static str },
    #[error("provider catalog contains an empty model id")]
    EmptyModel,
    #[error("provider catalog contains duplicate model `{model}`")]
    DuplicateModel { model: String },
    #[error(
        "provider `{provider_key}` namespace is already `{current}` and cannot change to `{requested}`"
    )]
    NamespaceChanged {
        provider_key: String,
        current: String,
        requested: String,
    },
    #[error("provider namespace `{namespace}` is already used by `{provider_key}`")]
    NamespaceInUse {
        namespace: String,
        provider_key: String,
    },
    #[error("model registry key `{registry_key}` is already used by `{provider_key}`")]
    RegistryKeyInUse {
        registry_key: String,
        provider_key: String,
    },
    #[error("load provider model namespace: {message}")]
    NamespaceStore { message: String },
}

pub struct PreparedProviderCatalog {
    _transaction: std::sync::MutexGuard<'static, ()>,
    catalog: ProviderCatalog,
}

impl PreparedProviderCatalog {
    pub fn namespace(&self) -> &str {
        &self.catalog.descriptor.namespace
    }
}

pub(crate) struct PreparedConfigLayer {
    _transaction: std::sync::MutexGuard<'static, ()>,
    config: ConfigLayer,
    presets: BTreeMap<ModelIdentity, PresetModelEntry>,
}

impl PreparedConfigLayer {
    pub(crate) fn snapshot(&self) -> ProviderConfig {
        self.config.values.clone()
    }
}

fn prepare_config_layer(config: ConfigLayer) -> Result<PreparedConfigLayer, CatalogError> {
    let presets = build_preset_models(&config);
    let transaction = REGISTRY_TRANSACTION_LOCK.lock().unwrap();
    let state = REGISTRY_STATE.read().unwrap();
    for (preset_identity, preset) in &presets {
        if let Some(catalog) = state.catalogs.values().find(|catalog| {
            catalog.models.values().any(|model| {
                model.registry_key == preset.registry_key
                    && ModelIdentity::new(&catalog.descriptor.provider_key, &model.api_model)
                        != *preset_identity
            })
        }) {
            return Err(CatalogError::RegistryKeyInUse {
                registry_key: preset.registry_key.clone(),
                provider_key: catalog.descriptor.provider_key.clone(),
            });
        }
    }
    drop(state);
    Ok(PreparedConfigLayer {
        _transaction: transaction,
        config,
        presets,
    })
}

pub(crate) fn prepare_config_text(text: &str) -> anyhow::Result<PreparedConfigLayer> {
    let config = parse_config_layer(text)
        .map_err(|error| anyhow::anyhow!("parse config.toml: {error}"))?
        .unwrap_or_default();
    prepare_config_layer(config).map_err(Into::into)
}

pub(crate) fn commit_prepared_config(prepared: PreparedConfigLayer) {
    let PreparedConfigLayer {
        _transaction,
        config,
        presets,
    } = prepared;
    let mut state = REGISTRY_STATE.write().unwrap();
    state.config = config;
    state.preset_models = presets;
}

fn install_config_layer(config: ConfigLayer) -> Result<(), CatalogError> {
    let prepared = prepare_config_layer(config)?;
    commit_prepared_config(prepared);
    Ok(())
}

pub fn replace_provider_catalog(
    descriptor: ProviderDescriptor,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<CatalogDelta, CatalogError> {
    let prepared = prepare_provider_catalog(descriptor, models)?;
    Ok(commit_prepared_provider_catalog(prepared))
}

pub fn prepare_provider_catalog(
    descriptor: ProviderDescriptor,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<PreparedProviderCatalog, CatalogError> {
    for (field, value) in [
        ("provider_key", descriptor.provider_key.as_str()),
        ("provider_name", descriptor.provider_name.as_str()),
        ("namespace", descriptor.namespace.as_str()),
    ] {
        if value.trim().is_empty() {
            return Err(CatalogError::EmptyDescriptorField { field });
        }
    }
    let mut next_models = BTreeMap::new();
    for model in models {
        if model.slug.trim().is_empty() {
            return Err(CatalogError::EmptyModel);
        }
        let entry = CatalogModelEntry {
            registry_key: qualified_model_key(&descriptor.namespace, &model.slug),
            api_model: model.slug.clone(),
            context_budget: model.context_budget,
            capability_knowledge: model.capability_knowledge.clone(),
        };
        if next_models.insert(model.slug.clone(), entry).is_some() {
            return Err(CatalogError::DuplicateModel {
                model: model.slug.clone(),
            });
        }
    }

    let transaction = REGISTRY_TRANSACTION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let state = REGISTRY_STATE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = state.catalogs.get(&descriptor.provider_key);
    if let Some(previous) = previous
        && previous.descriptor.namespace != descriptor.namespace
    {
        return Err(CatalogError::NamespaceChanged {
            provider_key: descriptor.provider_key,
            current: previous.descriptor.namespace.clone(),
            requested: descriptor.namespace,
        });
    }
    if let Some((provider_key, _)) = state.catalogs.iter().find(|(provider_key, catalog)| {
        provider_key.as_str() != descriptor.provider_key
            && catalog.descriptor.namespace == descriptor.namespace
    }) {
        return Err(CatalogError::NamespaceInUse {
            namespace: descriptor.namespace,
            provider_key: provider_key.clone(),
        });
    }
    for model in next_models.values() {
        if let Some((provider_key, _)) = state
            .catalogs
            .iter()
            .filter(|(provider_key, _)| provider_key.as_str() != descriptor.provider_key)
            .find(|(_, catalog)| {
                catalog
                    .models
                    .values()
                    .any(|existing| existing.registry_key == model.registry_key)
            })
        {
            return Err(CatalogError::RegistryKeyInUse {
                registry_key: model.registry_key.clone(),
                provider_key: provider_key.clone(),
            });
        }
        let identity = ModelIdentity::new(&descriptor.provider_key, &model.api_model);
        if let Some((preset_identity, _)) =
            state
                .preset_models
                .iter()
                .find(|(preset_identity, preset)| {
                    preset.registry_key == model.registry_key && **preset_identity != identity
                })
        {
            return Err(CatalogError::RegistryKeyInUse {
                registry_key: model.registry_key.clone(),
                provider_key: preset_identity.provider_key.clone(),
            });
        }
    }
    drop(state);
    Ok(PreparedProviderCatalog {
        _transaction: transaction,
        catalog: ProviderCatalog {
            descriptor,
            models: next_models,
        },
    })
}

pub fn commit_prepared_provider_catalog(prepared: PreparedProviderCatalog) -> CatalogDelta {
    let PreparedProviderCatalog {
        _transaction,
        catalog,
    } = prepared;
    let ProviderCatalog {
        descriptor,
        models: next_models,
    } = catalog;
    let mut state = REGISTRY_STATE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous = state.catalogs.get(&descriptor.provider_key);
    let added = next_models
        .keys()
        .filter(|key| previous.is_none_or(|catalog| !catalog.models.contains_key(*key)))
        .count();
    let updated = next_models
        .iter()
        .filter(|(key, next)| {
            previous
                .and_then(|catalog| catalog.models.get(*key))
                .is_some_and(|old| !catalog_model_eq(old, next))
        })
        .count();
    let removed = previous
        .map(|catalog| {
            catalog
                .models
                .keys()
                .filter(|key| !next_models.contains_key(*key))
                .count()
        })
        .unwrap_or(0);
    let total = next_models.len();
    let changed = previous.is_none_or(|catalog| {
        catalog.descriptor != descriptor
            || catalog.models.len() != next_models.len()
            || catalog.models.iter().any(|(model, entry)| {
                next_models
                    .get(model)
                    .is_none_or(|next| !catalog_model_eq(entry, next))
            })
    });
    state.catalogs.insert(
        descriptor.provider_key.clone(),
        ProviderCatalog {
            descriptor,
            models: next_models,
        },
    );
    if changed {
        state.catalog_revision = state.catalog_revision.wrapping_add(1);
        CATALOG_REVISION.send_replace(state.catalog_revision);
    }
    CatalogDelta {
        added,
        updated,
        removed,
        total,
    }
}

pub fn remove_provider_catalog(provider_key: &str) -> bool {
    let _transaction = REGISTRY_TRANSACTION_LOCK.lock().unwrap();
    let mut state = REGISTRY_STATE.write().unwrap();
    let removed = state.catalogs.remove(provider_key).is_some();
    if removed {
        state.catalog_revision = state.catalog_revision.wrapping_add(1);
        CATALOG_REVISION.send_replace(state.catalog_revision);
    }
    removed
}

pub(crate) fn provider_catalog_namespace(provider_key: &str) -> Option<String> {
    REGISTRY_STATE
        .read()
        .unwrap()
        .catalogs
        .get(provider_key)
        .map(|catalog| catalog.descriptor.namespace.clone())
}

pub fn model_catalog_revision() -> u64 {
    REGISTRY_STATE.read().unwrap().catalog_revision
}

pub fn subscribe_model_catalog() -> tokio::sync::watch::Receiver<u64> {
    CATALOG_REVISION.subscribe()
}

fn catalog_model_eq(left: &CatalogModelEntry, right: &CatalogModelEntry) -> bool {
    left.registry_key == right.registry_key
        && left.api_model == right.api_model
        && left.context_budget == right.context_budget
        && left.capability_knowledge == right.capability_knowledge
}

fn qualified_model_key(namespace: &str, api_model: &str) -> String {
    let escaped_namespace = namespace.replace('%', "%25").replace(':', "%3A");
    format!("{escaped_namespace}:{api_model}")
}

fn build_preset_models(config: &ConfigLayer) -> BTreeMap<ModelIdentity, PresetModelEntry> {
    let providers: BTreeMap<String, ProviderEntry> = config
        .values
        .providers
        .iter()
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect();
    let mut candidates = Vec::new();
    for (provider_name, provider) in providers {
        let Some(base_url) = provider.base_url.as_deref() else {
            continue;
        };
        let Some(preset) = PROVIDER_PRESETS
            .iter()
            .find(|preset| preset.base_url == base_url)
        else {
            continue;
        };
        for model in preset.models {
            candidates.push((
                ModelIdentity::new(&provider_name, model.id),
                ModelEntry {
                    model: model.id.to_string(),
                    provider: Some(provider_name.clone()),
                    context_budget: Some(model.context_budget),
                    thinking: Some(model.thinking),
                    discovered: true,
                    ..Default::default()
                },
            ));
        }
    }
    candidates
        .into_iter()
        .map(|(identity, entry)| {
            let registry_key = qualified_model_key(&identity.provider_key, &identity.api_model);
            (
                identity,
                PresetModelEntry {
                    registry_key,
                    entry,
                },
            )
        })
        .collect()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum CapabilityFieldKnowledge {
    #[default]
    Unknown,
    Legacy,
    Advertised,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ResolvedCapabilityKnowledge {
    reasoning_efforts: CapabilityFieldKnowledge,
    reasoning_modes: CapabilityFieldKnowledge,
    input_modalities: CapabilityFieldKnowledge,
}

#[derive(Debug, Clone)]
struct ResolvedModel {
    key: String,
    identity: Option<ModelIdentity>,
    entry: ModelEntry,
    capability_knowledge: ResolvedCapabilityKnowledge,
    wire_profile: ReasoningWireProfile,
}

fn profile_from_provider_entry(entry: &ProviderEntry) -> ReasoningWireProfile {
    match entry.kind.as_str() {
        "openai" | "openai-compat" => match entry.reasoning_format.unwrap_or_else(|| {
            crate::providers::openai::OpenAiReasoningFormat::for_provider_kind(&entry.kind)
        }) {
            crate::providers::openai::OpenAiReasoningFormat::Official => {
                ReasoningWireProfile::OpenAiOfficial
            }
            crate::providers::openai::OpenAiReasoningFormat::CompatibleThinking => {
                ReasoningWireProfile::CompatibleThinking
            }
        },
        "anthropic" => ReasoningWireProfile::AnthropicMessages,
        "codex" => ReasoningWireProfile::CodexResponses,
        _ => ReasoningWireProfile::Unknown,
    }
}

fn fallback_profile(provider: &str) -> ReasoningWireProfile {
    match provider {
        "openai" => ReasoningWireProfile::OpenAiOfficial,
        "openai-compat" => ReasoningWireProfile::CompatibleThinking,
        "anthropic" => ReasoningWireProfile::AnthropicMessages,
        "codex" => ReasoningWireProfile::CodexResponses,
        _ => ReasoningWireProfile::Unknown,
    }
}

fn provider_entries_in_state(state: &RegistryState) -> BTreeMap<String, ProviderEntry> {
    state
        .config
        .values
        .providers
        .iter()
        .map(|(name, entry)| (name.clone(), entry.clone()))
        .collect()
}

fn provider_profile_in_state(state: &RegistryState, provider: &str) -> ReasoningWireProfile {
    if let Some(catalog) = state.catalogs.get(provider) {
        return catalog.descriptor.wire_profile;
    }
    provider_entries_in_state(state)
        .get(provider)
        .map(profile_from_provider_entry)
        .unwrap_or_else(|| fallback_profile(provider))
}

fn catalog_model_to_resolved(
    catalog: &ProviderCatalog,
    model: &CatalogModelEntry,
) -> ResolvedModel {
    let capabilities = model
        .capability_knowledge
        .advertised()
        .cloned()
        .unwrap_or_default();
    let thinking = match &model.capability_knowledge {
        CapabilityKnowledge::Legacy { thinking } => Some(*thinking),
        CapabilityKnowledge::Advertised(_) => None,
    };
    let field_knowledge = match &model.capability_knowledge {
        CapabilityKnowledge::Legacy { .. } => CapabilityFieldKnowledge::Legacy,
        CapabilityKnowledge::Advertised(_) => CapabilityFieldKnowledge::Advertised,
    };
    ResolvedModel {
        key: model.registry_key.clone(),
        identity: Some(ModelIdentity::new(
            &catalog.descriptor.provider_key,
            &model.api_model,
        )),
        entry: ModelEntry {
            model: model.api_model.clone(),
            provider: Some(catalog.descriptor.provider_key.clone()),
            context_budget: model.context_budget,
            thinking,
            reasoning_efforts: capabilities.reasoning_efforts,
            default_reasoning_effort: capabilities.default_reasoning_effort,
            reasoning_modes: capabilities.reasoning_modes,
            default_reasoning_mode: capabilities.default_reasoning_mode,
            input_modalities: capabilities.input_modalities,
            discovered: true,
            ..Default::default()
        },
        capability_knowledge: ResolvedCapabilityKnowledge {
            reasoning_efforts: field_knowledge,
            reasoning_modes: field_knowledge,
            input_modalities: field_knowledge,
        },
        wire_profile: catalog.descriptor.wire_profile,
    }
}

fn model_identity(entry: &ModelEntry) -> Option<ModelIdentity> {
    let provider = entry.provider.as_deref()?;
    (!entry.model.is_empty()).then(|| ModelIdentity::new(provider, &entry.model))
}

fn model_entry_to_resolved(
    state: &RegistryState,
    key: String,
    entry: ModelEntry,
    declarations: &CapabilityDeclarations,
    legacy_capabilities: bool,
) -> ResolvedModel {
    let wire_profile = entry
        .provider
        .as_deref()
        .map(|provider| provider_profile_in_state(state, provider))
        .unwrap_or(ReasoningWireProfile::Unknown);
    let unknown_or_legacy = if legacy_capabilities {
        CapabilityFieldKnowledge::Legacy
    } else {
        CapabilityFieldKnowledge::Unknown
    };
    ResolvedModel {
        key,
        identity: model_identity(&entry),
        entry,
        capability_knowledge: ResolvedCapabilityKnowledge {
            reasoning_efforts: if declarations.reasoning_efforts {
                CapabilityFieldKnowledge::Advertised
            } else {
                unknown_or_legacy
            },
            reasoning_modes: if declarations.reasoning_modes {
                CapabilityFieldKnowledge::Advertised
            } else {
                unknown_or_legacy
            },
            input_modalities: if declarations.input_modalities {
                CapabilityFieldKnowledge::Advertised
            } else {
                unknown_or_legacy
            },
        },
        wire_profile,
    }
}

fn entries_have_compatible_identity(base: &ModelEntry, overlay: &ModelEntry) -> bool {
    !overlay.model.is_empty()
        && overlay.model == base.model
        && overlay
            .provider
            .as_ref()
            .is_none_or(|provider| base.provider.as_ref() == Some(provider))
}

fn overlay_model_entry(
    state: &RegistryState,
    key: String,
    base: &ResolvedModel,
    overlay: &ModelEntry,
    declarations: &CapabilityDeclarations,
) -> ResolvedModel {
    let mut entry = base.entry.clone();
    if !overlay.model.is_empty() {
        entry.model = overlay.model.clone();
    }
    if overlay.provider.is_some() {
        entry.provider = overlay.provider.clone();
    }
    entry.context_budget = overlay.context_budget.or(entry.context_budget);
    entry.compact_threshold_ratio = overlay
        .compact_threshold_ratio
        .or(entry.compact_threshold_ratio);
    entry.thinking = overlay.thinking.or(entry.thinking);
    entry.reasoning = overlay.reasoning.clone().or(entry.reasoning);
    entry.reasoning_mode = overlay.reasoning_mode.clone().or(entry.reasoning_mode);
    entry.reasoning_budget_tokens = overlay
        .reasoning_budget_tokens
        .or(entry.reasoning_budget_tokens);
    if declarations.reasoning_efforts {
        entry.reasoning_efforts = overlay.reasoning_efforts.clone();
    }
    entry.default_reasoning_effort = overlay
        .default_reasoning_effort
        .clone()
        .or(entry.default_reasoning_effort);
    if declarations.reasoning_modes {
        entry.reasoning_modes = overlay.reasoning_modes.clone();
    }
    entry.default_reasoning_mode = overlay
        .default_reasoning_mode
        .clone()
        .or(entry.default_reasoning_mode);
    if declarations.input_modalities {
        entry.input_modalities = overlay.input_modalities.clone();
    }
    entry.image_detail = overlay.image_detail.or(entry.image_detail);
    entry.max_tokens = overlay.max_tokens.or(entry.max_tokens);
    entry.enabled = overlay.enabled.or(entry.enabled);
    entry.discovered |= overlay.discovered;

    let mut capability_knowledge = base.capability_knowledge;
    if declarations.reasoning_efforts {
        capability_knowledge.reasoning_efforts = CapabilityFieldKnowledge::Advertised;
    }
    if declarations.reasoning_modes {
        capability_knowledge.reasoning_modes = CapabilityFieldKnowledge::Advertised;
    }
    if declarations.input_modalities {
        capability_knowledge.input_modalities = CapabilityFieldKnowledge::Advertised;
    }
    let wire_profile = entry
        .provider
        .as_deref()
        .map(|provider| provider_profile_in_state(state, provider))
        .filter(|profile| *profile != ReasoningWireProfile::Unknown)
        .unwrap_or(base.wire_profile);
    ResolvedModel {
        key,
        identity: base.identity.clone(),
        entry,
        capability_knowledge,
        wire_profile,
    }
}

fn config_matches_base(config_key: &str, config: &ModelEntry, base: &ResolvedModel) -> bool {
    model_identity(config)
        .zip(base.identity.clone())
        .is_some_and(|(config, base)| config == base)
        || (config_key == base.key && entries_have_compatible_identity(&base.entry, config))
}

fn resolved_models_in_state(state: &RegistryState) -> BTreeMap<String, ResolvedModel> {
    let mut bases = BTreeMap::new();
    for (identity, preset) in &state.preset_models {
        let model = model_entry_to_resolved(
            state,
            preset.registry_key.clone(),
            preset.entry.clone(),
            &CapabilityDeclarations::default(),
            true,
        );
        bases.insert(identity.clone(), model);
    }
    for catalog in state.catalogs.values() {
        for model in catalog.models.values() {
            let model = catalog_model_to_resolved(catalog, model);
            if let Some(identity) = model.identity.clone() {
                bases.insert(identity, model);
            }
        }
    }

    let mut configs: Vec<(&String, &ModelEntry)> = state.config.values.models.iter().collect();
    configs.sort_by(|left, right| left.0.cmp(right.0));
    let associations: BTreeMap<String, ResolvedModel> = configs
        .iter()
        .filter_map(|(key, entry)| {
            let base = model_identity(entry)
                .and_then(|identity| bases.get(&identity))
                .or_else(|| {
                    bases
                        .values()
                        .find(|base| config_matches_base(key, entry, base))
                })?;
            Some(((*key).clone(), base.clone()))
        })
        .collect();
    let suppressed: BTreeSet<String> = associations.values().map(|base| base.key.clone()).collect();
    let mut resolved: BTreeMap<String, ResolvedModel> = bases
        .into_values()
        .filter(|base| !suppressed.contains(&base.key))
        .map(|base| (base.key.clone(), base))
        .collect();

    for (key, entry) in &state.legacy_models {
        resolved.entry(key.clone()).or_insert_with(|| {
            model_entry_to_resolved(
                state,
                key.clone(),
                entry.clone(),
                &CapabilityDeclarations::inferred(entry),
                false,
            )
        });
    }

    for (key, entry) in configs {
        let key = key.clone();
        let declarations = state.config.declarations(&key);
        if let Some(base) = associations.get(&key) {
            resolved.insert(
                key.clone(),
                overlay_model_entry(state, key, base, entry, &declarations),
            );
        } else {
            resolved.insert(
                key.clone(),
                model_entry_to_resolved(state, key, entry.clone(), &declarations, false),
            );
        }
    }
    resolved
}

fn resolve_alias_in_state(state: &RegistryState, name: &str) -> String {
    let mut current = name.to_string();
    let mut seen = BTreeSet::new();
    while let Some(entry) = state.config.values.aliases.get(&current) {
        if !seen.insert(current.clone()) {
            break;
        }
        current = entry.model.clone();
    }
    current
}

fn resolved_model_in_state(state: &RegistryState, name: &str) -> Option<ResolvedModel> {
    let resolved = resolve_alias_in_state(state, name);
    let mut models = resolved_models_in_state(state);
    if let Some(model) = models.remove(&resolved) {
        return Some(model);
    }

    // Preserve aliases written before preset model keys became provider-qualified.
    // A bare API model is only safe when it identifies exactly one model.
    let mut matches = models.into_values().filter(|model| {
        model.entry.model == resolved
            && model
                .identity
                .as_ref()
                .is_some_and(|identity| state.preset_models.contains_key(identity))
    });
    let model = matches.next()?;
    matches.next().is_none().then_some(model)
}

#[derive(Debug, Clone)]
pub struct ModelRow {
    pub slug: String,
    pub provider_name: String,
    pub context_budget: u64,
    pub max_output_tokens: Option<u32>,
    pub thinking: bool,
    pub reasoning: ReasoningSelection,
    pub capabilities: ModelCapabilities,
    pub image_detail: ImageDetail,
}

#[derive(Debug, Clone)]
pub struct ProviderGroup {
    pub provider_name: String,
    pub models: Vec<ModelRow>,
}

/// Return all models grouped by provider, with complete metadata.
/// Single canonical source for UI — no manual union of all_model_entries +
/// discovered_models + aliases.
pub fn all_provider_groups() -> Vec<ProviderGroup> {
    provider_groups(false)
}

pub fn enabled_provider_names() -> std::collections::HashSet<String> {
    let auth = AuthStore::load().unwrap_or_default();
    enabled_provider_names_from_auth(&auth)
}

fn enabled_provider_names_from_auth(auth: &AuthStore) -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> = all_provider_entries()
        .into_iter()
        .filter(|(_, entry)| entry.enabled.unwrap_or(true))
        .map(|(name, _)| name)
        .collect();
    names.extend(
        auth.providers
            .iter()
            .filter(|provider| provider.enabled)
            .map(|provider| provider.id.clone()),
    );
    names
}

/// Return enabled configured providers even when no model has been registered yet.
pub fn all_provider_groups_with_empty() -> Vec<ProviderGroup> {
    provider_groups(true)
}

fn provider_groups(include_empty: bool) -> Vec<ProviderGroup> {
    let state = REGISTRY_STATE.read().unwrap();
    let entries = resolved_models_in_state(&state);
    let mut groups: std::collections::BTreeMap<String, Vec<ModelRow>> =
        std::collections::BTreeMap::new();
    for (name, resolved) in entries {
        let info = model_info_from_resolved(&resolved);
        let provider = resolved
            .entry
            .provider
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let row = ModelRow {
            slug: name,
            provider_name: provider.clone(),
            context_budget: info.context_budget,
            max_output_tokens: info.max_output_tokens,
            thinking: info.thinking_enabled(),
            reasoning: info.reasoning.clone(),
            capabilities: info.capabilities.clone(),
            image_detail: info.image_detail,
        };
        groups.entry(provider).or_default().push(row);
    }
    if include_empty {
        for (provider, entry) in provider_entries_in_state(&state) {
            if entry.enabled.unwrap_or(true) {
                groups.entry(provider).or_default();
            }
        }
        for catalog in state.catalogs.values() {
            groups
                .entry(catalog.descriptor.provider_key.clone())
                .or_default();
        }
    }
    groups
        .into_iter()
        .map(|(provider_name, models)| ProviderGroup {
            provider_name,
            models,
        })
        .collect()
}

pub fn resolve_alias(name: &str) -> String {
    resolve_alias_in_state(&REGISTRY_STATE.read().unwrap(), name)
}

pub fn model_entry(name: &str) -> Option<ModelEntry> {
    resolved_model_in_state(&REGISTRY_STATE.read().unwrap(), name).map(|model| model.entry)
}

pub fn api_model_id(name: &str) -> String {
    model_entry(name)
        .and_then(|entry| (!entry.model.is_empty()).then_some(entry.model))
        .unwrap_or_else(|| name.to_string())
}

pub fn all_model_entries() -> Vec<(String, ModelEntry)> {
    resolved_models_in_state(&REGISTRY_STATE.read().unwrap())
        .into_iter()
        .map(|(name, model)| (name, model.entry))
        .collect()
}

pub fn provider_display_name(provider_key: &str) -> String {
    AuthStore::load()
        .ok()
        .and_then(|auth| {
            auth.providers
                .into_iter()
                .find(|provider| provider.id == provider_key)
        })
        .map(|provider| match provider.account {
            Some(account) if !account.is_empty() => format!("{} · {account}", provider.name),
            _ => provider.name,
        })
        .unwrap_or_else(|| provider_key.to_string())
}

pub fn is_provider_enabled(name: &str) -> bool {
    let state = REGISTRY_STATE.read().unwrap();
    if let Some(entry) = provider_entries_in_state(&state).get(name) {
        return entry.enabled.unwrap_or(true);
    }
    let auth_provider = AuthStore::load().ok().and_then(|auth| {
        auth.providers
            .into_iter()
            .find(|provider| provider.id == name)
    });
    if state.catalogs.contains_key(name) {
        return auth_provider.is_some_and(|provider| provider.enabled);
    }
    auth_provider.is_none_or(|provider| provider.enabled)
}

pub fn all_provider_entries() -> Vec<(String, ProviderEntry)> {
    provider_entries_in_state(&REGISTRY_STATE.read().unwrap())
        .into_iter()
        .collect()
}

pub fn all_aliases() -> Vec<(String, String)> {
    let state = REGISTRY_STATE.read().unwrap();
    let mut aliases: Vec<(String, String)> = state
        .config
        .values
        .aliases
        .iter()
        .map(|(name, entry)| (name.clone(), entry.model.clone()))
        .collect();
    aliases.sort_by(|left, right| left.0.cmp(&right.0));
    aliases
}

pub fn model_info(name: &str) -> ModelInfo {
    let state = REGISTRY_STATE.read().unwrap();
    if let Some(model) = resolved_model_in_state(&state, name) {
        return model_info_from_resolved(&model);
    }
    ModelInfo {
        name: resolve_alias_in_state(&state, name),
        context_budget: 0,
        compact_threshold_ratio: 0.8,
        reasoning: crate::provider::ReasoningSelection::ProviderDefault,
        capabilities: ModelCapabilities::default(),
        image_detail: ImageDetail::Auto,
        max_output_tokens: None,
    }
}

fn model_info_from_resolved(model: &ResolvedModel) -> ModelInfo {
    let enabled = model.entry.enabled.unwrap_or(true);
    ModelInfo {
        name: model.key.clone(),
        context_budget: if enabled {
            model.entry.context_budget.unwrap_or(0)
        } else {
            0
        },
        compact_threshold_ratio: model.entry.compact_threshold_ratio.unwrap_or(0.8),
        reasoning: reasoning_selection(&model.entry),
        capabilities: model_capabilities(&model.entry),
        image_detail: model.entry.image_detail.unwrap_or_default(),
        max_output_tokens: model.entry.max_tokens,
    }
}

pub fn reasoning_wire_profile_for_provider(provider: &str) -> ReasoningWireProfile {
    provider_profile_in_state(&REGISTRY_STATE.read().unwrap(), provider)
}

pub fn reasoning_wire_profile_for_model(model: &str) -> ReasoningWireProfile {
    resolved_model_in_state(&REGISTRY_STATE.read().unwrap(), model)
        .map(|model| model.wire_profile)
        .unwrap_or(ReasoningWireProfile::Unknown)
}

pub fn resolve_reasoning_for_model(
    model: &str,
    selection: &ReasoningSelection,
) -> Result<ReasoningSelection, String> {
    let state = REGISTRY_STATE.read().unwrap();
    let Some(model) = resolved_model_in_state(&state, model) else {
        return ReasoningWireProfile::Unknown
            .validate(selection, None)
            .map(|()| selection.clone());
    };
    resolve_reasoning_for_resolved(&model, selection)
}

/// Return whether a wire-valid selection still requires capability fields that are unknown.
pub fn reasoning_selection_uses_legacy_capabilities(
    model: &str,
    selection: &ReasoningSelection,
) -> bool {
    let state = REGISTRY_STATE.read().unwrap();
    let Some(model) = resolved_model_in_state(&state, model) else {
        return false;
    };
    if model
        .wire_profile
        .validate(selection, model.entry.max_tokens)
        .is_err()
    {
        return false;
    }
    let checks_effort_metadata = matches!(
        selection,
        ReasoningSelection::Effort { effort, .. } if !matches!(effort, ReasoningEffort::None)
    ) || matches!(selection, ReasoningSelection::BudgetTokens { .. });
    (checks_effort_metadata
        && matches!(
            model.capability_knowledge.reasoning_efforts,
            CapabilityFieldKnowledge::Legacy
        ))
        || (selection.execution_mode().is_some()
            && matches!(
                model.capability_knowledge.reasoning_modes,
                CapabilityFieldKnowledge::Legacy
            ))
}

fn resolve_reasoning_for_resolved(
    model: &ResolvedModel,
    selection: &ReasoningSelection,
) -> Result<ReasoningSelection, String> {
    model
        .wire_profile
        .validate(selection, model.entry.max_tokens)?;
    if let ReasoningSelection::Effort { effort, .. } = selection
        && !matches!(effort, ReasoningEffort::None)
    {
        match model.capability_knowledge.reasoning_efforts {
            CapabilityFieldKnowledge::Legacy => {
                return Err(format!(
                    "model `{}` has legacy reasoning metadata; use `auto` until its catalog is refreshed",
                    model.key
                ));
            }
            CapabilityFieldKnowledge::Advertised
                if !model.entry.reasoning_efforts.contains(effort) =>
            {
                let available = model
                    .entry
                    .reasoning_efforts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(if available.is_empty() {
                    format!(
                        "model `{}` does not advertise exact reasoning efforts",
                        model.key
                    )
                } else {
                    format!(
                        "reasoning effort `{effort}` is not supported by model `{}`; available: {available}",
                        model.key
                    )
                });
            }
            _ => {}
        }
    }
    if matches!(selection, ReasoningSelection::BudgetTokens { .. })
        && matches!(
            model.capability_knowledge.reasoning_efforts,
            CapabilityFieldKnowledge::Legacy
        )
    {
        return Err(format!(
            "model `{}` has legacy reasoning metadata; use `auto` until its catalog is refreshed",
            model.key
        ));
    }
    if let Some(mode) = selection.execution_mode() {
        match model.capability_knowledge.reasoning_modes {
            CapabilityFieldKnowledge::Legacy => {
                return Err(format!(
                    "model `{}` has legacy reasoning metadata and cannot validate mode `{mode}`",
                    model.key
                ));
            }
            CapabilityFieldKnowledge::Advertised if !model.entry.reasoning_modes.contains(mode) => {
                let available = model
                    .entry
                    .reasoning_modes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(if available.is_empty() {
                    format!("model `{}` does not advertise reasoning modes", model.key)
                } else {
                    format!(
                        "reasoning mode `{mode}` is not supported by model `{}`; available: {available}",
                        model.key
                    )
                });
            }
            _ => {}
        }
    }
    let info = model_info_from_resolved(model);
    let profile = model.wire_profile;
    let resolved = resolve_reasoning(selection, &info.capabilities)?;
    let resolved = if matches!(selection, ReasoningSelection::Auto { .. })
        && matches!(
            profile,
            ReasoningWireProfile::CompatibleThinking
                | ReasoningWireProfile::CodexResponses
                | ReasoningWireProfile::AnthropicMessages
        ) {
        ReasoningSelection::Auto {
            execution_mode: resolved.execution_mode().cloned(),
        }
    } else {
        resolved
    };
    profile.validate(&resolved, info.max_output_tokens)?;
    Ok(resolved)
}

pub fn reasoning_selections_for_provider(
    provider: &str,
    capabilities: &ModelCapabilities,
) -> Vec<ReasoningSelection> {
    let profile = reasoning_wire_profile_for_provider(provider);
    reasoning_selections(profile, capabilities, true)
}

fn reasoning_selections(
    profile: ReasoningWireProfile,
    capabilities: &ModelCapabilities,
    use_profile_fallback: bool,
) -> Vec<ReasoningSelection> {
    let mut choices = vec![
        ReasoningSelection::ProviderDefault,
        ReasoningSelection::Disabled,
        ReasoningSelection::Auto {
            execution_mode: None,
        },
    ];
    let efforts = if use_profile_fallback && capabilities.reasoning_efforts.is_empty() {
        profile.fallback_efforts()
    } else {
        capabilities.reasoning_efforts.as_slice()
    };
    for effort in efforts {
        let selection = ReasoningSelection::Effort {
            effort: effort.clone(),
            execution_mode: None,
        };
        if profile.validate(&selection, None).is_ok() && !choices.contains(&selection) {
            choices.push(selection);
        }
    }
    if profile.supports_token_budget() {
        choices.push(ReasoningSelection::BudgetTokens { tokens: 4096 });
    }
    choices
}

pub fn reasoning_selections_for_model(model: &str) -> Vec<ReasoningSelection> {
    let state = REGISTRY_STATE.read().unwrap();
    let Some(model) = resolved_model_in_state(&state, model) else {
        return vec![
            ReasoningSelection::ProviderDefault,
            ReasoningSelection::Disabled,
            ReasoningSelection::Auto {
                execution_mode: None,
            },
        ];
    };
    let info = model_info_from_resolved(&model);
    let mut choices = match model.capability_knowledge.reasoning_efforts {
        CapabilityFieldKnowledge::Legacy => vec![
            ReasoningSelection::ProviderDefault,
            ReasoningSelection::Disabled,
            ReasoningSelection::Auto {
                execution_mode: None,
            },
        ],
        CapabilityFieldKnowledge::Advertised => {
            reasoning_selections(model.wire_profile, &info.capabilities, false)
        }
        CapabilityFieldKnowledge::Unknown => {
            reasoning_selections(model.wire_profile, &info.capabilities, true)
        }
    };
    choices.retain(|selection| resolve_reasoning_for_resolved(&model, selection).is_ok());
    choices
}

pub fn effective_reasoning_for_model(
    model: &str,
    input_selection: Option<&ReasoningSelection>,
) -> Result<Option<ReasoningSelection>, String> {
    let state = REGISTRY_STATE.read().unwrap();
    let Some(model) = resolved_model_in_state(&state, model) else {
        return Ok(None);
    };
    let info = model_info_from_resolved(&model);
    let requested = input_selection.unwrap_or(&info.reasoning);
    let provider_supports_reasoning = model.wire_profile != ReasoningWireProfile::Unknown;
    let should_display = input_selection.is_some()
        || !matches!(&info.reasoning, ReasoningSelection::ProviderDefault)
        || !info.capabilities.reasoning_efforts.is_empty()
        || info.capabilities.default_reasoning_effort.is_some()
        || provider_supports_reasoning;
    if !should_display {
        return Ok(None);
    }
    resolve_reasoning_for_resolved(&model, requested).map(Some)
}

impl ModelInfo {
    pub fn compact_threshold_tokens(&self) -> u64 {
        let reserved = self.max_output_tokens.unwrap_or(0) as u64;
        let available = self.context_budget.saturating_sub(reserved);
        (available as f64 * self.compact_threshold_ratio) as u64
    }

    pub fn compaction_trigger_threshold(&self) -> u64 {
        if self.context_budget == 0 {
            return u64::MAX;
        }
        let budget = self.context_budget;

        let configured_output = self.max_output_tokens.unwrap_or(32_000) as u64;
        let output_cap = (budget as f64 * 0.20) as u64;
        let output_reserve = configured_output.min(output_cap).max(8_000);

        let safety = (budget as f64 * 0.05) as u64;
        let safety_margin = safety.max(4_000);

        let trigger = budget
            .saturating_sub(output_reserve)
            .saturating_sub(safety_margin);
        let floor = (budget as f64 * 0.50) as u64;
        let ceiling = (budget as f64 * 0.95) as u64;
        trigger.clamp(floor, ceiling)
    }

    pub fn compaction_target_after(&self) -> u64 {
        let trigger = self.compaction_trigger_threshold();
        let budget_cap = (self.context_budget as f64 * 0.25) as u64;
        let trigger_cap = (trigger as f64 * 0.75) as u64;
        budget_cap.min(trigger_cap)
    }

    pub fn thinking_enabled(&self) -> bool {
        self.reasoning.enabled()
    }
}

fn reasoning_selection(entry: &ModelEntry) -> ReasoningSelection {
    if let Some(tokens) = entry.reasoning_budget_tokens {
        return ReasoningSelection::BudgetTokens { tokens };
    }
    let execution_mode = entry
        .reasoning_mode
        .as_deref()
        .and_then(|value| value.parse().ok());
    if let Some(value) = entry.reasoning.as_deref() {
        return match value.trim().to_ascii_lowercase().as_str() {
            "default" | "provider_default" => ReasoningSelection::ProviderDefault,
            "off" | "none" | "disabled" => ReasoningSelection::Disabled,
            "auto" => ReasoningSelection::Auto { execution_mode },
            _ => value
                .parse()
                .map(|effort| ReasoningSelection::Effort {
                    effort,
                    execution_mode,
                })
                .unwrap_or_default(),
        };
    }
    match entry.thinking {
        Some(true) => ReasoningSelection::Auto { execution_mode },
        Some(false) => ReasoningSelection::Disabled,
        None => ReasoningSelection::ProviderDefault,
    }
}

fn model_capabilities(entry: &ModelEntry) -> ModelCapabilities {
    ModelCapabilities {
        reasoning_efforts: entry.reasoning_efforts.clone(),
        default_reasoning_effort: entry.default_reasoning_effort.clone(),
        reasoning_modes: entry.reasoning_modes.clone(),
        default_reasoning_mode: entry.default_reasoning_mode.clone(),
        input_modalities: entry.input_modalities.clone(),
    }
}

pub fn resolve_reasoning(
    selection: &ReasoningSelection,
    capabilities: &ModelCapabilities,
) -> Result<ReasoningSelection, String> {
    let validate_mode = |mode: &Option<ReasoningExecutionMode>| -> Result<(), String> {
        if let Some(mode) = mode
            && !capabilities.reasoning_modes.is_empty()
            && !capabilities.reasoning_modes.contains(mode)
        {
            return Err(format!(
                "reasoning mode `{mode}` is not supported; available: {}",
                capabilities
                    .reasoning_modes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(())
    };

    match selection {
        ReasoningSelection::ProviderDefault | ReasoningSelection::Disabled => Ok(selection.clone()),
        ReasoningSelection::Auto { execution_mode } => {
            validate_mode(execution_mode)?;
            let mode = execution_mode
                .clone()
                .or_else(|| capabilities.default_reasoning_mode.clone());
            if let Some(effort) = capabilities.default_reasoning_effort.clone() {
                Ok(ReasoningSelection::Effort {
                    effort,
                    execution_mode: mode,
                })
            } else {
                Ok(ReasoningSelection::Auto {
                    execution_mode: mode,
                })
            }
        }
        ReasoningSelection::Effort {
            effort: ReasoningEffort::None,
            ..
        } => Ok(ReasoningSelection::Disabled),
        ReasoningSelection::Effort {
            effort,
            execution_mode,
        } => {
            validate_mode(execution_mode)?;
            if !capabilities.reasoning_efforts.is_empty()
                && !capabilities.reasoning_efforts.contains(effort)
            {
                return Err(format!(
                    "reasoning effort `{effort}` is not supported; available: {}",
                    capabilities
                        .reasoning_efforts
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            Ok(selection.clone())
        }
        ReasoningSelection::BudgetTokens { .. } => {
            if capabilities.reasoning_efforts.is_empty() {
                Ok(selection.clone())
            } else {
                Err("token-budget reasoning is not supported by this model".into())
            }
        }
    }
}

pub use crate::known_models::{KNOWN_MODELS, lookup_known_model};
/// Register derived preset models for a provider with a matching base URL.
/// User configuration overlays take priority over this layer.
pub fn register_preset_models_for(provider_name: &str, base_url: &str) {
    let config = {
        let state = REGISTRY_STATE.read().unwrap();
        let configured = state
            .config
            .values
            .providers
            .get(provider_name)
            .and_then(|provider| provider.base_url.as_deref());
        if configured != Some(base_url) {
            return;
        }
        state.config.clone()
    };
    if let Err(error) = install_config_layer(config) {
        crate::notify!(error, "preset model update rejected: {error}");
    }
}

/// Register preset models for all configured providers with matching base URLs.
pub fn register_all_preset_models() {
    let config = REGISTRY_STATE.read().unwrap().config.clone();
    if let Err(error) = install_config_layer(config) {
        crate::notify!(error, "preset model update rejected: {error}");
    }
}

// Config migration (v1 to v2)

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelMigrationOutcome {
    NotNeeded,
    Migrated { backup: std::path::PathBuf },
}

pub fn migrate_config_if_needed(
    text: &str,
) -> Result<Option<String>, crate::config_hub::ConfigError> {
    let mut doc = text.parse::<toml_edit::DocumentMut>()?;
    let legacy = has_legacy_model_fields(&doc);
    match doc.get("config_version") {
        None if !legacy => return Ok(None),
        None => {}
        Some(version) => match version.as_integer() {
            Some(1) if legacy => {}
            Some(1) | Some(2) => return Ok(None),
            Some(value) => {
                return Err(crate::config_hub::ConfigError::Invalid(format!(
                    "unsupported config_version {value}"
                )));
            }
            None => {
                return Err(crate::config_hub::ConfigError::Invalid(
                    "config_version must be an integer".into(),
                ));
            }
        },
    }

    let provider_names: std::collections::HashSet<String> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();
    let models = doc
        .get("models")
        .and_then(|item| item.as_table())
        .ok_or_else(|| crate::config_hub::ConfigError::Invalid("models is not a table".into()))?;
    let mut groups: std::collections::BTreeMap<(String, String, String), Vec<String>> =
        std::collections::BTreeMap::new();

    for (name, entry) in models.iter() {
        let api_key = entry
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let base_url = entry
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ptype = entry
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("openai-compat")
            .to_string();

        if !api_key.is_empty()
            || !base_url.is_empty()
            || (matches!(
                ptype.as_str(),
                "openai" | "openai-compat" | "anthropic" | "codex"
            ) && !provider_names.contains(&ptype))
        {
            groups
                .entry((ptype, api_key, base_url))
                .or_default()
                .push(name.to_string());
        }
    }

    if groups.is_empty() {
        return Ok(None);
    }

    let mut used_names: std::collections::HashSet<String> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name.to_string()).collect())
        .unwrap_or_default();

    for ((ptype, api_key, base_url), model_names) in &groups {
        let provider_name = pick_provider_name(ptype, base_url, &mut used_names);
        used_names.insert(provider_name.clone());

        // Create [providers.{name}] section
        let mut table = toml_edit::Table::new();
        table.insert("kind", toml_edit::value(ptype.clone()));
        if !api_key.is_empty() {
            table.insert("api_key", toml_edit::value(api_key.clone()));
        }
        if !base_url.is_empty() {
            table.insert("base_url", toml_edit::value(base_url.clone()));
        }
        table.insert("enabled", toml_edit::value(true));

        // Ensure [providers] table exists
        if doc.get("providers").is_none() {
            doc.insert("providers", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        if let Some(providers) = doc.get_mut("providers").and_then(|p| p.as_table_mut()) {
            providers.insert(&provider_name, toml_edit::Item::Table(table));
        }

        // Update each model's provider field and remove api_key/base_url
        for model_name in model_names {
            if let Some(model) = doc
                .get_mut("models")
                .and_then(|m| m.as_table_mut())
                .and_then(|t| t.get_mut(model_name.as_str()))
                .and_then(|e| e.as_table_mut())
            {
                model.insert("provider", toml_edit::value(&provider_name));
                model.remove("api_key");
                model.remove("base_url");
            }
        }
    }

    doc.insert("config_version", toml_edit::value(2i64));
    let migrated = doc.to_string();
    parse_config(&migrated).ok_or_else(|| {
        crate::config_hub::ConfigError::Invalid("validate migrated config.toml".into())
    })?;
    Ok(Some(migrated))
}

fn has_legacy_model_fields(doc: &toml_edit::DocumentMut) -> bool {
    let provider_names: std::collections::HashSet<&str> = doc
        .get("providers")
        .and_then(|item| item.as_table())
        .map(|providers| providers.iter().map(|(name, _)| name).collect())
        .unwrap_or_default();
    doc.get("models")
        .and_then(|item| item.as_table())
        .is_some_and(|models| {
            models.iter().any(|(_, entry)| {
                entry.get("api_key").is_some()
                    || entry.get("base_url").is_some()
                    || entry
                        .get("provider")
                        .and_then(|value| value.as_str())
                        .is_some_and(|provider| {
                            matches!(provider, "openai" | "openai-compat" | "anthropic" | "codex")
                                && !provider_names.contains(provider)
                        })
            })
        })
}

fn pick_provider_name(
    ptype: &str,
    base_url: &str,
    used: &mut std::collections::HashSet<String>,
) -> String {
    // Try to match base_url against PROVIDER_PRESETS
    for preset in PROVIDER_PRESETS {
        if !base_url.is_empty() && preset.base_url == base_url {
            let name = preset.name.to_lowercase();
            if !used.contains(&name) {
                return name;
            }
        }
    }
    // Fall back to provider type, with suffix for duplicates
    let base = ptype.to_string();
    if !used.contains(&base) {
        return base;
    }
    for i in 2.. {
        let candidate = format!("{base}-{i}");
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

// Alias CRUD (writes config.toml)

pub fn read_config_toml_pub() -> Option<String> {
    crate::config_hub::ConfigHub::global()
        .ok()?
        .read_config_toml()
        .ok()
}

#[cfg(test)]
pub(crate) fn reload_from_text(text: &str) -> anyhow::Result<()> {
    let prepared = prepare_config_text(text)?;
    commit_prepared_config(prepared);
    Ok(())
}

/// Unified config parser — parses `[providers.X]`, `[models.X]`, and `[alias.X]`
/// sections from a TOML string into a [ProviderConfig].
pub fn parse_config(text: &str) -> Option<ProviderConfig> {
    parse_config_layer(text)
        .ok()
        .flatten()
        .map(|layer| layer.values)
}

fn parse_config_layer(text: &str) -> Result<Option<ConfigLayer>, toml::de::Error> {
    #[derive(serde::Deserialize, Default)]
    struct RawProvider {
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        reasoning_format: Option<crate::providers::openai::OpenAiReasoningFormat>,
        #[serde(default)]
        prompt_cache_key: Option<bool>,
        #[serde(default)]
        enabled: Option<bool>,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawModel {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        context_budget: Option<u64>,
        #[serde(default)]
        compact_threshold_ratio: Option<f64>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        reasoning: Option<String>,
        #[serde(default)]
        reasoning_mode: Option<String>,
        #[serde(default)]
        reasoning_budget_tokens: Option<u32>,
        #[serde(default)]
        reasoning_efforts: Option<Vec<ReasoningEffort>>,
        #[serde(default)]
        default_reasoning_effort: Option<ReasoningEffort>,
        #[serde(default)]
        reasoning_modes: Option<Vec<ReasoningExecutionMode>>,
        #[serde(default)]
        default_reasoning_mode: Option<ReasoningExecutionMode>,
        #[serde(default)]
        input_modalities: Option<Vec<InputModality>>,
        #[serde(default)]
        image_detail: Option<ImageDetail>,
        #[serde(default)]
        max_tokens: Option<u32>,
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        discovered: bool,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawAlias {
        model: String,
    }

    #[derive(serde::Deserialize, Default)]
    struct RawFile {
        #[serde(default)]
        providers: std::collections::HashMap<String, RawProvider>,
        #[serde(default)]
        models: std::collections::HashMap<String, RawModel>,
        #[serde(default)]
        alias: std::collections::HashMap<String, RawAlias>,
    }

    let raw: RawFile = toml::from_str(text)?;
    let mut cfg = ProviderConfig::default();
    let mut declarations = HashMap::new();

    for (key, p) in raw.providers {
        cfg.providers.insert(
            key.clone(),
            ProviderEntry {
                name: p.name.unwrap_or(key),
                kind: p.kind.unwrap_or_default(),
                api_key: p.api_key,
                api_key_env: p.api_key_env,
                base_url: p.base_url,
                max_tokens: p.max_tokens,
                reasoning_format: p.reasoning_format,
                prompt_cache_key: p.prompt_cache_key,
                enabled: p.enabled,
            },
        );
    }

    for (name, m) in raw.models {
        declarations.insert(
            name.clone(),
            CapabilityDeclarations {
                reasoning_efforts: m.reasoning_efforts.is_some(),
                reasoning_modes: m.reasoning_modes.is_some(),
                input_modalities: m.input_modalities.is_some(),
            },
        );
        cfg.models.insert(
            name,
            ModelEntry {
                model: m.model.unwrap_or_default(),
                provider: m.provider,
                context_budget: m.context_budget,
                compact_threshold_ratio: m.compact_threshold_ratio,
                thinking: m.thinking,
                reasoning: m.reasoning,
                reasoning_mode: m.reasoning_mode,
                reasoning_budget_tokens: m.reasoning_budget_tokens,
                reasoning_efforts: m.reasoning_efforts.unwrap_or_default(),
                default_reasoning_effort: m.default_reasoning_effort,
                reasoning_modes: m.reasoning_modes.unwrap_or_default(),
                default_reasoning_mode: m.default_reasoning_mode,
                input_modalities: m.input_modalities.unwrap_or_default(),
                image_detail: m.image_detail,
                max_tokens: m.max_tokens,
                enabled: m.enabled,
                discovered: m.discovered,
            },
        );
    }

    for (name, a) in raw.alias {
        cfg.aliases.insert(name, AliasEntry { model: a.model });
    }

    if cfg.providers.is_empty() && cfg.models.is_empty() && cfg.aliases.is_empty() {
        return Ok(None);
    }
    Ok(Some(ConfigLayer {
        values: cfg,
        capabilities: declarations,
    }))
}

pub fn add_alias_to_config(alias: &str, model: &str) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .add_alias(alias, model)
        .map_err(Into::into)
}

pub fn upsert_provider_config(
    name: &str,
    kind: &str,
    api_key: Option<&str>,
    api_key_env: Option<&str>,
    base_url: Option<&str>,
    max_tokens: Option<u32>,
    enabled: bool,
) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .upsert_provider(crate::config_hub::ProviderConfigUpdate {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            reasoning_format: None,
            prompt_cache_key: None,
            enabled,
        })
        .map_err(Into::into)
}

#[derive(Debug, Clone)]
pub struct ModelConfigUpdate<'a> {
    pub old_name: Option<&'a str>,
    pub name: &'a str,
    pub model: &'a str,
    pub provider: Option<&'a str>,
    pub context_budget: u64,
    pub reasoning: ReasoningSelection,
    pub capabilities: Option<ModelCapabilities>,
    pub image_detail: Option<ImageDetail>,
    pub max_tokens: Option<u32>,
    pub enabled: bool,
}

pub(crate) fn apply_model_config_update(
    doc: &mut toml_edit::DocumentMut,
    update: ModelConfigUpdate<'_>,
) -> anyhow::Result<()> {
    if doc.get("models").is_none() {
        doc.insert("models", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    {
        let models = doc
            .get_mut("models")
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| anyhow::anyhow!("models is not a table"))?;
        if let Some(old_name) = update.old_name.filter(|old| *old != update.name) {
            models.remove(old_name);
        }
        let entry = models
            .entry(update.name)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("model entry is not a table"))?;
        entry.insert("model", toml_edit::value(update.model));
        if let Some(provider) = update.provider {
            entry.insert("provider", toml_edit::value(provider));
        } else {
            entry.remove("provider");
        }
        entry.insert(
            "context_budget",
            toml_edit::value(update.context_budget as i64),
        );
        entry.remove("thinking");
        entry.remove("reasoning");
        entry.remove("reasoning_mode");
        entry.remove("reasoning_budget_tokens");
        match &update.reasoning {
            ReasoningSelection::ProviderDefault => {}
            ReasoningSelection::Disabled => {
                entry.insert("reasoning", toml_edit::value("off"));
            }
            ReasoningSelection::Auto { execution_mode } => {
                entry.insert("reasoning", toml_edit::value("auto"));
                if let Some(mode) = execution_mode {
                    entry.insert("reasoning_mode", toml_edit::value(mode.to_string()));
                }
            }
            ReasoningSelection::Effort {
                effort,
                execution_mode,
            } => {
                entry.insert("reasoning", toml_edit::value(effort.to_string()));
                if let Some(mode) = execution_mode {
                    entry.insert("reasoning_mode", toml_edit::value(mode.to_string()));
                }
            }
            ReasoningSelection::BudgetTokens { tokens } => {
                entry.insert(
                    "reasoning_budget_tokens",
                    toml_edit::value(i64::from(*tokens)),
                );
            }
        }
        if let Some(capabilities) = update.capabilities {
            insert_string_array(
                entry,
                "reasoning_efforts",
                capabilities
                    .reasoning_efforts
                    .iter()
                    .map(ToString::to_string),
            );
            if let Some(default) = capabilities.default_reasoning_effort {
                entry.insert(
                    "default_reasoning_effort",
                    toml_edit::value(default.to_string()),
                );
            } else {
                entry.remove("default_reasoning_effort");
            }
            insert_string_array(
                entry,
                "reasoning_modes",
                capabilities.reasoning_modes.iter().map(ToString::to_string),
            );
            if let Some(default) = capabilities.default_reasoning_mode {
                entry.insert(
                    "default_reasoning_mode",
                    toml_edit::value(default.to_string()),
                );
            } else {
                entry.remove("default_reasoning_mode");
            }
            insert_string_array(
                entry,
                "input_modalities",
                capabilities
                    .input_modalities
                    .iter()
                    .map(|modality| match modality {
                        InputModality::Text => "text".to_string(),
                        InputModality::Image => "image".to_string(),
                        InputModality::Audio => "audio".to_string(),
                    }),
            );
        }
        if let Some(detail) = update.image_detail {
            let value = match detail {
                ImageDetail::Auto => "auto",
                ImageDetail::Low => "low",
                ImageDetail::High => "high",
                ImageDetail::Original => "original",
            };
            entry.insert("image_detail", toml_edit::value(value));
        }
        if let Some(max_tokens) = update.max_tokens {
            entry.insert("max_tokens", toml_edit::value(max_tokens as i64));
        } else {
            entry.remove("max_tokens");
        }
        entry.insert("enabled", toml_edit::value(update.enabled));
    }

    if let Some(old_name) = update.old_name.filter(|old| *old != update.name)
        && let Some(aliases) = doc.get_mut("alias").and_then(|item| item.as_table_mut())
    {
        for (_, alias) in aliases.iter_mut() {
            if let Some(table) = alias.as_table_mut() {
                if table.get("model").and_then(|item| item.as_str()) == Some(old_name) {
                    table.insert("model", toml_edit::value(update.name));
                }
            } else if let Some(inline) = alias.as_inline_table_mut()
                && inline.get("model").and_then(|value| value.as_str()) == Some(old_name)
            {
                inline.insert("model", toml_edit::Value::from(update.name));
            }
        }
    }
    Ok(())
}

fn insert_string_array(
    entry: &mut toml_edit::Table,
    key: &str,
    values: impl Iterator<Item = String>,
) {
    let mut array = toml_edit::Array::new();
    for value in values {
        array.push(value);
    }
    entry.insert(key, toml_edit::value(array));
}

pub fn upsert_model_config(update: ModelConfigUpdate<'_>) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .upsert_model(update)
        .map_err(Into::into)
}

pub fn remove_alias_from_config(alias: &str) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .remove_alias(alias)
        .map_err(Into::into)
}

pub fn update_alias_in_config(
    old_alias: &str,
    new_alias: &str,
    new_model: &str,
) -> anyhow::Result<()> {
    crate::config_hub::ConfigHub::global()?
        .update_alias(Some(old_alias), new_alias, new_model)
        .map_err(Into::into)
}

// Provider presets + first-run detection

pub struct ProviderPreset {
    pub name: &'static str,
    pub description: &'static str,
    pub base_url: &'static str,
    pub provider_type: &'static str,
    pub models: &'static [ProviderPresetModel],
    pub key_url: Option<&'static str>,
    pub needs_api_key: bool,
}

pub struct ProviderPresetModel {
    pub id: &'static str,
    pub description: &'static str,
    pub context_budget: u64,
    pub thinking: bool,
}

pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "DeepSeek",
        description: "Recommended — cheap, smart, supports thinking",
        base_url: "https://api.deepseek.com",
        provider_type: "openai-compat",
        models: &[
            ProviderPresetModel {
                id: "deepseek-v4-flash",
                description: "Fast & capable",
                context_budget: 1000000,
                thinking: false,
            },
            ProviderPresetModel {
                id: "deepseek-v4-pro",
                description: "Thinking mode",
                context_budget: 1000000,
                thinking: true,
            },
        ],
        key_url: Some("https://platform.deepseek.com"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "OpenAI",
        description: "GPT-4o / GPT-4o-mini",
        base_url: "https://api.openai.com/v1",
        provider_type: "openai",
        models: &[
            ProviderPresetModel {
                id: "gpt-4o",
                description: "Most capable",
                context_budget: 128000,
                thinking: false,
            },
            ProviderPresetModel {
                id: "gpt-4o-mini",
                description: "Fast & cheap",
                context_budget: 128000,
                thinking: false,
            },
        ],
        key_url: Some("https://platform.openai.com/api-keys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "Anthropic",
        description: "Claude models",
        base_url: "https://api.anthropic.com",
        provider_type: "anthropic",
        models: &[ProviderPresetModel {
            id: "claude-sonnet-4-20250514",
            description: "Claude Sonnet 4",
            context_budget: 200000,
            thinking: true,
        }],
        key_url: Some("https://console.anthropic.com/settings/keys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "ZhipuAI",
        description: "GLM models",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        provider_type: "openai-compat",
        models: &[ProviderPresetModel {
            id: "glm-5.2",
            description: "GLM 5.2",
            context_budget: 1000000,
            thinking: true,
        }],
        key_url: Some("https://open.bigmodel.cn/usercenter/apikeys"),
        needs_api_key: true,
    },
    ProviderPreset {
        name: "Ollama",
        description: "Local models, no API key needed",
        base_url: "http://localhost:11434/v1",
        provider_type: "openai-compat",
        models: &[],
        key_url: None,
        needs_api_key: false,
    },
    ProviderPreset {
        name: "Codex",
        description: "ChatGPT Plus/Pro OAuth",
        base_url: "https://chatgpt.com/backend-api/codex",
        provider_type: "codex",
        models: &[],
        key_url: None,
        needs_api_key: false,
    },
];

pub fn is_first_run() -> bool {
    let providers = all_provider_entries();
    let config_configured = providers.iter().any(|(_, e)| {
        e.api_key.as_deref().is_some_and(|k| !k.is_empty())
            || e.api_key_env
                .as_deref()
                .is_some_and(|env| std::env::var(env).is_ok_and(|v| !v.trim().is_empty()))
    });
    let env_configured =
        std::env::var("ANTHROPIC_API_KEY").is_ok() || std::env::var("OPENAI_API_KEY").is_ok();
    let auth_configured = AuthStore::load()
        .is_ok_and(|store| store.providers.iter().any(|provider| provider.enabled));
    let smart_resolves = {
        let resolved = resolve_alias("smart");
        resolved != "smart" && model_entry(&resolved).is_some()
    };
    !(config_configured || env_configured || auth_configured) || !smart_resolves
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that mutate the global registry must hold this lock to avoid races
    /// when cargo test runs them in parallel.
    static TEST_CFG_LOCK: &std::sync::Mutex<()> = &MODEL_CONFIG_LOCK;

    struct IsolatedRegistry {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for IsolatedRegistry {
        fn drop(&mut self) {
            *REGISTRY_STATE.write().unwrap() = RegistryState::default();
            CATALOG_REVISION.send_replace(0);
        }
    }

    fn isolated_registry() -> IsolatedRegistry {
        let lock = TEST_CFG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *REGISTRY_STATE.write().unwrap() = RegistryState::default();
        CATALOG_REVISION.send_replace(0);
        IsolatedRegistry { _lock: lock }
    }

    fn descriptor(
        provider_key: &str,
        namespace: &str,
        wire_profile: ReasoningWireProfile,
    ) -> ProviderDescriptor {
        ProviderDescriptor {
            provider_key: provider_key.to_string(),
            provider_name: "Test Provider".to_string(),
            namespace: namespace.to_string(),
            wire_profile,
        }
    }

    fn advertised_model(
        slug: &str,
        context_budget: u64,
        reasoning_efforts: Vec<ReasoningEffort>,
    ) -> crate::provider::DiscoveredModelDetails {
        crate::provider::DiscoveredModelDetails {
            slug: slug.to_string(),
            context_budget: Some(context_budget),
            capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities {
                reasoning_efforts,
                input_modalities: vec![InputModality::Text, InputModality::Image],
                ..Default::default()
            }),
        }
    }

    #[test]
    fn enabled_auth_provider_names_match_discovered_model_groups() {
        let _registry = isolated_registry();
        let auth = AuthStore {
            providers: vec![
                crate::auth_store::StoredProvider {
                    id: "disabled-id".into(),
                    name: "Codex".into(),
                    kind: crate::auth_store::ProviderKind::Codex,
                    access_token: String::new(),
                    refresh_token: None,
                    expires_at: 0,
                    account: Some("old@example.com".into()),
                    enabled: false,
                    model_cache: None,
                },
                crate::auth_store::StoredProvider {
                    id: "enabled-id".into(),
                    name: "Codex".into(),
                    kind: crate::auth_store::ProviderKind::Codex,
                    access_token: String::new(),
                    refresh_token: None,
                    expires_at: 0,
                    account: Some("current@example.com".into()),
                    enabled: true,
                    model_cache: None,
                },
            ],
        };

        let names = enabled_provider_names_from_auth(&auth);
        assert!(names.contains("enabled-id"));
        assert!(!names.contains("disabled-id"));

        let models = vec![crate::provider::DiscoveredModelDetails {
            slug: "codex/gpt-test".into(),
            context_budget: Some(272_000),
            capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                ModelCapabilities::default(),
            ),
        }];
        assert_eq!(
            shortest_unique_provider_id("1234567-account", &["1234567-account".into()]),
            "123456"
        );
        assert_eq!(
            shortest_unique_provider_id(
                "abcdef1-account",
                &["abcdef1-account".into(), "abcdef2-account".into()]
            ),
            "abcdef1"
        );
        register_discovered_details_for_provider("enabled-id", "Codex", &models).unwrap();
        let model_key = "enable@Codex:codex/gpt-test";
        let entry = model_entry(model_key).unwrap();
        assert_eq!(entry.provider.as_deref(), Some("enabled-id"));
        assert_eq!(entry.model, "codex/gpt-test");
    }

    #[test]
    fn unregistered_model_returns_zero_budget() {
        let _registry = isolated_registry();
        *REGISTRY_STATE.write().unwrap() = RegistryState::default();
        assert_eq!(model_info("mystery-model").context_budget, 0);
        assert_eq!(model_info("").context_budget, 0);
    }

    #[test]
    fn threshold_is_eighty_percent() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        assert_eq!(info.compact_threshold_tokens(), 160_000);
    }

    #[test]
    fn compaction_trigger_is_near_budget_top() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        let trigger = info.compaction_trigger_threshold();
        assert!(
            trigger > 150_000 && trigger <= 190_000,
            "trigger should be near the top of the budget, got {trigger}"
        );
    }

    #[test]
    fn compaction_target_is_lower_than_trigger() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("claude-opus-4.7");
        let trigger = info.compaction_trigger_threshold();
        let target = info.compaction_target_after();
        assert!(
            target < trigger,
            "target {target} should be less than trigger {trigger}"
        );
        assert_eq!(target, 50_000, "compaction target should be 25% of context");
    }

    #[test]
    fn alias_resolves_to_real_model() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "claude-opus-4.7".into(),
            ModelEntry {
                model: "claude-opus-4.7".into(),
                context_budget: Some(200_000),
                ..Default::default()
            },
        );
        cfg.aliases.insert(
            "smart".into(),
            AliasEntry {
                model: "claude-opus-4.7".into(),
            },
        );
        set_model_config(cfg);
        let info = model_info("smart");
        assert_eq!(info.context_budget, 200_000);
        assert_eq!(info.name, "claude-opus-4.7");
    }

    #[test]
    fn custom_model_overrides_budget() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "my-local-model".into(),
            ModelEntry {
                model: "my-local-model".into(),
                context_budget: Some(8192),
                compact_threshold_ratio: Some(0.9),
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("my-local-model");
        assert_eq!(info.context_budget, 8192);
        assert_eq!(info.compact_threshold_ratio, 0.9);
    }

    #[test]
    fn compact_threshold_reserves_configured_output_tokens() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.models.insert(
            "large-output".into(),
            ModelEntry {
                model: "large-output".into(),
                context_budget: Some(1_000_000),
                compact_threshold_ratio: Some(0.8),
                thinking: None,
                max_tokens: Some(400_000),
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("large-output");
        assert_eq!(info.compact_threshold_tokens(), 480_000);
        let trigger = info.compaction_trigger_threshold();
        assert!(
            trigger > 700_000,
            "trigger with capped output reserve should be > 700K, got {trigger}"
        );
    }

    #[test]
    fn alias_chains_through_custom_model() {
        let _registry = isolated_registry();
        let mut cfg = ModelConfig::default();
        cfg.aliases.insert(
            "default".into(),
            AliasEntry {
                model: "my-model".into(),
            },
        );
        cfg.models.insert(
            "my-model".into(),
            ModelEntry {
                model: "my-model".into(),
                context_budget: Some(65_536),
                compact_threshold_ratio: None,
                thinking: None,
                ..Default::default()
            },
        );
        set_model_config(cfg);
        let info = model_info("default");
        assert_eq!(info.name, "my-model");
        assert_eq!(info.context_budget, 65_536);
    }

    #[test]
    fn discovered_models_survive_set_model_config() {
        let _registry = isolated_registry();
        set_discovered_models(vec!["z".into(), "a".into(), "z".into()]);
        assert_eq!(discovered_models(), ["z", "a", "z"]);
        register_discovered(
            "pid-abc",
            "Codex",
            &[crate::provider::DiscoveredModel {
                slug: "codex/gpt-5".to_string(),
                context_budget: Some(128_000),
                thinking: true,
            }],
        );
        assert!(model_entry("Codex:codex/gpt-5").is_some());
        assert_eq!(
            model_entry("Codex:codex/gpt-5").unwrap().model,
            "Codex:codex/gpt-5"
        );
        register_discovered("pid-abc", "Codex", &[]);
        assert!(model_entry("Codex:codex/gpt-5").is_some());

        let mut cfg = ModelConfig::default();
        cfg.aliases.insert(
            "cheap".into(),
            AliasEntry {
                model: "claude-opus-4.7".into(),
            },
        );
        set_model_config(cfg);

        assert!(
            model_entry("Codex:codex/gpt-5").is_some(),
            "discovered models should survive set_model_config"
        );
        assert_eq!(resolve_alias("cheap"), "claude-opus-4.7");
    }

    #[test]
    fn reload_replaces_config_models_and_preserves_discovered_models() {
        let _registry = isolated_registry();
        let mut initial = ProviderConfig::default();
        initial.models.insert(
            "old-config".into(),
            ModelEntry {
                model: "provider/old".into(),
                discovered: false,
                ..Default::default()
            },
        );
        set_provider_config(initial);
        register_discovered_details(
            "dynamic-provider",
            "Dynamic",
            &[crate::provider::DiscoveredModelDetails {
                slug: "provider/dynamic".into(),
                context_budget: Some(64_000),
                capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities::default()),
            }],
        )
        .unwrap();

        reload_from_text(
            r#"
[models.new-config]
model = "provider/new"
"#,
        )
        .unwrap();

        assert!(model_entry("old-config").is_none());
        assert!(model_entry("new-config").is_some());
        assert!(model_entry("dynami@Dynamic:provider/dynamic").is_some());
    }

    #[test]
    fn discovered_models_survive_reload_from_text_alias_crud() {
        let _registry = isolated_registry();
        register_discovered(
            "pid-abc",
            "Codex",
            &[crate::provider::DiscoveredModel {
                slug: "codex/gpt-5".to_string(),
                context_budget: Some(128_000),
                thinking: true,
            }],
        );

        let toml = r#"
[alias]
smart = { model = "Codex:codex/gpt-5" }
"#;
        reload_from_text(toml).unwrap();

        assert!(
            model_entry("Codex:codex/gpt-5").is_some(),
            "discovered models should survive alias CRUD"
        );
        assert_eq!(resolve_alias("smart"), "Codex:codex/gpt-5");
    }

    #[test]
    fn model_config_update_replaces_name_and_preserves_other_sections() {
        let mut doc = r#"
# keep this comment
[providers.openai]
kind = "openai"

[alias]
smart = { model = "old-name" }
cheap = { model = "other-name" }

[alias.deep]
model = "old-name"

[models.old-name]
model = "old-id"
provider = "openai"
enabled = true
"#
        .parse::<toml_edit::DocumentMut>()
        .unwrap();

        apply_model_config_update(
            &mut doc,
            ModelConfigUpdate {
                old_name: Some("old-name"),
                name: "new-name",
                model: "new-id",
                provider: Some("openai"),
                context_budget: 128_000,
                reasoning: ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
                capabilities: None,
                image_detail: None,
                max_tokens: Some(4096),
                enabled: false,
            },
        )
        .unwrap();

        let out = doc.to_string();
        assert!(out.contains("# keep this comment"));
        assert!(out.contains("[providers.openai]"));
        assert!(out.contains("[alias]"));
        assert!(out.contains("smart = { model = \"new-name\" }"));
        assert!(out.contains("cheap = { model = \"other-name\" }"));
        assert!(out.contains("[alias.deep]"));
        assert!(out.contains("model = \"new-name\""));
        assert!(out.contains("[models.new-name]"));
        assert!(!out.contains("[models.old-name]"));
        assert!(out.contains("model = \"new-id\""));
        assert!(out.contains("context_budget = 128000"));
        assert!(out.contains("reasoning = \"high\""));
        assert!(out.contains("max_tokens = 4096"));
        assert!(out.contains("enabled = false"));
    }

    #[test]
    fn model_config_update_preserves_explicit_empty_capability_knowledge() {
        let _registry = isolated_registry();
        let mut doc = r#"
[providers.official]
kind = "openai"
"#
        .parse::<toml_edit::DocumentMut>()
        .unwrap();

        apply_model_config_update(
            &mut doc,
            ModelConfigUpdate {
                old_name: None,
                name: "known-empty",
                model: "api/model",
                provider: Some("official"),
                context_budget: 128_000,
                reasoning: ReasoningSelection::ProviderDefault,
                capabilities: Some(ModelCapabilities::default()),
                image_detail: None,
                max_tokens: None,
                enabled: true,
            },
        )
        .unwrap();

        let out = doc.to_string();
        assert!(out.contains("reasoning_efforts = []"));
        assert!(out.contains("reasoning_modes = []"));
        assert!(out.contains("input_modalities = []"));
        reload_from_text(&out).unwrap();
        assert_eq!(
            reasoning_selections_for_model("known-empty")
                .into_iter()
                .map(|selection| selection.to_string())
                .collect::<Vec<_>>(),
            ["default", "off", "auto"]
        );
    }

    #[test]
    fn model_config_reads_reasoning_capabilities_and_legacy_bool() {
        let _registry = isolated_registry();
        let cfg = parse_config(
            r#"
[models.modern]
model = "gpt-modern"
reasoning = "xhigh"
reasoning_mode = "pro"
reasoning_efforts = ["low", "high", "xhigh"]
reasoning_modes = ["standard", "pro"]
input_modalities = ["text", "image"]
image_detail = "high"

[models.legacy]
model = "legacy"
thinking = true
"#,
        )
        .unwrap();
        set_model_config(cfg);

        let modern = model_info("modern");
        assert_eq!(
            modern.reasoning,
            ReasoningSelection::Effort {
                effort: ReasoningEffort::XHigh,
                execution_mode: Some(ReasoningExecutionMode::Pro),
            }
        );
        assert_eq!(
            modern.capabilities.input_modalities,
            vec![InputModality::Text, InputModality::Image]
        );
        assert_eq!(modern.image_detail, ImageDetail::High);
        assert_eq!(
            model_info("legacy").reasoning,
            ReasoningSelection::Auto {
                execution_mode: None
            }
        );
    }

    #[test]
    fn provider_config_reads_explicit_reasoning_wire_format() {
        let cfg = parse_config(
            r#"
[providers.openai-compatible]
kind = "openai-compat"
reasoning_format = "reasoning-effort"
"#,
        )
        .unwrap();

        assert_eq!(
            cfg.providers["openai-compatible"].reasoning_format,
            Some(crate::providers::openai::OpenAiReasoningFormat::Official)
        );
    }

    #[test]
    fn known_provider_profile_displays_default_reasoning_without_model_hints() {
        let _registry = isolated_registry();
        let cfg = parse_config(
            r#"
[providers.messages]
kind = "anthropic"

[models.plain]
model = "claude-plain"
provider = "messages"
context_budget = 128000

[models.disabled]
model = "claude-disabled"
provider = "messages"
context_budget = 128000
thinking = false

[alias]
smart = { model = "plain" }
"#,
        )
        .unwrap();
        set_provider_config(cfg);

        assert_eq!(
            effective_reasoning_for_model("smart", None).unwrap(),
            Some(ReasoningSelection::ProviderDefault)
        );
        assert_eq!(
            effective_reasoning_for_model("disabled", None).unwrap(),
            Some(ReasoningSelection::Disabled)
        );
    }

    #[test]
    fn compatible_thinking_profile_limits_shared_model_choices() {
        let _registry = isolated_registry();
        let cfg = parse_config(
            r#"
[providers.openai-compatible]
kind = "openai-compat"
reasoning_format = "thinking-toggle"

[models.custom-reasoning]
model = "vendor/reasoning-model"
provider = "openai-compatible"
context_budget = 128000
thinking = true
reasoning_efforts = ["high"]
default_reasoning_effort = "high"
"#,
        )
        .unwrap();
        set_provider_config(cfg);

        assert_eq!(
            reasoning_wire_profile_for_model("custom-reasoning"),
            ReasoningWireProfile::CompatibleThinking
        );
        assert_eq!(
            reasoning_selections_for_model("custom-reasoning")
                .into_iter()
                .map(|selection| selection.to_string())
                .collect::<Vec<_>>(),
            ["default", "off", "auto"]
        );
        assert_eq!(
            resolve_reasoning_for_model(
                "custom-reasoning",
                &ReasoningSelection::Auto {
                    execution_mode: None,
                },
            )
            .unwrap(),
            ReasoningSelection::Auto {
                execution_mode: None,
            }
        );
        assert!(
            resolve_reasoning_for_model(
                "custom-reasoning",
                &ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
            )
            .unwrap_err()
            .contains("cannot represent effort `high`")
        );
    }

    #[test]
    fn provider_catalog_replacement_is_atomic_and_tracks_api_identity() {
        let _registry = isolated_registry();
        let original = descriptor(
            "catalog-provider",
            "stable",
            ReasoningWireProfile::CodexResponses,
        );
        let first = vec![
            advertised_model("api/a", 64_000, vec![ReasoningEffort::Low]),
            advertised_model("api/b", 128_000, vec![ReasoningEffort::High]),
        ];
        assert_eq!(
            replace_provider_catalog(original.clone(), &first).unwrap(),
            CatalogDelta {
                added: 2,
                updated: 0,
                removed: 0,
                total: 2,
            }
        );
        let first_revision = model_catalog_revision();
        assert_eq!(model_entry("stable:api/a").unwrap().model, "api/a");

        let colliding = descriptor(
            "other-provider",
            "stable",
            ReasoningWireProfile::OpenAiOfficial,
        );
        assert!(matches!(
            replace_provider_catalog(colliding, &[advertised_model("api/other", 32_000, vec![])]),
            Err(CatalogError::NamespaceInUse { .. })
        ));
        assert_eq!(model_catalog_revision(), first_revision);
        assert!(model_entry("stable:api/a").is_some());

        replace_provider_catalog(
            descriptor("colon-left", "a", ReasoningWireProfile::OpenAiOfficial),
            &[advertised_model("b:c", 32_000, vec![])],
        )
        .unwrap();
        replace_provider_catalog(
            descriptor("colon-right", "a:b", ReasoningWireProfile::OpenAiOfficial),
            &[advertised_model("c", 32_000, vec![])],
        )
        .unwrap();
        assert_eq!(model_entry("a:b:c").unwrap().model, "b:c");
        assert_eq!(model_entry("a%3Ab:c").unwrap().model, "c");

        let revision_after_colon_namespaces = model_catalog_revision();
        let duplicate = vec![first[0].clone(), first[0].clone()];
        assert!(matches!(
            replace_provider_catalog(original.clone(), &duplicate),
            Err(CatalogError::DuplicateModel { .. })
        ));
        assert_eq!(model_catalog_revision(), revision_after_colon_namespaces);
        assert!(model_entry("stable:api/b").is_some());

        let second = vec![
            advertised_model("api/b", 256_000, vec![ReasoningEffort::XHigh]),
            advertised_model("api/c", 512_000, vec![ReasoningEffort::High]),
        ];
        assert_eq!(
            replace_provider_catalog(original.clone(), &second).unwrap(),
            CatalogDelta {
                added: 1,
                updated: 1,
                removed: 1,
                total: 2,
            }
        );
        assert!(model_entry("stable:api/a").is_none());
        assert_eq!(
            model_entry("stable:api/b").unwrap().context_budget,
            Some(256_000)
        );
        assert_eq!(model_entry("stable:api/c").unwrap().model, "api/c");

        let renamed_namespace = descriptor(
            "catalog-provider",
            "changed",
            ReasoningWireProfile::CodexResponses,
        );
        assert!(matches!(
            replace_provider_catalog(renamed_namespace, &second),
            Err(CatalogError::NamespaceChanged { .. })
        ));
        assert!(model_entry("stable:api/b").is_some());

        let empty = ProviderDescriptor {
            wire_profile: ReasoningWireProfile::OpenAiOfficial,
            ..original
        };
        assert_eq!(
            replace_provider_catalog(empty, &[]).unwrap(),
            CatalogDelta {
                added: 0,
                updated: 0,
                removed: 2,
                total: 0,
            }
        );
        assert!(model_entry("stable:api/b").is_none());
        assert_eq!(
            reasoning_wire_profile_for_provider("catalog-provider"),
            ReasoningWireProfile::OpenAiOfficial
        );
        assert!(
            all_provider_groups_with_empty()
                .iter()
                .any(|group| group.provider_name == "catalog-provider" && group.models.is_empty())
        );
    }

    #[test]
    fn provider_namespace_prefixes_distinguish_peers_and_normalization_collisions() {
        assert_eq!(
            shortest_unique_provider_id(
                "abcdef-1111",
                &["abcdef-1111".into(), "abcdef-2222".into()],
            ),
            "abcdef1"
        );
        assert_eq!(
            shortest_unique_provider_id("abc-def", &["abc-def".into(), "abcdef".into()]),
            "abc-def"
        );
    }

    #[test]
    fn config_overlay_uses_identity_and_explicit_empty_capabilities() {
        let _registry = isolated_registry();
        replace_provider_catalog(
            descriptor(
                "catalog-provider",
                "stable",
                ReasoningWireProfile::OpenAiOfficial,
            ),
            &[advertised_model(
                "api/reasoning",
                128_000,
                vec![ReasoningEffort::High],
            )],
        )
        .unwrap();

        reload_from_text(
            r#"
[models.renamed]
model = "api/reasoning"
provider = "catalog-provider"
reasoning_efforts = []
"#,
        )
        .unwrap();

        assert!(model_entry("stable:api/reasoning").is_none());
        let renamed = model_entry("renamed").unwrap();
        assert_eq!(renamed.model, "api/reasoning");
        assert!(renamed.reasoning_efforts.is_empty());
        assert_eq!(
            renamed.input_modalities,
            vec![InputModality::Text, InputModality::Image]
        );
        assert_eq!(
            reasoning_selections_for_model("renamed")
                .into_iter()
                .map(|selection| selection.to_string())
                .collect::<Vec<_>>(),
            ["default", "off", "auto"]
        );
        assert!(
            resolve_reasoning_for_model(
                "renamed",
                &ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
            )
            .unwrap_err()
            .contains("does not advertise exact reasoning efforts")
        );

        assert!(reload_from_text("[models.broken]\ncontext_budget = \"large\"").is_err());
        assert!(model_entry("renamed").is_some());
        assert!(model_entry("broken").is_none());

        assert!(remove_provider_catalog("catalog-provider"));
        let remaining = model_entry("renamed").unwrap();
        assert!(remaining.reasoning_efforts.is_empty());
        assert!(remaining.input_modalities.is_empty());
    }

    #[test]
    fn same_key_config_requires_api_model_to_inherit_catalog_metadata() {
        let _registry = isolated_registry();
        replace_provider_catalog(
            descriptor(
                "catalog-provider",
                "stable",
                ReasoningWireProfile::OpenAiOfficial,
            ),
            &[advertised_model(
                "api/reasoning",
                128_000,
                vec![ReasoningEffort::High],
            )],
        )
        .unwrap();

        reload_from_text(
            r#"
[models."stable:api/reasoning"]
model = "api/reasoning"
context_budget = 112000
"#,
        )
        .unwrap();

        let bound = model_entry("stable:api/reasoning").unwrap();
        assert_eq!(bound.model, "api/reasoning");
        assert_eq!(bound.provider.as_deref(), Some("catalog-provider"));
        assert_eq!(bound.context_budget, Some(112_000));
        assert_eq!(bound.reasoning_efforts, vec![ReasoningEffort::High]);
        assert_eq!(
            bound.input_modalities,
            vec![InputModality::Text, InputModality::Image]
        );
        assert!(bound.discovered);

        reload_from_text(
            r#"
[models."stable:api/reasoning"]
context_budget = 96000
"#,
        )
        .unwrap();

        let model = model_entry("stable:api/reasoning").unwrap();
        assert!(model.model.is_empty());
        assert_eq!(model.provider, None);
        assert_eq!(model.context_budget, Some(96_000));
        assert!(model.reasoning_efforts.is_empty());
        assert!(model.input_modalities.is_empty());
        assert!(!model.discovered);
        assert_eq!(
            reasoning_selections_for_model("stable:api/reasoning")
                .into_iter()
                .map(|selection| selection.to_string())
                .collect::<Vec<_>>(),
            ["default", "off", "auto"]
        );
    }

    #[test]
    fn input_capability_override_does_not_hide_reasoning_fallback() {
        let _registry = isolated_registry();
        reload_from_text(
            r#"
[providers.official]
kind = "openai"

[models.vision-only]
model = "api/vision"
provider = "official"
input_modalities = ["text", "image"]
"#,
        )
        .unwrap();

        let choices = reasoning_selections_for_model("vision-only")
            .into_iter()
            .map(|selection| selection.to_string())
            .collect::<Vec<_>>();
        assert!(choices.contains(&"high".to_string()));
        assert!(choices.contains(&"xhigh".to_string()));
        assert!(
            resolve_reasoning_for_model(
                "vision-only",
                &ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
            )
            .is_ok()
        );
    }

    #[test]
    fn legacy_and_advertised_empty_catalogs_have_safe_reasoning_choices() {
        let _registry = isolated_registry();
        replace_provider_catalog(
            descriptor(
                "legacy-provider",
                "legacy",
                ReasoningWireProfile::CodexResponses,
            ),
            &[crate::provider::DiscoveredModelDetails {
                slug: "api/legacy".into(),
                context_budget: Some(64_000),
                capability_knowledge: CapabilityKnowledge::Legacy { thinking: false },
            }],
        )
        .unwrap();
        replace_provider_catalog(
            descriptor(
                "empty-provider",
                "empty",
                ReasoningWireProfile::CodexResponses,
            ),
            &[advertised_model("api/empty", 64_000, vec![])],
        )
        .unwrap();

        for model in ["legacy:api/legacy", "empty:api/empty"] {
            assert_eq!(
                reasoning_selections_for_model(model)
                    .into_iter()
                    .map(|selection| selection.to_string())
                    .collect::<Vec<_>>(),
                ["default", "off", "auto"]
            );
            assert!(
                resolve_reasoning_for_model(
                    model,
                    &ReasoningSelection::Effort {
                        effort: ReasoningEffort::High,
                        execution_mode: None,
                    },
                )
                .is_err()
            );
        }
        assert_eq!(
            effective_reasoning_for_model("legacy:api/legacy", None).unwrap(),
            Some(ReasoningSelection::Disabled)
        );
        assert_eq!(
            effective_reasoning_for_model("empty:api/empty", None).unwrap(),
            Some(ReasoningSelection::ProviderDefault)
        );
    }

    #[test]
    fn uuid_codex_catalog_uses_advertised_efforts_for_choices_and_validation() {
        let _registry = isolated_registry();
        replace_provider_catalog(
            descriptor(
                "018f0f2e-7b9a-7fd0-ae41-8c772ccae64b",
                "account",
                ReasoningWireProfile::CodexResponses,
            ),
            &[advertised_model(
                "gpt-5.6-sol",
                272_000,
                vec![ReasoningEffort::Low, ReasoningEffort::High],
            )],
        )
        .unwrap();
        let mut config = ProviderConfig::default();
        config.aliases.insert(
            "smart".into(),
            AliasEntry {
                model: "account:gpt-5.6-sol".into(),
            },
        );
        set_provider_config(config);

        assert_eq!(
            reasoning_wire_profile_for_model("smart"),
            ReasoningWireProfile::CodexResponses
        );
        assert_eq!(
            reasoning_selections_for_model("smart")
                .into_iter()
                .map(|selection| selection.to_string())
                .collect::<Vec<_>>(),
            ["default", "off", "auto", "low", "high"]
        );
        assert!(
            resolve_reasoning_for_model(
                "smart",
                &ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
            )
            .is_ok()
        );
        let error = resolve_reasoning_for_model(
            "smart",
            &ReasoningSelection::Effort {
                effort: ReasoningEffort::XHigh,
                execution_mode: None,
            },
        )
        .unwrap_err();
        assert!(error.contains("available: low, high"));
    }

    #[test]
    fn preset_and_catalog_overlays_do_not_duplicate_renamed_models() {
        let _registry = isolated_registry();
        let mut cfg = ProviderConfig::default();
        for provider in ["first", "second"] {
            cfg.providers.insert(
                provider.into(),
                ProviderEntry {
                    kind: "openai".into(),
                    base_url: Some("https://api.openai.com/v1".into()),
                    ..Default::default()
                },
            );
        }
        for name in ["renamed", "also-renamed"] {
            cfg.models.insert(
                name.into(),
                ModelEntry {
                    model: "gpt-4o".into(),
                    provider: Some("first".into()),
                    ..Default::default()
                },
            );
        }
        set_provider_config(cfg);

        let names: BTreeSet<_> = all_model_entries()
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains("renamed"));
        assert!(names.contains("also-renamed"));
        assert!(names.contains("second:gpt-4o"));
        assert!(!names.contains("first:gpt-4o"));
        assert!(!names.contains("gpt-4o"));

        replace_provider_catalog(
            descriptor(
                "catalog-provider",
                "stable",
                ReasoningWireProfile::OpenAiOfficial,
            ),
            &[advertised_model(
                "api/reasoning",
                128_000,
                vec![ReasoningEffort::High],
            )],
        )
        .unwrap();
        let mut cfg = ProviderConfig::default();
        cfg.models.insert(
            "stable:api/reasoning".into(),
            ModelEntry {
                model: "api/different".into(),
                provider: Some("different-provider".into()),
                ..Default::default()
            },
        );
        set_provider_config(cfg);
        let shadow = model_entry("stable:api/reasoning").unwrap();
        assert_eq!(shadow.model, "api/different");
        assert!(shadow.reasoning_efforts.is_empty());
    }

    #[test]
    fn catalog_rejects_a_registry_key_owned_by_a_different_preset_identity() {
        let _registry = isolated_registry();
        let mut cfg = ProviderConfig::default();
        cfg.providers.insert(
            "stable".into(),
            ProviderEntry {
                kind: "openai".into(),
                base_url: Some("https://api.openai.com/v1".into()),
                ..Default::default()
            },
        );
        set_provider_config(cfg);

        assert!(matches!(
            replace_provider_catalog(
                descriptor(
                    "different-provider",
                    "stable",
                    ReasoningWireProfile::OpenAiOfficial,
                ),
                &[advertised_model("gpt-4o", 128_000, vec![])],
            ),
            Err(CatalogError::RegistryKeyInUse { .. })
        ));
        assert_eq!(model_entry("stable:gpt-4o").unwrap().model, "gpt-4o");
        assert_eq!(api_model_id("stable:gpt-4o"), "gpt-4o");
    }

    #[test]
    fn config_reload_rejects_a_preset_key_owned_by_a_different_catalog_identity() {
        let _registry = isolated_registry();
        replace_provider_catalog(
            descriptor(
                "different-provider",
                "stable",
                ReasoningWireProfile::OpenAiOfficial,
            ),
            &[advertised_model("gpt-4o", 128_000, vec![])],
        )
        .unwrap();

        let error = reload_from_text(
            r#"
[providers.stable]
kind = "openai"
base_url = "https://api.openai.com/v1"
"#,
        )
        .unwrap_err();

        assert!(error.to_string().contains("already used"));
        assert!(all_provider_entries().is_empty());
        assert_eq!(
            model_entry("stable:gpt-4o").unwrap().provider.as_deref(),
            Some("different-provider")
        );
    }

    #[test]
    fn preset_keys_and_canonical_aliases_do_not_drift_as_providers_change() {
        let _registry = isolated_registry();
        let provider = || ProviderEntry {
            kind: "openai".into(),
            base_url: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        };
        let mut one = ProviderConfig::default();
        one.providers.insert("first".into(), provider());
        one.aliases.insert(
            "smart".into(),
            AliasEntry {
                model: "first:gpt-4o".into(),
            },
        );
        one.aliases.insert(
            "legacy".into(),
            AliasEntry {
                model: "gpt-4o".into(),
            },
        );
        set_provider_config(one.clone());
        assert!(model_entry("first:gpt-4o").is_some());
        assert_eq!(model_entry("smart").unwrap().model, "gpt-4o");
        assert_eq!(model_entry("legacy").unwrap().model, "gpt-4o");

        replace_provider_catalog(
            descriptor(
                "dynamic-provider",
                "dynamic",
                ReasoningWireProfile::OpenAiOfficial,
            ),
            &[advertised_model("gpt-4o", 128_000, vec![])],
        )
        .unwrap();
        assert_eq!(
            model_entry("legacy").unwrap().provider.as_deref(),
            Some("first")
        );

        let mut two = one.clone();
        two.providers.insert("second".into(), provider());
        set_provider_config(two);
        assert!(model_entry("first:gpt-4o").is_some());
        assert!(model_entry("second:gpt-4o").is_some());
        assert_eq!(model_entry("smart").unwrap().model, "gpt-4o");
        assert!(model_entry("legacy").is_none());

        set_provider_config(one);
        assert!(model_entry("first:gpt-4o").is_some());
        assert!(model_entry("second:gpt-4o").is_none());
        assert_eq!(model_entry("smart").unwrap().model, "gpt-4o");
        assert_eq!(model_entry("legacy").unwrap().model, "gpt-4o");
    }

    #[test]
    fn resolver_rejects_known_unsupported_effort() {
        let capabilities = ModelCapabilities {
            reasoning_efforts: vec![ReasoningEffort::Low, ReasoningEffort::High],
            ..Default::default()
        };
        let error = resolve_reasoning(
            &ReasoningSelection::Effort {
                effort: ReasoningEffort::XHigh,
                execution_mode: None,
            },
            &capabilities,
        )
        .unwrap_err();
        assert!(error.contains("available: low, high"));
    }
}
