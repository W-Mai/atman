use std::sync::Arc;

use crate::model_registry::ProviderEntry;
use crate::provider::{Provider, ProviderRegistry};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::openai::OpenAiProvider;

/// Whether a config-backed provider can be used by the current process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigProviderAvailability {
    Available,
    Disabled,
    MissingCredential,
    UnsupportedKind,
}

pub(crate) fn reconcile_config_provider_deferred(
    registry: &ProviderRegistry,
    name: &str,
    entry: &ProviderEntry,
) -> (ConfigProviderAvailability, Option<Arc<dyn Provider>>) {
    reconcile_config_provider_with(registry, name, entry, |name| std::env::var(name).ok())
}

/// Build a config-backed provider with the same credential and endpoint resolution as live registration.
pub fn build_config_provider(
    name: &str,
    entry: &ProviderEntry,
) -> Result<Arc<dyn Provider>, ConfigProviderAvailability> {
    build_config_provider_with(name, entry, |name| std::env::var(name).ok())
}

/// Inspect config-backed provider availability without exposing credentials.
pub fn config_provider_availability(entry: &ProviderEntry) -> ConfigProviderAvailability {
    match resolve_provider_credential(entry, &|name| std::env::var(name).ok()) {
        Ok(_) => ConfigProviderAvailability::Available,
        Err(availability) => availability,
    }
}

fn reconcile_config_provider_with(
    registry: &ProviderRegistry,
    name: &str,
    entry: &ProviderEntry,
    read_env: impl Fn(&str) -> Option<String>,
) -> (ConfigProviderAvailability, Option<Arc<dyn Provider>>) {
    let registry_key = format!("config:{name}");
    let provider = match build_config_provider_with(&registry_key, entry, read_env) {
        Ok(provider) => provider,
        Err(availability) => {
            return (availability, registry.take_named(&registry_key));
        }
    };
    (
        ConfigProviderAvailability::Available,
        registry.register_named(registry_key, provider),
    )
}

fn build_config_provider_with(
    registry_key: &str,
    entry: &ProviderEntry,
    read_env: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn Provider>, ConfigProviderAvailability> {
    let api_key = resolve_provider_credential(entry, &read_env)?;
    let base_url = resolve_base_url(entry, &read_env);
    match entry.kind.as_str() {
        "anthropic" => {
            let mut provider = AnthropicProvider::new(registry_key, api_key);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(&base_url);
            }
            if let Some(max_tokens) = entry.max_tokens {
                provider = provider.with_max_tokens(max_tokens);
            }
            Ok(Arc::new(provider))
        }
        "openai" | "openai-compat" => {
            let mut provider = OpenAiProvider::new(registry_key, api_key).with_reasoning_format(
                entry.reasoning_format.unwrap_or_else(|| {
                    crate::providers::openai::OpenAiReasoningFormat::for_provider_kind(&entry.kind)
                }),
            );
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(&base_url);
            }
            if let Some(max_tokens) = entry.max_tokens {
                provider = provider.with_max_tokens(max_tokens);
            }
            Ok(Arc::new(provider))
        }
        _ => unreachable!("supported provider kind was checked above"),
    }
}

fn resolve_provider_credential(
    entry: &ProviderEntry,
    read_env: &impl Fn(&str) -> Option<String>,
) -> Result<String, ConfigProviderAvailability> {
    if entry.enabled == Some(false) {
        return Err(ConfigProviderAvailability::Disabled);
    }
    if !matches!(
        entry.kind.as_str(),
        "anthropic" | "openai" | "openai-compat"
    ) {
        return Err(ConfigProviderAvailability::UnsupportedKind);
    }
    resolve_api_key(entry, read_env).ok_or(ConfigProviderAvailability::MissingCredential)
}

fn resolve_api_key(
    entry: &ProviderEntry,
    read_env: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    entry
        .api_key_env
        .as_deref()
        .and_then(read_env)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            entry
                .api_key
                .clone()
                .filter(|value| !value.trim().is_empty())
        })
        .or_else(|| {
            let variable = match entry.kind.as_str() {
                "openai" | "openai-compat" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                _ => return None,
            };
            read_env(variable).filter(|value| !value.trim().is_empty())
        })
}

