use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use atman_runtime::auth_store::StoredProvider;
use atman_runtime::model_registry::CatalogDelta;
use atman_runtime::oauth::{OAuthProvider, TokenResult};
use atman_runtime::provider::Provider;
use atman_runtime::provider_lifecycle::ProviderLifecycle;

const CALLBACK_PORT: u16 = 1455;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn oauth_login<P: OAuthProvider + Provider + 'static>(
    lifecycle: &ProviderLifecycle,
    name: &str,
) -> Result<(StoredProvider, CatalogDelta)> {
    let (auth_url, pkce, state) = P::authorize_url();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let name = name.to_string();
    let verifier = pkce.verifier.clone();
    let lifecycle_for_exchange = lifecycle.clone();

    let exchange_fn = move |code: String| {
        let verifier = verifier.clone();
        let name = name.clone();
        let tx = tx.clone();
        let lifecycle = lifecycle_for_exchange.clone();
        Box::pin(async move {
            match P::exchange_code(&code, &verifier).await {
                Ok(tokens) => match install_oauth_tokens::<P>(&lifecycle, name, tokens).await {
                    Ok(outcome) => {
                        let _ = tx.send(Ok(outcome));
                        Ok(())
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        let _ = tx.send(Err(anyhow::anyhow!(message.clone())));
                        Err(message)
                    }
                },
                Err(e) => {
                    let msg = format!("{e:#}");
                    let _ = tx.send(Err(anyhow::anyhow!("{msg}")));
                    Err(msg)
                }
            }
        })
            as Pin<Box<dyn std::future::Future<Output = std::result::Result<(), String>> + Send>>
    };

    let listener = atman_runtime::oauth_server::bind_oauth_callback_listener(CALLBACK_PORT)?;
    let callback = atman_runtime::oauth_server::capture_oauth_callback_on_listener(
        listener,
        state,
        exchange_fn,
        CALLBACK_TIMEOUT,
    );

    if let Err(error) = open::that(&auth_url) {
        return Err(error).context("open OAuth authorization page");
    }

    callback.await?;
    rx.recv().await.context("no auth outcome")?
}

