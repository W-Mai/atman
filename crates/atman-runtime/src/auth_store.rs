use std::collections::{HashMap, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::config_dir;

const AUTH_FILENAME: &str = "auth.json";
const MODEL_CACHE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MODEL_CACHE_FRESHNESS_WINDOW_SECONDS: i64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelCacheFreshness {
    Fresh,
    Missing,
    LegacySchema,
    MissingCapabilities,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Codex,
    AnthropicOauth,
    GitHubCopilot,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCache {
    pub fetched_at: i64,
    pub models: Vec<CachedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedModel {
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u64>,
    pub thinking: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredProvider {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_cache: Option<ModelCache>,
}

impl std::fmt::Debug for StoredProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredProvider")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("account", &self.account.as_ref().map(|_| "[redacted]"))
            .field("enabled", &self.enabled)
            .field("model_cache", &self.model_cache)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<StoredProvider>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AuthStoreDocument {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    providers: Vec<StoredProviderDocument>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredProviderDocument {
    id: String,
    name: String,
    kind: ProviderKind,
    access_token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    expires_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    account: Option<String>,
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    credential_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    catalog_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_cache: Option<ModelCacheDocument>,
}

impl std::fmt::Debug for StoredProviderDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StoredProviderDocument")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("access_token", &"[redacted]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[redacted]"),
            )
            .field("expires_at", &self.expires_at)
            .field("account", &self.account.as_ref().map(|_| "[redacted]"))
            .field("enabled", &self.enabled)
            .field("credential_revision", &self.credential_revision)
            .field("catalog_revision", &self.catalog_revision)
            .field("model_namespace", &self.model_namespace)
            .field("model_cache", &self.model_cache)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelCacheDocument {
    #[serde(default, skip_serializing_if = "is_zero")]
    schema_version: u32,
    fetched_at: i64,
    models: Vec<CachedModelDocument>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModelDocument {
    slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_budget: Option<u64>,
    thinking: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<crate::provider::ModelCapabilities>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthProviderCatalogSnapshot {
    revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthProviderCredentialSnapshot {
    revision: String,
}

#[derive(Debug, Clone)]
pub(crate) enum AuthCredentialCommit {
    Updated { provider: StoredProvider },
    Missing,
    Changed,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl AuthStoreDocument {
    pub(crate) fn legacy_view(&self) -> AuthStore {
        AuthStore {
            providers: self
                .providers
                .iter()
                .map(StoredProviderDocument::legacy_view)
                .collect(),
        }
    }

    pub(crate) fn merge_legacy_view(&mut self, store: AuthStore) {
        let mut previous: HashMap<String, VecDeque<StoredProviderDocument>> = HashMap::new();
        for provider in self.providers.drain(..) {
            previous
                .entry(provider.id.clone())
                .or_default()
                .push_back(provider);
        }
        self.providers = store
            .providers
            .into_iter()
            .map(
                |provider| match previous.get_mut(&provider.id).and_then(VecDeque::pop_front) {
                    Some(document) => document.merge(provider),
                    None => provider.into(),
                },
            )
            .collect();
    }

    pub(crate) fn update_model_cache_details(
        &mut self,
        provider_id: &str,
        model_namespace: &str,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<bool, String> {
        let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
        else {
            return Ok(false);
        };
        assign_model_namespace(provider, provider_id, model_namespace)?;
        provider.model_cache = Some(ModelCacheDocument::from_details(fetched_at, models));
        provider.bump_catalog_revision();
        Ok(true)
    }

    pub(crate) fn ensure_model_namespace(
        &mut self,
        provider_id: &str,
        model_namespace: &str,
    ) -> Result<bool, String> {
        let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
        else {
            return Err(format!("auth provider `{provider_id}` does not exist"));
        };
        let changed = assign_model_namespace(provider, provider_id, model_namespace)?;
        if changed {
            provider.bump_catalog_revision();
        }
        Ok(changed)
    }

    pub(crate) fn set_provider_enabled(
        &mut self,
        provider_id: &str,
        enabled: bool,
    ) -> Option<bool> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)?;
        if provider.enabled == enabled {
            return Some(false);
        }
        provider.enabled = enabled;
        provider.bump_catalog_revision();
        Some(true)
    }

    pub(crate) fn update_model_cache(&mut self, provider_id: &str, cache: ModelCache) -> bool {
        let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
        else {
            return false;
        };
        provider.model_cache = Some(cache.into());
        provider.bump_catalog_revision();
        true
    }

    pub(crate) fn model_namespace(&self, provider_id: &str) -> Option<String> {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)?
            .model_namespace
            .clone()
    }

    pub(crate) fn model_cache_details(
        &self,
        provider_id: &str,
    ) -> Option<Vec<crate::provider::DiscoveredModelDetails>> {
        self.providers
            .iter()
            .find(|provider| provider.id == provider_id)?
            .model_cache
            .as_ref()
            .map(ModelCacheDocument::details)
    }

    pub(crate) fn model_cache_freshness(
        &self,
        provider_id: &str,
        now: i64,
        max_age_seconds: i64,
    ) -> Option<ModelCacheFreshness> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)?;
        Some(match provider.model_cache.as_ref() {
            Some(cache) => cache.freshness(now, max_age_seconds),
            None => ModelCacheFreshness::Missing,
        })
    }

    pub(crate) fn provider_catalog_snapshot(
        &self,
        provider_id: &str,
    ) -> Option<AuthProviderCatalogSnapshot> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)?;
        Some(AuthProviderCatalogSnapshot {
            revision: provider.catalog_revision.clone()?,
        })
    }

    pub(crate) fn provider_credential_state(
        &self,
        provider_id: &str,
    ) -> Option<(StoredProvider, Option<AuthProviderCredentialSnapshot>)> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)?;
        Some((
            provider.legacy_view(),
            provider
                .credential_revision
                .clone()
                .map(|revision| AuthProviderCredentialSnapshot { revision }),
        ))
    }

    pub(crate) fn ensure_provider_catalog_state(
        &mut self,
        provider_id: &str,
    ) -> Option<((StoredProvider, AuthProviderCatalogSnapshot), bool)> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)?;
        let changed = provider.catalog_revision.is_none();
        let revision = provider
            .catalog_revision
            .get_or_insert_with(new_catalog_revision)
            .clone();
        Some((
            (
                provider.legacy_view(),
                AuthProviderCatalogSnapshot { revision },
            ),
            changed,
        ))
    }

    pub(crate) fn ensure_provider_credential_state(
        &mut self,
        provider_id: &str,
    ) -> Option<((StoredProvider, AuthProviderCredentialSnapshot), bool)> {
        let provider = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)?;
        let changed = provider.credential_revision.is_none();
        let revision = provider
            .credential_revision
            .get_or_insert_with(new_credential_revision)
            .clone();
        Some((
            (
                provider.legacy_view(),
                AuthProviderCredentialSnapshot { revision },
            ),
            changed,
        ))
    }

    pub(crate) fn update_provider_credentials(
        &mut self,
        provider_id: &str,
        expected: &AuthProviderCredentialSnapshot,
        access_token: String,
        refresh_token: Option<String>,
        expires_at: i64,
        account: Option<String>,
    ) -> AuthCredentialCommit {
        let Some(provider) = self
            .providers
            .iter_mut()
            .find(|provider| provider.id == provider_id)
        else {
            return AuthCredentialCommit::Missing;
        };
        if provider.credential_revision.as_deref() != Some(expected.revision.as_str()) {
            return AuthCredentialCommit::Changed;
        }

        provider.access_token = access_token;
        provider.expires_at = expires_at;
        if refresh_token.is_some() {
            provider.refresh_token = refresh_token;
        }
        if account.is_some() {
            provider.account = account;
        }
        provider.bump_credential_revision();
        AuthCredentialCommit::Updated {
            provider: provider.legacy_view(),
        }
    }
}

