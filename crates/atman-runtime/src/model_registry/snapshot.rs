use super::*;

/// Frozen model configuration and catalog layers, independent of later registry writes and writer locks. Read operations use the same resolver as the source registry; call configurations retain the original source identity. Authentication inputs remain explicit.
///
/// Mutation is unavailable on a snapshot:
///
/// ```compile_fail
/// use atman_runtime::model_registry::ModelRegistry;
/// let snapshot = ModelRegistry::new().snapshot();
/// snapshot.replace_config(Default::default());
/// ```
#[derive(Debug)]
pub struct ModelRegistrySnapshot {
    registry: ModelRegistry,
}

impl ModelRegistrySnapshot {
    pub(super) fn new(registry: ModelRegistry) -> Self {
        Self { registry }
    }

    pub fn discovered_models(&self) -> Vec<String> {
        self.registry.discovered_models()
    }

    pub fn model_catalog_revision(&self) -> u64 {
        self.registry.model_catalog_revision()
    }

    pub fn all_provider_groups(&self) -> Vec<ProviderGroup> {
        self.registry.all_provider_groups()
    }

    pub fn all_provider_groups_with_empty(&self) -> Vec<ProviderGroup> {
        self.registry.all_provider_groups_with_empty()
    }

    pub fn enabled_provider_names(&self, auth: &AuthStore) -> std::collections::HashSet<String> {
        self.registry.enabled_provider_names(auth)
    }

    pub fn is_provider_enabled(&self, name: &str, auth: &AuthStore) -> bool {
        self.registry.is_provider_enabled(name, auth)
    }

    pub fn resolve_alias(&self, name: &str) -> String {
        self.registry.resolve_alias(name)
    }

    pub fn model_entry(&self, name: &str) -> Option<ModelEntry> {
        self.registry.model_entry(name)
    }

    pub fn api_model_id(&self, name: &str) -> String {
        self.registry.api_model_id(name)
    }

    pub fn model_call_config(&self, name: &str) -> ModelCallConfig {
        self.registry.model_call_config(name)
    }

    pub fn draft_model_call_config(
        &self,
        identity: Option<&str>,
        provider: &str,
        api_model: &str,
        api: Option<OpenAiApi>,
        max_tokens: Option<u32>,
    ) -> ModelCallConfig {
        self.registry
            .draft_model_call_config(identity, provider, api_model, api, max_tokens)
    }

    pub fn all_model_entries(&self) -> Vec<(String, ModelEntry)> {
        self.registry.all_model_entries()
    }

    pub fn all_provider_entries(&self) -> Vec<(String, ProviderEntry)> {
        self.registry.all_provider_entries()
    }

    pub fn all_aliases(&self) -> Vec<(String, String)> {
        self.registry.all_aliases()
    }

    pub fn model_info(&self, name: &str) -> ModelInfo {
        self.registry.model_info(name)
    }

    pub fn reasoning_wire_profile_for_provider(&self, provider: &str) -> ReasoningWireProfile {
        self.registry.reasoning_wire_profile_for_provider(provider)
    }

    pub fn reasoning_wire_profile_for_model(&self, model: &str) -> ReasoningWireProfile {
        self.registry.reasoning_wire_profile_for_model(model)
    }

    pub fn resolve_reasoning_for_model(
        &self,
        model: &str,
        selection: &ReasoningSelection,
    ) -> Result<ReasoningSelection, String> {
        self.registry.resolve_reasoning_for_model(model, selection)
    }

    pub fn reasoning_selection_uses_legacy_capabilities(
        &self,
        model: &str,
        selection: &ReasoningSelection,
    ) -> bool {
        self.registry
            .reasoning_selection_uses_legacy_capabilities(model, selection)
    }

    pub fn reasoning_selections_for_model(&self, model: &str) -> Vec<ReasoningSelection> {
        self.registry.reasoning_selections_for_model(model)
    }

    pub fn reasoning_selections_for_model_draft(
        &self,
        identity: Option<&str>,
        provider: &str,
        api_model: &str,
        api: Option<OpenAiApi>,
        max_tokens: Option<u32>,
    ) -> Vec<ReasoningSelection> {
        self.registry
            .reasoning_selections_for_model_draft(identity, provider, api_model, api, max_tokens)
    }

    pub fn effective_reasoning_for_model(
        &self,
        model: &str,
        input_selection: Option<&ReasoningSelection>,
    ) -> Result<Option<ReasoningSelection>, String> {
        self.registry
            .effective_reasoning_for_model(model, input_selection)
    }

    pub fn is_first_run(&self, auth: &AuthStore) -> bool {
        self.registry.is_first_run(auth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_reads_retain_layers_and_call_identity_across_source_writes_and_locks() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<ModelRegistrySnapshot>();
        let registry = ModelRegistry::new();
        registry.replace_config_text("[providers.gateway]\nkind='openai'\n[models.example]\nprovider='gateway'\nmodel='original'\n[alias.smart]\nmodel='example'\n").unwrap();
        registry
            .replace_provider_catalog(
                ProviderDescriptor {
                    provider_key: "account".into(),
                    provider_name: "Account".into(),
                    namespace: "account".into(),
                    wire_profile: ReasoningWireProfile::Unknown,
                },
                &[crate::provider::DiscoveredModelDetails::from(
                    crate::provider::DiscoveredModel {
                        slug: "cached".into(),
                        context_budget: Some(8192),
                        thinking: false,
                    },
                )],
            )
            .unwrap();
        let snapshot = registry.snapshot();
        let groups = snapshot.all_provider_groups_with_empty();
        let revision = snapshot.model_catalog_revision();
        let call = snapshot.model_call_config("smart");
        assert!(registry.owns_call(&call));
        registry.replace_config_text("[providers.gateway]\nkind='openai'\n[models.example]\nprovider='gateway'\nmodel='changed'\n[alias.smart]\nmodel='example'\n").unwrap();
        registry.remove_provider_catalog("account");
        let writer = registry.state.write().unwrap();
        assert_eq!(snapshot.model_entry("smart").unwrap().model, "original");
        assert_eq!(snapshot.model_catalog_revision(), revision);
        assert_eq!(
            snapshot.all_provider_groups_with_empty().len(),
            groups.len()
        );
        assert!(registry.owns_call(&snapshot.draft_model_call_config(
            Some("smart"),
            "gateway",
            "original",
            None,
            None
        )));
        drop(writer);
        assert_eq!(registry.model_entry("smart").unwrap().model, "changed");
        assert!(!ModelRegistry::new().owns_call(&call));
    }
}
