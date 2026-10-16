use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use super::{ConfigError, ConfigHub};
use crate::model_registry::{ModelRegistry, ProviderConfig, commit_prepared_config};

type Source = (PathBuf, PathBuf);

static SCOPES: LazyLock<Mutex<BTreeMap<Source, Weak<ModelRegistry>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static DEFAULT_SOURCE: LazyLock<Option<Source>> = LazyLock::new(|| {
    crate::storage::config_dir().ok().map(|dir| {
        let dir = crate::fs_access::canonicalize_stable(&dir);
        (dir.join("config.toml"), dir.join("auth.json"))
    })
});

pub(super) fn models(config: PathBuf, auth: PathBuf) -> Arc<ModelRegistry> {
    let mut scopes = SCOPES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    scopes.retain(|_, scope| scope.strong_count() > 0);
    let source = (config, auth);
    if let Some(models) = scopes.get(&source).and_then(Weak::upgrade) {
        return models;
    }
    let models = if DEFAULT_SOURCE.as_ref() == Some(&source) {
        crate::model_registry::default_registry().clone()
    } else {
        Arc::new(ModelRegistry::new())
    };
    scopes.insert(source, Arc::downgrade(&models));
    models
}

/// Validate every live projection before writing the shared configuration source.
/// Scope registration waits until publication; lifecycle callbacks run after this lock is released.
pub(super) fn publish(
    hub: &ConfigHub,
    text: &str,
    persist: impl FnOnce() -> Result<(), ConfigError>,
) -> Result<ProviderConfig, ConfigError> {
    let scopes = SCOPES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let config = hub.config_toml_path();
    let models = scopes
        .iter()
        .filter(|((path, _), _)| path == &config)
        .filter_map(|(_, scope)| scope.upgrade())
        .collect::<Vec<_>>();
    let prepared = models
        .iter()
        .map(|models| {
            models
                .prepare_config_text(text)
                .map_err(|error| ConfigError::Invalid(error.to_string()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot = prepared
        .first()
        .expect("the calling hub owns a registered scope")
        .snapshot();
    persist()?;
    for update in prepared {
        commit_prepared_config(update);
    }
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_registry::ProviderDescriptor;
    use crate::provider::{DiscoveredModelDetails, ReasoningWireProfile};

    #[test]
    fn source_identity_is_stable_before_directory_creation() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("new/config");
        let first = ConfigHub::from_config_dir(&nested);
        std::fs::create_dir_all(&nested).unwrap();
        let second = ConfigHub::from_auth_path(nested.join("./auth.json"));
        assert!(Arc::ptr_eq(first.models(), second.models()));
        let other = ConfigHub::from_auth_path(nested.join("another.json"));
        assert!(!Arc::ptr_eq(first.models(), other.models()));
        let global = ConfigHub::global().unwrap();
        assert!(Arc::ptr_eq(
            global.models(),
            crate::model_registry::default_registry()
        ));
    }

    #[test]
    fn shared_config_commits_all_scopes_or_preserves_every_projection() {
        let dir = tempfile::tempdir().unwrap();
        let first = ConfigHub::from_auth_path(dir.path().join("first.json"));
        let second = ConfigHub::from_auth_path(dir.path().join("second.json"));
        first.add_alias("smart", "old").unwrap();
        assert_eq!(second.models().resolve_alias("smart"), "old");
        second
            .models()
            .replace_provider_catalog(
                ProviderDescriptor {
                    provider_key: "account".into(),
                    provider_name: "Account".into(),
                    namespace: "reserved".into(),
                    wire_profile: ReasoningWireProfile::Unknown,
                },
                &[DiscoveredModelDetails::from(
                    crate::provider::DiscoveredModel {
                        slug: "gpt-4o".into(),
                        context_budget: Some(8192),
                        thinking: false,
                    },
                )],
            )
            .unwrap();
        let before = first.read_config_toml().unwrap();
        let revisions = (
            first.models().model_catalog_revision(),
            second.models().model_catalog_revision(),
        );
        // This registry key conflicts only in the second authentication scope.
        let error = first
            .update_config_toml(|doc| {
                doc["providers"]["reserved"]["kind"] = toml_edit::value("openai");
                doc["providers"]["reserved"]["base_url"] =
                    toml_edit::value("https://api.openai.com/v1");
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("reserved"));
        assert_eq!(first.read_config_toml().unwrap(), before);
        assert_eq!(
            (
                first.models().model_catalog_revision(),
                second.models().model_catalog_revision()
            ),
            revisions
        );
        second.update_alias(Some("smart"), "smart", "new").unwrap();
        assert_eq!(first.models().resolve_alias("smart"), "new");
        assert!(first.models().model_entry("reserved:gpt-4o").is_none());
        assert!(second.models().model_entry("reserved:gpt-4o").is_some());
        let before = first.read_config_toml().unwrap();
        let revisions = (
            first.models().model_catalog_revision(),
            second.models().model_catalog_revision(),
        );
        assert!(
            publish(&first, "[alias.smart]\nmodel = 'unwritten'", || Err(
                ConfigError::Invalid("write failed".into())
            ))
            .is_err()
        );
        assert_eq!(first.read_config_toml().unwrap(), before);
        assert_eq!(
            (
                first.models().model_catalog_revision(),
                second.models().model_catalog_revision()
            ),
            revisions
        );
    }
}
