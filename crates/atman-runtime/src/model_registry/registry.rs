use super::*;
use std::sync::{Mutex, RwLock};

/// Model configuration, catalogs, and change notifications owned by one host scope.
/// Preparation holds this registry's transaction until commit or drop; no process-global configuration is read by the registry.
pub struct ModelRegistry {
    pub(super) state: RwLock<RegistryState>,
    pub(super) catalog_revision: tokio::sync::watch::Sender<u64>,
    transaction: Mutex<()>,
}

impl std::fmt::Debug for ModelRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ModelRegistry")
            .finish_non_exhaustive()
    }
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self {
            state: RwLock::new(RegistryState::default()),
            catalog_revision: tokio::sync::watch::channel(0).0,
            transaction: Mutex::new(()),
        }
    }
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically replace configured models, aliases, and providers while preserving dynamic catalogs.
    pub fn replace_config(&self, config: ProviderConfig) -> Result<(), CatalogError> {
        self.install_config_layer(ConfigLayer::inferred(config))
    }

    /// Parse and replace configuration without losing explicit empty capability declarations.
    pub fn replace_config_text(&self, text: &str) -> anyhow::Result<()> {
        let prepared = self.prepare_config_text(text)?;
        commit_prepared_config(prepared);
        Ok(())
    }

    /// Return configuration and cached catalogs for management, including disabled providers. Cached records are projected without registering callable models or changing provider state.
    pub fn all_provider_groups_for_management(
        &self,
        auth: &AuthStore,
        hub: Option<&crate::config_hub::ConfigHub>,
    ) -> Vec<ProviderGroup> {
        let mut state = self.state.read().unwrap().clone();
        for provider in &auth.providers {
            if state.catalogs.contains_key(&provider.id) {
                continue;
            }
            let namespace = hub
                .as_ref()
                .and_then(|hub| hub.load_auth_model_namespace(&provider.id).ok().flatten());
            let Ok(namespace) = provider_namespace(
                &state,
                &provider.id,
                &provider.name,
                auth,
                namespace.as_deref(),
            ) else {
                continue;
            };
            let models = hub
                .as_ref()
                .and_then(|hub| {
                    hub.load_auth_model_cache_details(&provider.id)
                        .ok()
                        .flatten()
                })
                .or_else(|| {
                    provider
                        .model_cache
                        .as_ref()
                        .map(crate::auth_store::cached_to_discovered_details)
                })
                .unwrap_or_default();
            let descriptor = ProviderDescriptor {
                provider_key: provider.id.clone(),
                provider_name: provider.name.clone(),
                namespace,
                wire_profile: match provider.kind {
                    crate::auth_store::ProviderKind::Codex => ReasoningWireProfile::CodexResponses,
                    crate::auth_store::ProviderKind::AnthropicOauth => {
                        ReasoningWireProfile::AnthropicMessages
                    }
                    crate::auth_store::ProviderKind::GitHubCopilot
                    | crate::auth_store::ProviderKind::Custom => ReasoningWireProfile::Unknown,
                },
            };
            if let Ok(catalog) = build_provider_catalog(descriptor, &models) {
                state.catalogs.insert(provider.id.clone(), catalog);
            }
        }
        provider_groups_in_state(&state, true, true)
    }

    pub fn is_provider_enabled(&self, name: &str, auth: &AuthStore) -> bool {
        let state = self.state.read().unwrap();
        if let Some(entry) = provider_entries_in_state(&state).get(name) {
            return entry.enabled.unwrap_or(true);
        }
        let auth_provider = auth.providers.iter().find(|provider| provider.id == name);
        if state.catalogs.contains_key(name) {
            return auth_provider.is_some_and(|provider| provider.enabled);
        }
        auth_provider.is_none_or(|provider| provider.enabled)
    }

    pub fn set_discovered_models(&self, models: Vec<String>) {
        let _transaction = self.transaction.lock().unwrap();
        self.state.write().unwrap().legacy_discovered_models = models;
    }

    pub fn discovered_models(&self) -> Vec<String> {
        let state = self.state.read().unwrap();
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
    pub fn set_provider_config(&self, cfg: ProviderConfig) {
        if let Err(error) = self.install_config_layer(ConfigLayer::inferred(cfg)) {
            crate::notify!(error, "model config update rejected: {error}");
        }
    }

    /// Register additional model entries without clobbering existing ones.
    pub fn register_model_entries(&self, entries: Vec<(String, ModelEntry)>) {
        let _transaction = self.transaction.lock().unwrap();
        let mut state = self.state.write().unwrap();
        for (name, entry) in entries {
            if state.config.values.models.contains_key(&name)
                || state.legacy_models.contains_key(&name)
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
    pub fn register_provider_entries(&self, entries: Vec<(String, ProviderEntry)>) {
        let transaction = self.transaction.lock().unwrap();
        let mut config = self.state.read().unwrap().config.clone();
        for (name, entry) in entries {
            config.values.providers.entry(name).or_insert(entry);
        }
        match self.prepare_config_layer_locked(config, transaction) {
            Ok(prepared) => commit_prepared_config(prepared),
            Err(error) => crate::notify!(error, "provider config update rejected: {error}"),
        }
    }

    pub(super) fn register_legacy_discovered_entries(
        &self,
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
        self.register_model_entries(entries);
        self.set_discovered_models(names);
    }

    pub fn prepare_discovered_details_for_provider_with_auth(
        &self,
        provider_key: &str,
        provider_name: &str,
        auth: &AuthStore,
        persisted_namespace: Option<&str>,
        wire_profile: ReasoningWireProfile,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<PreparedProviderCatalog<'_>, CatalogError> {
        self.prepare_discovered_details_with_profile(
            provider_key,
            provider_name,
            wire_profile,
            auth,
            persisted_namespace,
            models,
        )
    }

    pub(super) fn prepare_discovered_details_with_profile(
        &self,
        provider_key: &str,
        provider_name: &str,
        wire_profile: ReasoningWireProfile,
        auth: &AuthStore,
        persisted_namespace: Option<&str>,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<PreparedProviderCatalog<'_>, CatalogError> {
        let model_namespace = provider_namespace(
            &self.state.read().unwrap(),
            provider_key,
            provider_name,
            auth,
            persisted_namespace,
        )?;
        let descriptor = ProviderDescriptor {
            provider_key: provider_key.to_string(),
            provider_name: provider_name.to_string(),
            namespace: model_namespace,
            wire_profile,
        };
        self.prepare_provider_catalog(descriptor, models)
    }

    pub(super) fn prepare_config_layer(
        &self,
        config: ConfigLayer,
    ) -> Result<PreparedConfigLayer<'_>, CatalogError> {
        self.prepare_config_layer_locked(config, self.transaction.lock().unwrap())
    }

    fn prepare_config_layer_locked<'a>(
        &'a self,
        config: ConfigLayer,
        transaction: std::sync::MutexGuard<'a, ()>,
    ) -> Result<PreparedConfigLayer<'a>, CatalogError> {
        for (name, entry) in &config.values.providers {
            if entry.api.is_some() && !entry.supports_api_selection() {
                return Err(CatalogError::InvalidApiConfiguration {
                    message: format!(
                        "provider `{name}` of kind `{}` does not support API selection",
                        entry.kind
                    ),
                });
            }
            if entry.supports_api_selection()
                && let Some(base_url) = entry
                    .base_url
                    .as_deref()
                    .filter(|url| !url.trim().is_empty())
            {
                crate::config_provider::validate_openai_base_url(base_url).map_err(|message| {
                    CatalogError::InvalidApiConfiguration {
                        message: format!("provider `{name}`: {message}"),
                    }
                })?;
            }
        }
        let presets = build_preset_models(&config);
        let state = self.state.read().unwrap();
        let candidate = RegistryState {
            config: config.clone(),
            preset_models: presets.clone(),
            ..state.clone()
        };
        for (name, entry) in &config.values.models {
            if entry.api.is_none() {
                continue;
            }
            let provider = entry.provider.as_deref().map(str::to_owned).or_else(|| {
                resolved_model_in_state(&candidate, name).and_then(|model| model.entry.provider)
            });
            let supported = provider
                .as_deref()
                .and_then(|key| config.values.providers.get(key))
                .is_some_and(ProviderEntry::supports_api_selection);
            if !supported {
                return Err(CatalogError::InvalidApiConfiguration {
                    message: format!(
                        "model `{name}` can select an API only on an OpenAI-compatible config provider"
                    ),
                });
            }
        }
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
            registry: self,
            _transaction: transaction,
            config,
            presets,
        })
    }

    pub(crate) fn prepare_config_text(
        &self,
        text: &str,
    ) -> anyhow::Result<PreparedConfigLayer<'_>> {
        let config = parse_config_layer(text)
            .map_err(|error| anyhow::anyhow!("parse config.toml: {error}"))?
            .unwrap_or_default();
        self.prepare_config_layer(config).map_err(Into::into)
    }

    pub(super) fn install_config_layer(&self, config: ConfigLayer) -> Result<(), CatalogError> {
        let prepared = self.prepare_config_layer(config)?;
        commit_prepared_config(prepared);
        Ok(())
    }

    pub fn replace_provider_catalog(
        &self,
        descriptor: ProviderDescriptor,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<CatalogDelta, CatalogError> {
        let prepared = self.prepare_provider_catalog(descriptor, models)?;
        Ok(commit_prepared_provider_catalog(prepared))
    }

    pub fn prepare_provider_catalog(
        &self,
        descriptor: ProviderDescriptor,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<PreparedProviderCatalog<'_>, CatalogError> {
        let ProviderCatalog {
            descriptor,
            models: next_models,
        } = build_provider_catalog(descriptor, models)?;
        let transaction = self
            .transaction
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = self
            .state
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
            registry: self,
            _transaction: transaction,
            catalog: ProviderCatalog {
                descriptor,
                models: next_models,
            },
        })
    }

    pub fn remove_provider_catalog(&self, provider_key: &str) -> bool {
        let _transaction = self.transaction.lock().unwrap();
        let mut state = self.state.write().unwrap();
        let removed = state.catalogs.remove(provider_key).is_some();
        if removed {
            state.catalog_revision = state.catalog_revision.wrapping_add(1);
            self.catalog_revision.send_replace(state.catalog_revision);
        }
        removed
    }

    pub(crate) fn provider_catalog_namespace(&self, provider_key: &str) -> Option<String> {
        self.state
            .read()
            .unwrap()
            .catalogs
            .get(provider_key)
            .map(|catalog| catalog.descriptor.namespace.clone())
    }

    pub fn model_catalog_revision(&self) -> u64 {
        self.state.read().unwrap().catalog_revision
    }

    pub fn subscribe_model_catalog(&self) -> tokio::sync::watch::Receiver<u64> {
        self.catalog_revision.subscribe()
    }

    /// Return all models grouped by provider, with complete metadata.
    /// Single canonical source for UI — no manual union of all_model_entries +
    /// discovered_models + aliases.
    pub fn all_provider_groups(&self) -> Vec<ProviderGroup> {
        self.provider_groups(false)
    }

    pub fn enabled_provider_names(&self, auth: &AuthStore) -> std::collections::HashSet<String> {
        let mut names: std::collections::HashSet<String> = self
            .all_provider_entries()
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
    pub fn all_provider_groups_with_empty(&self) -> Vec<ProviderGroup> {
        self.provider_groups(true)
    }

    pub(super) fn provider_groups(&self, include_empty: bool) -> Vec<ProviderGroup> {
        let state = self.state.read().unwrap();
        provider_groups_in_state(&state, include_empty, false)
    }

    pub fn resolve_alias(&self, name: &str) -> String {
        resolve_alias_in_state(&self.state.read().unwrap(), name)
    }

    pub fn model_entry(&self, name: &str) -> Option<ModelEntry> {
        resolved_model_in_state(&self.state.read().unwrap(), name).map(|model| model.entry)
    }

    pub fn api_model_id(&self, name: &str) -> String {
        self.model_entry(name)
            .and_then(|entry| (!entry.model.is_empty()).then_some(entry.model))
            .unwrap_or_else(|| name.to_string())
    }

    pub fn model_call_config(&self, name: &str) -> ModelCallConfig {
        let state = self.state.read().unwrap();
        let resolved = resolved_model_in_state(&state, name);
        model_call_config_from_resolved(&state, name, resolved)
    }

    /// Resolves an unsaved model draft through the same capability and protocol projection as saved models.
    pub fn draft_model_call_config(
        &self,
        identity: Option<&str>,
        provider: &str,
        api_model: &str,
        api: Option<OpenAiApi>,
        max_tokens: Option<u32>,
    ) -> ModelCallConfig {
        let state = self.state.read().unwrap();
        let key = identity.unwrap_or(api_model);
        let target = ModelIdentity::new(provider, api_model);
        let mut bases = base_models_in_state(&state);
        let base_max_tokens = bases.get(&target).and_then(|base| base.entry.max_tokens);
        let mut resolved = identity
            .and_then(|name| resolved_model_in_state(&state, name))
            .filter(|model| model.identity.as_ref() == Some(&target))
            .or_else(|| bases.remove(&target))
            .unwrap_or_else(|| {
                model_entry_to_resolved(
                    &state,
                    key.into(),
                    ModelEntry {
                        model: api_model.into(),
                        provider: Some(provider.into()),
                        ..Default::default()
                    },
                    &CapabilityDeclarations::default(),
                    false,
                )
            });
        resolved.key = key.into();
        resolved.entry.api = api;
        resolved.entry.max_tokens = max_tokens.or(base_max_tokens);
        resolved.entry.enabled = Some(true);
        resolved.wire_profile = model_profile_in_state(&state, &resolved.entry);
        model_call_config_from_resolved(&state, key, Some(resolved))
    }

    pub fn all_model_entries(&self) -> Vec<(String, ModelEntry)> {
        resolved_models_in_state(&self.state.read().unwrap())
            .into_iter()
            .map(|(name, model)| (name, model.entry))
            .collect()
    }

    pub fn all_provider_entries(&self) -> Vec<(String, ProviderEntry)> {
        provider_entries_in_state(&self.state.read().unwrap())
            .into_iter()
            .collect()
    }

    pub fn all_aliases(&self) -> Vec<(String, String)> {
        let state = self.state.read().unwrap();
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

    pub fn model_info(&self, name: &str) -> ModelInfo {
        let state = self.state.read().unwrap();
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

    pub fn reasoning_wire_profile_for_provider(&self, provider: &str) -> ReasoningWireProfile {
        provider_profile_in_state(&self.state.read().unwrap(), provider)
    }

    pub fn reasoning_wire_profile_for_model(&self, model: &str) -> ReasoningWireProfile {
        resolved_model_in_state(&self.state.read().unwrap(), model)
            .map(|model| model.wire_profile)
            .unwrap_or(ReasoningWireProfile::Unknown)
    }

    pub fn resolve_reasoning_for_model(
        &self,
        model: &str,
        selection: &ReasoningSelection,
    ) -> Result<ReasoningSelection, String> {
        let state = self.state.read().unwrap();
        let Some(model) = resolved_model_in_state(&state, model) else {
            return ReasoningWireProfile::Unknown
                .validate(selection, None)
                .map(|()| selection.clone());
        };
        resolve_reasoning_for_resolved(&model, selection)
    }

    /// Return whether a wire-valid selection still requires capability fields that are unknown.
    pub fn reasoning_selection_uses_legacy_capabilities(
        &self,
        model: &str,
        selection: &ReasoningSelection,
    ) -> bool {
        let state = self.state.read().unwrap();
        let Some(model) = resolved_model_in_state(&state, model) else {
            return false;
        };
        if model
            .wire_profile
            .validate(selection, model.reasoning_max_tokens())
            .is_err()
        {
            return false;
        }
        let checks_effort_metadata = matches!(
            selection,
            ReasoningSelection::Effort { effort, .. } if !matches!(effort, ReasoningEffort::None)
        ) || matches!(
            selection,
            ReasoningSelection::BudgetTokens { .. }
        );
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

    pub fn reasoning_selections_for_model(&self, model: &str) -> Vec<ReasoningSelection> {
        let state = self.state.read().unwrap();
        let Some(model) = resolved_model_in_state(&state, model) else {
            return vec![
                ReasoningSelection::ProviderDefault,
                ReasoningSelection::Disabled,
                ReasoningSelection::Auto {
                    execution_mode: None,
                },
            ];
        };
        reasoning_selections_for_resolved(&model)
    }

    pub fn reasoning_selections_for_model_draft(
        &self,
        identity: Option<&str>,
        provider: &str,
        api_model: &str,
        api: Option<OpenAiApi>,
        max_tokens: Option<u32>,
    ) -> Vec<ReasoningSelection> {
        let snapshot = self.draft_model_call_config(identity, provider, api_model, api, max_tokens);
        snapshot
            .resolved
            .as_ref()
            .map(reasoning_selections_for_resolved)
            .unwrap_or_default()
    }

    pub fn effective_reasoning_for_model(
        &self,
        model: &str,
        input_selection: Option<&ReasoningSelection>,
    ) -> Result<Option<ReasoningSelection>, String> {
        let state = self.state.read().unwrap();
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

    /// Register derived preset models for a provider with a matching base URL.
    /// User configuration overlays take priority over this layer.
    pub fn register_preset_models_for(&self, provider_name: &str, base_url: &str) {
        let transaction = self.transaction.lock().unwrap();
        let config = {
            let state = self.state.read().unwrap();
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
        match self.prepare_config_layer_locked(config, transaction) {
            Ok(prepared) => commit_prepared_config(prepared),
            Err(error) => crate::notify!(error, "preset model update rejected: {error}"),
        }
    }

    /// Register preset models for all configured providers with matching base URLs.
    pub fn register_all_preset_models(&self) {
        let transaction = self.transaction.lock().unwrap();
        let config = self.state.read().unwrap().config.clone();
        match self.prepare_config_layer_locked(config, transaction) {
            Ok(prepared) => commit_prepared_config(prepared),
            Err(error) => crate::notify!(error, "preset model update rejected: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(api_model: &str, api: OpenAiApi, budget: u64) -> ProviderConfig {
        ProviderConfig {
            providers: HashMap::from([(
                "gateway".into(),
                ProviderEntry {
                    name: "gateway".into(),
                    kind: "openai-compat".into(),
                    api: Some(api),
                    ..Default::default()
                },
            )]),
            models: HashMap::from([(
                "model".into(),
                ModelEntry {
                    model: api_model.into(),
                    provider: Some("gateway".into()),
                    context_budget: Some(budget),
                    ..Default::default()
                },
            )]),
            aliases: HashMap::from([(
                "smart".into(),
                AliasEntry {
                    model: "model".into(),
                },
            )]),
        }
    }

    fn descriptor(provider: &str) -> ProviderDescriptor {
        ProviderDescriptor {
            provider_key: provider.into(),
            provider_name: "catalog".into(),
            namespace: "catalog".into(),
            wire_profile: ReasoningWireProfile::OpenAiResponses,
        }
    }

    fn discovered(budget: u64) -> Vec<crate::provider::DiscoveredModelDetails> {
        vec![crate::provider::DiscoveredModelDetails {
            slug: "remote".into(),
            context_budget: Some(budget),
            capability_knowledge: CapabilityKnowledge::Advertised(ModelCapabilities::default()),
        }]
    }

    #[test]
    fn scopes_isolate_resolution_transactions_and_notifications() {
        let left = ModelRegistry::new();
        let right = ModelRegistry::new();
        left.replace_config(config("vendor/left", OpenAiApi::Responses, 32768))
            .unwrap();
        right
            .replace_config(config("vendor/right", OpenAiApi::ChatCompletions, 65536))
            .unwrap();
        let captured_left = left.model_call_config("smart");
        let mut left_updates = left.subscribe_model_catalog();
        let right_updates = right.subscribe_model_catalog();

        // Prepared updates retain their owner; another scope can prepare concurrently.
        let prepared_left = left
            .prepare_provider_catalog(descriptor("shared"), &discovered(4096))
            .unwrap();
        assert!(left.transaction.try_lock().is_err());
        let prepared_right = right
            .prepare_provider_catalog(descriptor("shared"), &discovered(8192))
            .unwrap();
        commit_prepared_provider_catalog(prepared_left);
        assert!(left_updates.has_changed().unwrap());
        assert!(!right_updates.has_changed().unwrap());
        assert!(right.model_entry("catalog:remote").is_none());
        commit_prepared_provider_catalog(prepared_right);
        assert_eq!(left.model_info("catalog:remote").context_budget, 4096);
        assert_eq!(right.model_info("catalog:remote").context_budget, 8192);
        assert_eq!(captured_left.api_model, "vendor/left");
        assert_eq!(captured_left.api, Some(OpenAiApi::Responses));
        assert_eq!(captured_left.info.context_budget, 32768);
        let right_call = right.model_call_config("smart");
        assert_eq!(right_call.api_model, "vendor/right");
        assert_eq!(right_call.api, Some(OpenAiApi::ChatCompletions));
        assert_eq!(right_call.info.context_budget, 65536);

        // No-op updates do not wake observers; removal affects only its owner.
        left_updates.borrow_and_update();
        left.replace_provider_catalog(descriptor("shared"), &discovered(4096))
            .unwrap();
        assert!(!left_updates.has_changed().unwrap());
        let right_revision = right.model_catalog_revision();
        assert!(left.remove_provider_catalog("shared"));
        assert_eq!(right.model_catalog_revision(), right_revision);
        assert!(right.model_entry("catalog:remote").is_some());
        left.replace_config(config("vendor/changed", OpenAiApi::ChatCompletions, 1024))
            .unwrap();
        assert_eq!(captured_left.api_model, "vendor/left");
        assert_eq!(right.model_call_config("smart").api_model, "vendor/right");
    }

    #[test]
    fn validation_and_abandoned_preparations_preserve_their_scope() {
        let registry = ModelRegistry::new();
        registry
            .replace_config(config("vendor/model", OpenAiApi::Responses, 4096))
            .unwrap();
        let prepared = registry
            .prepare_config_text("[models.pending]\nmodel = 'pending'")
            .unwrap();
        assert!(registry.model_entry("pending").is_none());
        assert_eq!(
            registry.model_call_config("smart").api_model,
            "vendor/model"
        );
        drop(prepared);
        assert!(registry.transaction.try_lock().is_ok());
        assert!(
            registry
                .replace_config_text("[providers.bad]\nkind = 'anthropic'\napi = 'responses'")
                .is_err()
        );
        assert_eq!(
            registry.model_call_config("smart").api_model,
            "vendor/model"
        );

        let updates = registry.subscribe_model_catalog();
        drop(
            registry
                .prepare_provider_catalog(descriptor("one"), &discovered(1024))
                .unwrap(),
        );
        assert!(!updates.has_changed().unwrap());
        assert!(
            registry
                .all_provider_groups()
                .iter()
                .all(|group| group.provider_name != "one")
        );
        registry
            .replace_provider_catalog(descriptor("one"), &discovered(1024))
            .unwrap();
        let revision = registry.model_catalog_revision();
        assert!(matches!(
            registry.prepare_provider_catalog(descriptor("two"), &discovered(8192)),
            Err(CatalogError::NamespaceInUse { .. })
        ));
        assert_eq!(registry.model_catalog_revision(), revision);
        assert_eq!(registry.model_info("catalog:remote").context_budget, 1024);
        assert_eq!(
            registry
                .model_entry("catalog:remote")
                .unwrap()
                .provider
                .as_deref(),
            Some("one")
        );
    }

    #[test]
    fn auth_and_management_projections_use_explicit_sources_without_installing_models() {
        let registry = ModelRegistry::new();
        let mut auth = AuthStore {
            providers: vec![crate::auth_store::StoredProvider {
                id: "account".into(),
                name: "catalog".into(),
                kind: crate::auth_store::ProviderKind::Custom,
                access_token: String::new(),
                refresh_token: None,
                expires_at: 0,
                account: None,
                enabled: false,
                model_cache: None,
            }],
        };
        assert!(registry.enabled_provider_names(&auth).is_empty());
        let updates = registry.subscribe_model_catalog();
        let groups = registry.all_provider_groups_for_management(&auth, None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].provider_name, "account");
        assert!(registry.all_provider_groups().is_empty());
        assert!(!updates.has_changed().unwrap());
        assert!(!registry.is_provider_enabled("account", &auth));
        auth.providers[0].enabled = true;
        assert!(registry.is_provider_enabled("account", &auth));
        assert!(registry.enabled_provider_names(&auth).contains("account"));
        let prepared = registry
            .prepare_discovered_details_for_provider_with_auth(
                "account",
                "catalog",
                &auth,
                None,
                ReasoningWireProfile::OpenAiResponses,
                &discovered(2048),
            )
            .unwrap();
        let namespace = prepared.namespace().to_owned();
        commit_prepared_provider_catalog(prepared);
        assert!(registry.is_provider_enabled("account", &auth));
        assert!(!registry.is_provider_enabled("account", &AuthStore::default()));
        assert_eq!(
            registry
                .model_info(&format!("{namespace}:remote"))
                .context_budget,
            2048
        );
    }
}
