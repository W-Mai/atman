use anyhow::{Context, Result};
use base64::Engine;
use serde::Deserialize;

use crate::auth_store::{AuthStore, StoredProvider};

pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Refresh an expiring Codex access token and persist the updated tokens.
/// Returns the updated `StoredProvider` on success.
///
/// Returns `Ok(None)` if the token does not need refreshing (expires in >5 min).
pub async fn refresh_if_needed(provider: &StoredProvider) -> Result<Option<StoredProvider>> {
    let now = chrono::Utc::now().timestamp();
    let refresh_window = 300;
    if provider.expires_at > now + refresh_window {
        return Ok(None);
    }
    let refresh_token = provider
        .refresh_token
        .as_deref()
        .context("no refresh token available for codex provider")?;

    let tokens = refresh_access_token(refresh_token).await?;

    let mut updated = provider.clone();
    updated.access_token = tokens.access_token;
    if let Some(ref rt) = tokens.refresh_token {
        updated.refresh_token = Some(rt.clone());
    }
    updated.expires_at = tokens.expires_at;
    if let Some(acct) = tokens.account {
        updated.account = Some(acct);
    }

    let mut store = AuthStore::load().unwrap_or_default();
    store.remove(&provider.id);
    store.add(updated.clone());
    store.save()?;

    Ok(Some(updated))
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

struct TokenResult {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: i64,
    account: Option<String>,
}

async fn refresh_access_token(refresh_token: &str) -> Result<TokenResult> {
    let client = reqwest::Client::new();
    let resp = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .context("token refresh request")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!("token refresh failed (HTTP {status}): {body}");
    }

    let data: RefreshResponse = serde_json::from_str(&body).context("parse refresh response")?;

    let account = data
        .id_token
        .as_deref()
        .and_then(extract_account_from_id_token);

    let expires_at =
        parse_jwt_exp(&data.access_token).unwrap_or_else(|| chrono::Utc::now().timestamp() + 3600);

    Ok(TokenResult {
        access_token: data.access_token,
        refresh_token: data.refresh_token,
        expires_at,
        account,
    })
}

fn parse_jwt_exp(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].as_bytes())
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    v.get("exp")?.as_i64()
}

fn extract_account_from_id_token(id_token: &str) -> Option<String> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].as_bytes())
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    v.get("email")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}
