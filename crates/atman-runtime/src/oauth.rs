use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::auth_store::{AuthStore, StoredProvider};
use crate::provider::{DiscoveredModel, Provider};

pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);

        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let digest = hasher.finalize();
        let challenge = URL_SAFE_NO_PAD.encode(digest);

        Pkce {
            verifier,
            challenge,
        }
    }
}

pub struct TokenResult {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    pub account: Option<String>,
}

pub trait OAuthProvider: Provider {
    fn authorize_url() -> (String, Pkce, String);
    fn exchange_code(
        code: &str,
        verifier: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>;
    fn refresh_token(
        token: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenResult>> + Send>>;
    fn from_stored(stored: &StoredProvider) -> Self;
}

pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn parse_jwt_exp(token: &str) -> Option<i64> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1].as_bytes()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    v.get("exp")?.as_i64()
}

pub fn extract_account_from_id_token(id_token: &str) -> Option<String> {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = URL_SAFE_NO_PAD.decode(parts[1].as_bytes()).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&payload).ok()?;
    v.get("email")
        .and_then(|e| e.as_str())
        .map(|s| s.to_string())
}

pub async fn create_oauth_provider<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<(Arc<P>, Vec<DiscoveredModel>)> {
    create_oauth_provider_impl(stored, true).await
}

/// Same as `create_oauth_provider` but skips `discover_models()`.
/// Returns an empty model list. Use when discovery will happen in the background.
pub async fn create_oauth_provider_no_discover<P: OAuthProvider>(
    stored: &StoredProvider,
) -> Result<Arc<P>> {
    let (provider, _) = create_oauth_provider_impl(stored, false).await?;
    Ok(provider)
}

async fn create_oauth_provider_impl<P: OAuthProvider>(
    stored: &StoredProvider,
    discover: bool,
) -> Result<(Arc<P>, Vec<DiscoveredModel>)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock")?
        .as_secs() as i64;
    let refresh_window = 300;

    let mut updated = stored.clone();
    if now + refresh_window >= stored.expires_at {
        if let Some(ref rt) = stored.refresh_token {
            let tokens = P::refresh_token(rt).await?;
            updated.access_token = tokens.access_token;
            updated.expires_at = tokens.expires_at;
            if tokens.refresh_token.is_some() {
                updated.refresh_token = tokens.refresh_token;
            }
            let mut store = AuthStore::load().unwrap_or_default();
            store.remove(&stored.id);
            store.add(updated.clone());
            let _ = store.save();
        }
    }

    let provider = P::from_stored(&updated);
    let models = if discover {
        provider.discover_models().await
    } else {
        vec![]
    };
    Ok((Arc::new(provider), models))
}

