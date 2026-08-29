use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use atman_runtime::auth_store::{ProviderKind, StoredProvider};
use atman_runtime::oauth::OAuthProvider;
use atman_runtime::provider::Provider;

const CALLBACK_PORT: u16 = 1455;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);

pub async fn oauth_login<P: OAuthProvider + Provider>(
    kind: ProviderKind,
    name: &str,
) -> Result<StoredProvider> {
    let (auth_url, pkce, state) = P::authorize_url();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let name = name.to_string();
    let verifier = pkce.verifier.clone();

    let exchange_fn = Box::new(move |code: String| {
        let verifier = verifier.clone();
        let name = name.clone();
        let tx = tx.clone();
        Box::pin(async move {
            match P::exchange_code(&code, &verifier).await {
                Ok(tokens) => {
                    let id = uuid::Uuid::new_v4().to_string();
                    let provider = StoredProvider {
                        id: id.clone(),
                        name,
                        kind,
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token,
                        expires_at: tokens.expires_at,
                        account: tokens.account,
                        enabled: true,
                        model_cache: None,
                    };
                    if let Err(error) = atman_runtime::config_hub::ConfigHub::global()
                        .and_then(|hub| hub.add_auth_provider(provider.clone()))
                    {
                        let message = format!("save OAuth provider: {error}");
                        let _ = tx.send(Err(anyhow::anyhow!(message.clone())));
                        return Err(message);
                    }

                    // Discover models immediately after login.
                    let discover_provider = P::from_stored(&provider);
                    match discover_provider.try_discover_models().await {
                        Ok(models) => {
                            let prepared = if provider.kind == ProviderKind::Codex {
                                atman_runtime::model_registry::prepare_discovered_details_for_provider(
                                            &id,
                                            &provider.name,
                                            &models,
                                        )
                            } else {
                                atman_runtime::model_registry::prepare_discovered_details(
                                    &id,
                                    &provider.name,
                                    &models,
                                )
                            };
                            match prepared {
                                Ok(prepared) => match atman_runtime::auth_store::save_provider_model_cache_details(
                                    &id, prepared.namespace(), &models,
                                ) {
                                    Ok(()) => {
                                        atman_runtime::model_registry::commit_prepared_provider_catalog(
                                            prepared,
                                        );
                                    }
                                    Err(error) => atman_runtime::notify!(
                                        error,
                                        "model cache save failed after login: {error:#}"
                                    ),
                                },
                                Err(error) => {
                                        atman_runtime::notify!(
                                            error,
                                            "model catalog install failed after login: {error}"
                                        );
                                }
                            }
                        }
                        Err(error) => atman_runtime::notify!(
                            warn,
                            "model discovery failed after login: {error}"
                        ),
                    }

                    let _ = tx.send(Ok(provider));
                    Ok(())
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    let _ = tx.send(Err(anyhow::anyhow!("{msg}")));
                    Err(msg)
                }
            }
        })
            as Pin<
                Box<
                    dyn std::future::Future<Output = std::result::Result<(), String>> + Send + Send,
                >,
            >
    });

    let state_for_server = state.clone();
    let server = tokio::task::spawn(async move {
        atman_runtime::oauth_server::capture_oauth_callback(
            CALLBACK_PORT,
            state_for_server,
            exchange_fn,
            CALLBACK_TIMEOUT,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = open::that(&auth_url);

    server.await??;
    rx.recv().await.context("no auth outcome")?
}