fn assign_model_namespace(
    provider: &mut StoredProviderDocument,
    provider_id: &str,
    model_namespace: &str,
) -> Result<bool, String> {
    if let Some(existing) = provider.model_namespace.as_deref() {
        if existing == model_namespace {
            return Ok(false);
        }
        return Err(format!(
            "provider `{provider_id}` model namespace is already `{existing}`"
        ));
    }
    provider.model_namespace = Some(model_namespace.to_string());
    Ok(true)
}

impl StoredProviderDocument {
    fn legacy_view(&self) -> StoredProvider {
        StoredProvider {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind.clone(),
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            expires_at: self.expires_at,
            account: self.account.clone(),
            enabled: self.enabled,
            model_cache: self
                .model_cache
                .as_ref()
                .map(ModelCacheDocument::legacy_view),
        }
    }

    fn merge(mut self, provider: StoredProvider) -> Self {
        let credential_changed = self.kind != provider.kind
            || self.access_token != provider.access_token
            || self.refresh_token != provider.refresh_token
            || self.expires_at != provider.expires_at
            || self.account != provider.account;
        let catalog_changed = self.name != provider.name
            || self.kind != provider.kind
            || self.enabled != provider.enabled
            || self
                .model_cache
                .as_ref()
                .map(ModelCacheDocument::legacy_view)
                != provider.model_cache;
        let model_cache = match (provider.model_cache, self.model_cache.take()) {
            (Some(cache), Some(document)) if document.legacy_view() == cache => Some(document),
            (Some(cache), _) => Some(cache.into()),
            (None, _) => None,
        };
        let catalog_revision = if catalog_changed {
            Some(new_catalog_revision())
        } else {
            self.catalog_revision
        };
        let credential_revision = if credential_changed {
            Some(new_credential_revision())
        } else {
            self.credential_revision
        };
        Self {
            id: provider.id,
            name: provider.name,
            kind: provider.kind,
            access_token: provider.access_token,
            refresh_token: provider.refresh_token,
            expires_at: provider.expires_at,
            account: provider.account,
            enabled: provider.enabled,
            credential_revision,
            catalog_revision,
            model_namespace: self.model_namespace,
            model_cache,
        }
    }

