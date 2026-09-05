use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use atman_proto::{
    CatalogDelta, InitializeConfigRequest, InitializeConfigResponse, ProbeProviderRequest,
    ProbeResponse, ProviderKind, ProviderMutation, ProviderMutationResult, ProviderStateChange,
    SwitchDefaultModelResponse, UpsertModelConfigRequest, UpsertModelConfigResponse,
};
use atman_runtime::auth_store::StoredProvider;
use atman_runtime::oauth::{OAuthProvider, TokenResult};
use atman_runtime::provider::Provider;
use atman_runtime::provider_lifecycle::ProviderLifecycle;

const CALLBACK_PORT: u16 = 1455;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

pub fn initialize(
    state: &crate::state::DaemonState,
    launcher: &crate::run::RunLauncher,
    request: &InitializeConfigRequest,
) -> Result<InitializeConfigResponse> {
    let fs_access = request
        .fs_access
        .as_deref()
        .map(str::parse)
        .transpose()
        .map_err(|error: String| anyhow::anyhow!(error))?;
    let config_dir = launcher
        .config_dir
        .clone()
        .map(Ok)
        .unwrap_or_else(atman_runtime::storage::config_dir)?;
    let report = atman_runtime::config_init::init_config_dir_with_mode(&config_dir, fs_access)?;
    let lifecycle = state.provider_lifecycle_for(Some(&config_dir))?;
    lifecycle.config_hub().migrate_and_reload_models()?;
    lifecycle.reload_config_providers()?;
    Ok(InitializeConfigResponse {
        config_dir: report.config_dir.to_string_lossy().into_owned(),
        written: display_paths(report.written),
        skipped: display_paths(report.skipped),
        managed: display_paths(report.managed),
    })
}