pub fn callback_page(ok: bool, title: &str, message: &str) -> String {
    let icon = if ok { "✓" } else { "✗" };
    let color = if ok { "#0078a0" } else { "#c0392b" };
    let acetate = if ok {
        "rgba(0,120,160,0.10)"
    } else {
        "rgba(192,57,43,0.10)"
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="zh">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title} — atman</title>
<style>
  * {{ margin:0; padding:0; box-sizing:border-box; }}
  body {{
    font-family: "JetBrains Mono","Fira Code",Menlo,Consolas,monospace;
    background: linear-gradient(180deg,#f0f0f0 0%,#e8e8ec 100%);
    min-height: 100vh; display:flex; align-items:center; justify-content:center;
  }}
  .card {{
    background: #fff; border-radius: 12px; padding: 36px 44px;
    box-shadow: 0 2px 8px rgba(0,0,0,.06);
    text-align: center; max-width: 520px;
    border-top: 3px solid {color};
  }}
  .logo {{ margin-bottom: 20px; }}
  .logo pre {{
    font-size: 6.5px; line-height: 1.15; color: #0078a0;
    font-family: "JetBrains Mono","Fira Code",Menlo,Consolas,monospace;
  }}
  .icon {{
    font-size: 36px; color: {color}; margin-bottom: 16px;
    display: inline-block; width: 56px; height: 56px; line-height: 56px;
    border-radius: 50%; background: {acetate};
  }}
  h1 {{ font-size: 18px; font-weight: 600; color: #1e1e1e; margin-bottom: 8px; }}
  p  {{ font-size: 13px; color: #606060; line-height: 1.6; }}
</style>
</head>
<body>
<div class="card">
  <div class="logo"><pre>
      ⢀⡤⣾⢿⡿⢿⡿⣷⢤⡀                                           
     ⢠⢯⢎⠞⡵⠚⠓⢮⠳⡱⡽⡄                                          
     ⡟⡏⡏⣀⣳⣀⣀⣞⣀⡰⢹⢻    ████████╗███╗   ███╗ █████╗ ███╗   ██╗
  ⢀⣠⡄⣧⣇⡇⠻⠿⠿⠿⠿⠿⢿⡿⣷⣦⣄⡀ ╚══██╔══╝████╗ ████║██╔══██╗████╗  ██║
⢀⡴⡫⡪⠕⠹⡼⡜⡄    ⢠⢢⢮⠍⠺⢗⢝⢦⡀  ██║   ██╔████╔██║███████║██╔██╗ ██║
⡞⡞⡞   ⠙⣝⢞⢦⡀⢀⡴⡳⣫⠋   ⢳⢳⢳  ██║   ██║╚██╔╝██║██╔══██║██║╚██╗██║
⢧⢧⡣⡀   ⠈⣓⡡⣔⣽⡪⢞⠁   ⢀⢜⡼⡼  ██║   ██║     ██║██║  ██║██║ ╚████║
⠈⠓⠿⣾⣿⣿⣿⣿⡿⠿⠛⠙⠾⢷⣿⣿⣿⣿⣷⠿⠚⠁  ╚═╝   ╚═╝     ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝
</pre></div>
  <div class="icon">{icon}</div>
  <h1>{title}</h1>
  <p>{message}</p>
</div>
</body>
</html>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    #[test]
    fn pkce_generates_43char_verifier_and_valid_challenge() {
        let pkce = Pkce::generate();
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.challenge.is_empty());
        let mut hasher = Sha256::new();
        hasher.update(pkce.verifier.as_bytes());
        let digest = hasher.finalize();
        let expected = URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(pkce.challenge, expected);
    }

    #[test]
    fn generate_state_is_32_hex_chars() {
        let state = generate_state();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_jwt_exp_works() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"exp":123456789,"other":"data"}"#.as_bytes());
        let token = format!("header.{payload}.sig");
        assert_eq!(parse_jwt_exp(&token), Some(123456789));
    }

    #[test]
    fn parse_jwt_exp_returns_none_when_missing() {
        assert_eq!(parse_jwt_exp("not.a.jwt"), None);
        assert_eq!(parse_jwt_exp(""), None);
    }

    #[test]
    fn extract_account_prefers_email() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"email":"a@b.com","sub":"123"}"#.as_bytes());
        let token = format!("h.{payload}.sig");
        assert_eq!(
            extract_account_from_id_token(&token),
            Some("a@b.com".to_string())
        );
    }

    #[test]
    fn extract_account_returns_none_when_missing() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"name":"John"}"#.as_bytes());
        let token = format!("h.{payload}.sig");
        assert_eq!(extract_account_from_id_token(&token), None);
    }

    #[test]
    fn callback_page_ok_has_right_icon_and_color() {
        let html = callback_page(true, "OK", "done");
        assert!(html.contains("✓"));
        assert!(html.contains("#0078a0"));
    }

    #[test]
    fn callback_page_err_has_right_icon_and_color() {
        let html = callback_page(false, "Err", "fail");
        assert!(html.contains("✗"));
        assert!(html.contains("#c0392b"));
    }
}