    fn bump_catalog_revision(&mut self) {
        self.catalog_revision = Some(new_catalog_revision());
    }

    fn bump_credential_revision(&mut self) {
        let revision = new_credential_revision();
        self.credential_revision = Some(revision);
    }
}

impl From<StoredProvider> for StoredProviderDocument {
    fn from(provider: StoredProvider) -> Self {
        Self {
            id: provider.id,
            name: provider.name,
            kind: provider.kind,
            access_token: provider.access_token,
            refresh_token: provider.refresh_token,
            expires_at: provider.expires_at,
            account: provider.account,
            enabled: provider.enabled,
            credential_revision: Some(new_credential_revision()),
            catalog_revision: Some(new_catalog_revision()),
            model_namespace: None,
            model_cache: provider.model_cache.map(ModelCacheDocument::from),
        }
    }
}

fn new_catalog_revision() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn new_credential_revision() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

impl ModelCacheDocument {
    fn legacy_view(&self) -> ModelCache {
        ModelCache {
            fetched_at: self.fetched_at,
            models: self
                .models
                .iter()
                .map(CachedModelDocument::legacy_view)
                .collect(),
        }
    }

    fn from_details(fetched_at: i64, models: &[crate::provider::DiscoveredModelDetails]) -> Self {
        Self {
            schema_version: MODEL_CACHE_SCHEMA_VERSION,
            fetched_at,
            models: models
                .iter()
                .map(|model| CachedModelDocument {
                    slug: model.slug.clone(),
                    context_budget: model.context_budget,
                    thinking: model.capability_knowledge.thinking(),
                    capabilities: model.capability_knowledge.advertised().cloned(),
                })
                .collect(),
        }
    }

    fn details(&self) -> Vec<crate::provider::DiscoveredModelDetails> {
        let capabilities_are_current = self.schema_version == MODEL_CACHE_SCHEMA_VERSION;
        self.models
            .iter()
            .map(|model| crate::provider::DiscoveredModelDetails {
                slug: model.slug.clone(),
                context_budget: model.context_budget,
                capability_knowledge: if capabilities_are_current {
                    model
                        .capabilities
                        .clone()
                        .map(crate::provider::CapabilityKnowledge::Advertised)
                        .unwrap_or(crate::provider::CapabilityKnowledge::Legacy {
                            thinking: model.thinking,
                        })
                } else {
                    crate::provider::CapabilityKnowledge::Legacy {
                        thinking: model.thinking,
                    }
                },
            })
            .collect()
    }

    fn freshness(&self, now: i64, max_age_seconds: i64) -> ModelCacheFreshness {
        if self.schema_version != MODEL_CACHE_SCHEMA_VERSION {
            return ModelCacheFreshness::LegacySchema;
        }
        if self.models.iter().any(|model| model.capabilities.is_none()) {
            return ModelCacheFreshness::MissingCapabilities;
        }
        if now < self.fetched_at || now.saturating_sub(self.fetched_at) >= max_age_seconds {
            return ModelCacheFreshness::Expired;
        }
        ModelCacheFreshness::Fresh
    }
}

