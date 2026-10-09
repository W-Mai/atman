use std::sync::Arc;

use crate::model_registry::ProviderEntry;
use crate::provider::{OpenAiApi, Provider, ProviderRegistry};
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
    InvalidConfiguration,
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
    if entry.supports_api_selection()
        && let Some(url) = resolve_base_url(entry, &|name| std::env::var(name).ok())
        && validate_openai_base_url(&url).is_err()
    {
        return ConfigProviderAvailability::InvalidConfiguration;
    }
    match resolve_provider_credential(entry, &|name| std::env::var(name).ok()) {
        Ok(_) => ConfigProviderAvailability::Available,
        Err(availability) => availability,
    }
}

pub fn effective_api_endpoint(
    entry: &ProviderEntry,
    model_api: Option<OpenAiApi>,
) -> Option<String> {
    let api = entry.effective_api(model_api)?;
    let base = resolve_base_url(entry, &|name| std::env::var(name).ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".into());
    Some(format!(
        "{}{}",
        base.trim_end_matches('/'),
        api.endpoint_path()
    ))
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
            let mut provider =
                AnthropicProvider::new(registry_key, api_key).with_max_tools(entry.max_tools);
            if let Some(base_url) = base_url {
                provider = provider.with_base_url(&base_url);
            }
            if let Some(max_tokens) = entry.max_tokens {
                provider = provider.with_max_tokens(max_tokens);
            }
            Ok(Arc::new(provider))
        }
        "openai" | "openai-compat" => {
            let prompt_cache_key = entry.prompt_cache_key.unwrap_or_else(|| {
                entry.kind == "openai"
                    && base_url.as_deref().is_none_or(is_official_openai_base_url)
            });
            let base_url = base_url.unwrap_or_else(|| "https://api.openai.com/v1".into());
            validate_openai_base_url(&base_url)
                .map_err(|_| ConfigProviderAvailability::InvalidConfiguration)?;
            let client = reqwest::Client::new();
            let mut provider = OpenAiProvider::new(registry_key, &api_key)
                .with_reasoning_format(entry.reasoning_format.unwrap_or_else(|| {
                    crate::providers::openai::OpenAiReasoningFormat::for_provider_kind(&entry.kind)
                }))
                .with_prompt_cache_key(prompt_cache_key)
                .with_base_url(&base_url)
                .with_client(client.clone())
                .with_max_tools(entry.max_tools);
            let mut responses = crate::providers::responses::ResponsesProvider::new(
                registry_key,
                &base_url,
                api_key,
            )
            .with_client(client)
            .with_prompt_cache_key(prompt_cache_key)
            .with_max_tools(entry.max_tools);
            if let Some(max_tokens) = entry.max_tokens {
                provider = provider.with_max_tokens(max_tokens);
                responses = responses.with_max_tokens(max_tokens);
            }
            Ok(Arc::new(OpenAiConnection {
                default_api: entry.effective_api(None).expect("OpenAI provider"),
                chat: Arc::new(provider),
                responses: Arc::new(responses),
            }))
        }
        _ => unreachable!("supported provider kind was checked above"),
    }
}

struct OpenAiConnection {
    default_api: OpenAiApi,
    chat: Arc<dyn Provider>,
    responses: Arc<dyn Provider>,
}

impl OpenAiConnection {
    fn selected(&self, api: OpenAiApi) -> &Arc<dyn Provider> {
        match api {
            OpenAiApi::ChatCompletions => &self.chat,
            OpenAiApi::Responses => &self.responses,
        }
    }
}

impl Provider for OpenAiConnection {
    fn name(&self) -> &str {
        self.chat.name()
    }

    fn for_api(&self, api: OpenAiApi) -> Option<Arc<dyn Provider>> {
        Some(self.selected(api).clone())
    }

    fn capabilities(&self) -> crate::provider::ProviderCapabilities {
        self.selected(self.default_api).capabilities()
    }

    fn context_prefix(
        &self,
        req: &crate::provider::LlmRequest,
    ) -> Result<crate::context_plan::ContextPrefixSnapshot, crate::error::RuntimeError> {
        self.selected(self.default_api).context_prefix(req)
    }

    fn call<'a>(
        &'a self,
        req: crate::provider::LlmRequest,
    ) -> crate::tool::BoxFut<
        'a,
        Result<crate::provider::AssistantMessage, crate::error::RuntimeError>,
    > {
        self.selected(self.default_api).call(req)
    }

    fn call_streaming(
        &self,
        req: crate::provider::LlmRequest,
    ) -> crate::event::Observable<crate::provider::AssistantMessage> {
        self.selected(self.default_api).call_streaming(req)
    }

    fn discover_models(
        &self,
    ) -> crate::tool::BoxFut<'static, Vec<crate::provider::DiscoveredModel>> {
        self.chat.discover_models()
    }

    fn try_discover_models(
        &self,
    ) -> crate::tool::BoxFut<
        'static,
        Result<Vec<crate::provider::DiscoveredModelDetails>, crate::provider::ModelDiscoveryError>,
    > {
        self.chat.try_discover_models()
    }

    fn test_connection(&self) -> crate::tool::BoxFut<'_, Result<String, String>> {
        self.chat.test_connection()
    }
}