fn resolve_base_url(
    entry: &ProviderEntry,
    read_env: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    entry
        .base_url
        .clone()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let variable = match entry.kind.as_str() {
                "openai" | "openai-compat" => "OPENAI_BASE_URL",
                "anthropic" => "ANTHROPIC_BASE_URL",
                _ => return None,
            };
            read_env(variable).filter(|value| !value.trim().is_empty())
        })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn read_from(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<HashMap<_, _>>();
        move |key| values.get(key).cloned()
    }

    #[test]
    fn api_key_resolution_matches_cold_and_live_registration_order() {
        let mut entry = ProviderEntry {
            kind: "openai-compat".into(),
            api_key: Some("inline".into()),
            api_key_env: Some("CUSTOM_API_KEY".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_api_key(
                &entry,
                &read_from(&[("CUSTOM_API_KEY", "custom"), ("OPENAI_API_KEY", "fallback")]),
            )
            .as_deref(),
            Some("custom")
        );
        assert_eq!(
            resolve_api_key(&entry, &read_from(&[("OPENAI_API_KEY", "fallback")])).as_deref(),
            Some("inline")
        );
        entry.api_key = None;
        assert_eq!(
            resolve_api_key(&entry, &read_from(&[("OPENAI_API_KEY", "fallback")])).as_deref(),
            Some("fallback")
        );
    }

    #[test]
    fn configured_base_url_precedes_kind_fallback() {
        let mut entry = ProviderEntry {
            kind: "anthropic".into(),
            base_url: Some("https://configured.invalid".into()),
            ..Default::default()
        };
        let read_env = read_from(&[("ANTHROPIC_BASE_URL", "https://fallback.invalid")]);
        assert_eq!(
            resolve_base_url(&entry, &read_env).as_deref(),
            Some("https://configured.invalid")
        );
        entry.base_url = None;
        assert_eq!(
            resolve_base_url(&entry, &read_env).as_deref(),
            Some("https://fallback.invalid")
        );
    }

    #[test]
    fn reconciliation_removes_disabled_or_unusable_provider() {
        let registry = ProviderRegistry::new();
        let mut entry = ProviderEntry {
            kind: "openai-compat".into(),
            api_key: Some("test-key".into()),
            enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(
            reconcile_config_provider_with(&registry, "gateway", &entry, read_from(&[])).0,
            ConfigProviderAvailability::Available
        );
        assert!(registry.contains("config:gateway"));

        entry.enabled = Some(false);
        assert_eq!(
            reconcile_config_provider_with(&registry, "gateway", &entry, read_from(&[])).0,
            ConfigProviderAvailability::Disabled
        );
        assert!(!registry.contains("config:gateway"));

        entry.enabled = Some(true);
        entry.api_key = None;
        assert_eq!(
            reconcile_config_provider_with(&registry, "gateway", &entry, read_from(&[])).0,
            ConfigProviderAvailability::MissingCredential
        );
        assert!(!registry.contains("config:gateway"));
    }

    #[test]
    fn availability_distinguishes_disabled_missing_and_unsupported_providers() {
        let mut entry = ProviderEntry {
            kind: "openai-compat".into(),
            api_key: Some("test-key".into()),
            enabled: Some(true),
            ..Default::default()
        };
        assert_eq!(
            build_config_provider_with("config:test", &entry, read_from(&[]))
                .map(|_| ConfigProviderAvailability::Available)
                .unwrap_or_else(|availability| availability),
            ConfigProviderAvailability::Available
        );

        entry.enabled = Some(false);
        assert!(matches!(
            build_config_provider_with("config:test", &entry, read_from(&[])),
            Err(ConfigProviderAvailability::Disabled)
        ));

        entry.enabled = Some(true);
        entry.api_key = None;
        assert!(matches!(
            build_config_provider_with("config:test", &entry, read_from(&[])),
            Err(ConfigProviderAvailability::MissingCredential)
        ));

        entry.kind = "unsupported".into();
        assert!(matches!(
            build_config_provider_with("config:test", &entry, read_from(&[])),
            Err(ConfigProviderAvailability::UnsupportedKind)
        ));
    }
}