fn display_paths(paths: Vec<std::path::PathBuf>) -> Vec<String> {
    paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

pub async fn mutate(
    lifecycle: &ProviderLifecycle,
    mutation: ProviderMutation,
) -> Result<ProviderMutationResult> {
    match mutation {
        ProviderMutation::Login { kind, name } => {
            if kind != ProviderKind::Codex {
                bail!("OAuth login for {kind:?} is not supported");
            }
            let (provider, delta) =
                oauth_login::<atman_runtime::providers::codex::CodexProvider>(lifecycle, &name)
                    .await?;
            Ok(ProviderMutationResult::Installed {
                provider_id: provider.id,
                name: provider.name,
                kind: provider_kind(provider.kind),
                delta: catalog_delta(delta),
            })
        }
        ProviderMutation::SetEnabled {
            provider_id,
            enabled,
        } => {
            if !enabled {
                let change = lifecycle.disable_provider(&provider_id)?;
                return Ok(ProviderMutationResult::StateChanged {
                    provider_id,
                    enabled: Some(false),
                    change: state_change(change),
                    catalog: None,
                });
            }
            let provider = lifecycle
                .config_hub()
                .load_auth()?
                .providers
                .into_iter()
                .find(|provider| provider.id == provider_id)
                .with_context(|| format!("auth provider `{provider_id}` does not exist"))?;
            let expected_kind = provider.kind.clone();
            let live = atman_runtime::oauth::create_supported_managed_oauth_provider(
                &provider,
                lifecycle.config_hub().clone(),
            )?;
            let outcome = lifecycle
                .enable_provider(&provider_id, expected_kind, live)
                .await?;
            Ok(ProviderMutationResult::StateChanged {
                provider_id,
                enabled: Some(true),
                change: state_change(outcome.state),
                catalog: outcome.catalog.map(catalog_delta),
            })
        }
        ProviderMutation::Remove { provider_id } => {
            let change = lifecycle.remove_provider(&provider_id)?;
            Ok(ProviderMutationResult::StateChanged {
                provider_id,
                enabled: None,
                change: state_change(change),
                catalog: None,
            })
        }
        ProviderMutation::Refresh { provider_id } => {
            let delta = lifecycle.refresh_models(&provider_id).await?;
            Ok(ProviderMutationResult::Refreshed {
                provider_id,
                delta: catalog_delta(delta),
            })
        }
        ProviderMutation::UpsertConfig {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            reasoning_format,
            enabled,
            create,
        } => {
            if !atman_runtime::model_registry::config_provider_types().contains(&kind.as_str()) {
                bail!("config provider kind `{kind}` is not supported");
            }
            let reasoning_format = if reasoning_format.trim().is_empty() {
                None
            } else {
                Some(
                    reasoning_format
                        .parse()
                        .map_err(|error: String| anyhow::anyhow!(error))?,
                )
            };
            let update = atman_runtime::config_hub::ProviderConfigUpdate {
                name: &name,
                kind: &kind,
                api_key: (!api_key.is_empty()).then_some(api_key.as_str()),
                api_key_env: (!api_key_env.is_empty()).then_some(api_key_env.as_str()),
                base_url: (!base_url.is_empty()).then_some(base_url.as_str()),
                max_tokens,
                reasoning_format,
                prompt_cache_key: None,
                enabled,
            };
            if create {
                lifecycle.create_config_provider(update)?;
            } else {
                lifecycle.update_config_provider(update)?;
            }
            Ok(ProviderMutationResult::ConfigSaved {
                name,
                created: create,
            })
        }
    }
}

pub fn upsert_model(
    lifecycle: &ProviderLifecycle,
    request: &UpsertModelConfigRequest,
) -> Result<UpsertModelConfigResponse> {
    let reasoning = request
        .reasoning
        .parse()
        .map_err(|error: String| anyhow::anyhow!(error))?;
    lifecycle
        .config_hub()
        .upsert_model(atman_runtime::model_registry::ModelConfigUpdate {
            old_name: request.old_name.as_deref(),
            name: &request.name,
            model: &request.model,
            provider: request.provider.as_deref(),
            context_budget: request.context_budget,
            reasoning,
            capabilities: None,
            image_detail: None,
            max_tokens: request.max_tokens,
            enabled: request.enabled,
        })?;
    lifecycle.config_hub().migrate_and_reload_models()?;
    lifecycle.reload_config_providers()?;
    Ok(UpsertModelConfigResponse {
        name: request.name.clone(),
    })
}

pub fn switch_default_model(
    lifecycle: &ProviderLifecycle,
    requested_model: &str,
) -> Result<SwitchDefaultModelResponse> {
    lifecycle.config_hub().migrate_and_reload_models()?;
    lifecycle.reload_config_providers()?;
    let info = atman_runtime::model_registry::model_info(requested_model);
    anyhow::ensure!(info.context_budget != 0, "model or provider is disabled");
    let model = info.name;
    anyhow::ensure!(
        lifecycle.provider_registry().resolve(&model).is_some(),
        "provider is not available in the daemon"
    );
    lifecycle
        .config_hub()
        .update_alias(Some("smart"), "smart", &model)?;
    lifecycle.config_hub().migrate_and_reload_models()?;
    Ok(SwitchDefaultModelResponse { model })
}

pub async fn probe(request: ProbeProviderRequest) -> ProbeResponse {
    let reasoning_format = match request.reasoning_format {
        Some(value) if !value.trim().is_empty() => match value.parse() {
            Ok(format) => Some(format),
            Err(error) => {
                return ProbeResponse {
                    message: error,
                    ok: false,
                };
            }
        },
        _ => None,
    };
    let entry = atman_runtime::model_registry::ProviderEntry {
        name: request.name.clone(),
        kind: request.kind,
        api_key: request.api_key,
        api_key_env: request.api_key_env,
        base_url: request.base_url,
        max_tokens: request.max_tokens,
        reasoning_format,
        prompt_cache_key: request.prompt_cache_key,
        enabled: Some(request.enabled),
    };
    let provider =
        match atman_runtime::config_provider::build_config_provider(&request.name, &entry) {
            Ok(provider) => provider,
            Err(availability) => {
                let reason = match availability {
                atman_runtime::config_provider::ConfigProviderAvailability::Disabled => {
                    "is disabled"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::MissingCredential => {
                    "has no available credential"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::UnsupportedKind => {
                    "uses an unsupported provider kind"
                }
                _ => "is unavailable",
            };
                return ProbeResponse {
                    message: format!("\"{}\" {reason}", request.name),
                    ok: false,
                };
            }
        };
    match tokio::time::timeout(Duration::from_secs(15), provider.test_connection()).await {
        Ok(Ok(message)) => ProbeResponse { message, ok: true },
        Ok(Err(message)) => ProbeResponse {
            message: format!("\"{}\" {message}", request.name),
            ok: false,
        },
        Err(_) => ProbeResponse {
            message: format!("\"{}\" timed out after 15s", request.name),
            ok: false,
        },
    }
}

fn catalog_delta(delta: atman_runtime::model_registry::CatalogDelta) -> CatalogDelta {
    CatalogDelta {
        added: delta.added,
        updated: delta.updated,
        removed: delta.removed,
        total: delta.total,
    }
}

fn state_change(
    change: atman_runtime::provider_lifecycle::ProviderStateChange,
) -> ProviderStateChange {
    ProviderStateChange {
        auth_changed: change.auth_changed,
        live_changed: change.live_changed,
        catalog_changed: change.catalog_changed,
    }
}

fn provider_kind(kind: atman_runtime::auth_store::ProviderKind) -> ProviderKind {
    match kind {
        atman_runtime::auth_store::ProviderKind::Codex => ProviderKind::Codex,
        atman_runtime::auth_store::ProviderKind::AnthropicOauth => ProviderKind::AnthropicOauth,
        atman_runtime::auth_store::ProviderKind::GitHubCopilot => ProviderKind::GitHubCopilot,
        atman_runtime::auth_store::ProviderKind::Custom => ProviderKind::Custom,
    }
}

async fn oauth_login<P: OAuthProvider + Provider + 'static>(
    lifecycle: &ProviderLifecycle,
    name: &str,
) -> Result<(StoredProvider, atman_runtime::model_registry::CatalogDelta)> {
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
                Err(error) => {
                    let message = format!("{error:#}");
                    let _ = tx.send(Err(anyhow::anyhow!(message.clone())));
                    Err(message)
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
    open::that(&auth_url).context("open OAuth authorization page")?;
    callback.await?;
    rx.recv().await.context("no auth outcome")?
}

async fn install_oauth_tokens<P: OAuthProvider + Provider + 'static>(
    lifecycle: &ProviderLifecycle,
    name: String,
    tokens: TokenResult,
) -> Result<(StoredProvider, atman_runtime::model_registry::CatalogDelta)> {
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
    let models = snapshot.try_discover_models().await?;
    let delta = lifecycle
        .install_pre_discovered_provider(provider.clone(), managed, models)
        .await?;
    Ok((provider, delta))
}