impl From<ModelCache> for ModelCacheDocument {
    fn from(cache: ModelCache) -> Self {
        Self {
            schema_version: 0,
            fetched_at: cache.fetched_at,
            models: cache
                .models
                .into_iter()
                .map(CachedModelDocument::from)
                .collect(),
        }
    }
}

impl CachedModelDocument {
    fn legacy_view(&self) -> CachedModel {
        CachedModel {
            slug: self.slug.clone(),
            context_budget: self.context_budget,
            thinking: self.thinking,
        }
    }
}

impl From<CachedModel> for CachedModelDocument {
    fn from(model: CachedModel) -> Self {
        Self {
            slug: model.slug,
            context_budget: model.context_budget,
            thinking: model.thinking,
            capabilities: None,
        }
    }
}

impl AuthStore {
    pub fn load() -> Result<Self> {
        Ok(crate::config_hub::ConfigHub::global()?.load_auth()?)
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        let hub = crate::config_hub::ConfigHub::from_auth_path(path);
        hub.update_auth(|store| {
            *store = self.clone();
            Ok(())
        })?;
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let dir = config_dir()?;
        self.save_to(&dir.join(AUTH_FILENAME))
    }

    pub fn add(&mut self, p: StoredProvider) {
        self.providers.push(p);
    }

    pub fn remove(&mut self, id: &str) -> bool {
        let len_before = self.providers.len();
        self.providers.retain(|p| p.id != id);
        self.providers.len() < len_before
    }

    /// Update the model cache for a provider by ID. Returns false if provider not found.
    pub fn update_model_cache(&mut self, provider_id: &str, cache: ModelCache) -> bool {
        if let Some(p) = self.providers.iter_mut().find(|p| p.id == provider_id) {
            p.model_cache = Some(cache);
            true
        } else {
            false
        }
    }
}

/// Save discovered models as cache for a provider. Reads auth.json, updates, writes back.
pub fn save_provider_model_cache(
    provider_id: &str,
    models: &[crate::provider::DiscoveredModel],
) -> Result<()> {
    let cache = ModelCache {
        fetched_at: chrono::Utc::now().timestamp(),
        models: models
            .iter()
            .map(|model| CachedModel {
                slug: model.slug.clone(),
                context_budget: model.context_budget,
                thinking: model.thinking,
            })
            .collect(),
    };
    let _ = crate::config_hub::ConfigHub::global()?.update_auth_model_cache(provider_id, cache)?;
    Ok(())
}

/// Save discovered capability metadata in the versioned auth cache.
pub fn save_provider_model_cache_details(
    provider_id: &str,
    model_namespace: &str,
    models: &[crate::provider::DiscoveredModelDetails],
) -> Result<()> {
    let updated = crate::config_hub::ConfigHub::global()?.update_auth_model_cache_details(
        provider_id,
        model_namespace,
        chrono::Utc::now().timestamp(),
        models,
    )?;
    if !updated {
        anyhow::bail!("auth provider `{provider_id}` does not exist");
    }
    Ok(())
}

/// Load the stable model namespace assigned to one provider.
pub fn load_provider_model_namespace(provider_id: &str) -> Result<Option<String>> {
    Ok(crate::config_hub::ConfigHub::global()?.load_auth_model_namespace(provider_id)?)
}

/// Persist a provider namespace without changing its model cache freshness.
pub fn ensure_provider_model_namespace(provider_id: &str, model_namespace: &str) -> Result<()> {
    crate::config_hub::ConfigHub::global()?
        .ensure_auth_model_namespace(provider_id, model_namespace)?;
    Ok(())
}

/// Load cached models with capability provenance.
pub fn load_provider_model_cache_details(
    provider_id: &str,
) -> Result<Option<Vec<crate::provider::DiscoveredModelDetails>>> {
    Ok(crate::config_hub::ConfigHub::global()?.load_auth_model_cache_details(provider_id)?)
}

/// Convert cached models to discovered models for registry hydration.
pub fn cached_to_discovered(cache: &ModelCache) -> Vec<crate::provider::DiscoveredModel> {
    cache
        .models
        .iter()
        .map(|m| crate::provider::DiscoveredModel {
            slug: m.slug.clone(),
            context_budget: m.context_budget,
            thinking: m.thinking,
        })
        .collect()
}