/// Validate a connection root before appending a protocol endpoint.
pub fn validate_openai_base_url(base_url: &str) -> Result<(), String> {
    let parsed =
        reqwest::Url::parse(base_url).map_err(|error| format!("invalid base URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("base URL must be an HTTP or HTTPS connection root".into());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("base URL must not contain a query or fragment".into());
    }
    let path = parsed.path().trim_end_matches('/');
    if path.ends_with("/chat/completions") || path.ends_with("/responses") {
        return Err("base URL must be the connection root, such as https://gateway.example/v1; select the API separately".into());
    }
    Ok(())
}

fn is_official_openai_base_url(base_url: &str) -> bool {
    base_url.trim_end_matches('/') == "https://api.openai.com/v1"
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
    if entry.api.is_some() && !entry.supports_api_selection() {
        return Err(ConfigProviderAvailability::InvalidConfiguration);
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

    #[tokio::test]
    async fn configured_protocols_share_credentials_and_preserve_model_identity() {
        use crate::provider::{LlmRequest, ReasoningEffort, ReasoningSelection};
        use serde_json::json;
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "model":"vendor/model", "choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
            })))
            .expect(1).mount(&server).await;
        let terminal = json!({"type":"response.completed", "response":{
            "id":"response-1", "model":"vendor/model", "status":"completed",
            "output":[{"type":"message", "id":"message-1", "role":"assistant", "content":[{"type":"output_text","text":"ok"}]}]
        }});
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(format!("data: {terminal}\n\n"), "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let entry = ProviderEntry {
            kind: "openai-compat".into(),
            api_key: Some("test-key".into()),
            base_url: Some(server.uri()),
            api: Some(OpenAiApi::Responses),
            reasoning_format: Some(crate::providers::openai::OpenAiReasoningFormat::Official),
            ..Default::default()
        };
        let connection = build_config_provider_with("config:gateway", &entry, |_| None).unwrap();
        for api in [OpenAiApi::ChatCompletions, OpenAiApi::Responses] {
            let provider = connection.for_api(api).unwrap();
            let req = LlmRequest {
                model: "vendor/model".into(),
                messages: vec![crate::provider::user_text_message("hello")],
                system: Some("instructions".into()),
                input: crate::value::Value::Unit,
                schema: None,
                cache_prompt: false,
                prompt_cache_key: None,
                tools: vec![crate::tool::ToolSpec {
                    name: "probe".into(),
                    description: None,
                    input_schema: json!({"type":"object", "properties":{}}),
                }],
                reasoning: ReasoningSelection::Effort {
                    effort: ReasoningEffort::High,
                    execution_mode: None,
                },
                stall_timeout_secs: 0,
            };
            provider.context_prefix(&req).unwrap();
            assert_eq!(provider.call(req).await.unwrap().text_concat(), "ok");
        }
        let requests = server.received_requests().await.unwrap();
        let chat: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let responses: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
        for body in [&chat, &responses] {
            assert_eq!(body["model"], "vendor/model");
        }
        assert_eq!(chat["reasoning_effort"], "high");
        assert_eq!(responses["reasoning"]["effort"], "high");
        assert!(responses.get("thinking").is_none());
        assert!(responses.get("reasoning_effort").is_none());
        assert_eq!(responses["tools"][0]["strict"], false);
        assert_eq!(responses["store"], false);
    }

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
    fn prompt_cache_key_capability_is_conservative_for_custom_endpoints() {
        let official = ProviderEntry {
            name: "official".into(),
            kind: "openai".into(),
            api_key: Some("test-key".into()),
            ..Default::default()
        };
        let official_provider = build_config_provider("official", &official).unwrap();
        assert!(official_provider.capabilities().prompt_cache_key);
        let disabled = ProviderEntry {
            prompt_cache_key: Some(false),
            ..official.clone()
        };
        assert!(
            !build_config_provider("official", &disabled)
                .unwrap()
                .capabilities()
                .prompt_cache_key
        );
        let env_routed = build_config_provider_with(
            "official",
            &official,
            read_from(&[("OPENAI_BASE_URL", "https://gateway.example/v1")]),
        )
        .unwrap();
        assert!(!env_routed.capabilities().prompt_cache_key);

        let compatible = ProviderEntry {
            name: "compatible".into(),
            kind: "openai-compat".into(),
            api_key: Some("test-key".into()),
            ..Default::default()
        };
        let compatible_provider = build_config_provider("compatible", &compatible).unwrap();
        assert!(!compatible_provider.capabilities().prompt_cache_key);

        let custom_official_shape = ProviderEntry {
            name: "gateway".into(),
            kind: "openai".into(),
            api_key: Some("test-key".into()),
            base_url: Some("https://gateway.example/v1".into()),
            ..Default::default()
        };
        let custom_provider = build_config_provider("gateway", &custom_official_shape).unwrap();
        assert!(!custom_provider.capabilities().prompt_cache_key);

        let opted_in = ProviderEntry {
            prompt_cache_key: Some(true),
            ..custom_official_shape
        };
        let opted_in_provider = build_config_provider("gateway", &opted_in).unwrap();
        assert!(opted_in_provider.capabilities().prompt_cache_key);
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
