use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use atman_runtime::auth_store::{AuthStore, ProviderKind, StoredProvider};
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
                    let mut store = AuthStore::load().unwrap_or_default();
                    store.add(provider.clone());
                    let _ = store.save();

                    // Discover models immediately after login.
                    let discover_provider = P::from_stored(&provider);
                    let models = discover_provider.discover_models().await;
                    if !models.is_empty() {
                        let _ = atman_runtime::auth_store::save_provider_model_cache(&id, &models);
                        atman_runtime::model_registry::register_discovered(
                            &id,
                            &provider.name,
                            &models,
                        );
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
