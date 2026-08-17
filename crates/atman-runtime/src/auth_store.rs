use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::storage::config_dir;

const AUTH_FILENAME: &str = "auth.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Codex,
    AnthropicOauth,
    GitHubCopilot,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCache {
    pub fetched_at: i64,
    pub models: Vec<CachedModel>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedModel {
    pub slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_budget: Option<u64>,
    pub thinking: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthStore {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<StoredProvider>,
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
    crate::config_hub::ConfigHub::global()?.update_auth_model_cache(provider_id, cache)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
}