/// Adapt the public cache format with legacy capability provenance.
pub fn cached_to_discovered_details(
    cache: &ModelCache,
) -> Vec<crate::provider::DiscoveredModelDetails> {
    cached_to_discovered(cache)
        .into_iter()
        .map(crate::provider::DiscoveredModelDetails::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn provider(id: &str) -> StoredProvider {
        StoredProvider {
            id: id.into(),
            name: "OAuth account".into(),
            kind: ProviderKind::Codex,
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: 1,
            account: Some("account@example.test".into()),
            enabled: true,
            model_cache: None,
        }
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("auth.json");
        let store: AuthStore = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("auth.json");
        let mut store = AuthStore::default();
        store.add(StoredProvider {
            id: "test-1".into(),
            name: "Personal Codex".into(),
            kind: ProviderKind::Codex,
            access_token: "tok1".into(),
            refresh_token: Some("rt1".into()),
            expires_at: 1761735358,
            account: Some("x@example.com".into()),
            enabled: true,
            model_cache: None,
        });
        store.save_to(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let loaded: AuthStore = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].name, "Personal Codex");
    }

    #[test]
    fn stored_provider_debug_redacts_credentials() {
        let provider = provider("debug");
        let debug = format!("{provider:?}");

        assert!(debug.contains("[redacted]"));
        assert!(!debug.contains("old-access"));
        assert!(!debug.contains("old-refresh"));
        assert!(!debug.contains("account@example.test"));
    }

    #[test]
    fn auth_store_document_debug_redacts_credentials() {
        let document = AuthStoreDocument {
            providers: vec![provider("debug-document").into()],
        };
        let debug = format!("{document:?}");

        assert_eq!(debug.matches("[redacted]").count(), 3);
        assert!(!debug.contains("old-access"));
        assert!(!debug.contains("old-refresh"));
        assert!(!debug.contains("account@example.test"));
    }

    #[test]
    fn remove_existing_id_returns_true() {
        let mut store = AuthStore::default();
        store.add(StoredProvider {
            id: "keep".into(),
            name: "A".into(),
            kind: ProviderKind::Custom,
            access_token: "t".into(),
            refresh_token: None,
            expires_at: 0,
            account: None,
            enabled: true,
            model_cache: None,
        });
        store.add(StoredProvider {
            id: "del".into(),
            name: "B".into(),
            kind: ProviderKind::Custom,
            access_token: "t".into(),
            refresh_token: None,
            expires_at: 0,
            account: None,
            enabled: true,
            model_cache: None,
        });
        assert!(store.remove("del"));
        assert_eq!(store.providers.len(), 1);
        assert_eq!(store.providers[0].id, "keep");
    }

    #[test]
    fn remove_missing_id_returns_false() {
        let mut store = AuthStore::default();
        assert!(!store.remove("nope"));
    }

    #[test]
    fn provider_kind_serde_round_trip() {
        let kinds = vec![
            ProviderKind::Codex,
            ProviderKind::AnthropicOauth,
            ProviderKind::GitHubCopilot,
            ProviderKind::Custom,
        ];
        for k in kinds {
            let json = serde_json::to_string(&k).unwrap();
            let back: ProviderKind = serde_json::from_str(&json).unwrap();
            assert_eq!(back, k);
        }
    }

    #[test]
    fn auth_store_serde_backward_compat_empty_json() {
        let store: AuthStore = serde_json::from_str("{}").unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn auth_store_serde_backward_compat_null_providers() {
        let json = r#"{"providers": []}"#;
        let store: AuthStore = serde_json::from_str(json).unwrap();
        assert!(store.providers.is_empty());
    }

    #[test]
    fn public_model_cache_retains_the_v1_9_1_shape() {
        let cache = ModelCache {
            fetched_at: 10,
            models: vec![CachedModel {
                slug: "codex/test".into(),
                context_budget: Some(8_192),
                thinking: true,
            }],
        };

        let value = serde_json::to_value(cache).unwrap();
        assert!(value.get("schema_version").is_none());
        assert!(value["models"][0].get("capabilities").is_none());
    }

    #[test]
    fn legacy_model_cache_document_preserves_unknown_capabilities() {
        let cache: ModelCacheDocument = serde_json::from_str(
            r#"{"fetched_at":10,"models":[{"slug":"codex/test","thinking":true}]}"#,
        )
        .unwrap();

        assert_eq!(cache.schema_version, 0);
        assert_eq!(cache.models[0].capabilities, None);
        assert_eq!(
            cache.details()[0].capability_knowledge,
            crate::provider::CapabilityKnowledge::Legacy { thinking: true }
        );
    }

    #[test]
    fn unversioned_cache_does_not_trust_a_serialized_empty_capability_object() {
        let cache: ModelCacheDocument = serde_json::from_str(
            r#"{
                "fetched_at": 10,
                "models": [{
                    "slug": "codex/test",
                    "thinking": true,
                    "capabilities": {
                        "reasoning_efforts": [],
                        "reasoning_modes": [],
                        "input_modalities": []
                    }
                }]
            }"#,
        )
        .unwrap();

        assert_eq!(cache.schema_version, 0);
        assert!(cache.models[0].capabilities.is_some());
        assert_eq!(
            cache.details()[0].capability_knowledge,
            crate::provider::CapabilityKnowledge::Legacy { thinking: true }
        );
    }

    #[test]
    fn versioned_model_cache_distinguishes_explicit_empty_capabilities() {
        let cache = ModelCacheDocument::from_details(
            10,
            &[crate::provider::DiscoveredModelDetails {
                slug: "codex/test".into(),
                context_budget: Some(8_192),
                capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                    crate::provider::ModelCapabilities::default(),
                ),
            }],
        );
        let json = serde_json::to_string(&cache).unwrap();
        let decoded: ModelCacheDocument = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.schema_version, MODEL_CACHE_SCHEMA_VERSION);
        assert_eq!(
            decoded.models[0].capabilities,
            Some(crate::provider::ModelCapabilities::default())
        );
        assert_eq!(
            decoded.details()[0].capability_knowledge,
            crate::provider::CapabilityKnowledge::Advertised(
                crate::provider::ModelCapabilities::default()
            )
        );
    }

    #[test]
    fn model_cache_freshness_requires_current_complete_recent_metadata() {
        let explicit_empty = crate::provider::DiscoveredModelDetails {
            slug: "codex/test".into(),
            context_budget: Some(8_192),
            capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                crate::provider::ModelCapabilities::default(),
            ),
        };
        let fresh = ModelCacheDocument::from_details(100, &[explicit_empty]);
        let empty = ModelCacheDocument::from_details(100, &[]);
        let legacy: ModelCacheDocument = serde_json::from_str(
            r#"{"fetched_at":100,"models":[{"slug":"codex/test","thinking":true}]}"#,
        )
        .unwrap();
        let incomplete: ModelCacheDocument = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "fetched_at": 100,
                "models": [{"slug":"codex/test","thinking":true}]
            }"#,
        )
        .unwrap();
        let missing = AuthStoreDocument {
            providers: vec![provider("missing").into()],
        };

        assert_eq!(
            missing.model_cache_freshness("missing", 100, 900),
            Some(ModelCacheFreshness::Missing)
        );
        assert_eq!(fresh.freshness(999, 900), ModelCacheFreshness::Fresh);
        assert_eq!(empty.freshness(999, 900), ModelCacheFreshness::Fresh);
        assert_eq!(fresh.freshness(1_000, 900), ModelCacheFreshness::Expired);
        assert_eq!(fresh.freshness(99, 900), ModelCacheFreshness::Expired);
        assert_eq!(
            legacy.freshness(1_000, 900),
            ModelCacheFreshness::LegacySchema
        );
        assert_eq!(
            incomplete.freshness(1_000, 900),
            ModelCacheFreshness::MissingCapabilities
        );
    }

    #[test]
    fn versioned_wire_cache_is_readable_as_the_public_dto() {
        let wire = ModelCacheDocument::from_details(
            10,
            &[crate::provider::DiscoveredModelDetails {
                slug: "codex/test".into(),
                context_budget: Some(8_192),
                capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                    crate::provider::ModelCapabilities::default(),
                ),
            }],
        );

        let json = serde_json::to_string(&wire).unwrap();
        let legacy: ModelCache = serde_json::from_str(&json).unwrap();

        assert_eq!(legacy.fetched_at, 10);
        assert_eq!(legacy.models[0].slug, "codex/test");
        assert!(!legacy.models[0].thinking);
    }

    #[test]
    fn legacy_view_updates_preserve_namespace_and_capability_metadata() {
        let mut document = AuthStoreDocument::default();
        document.merge_legacy_view(AuthStore {
            providers: vec![StoredProvider {
                id: "provider".into(),
                name: "Provider".into(),
                kind: ProviderKind::Codex,
                access_token: "old".into(),
                refresh_token: None,
                expires_at: 1,
                account: None,
                enabled: true,
                model_cache: None,
            }],
        });
        assert!(
            document
                .update_model_cache_details(
                    "provider",
                    "stable-provider",
                    10,
                    &[crate::provider::DiscoveredModelDetails {
                        slug: "codex/test".into(),
                        context_budget: Some(8_192),
                        capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                            crate::provider::ModelCapabilities::default(),
                        ),
                    }],
                )
                .unwrap()
        );

        let mut legacy = document.legacy_view();
        legacy.providers[0].access_token = "new".into();
        document.merge_legacy_view(legacy);

        assert_eq!(document.legacy_view().providers[0].access_token, "new");
        assert_eq!(
            document.model_namespace("provider").as_deref(),
            Some("stable-provider")
        );
        assert!(matches!(
            document.model_cache_details("provider").unwrap()[0].capability_knowledge,
            crate::provider::CapabilityKnowledge::Advertised(_)
        ));
        assert!(
            document
                .update_model_cache_details("provider", "changed", 11, &[])
                .unwrap_err()
                .contains("already")
        );
        assert_eq!(
            document.model_namespace("provider").as_deref(),
            Some("stable-provider")
        );
    }

    #[test]
    fn provider_rename_preserves_credential_cas_and_invalidates_catalog() {
        let mut document = AuthStoreDocument::default();
        document.merge_legacy_view(AuthStore {
            providers: vec![provider("provider")],
        });
        let credential = document
            .provider_credential_state("provider")
            .unwrap()
            .1
            .unwrap();
        let catalog = document.provider_catalog_snapshot("provider").unwrap();

        let mut legacy = document.legacy_view();
        legacy.providers[0].name = "Renamed OAuth account".into();
        document.merge_legacy_view(legacy);

        assert_eq!(
            document
                .provider_credential_state("provider")
                .unwrap()
                .1
                .unwrap(),
            credential
        );
        assert_ne!(
            document.provider_catalog_snapshot("provider").unwrap(),
            catalog
        );
        assert!(matches!(
            document.update_provider_credentials(
                "provider",
                &credential,
                "fresh-access".into(),
                Some("fresh-refresh".into()),
                2,
                Some("fresh@example.test".into()),
            ),
            AuthCredentialCommit::Updated { .. }
        ));
    }

    #[test]
    fn legacy_duplicate_ids_do_not_cross_wire_capability_metadata() {
        let provider = |name: &str| StoredProvider {
            id: "duplicate".into(),
            name: name.into(),
            kind: ProviderKind::Codex,
            access_token: "old".into(),
            refresh_token: None,
            expires_at: 1,
            account: None,
            enabled: true,
            model_cache: None,
        };
        let details = |capabilities: crate::provider::ModelCapabilities| {
            ModelCacheDocument::from_details(
                10,
                &[crate::provider::DiscoveredModelDetails {
                    slug: "api/same".into(),
                    context_budget: Some(8_192),
                    capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                        capabilities,
                    ),
                }],
            )
        };
        let mut first: StoredProviderDocument = provider("first").into();
        first.model_cache = Some(details(crate::provider::ModelCapabilities::default()));
        let mut second: StoredProviderDocument = provider("second").into();
        second.model_cache = Some(details(crate::provider::ModelCapabilities {
            input_modalities: vec![crate::provider::InputModality::Image],
            ..Default::default()
        }));
        let mut document = AuthStoreDocument {
            providers: vec![first, second],
        };

        let mut legacy = document.legacy_view();
        legacy.providers[0].access_token = "new-first".into();
        legacy.providers[1].access_token = "new-second".into();
        document.merge_legacy_view(legacy);

        assert_eq!(document.providers[0].access_token, "new-first");
        assert_eq!(document.providers[1].access_token, "new-second");
        assert_eq!(
            document.providers[0].model_cache.as_ref().unwrap().models[0].capabilities,
            Some(crate::provider::ModelCapabilities::default())
        );
        assert_eq!(
            document.providers[1].model_cache.as_ref().unwrap().models[0]
                .capabilities
                .as_ref()
                .unwrap()
                .input_modalities,
            [crate::provider::InputModality::Image]
        );
    }
}
