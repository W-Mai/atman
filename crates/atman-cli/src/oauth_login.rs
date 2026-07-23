use std::time::Duration;

use anyhow::Result;
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

    let state_for_server = state.clone();
    let server = tokio::task::spawn(async move {
        atman_runtime::oauth_server::capture_oauth_callback(
            CALLBACK_PORT,
            state_for_server,
            CALLBACK_TIMEOUT,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(300)).await;

    let _ = open::that(&auth_url);

    let code = server.await??;
    let tokens = P::exchange_code(&code, &pkce.verifier).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let provider = StoredProvider {
        id: id.clone(),
        name: name.to_string(),
        kind,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
        account: tokens.account,
    };

    let mut store = AuthStore::load().unwrap_or_default();
    store.add(provider.clone());
    store.save()?;

    Ok(provider)
}