async fn install_oauth_tokens<P: OAuthProvider + Provider + 'static>(
    lifecycle: &ProviderLifecycle,
    name: String,
    tokens: TokenResult,
) -> Result<(StoredProvider, CatalogDelta)> {
    let provider = StoredProvider {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        kind: P::KIND.clone(),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
        account: tokens.account,
        enabled: true,
        model_cache: None,
    };
    let managed = atman_runtime::oauth::create_managed_oauth_provider_from_stored::<P>(
        &provider,
        lifecycle.config_hub().clone(),
    )?;
    let snapshot = P::from_stored(&provider);
    let models = snapshot
        .try_discover_models()
        .await
        .map_err(anyhow::Error::new)?;
    let delta = lifecycle
        .install_pre_discovered_provider(provider.clone(), managed, models)
        .await?;
    Ok((provider, delta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::error::RuntimeError;
    use atman_runtime::event::Observable;
    use atman_runtime::provider::{
        AssistantMessage, CapabilityKnowledge, DiscoveredModelDetails, LlmRequest,
        ModelDiscoveryError,
    };
    use atman_runtime::tool::BoxFut;

    struct CatalogCleanup(Option<String>);

    impl Drop for CatalogCleanup {
        fn drop(&mut self) {
            if let Some(provider_id) = self.0.take() {
                atman_runtime::model_registry::remove_provider_catalog(&provider_id);
            }
        }
    }

    struct TestOAuthProvider {
        id: String,
        discovery_fails: bool,
    }

    impl Provider for TestOAuthProvider {
        fn name(&self) -> &str {
            &self.id
        }

        fn call<'a>(
            &'a self,
            _req: LlmRequest,
        ) -> BoxFut<'a, std::result::Result<AssistantMessage, RuntimeError>> {
            Box::pin(async { Err(RuntimeError::ToolFailed("unused test provider".into())) })
        }

        fn call_streaming(&self, _req: LlmRequest) -> Observable<AssistantMessage> {
            panic!("unused test provider")
        }

        fn try_discover_models(
            &self,
        ) -> BoxFut<'static, std::result::Result<Vec<DiscoveredModelDetails>, ModelDiscoveryError>>
        {
            let discovery_fails = self.discovery_fails;
            Box::pin(async move {
                if discovery_fails {
                    return Err(ModelDiscoveryError::Transport("offline".into()));
                }
                Ok(vec![DiscoveredModelDetails {
                    slug: "test-model".into(),
                    context_budget: Some(64_000),
                    capability_knowledge: CapabilityKnowledge::Legacy { thinking: true },
                }])
            })
        }
    }

    impl OAuthProvider for TestOAuthProvider {
        const KIND: atman_runtime::auth_store::ProviderKind =
            atman_runtime::auth_store::ProviderKind::AnthropicOauth;

        fn authorize_url() -> (String, atman_runtime::oauth::Pkce, String) {
            panic!("unused test provider")
        }

        fn exchange_code(
            _code: &str,
            _verifier: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>
        {
            Box::pin(async { panic!("unused test provider") })
        }

        fn refresh_token(
            _token: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>
        {
            Box::pin(async { panic!("unused test provider") })
        }

        fn from_stored(stored: &StoredProvider) -> Self {
            Self {
                id: stored.id.clone(),
                discovery_fails: stored.access_token == "fail",
            }
        }

        fn from_managed_stored(
            stored: &StoredProvider,
            _hub: atman_runtime::config_hub::ConfigHub,
        ) -> Option<Self> {
            Some(Self {
                id: stored.id.clone(),
                discovery_fails: false,
            })
        }
    }

    fn tokens(access_token: &str) -> TokenResult {
        TokenResult {
            access_token: access_token.into(),
            refresh_token: Some("refresh".into()),
            expires_at: i64::MAX,
            account: Some("account".into()),
        }
    }

    #[test]
    fn token_install_uses_provider_kind_and_commits_runtime_state_atomically() {
        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = tempfile::tempdir().unwrap();
        let lifecycle = ProviderLifecycle::new(
            atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
            atman_runtime::provider::ProviderRegistry::new(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (provider, delta) = runtime
            .block_on(install_oauth_tokens::<TestOAuthProvider>(
                &lifecycle,
                "Test OAuth".into(),
                tokens("access"),
            ))
            .unwrap();
        let mut cleanup = CatalogCleanup(Some(provider.id.clone()));

        assert_eq!(
            provider.kind,
            atman_runtime::auth_store::ProviderKind::AnthropicOauth
        );
        assert_eq!(delta.total, 1);
        assert!(lifecycle.provider_registry().contains(&provider.id));
        let stored = lifecycle.config_hub().load_auth().unwrap();
        assert_eq!(stored.providers.len(), 1);
        assert!(stored.providers[0].model_cache.is_some());

        lifecycle.remove_provider(&provider.id).unwrap();
        cleanup.0 = None;
    }

    #[test]
    fn discovery_failure_leaves_no_auth_live_provider_or_catalog_change() {
        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = tempfile::tempdir().unwrap();
        let lifecycle = ProviderLifecycle::new(
            atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
            atman_runtime::provider::ProviderRegistry::new(),
        );
        let revision = atman_runtime::model_registry::model_catalog_revision();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(install_oauth_tokens::<TestOAuthProvider>(
                &lifecycle,
                "Test OAuth".into(),
                tokens("fail"),
            ))
            .unwrap_err();

        assert!(error.to_string().contains("offline"));
        assert!(
            lifecycle
                .config_hub()
                .load_auth()
                .unwrap()
                .providers
                .is_empty()
        );
        assert_eq!(
            atman_runtime::model_registry::model_catalog_revision(),
            revision
        );
    }

    #[test]
    fn supported_managed_factory_rejects_unimplemented_auth_kinds() {
        let config = tempfile::tempdir().unwrap();
        let stored = StoredProvider {
            id: "unsupported".into(),
            name: "Unsupported".into(),
            kind: atman_runtime::auth_store::ProviderKind::GitHubCopilot,
            access_token: "access".into(),
            refresh_token: None,
            expires_at: i64::MAX,
            account: None,
            enabled: false,
            model_cache: None,
        };
        let result = atman_runtime::oauth::create_supported_managed_oauth_provider(
            &stored,
            atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
        );

        assert!(
            result
                .err()
                .expect("unsupported kind must fail")
                .to_string()
                .contains("not supported")
        );
    }
}
