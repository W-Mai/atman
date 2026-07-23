use anyhow::{Context, Result};
use atman_runtime::auth_store::{AuthStore, ProviderKind, StoredProvider};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;

pub async fn login(name: &str) -> Result<StoredProvider> {
    let pkce = Pkce::generate();
    let state = generate_state();
    let auth_url = build_auth_url(&pkce, &state);

    let state_for_server = state.clone();
    let server = tokio::task::spawn(async move { wait_for_callback(state_for_server).await });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = open::that(&auth_url);

    let code = server.await??;

    let tokens = exchange_code(&code, &pkce).await?;

    let mut store = AuthStore::load().unwrap_or_default();
    let id = uuid::Uuid::new_v4().to_string();
    let provider = StoredProvider {
        id: id.clone(),
        name: name.to_string(),
        kind: ProviderKind::Codex,
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: tokens.expires_at,
        account: tokens.account,
    };
    store.add(provider.clone());
    store.save()?;

    Ok(provider)
}

struct Pkce {
    code_verifier: String,
    code_challenge: String,
}

impl Pkce {
    fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let code_verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut hasher = Sha256::new();
        hasher.update(code_verifier.as_bytes());
        let hash = hasher.finalize();
        let code_challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash);
        Self {
            code_verifier,
            code_challenge,
        }
    }
}

fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn build_auth_url(pkce: &Pkce, state: &str) -> String {
    use std::fmt::Write;
    let params: &[(&str, &str)] = &[
        ("response_type", "code"),
        ("client_id", atman_runtime::codex_token::CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", &pkce.code_challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ];
    let mut qs = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            qs.push('&');
        }
        write!(qs, "{k}={}", urlencoding(v)).unwrap();
    }
    format!("{AUTHORIZE_URL}?{qs}")
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                use std::fmt::Write;
                write!(out, "%{b:02X}").unwrap();
            }
        }
    }
    out
}

async fn wait_for_callback(expected_state: String) -> Result<String> {
    use axum::extract::Query;
    use axum::response::Html;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::{Mutex, oneshot};

    let result: Arc<Mutex<Option<Result<String>>>> = Arc::new(Mutex::new(None));
    let shutdown_slot: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));
    let shutdown_slot_for_closure = shutdown_slot.clone();
    let expected = expected_state;
    let result_clone = result.clone();

    let app = axum::Router::new().route(
        "/auth/callback",
        axum::routing::get(move |Query(params): Query<HashMap<String, String>>| {
            let result = result_clone.clone();
            let expected = expected.clone();
            let shutdown_slot = shutdown_slot_for_closure.clone();
            async move {
                let state_ok = params.get("state").map(|s| s == &expected).unwrap_or(false);
                let (outcome, page) = if !state_ok {
                    (
                        Err(anyhow::anyhow!("state mismatch")),
                        callback_page(
                            false,
                            "State mismatch",
                            "The OAuth state parameter did not match. Please try logging in again.",
                        ),
                    )
                } else if let Some(code) = params.get("code").cloned() {
                    (
                        Ok(code),
                        callback_page(true, "认证成功", "已接入 Codex 账户。您可以关闭此页面。"),
                    )
                } else if let Some(err) = params.get("error").cloned() {
                    (
                        Err(anyhow::anyhow!("oauth error: {err}")),
                        callback_page(false, "OAuth 错误", &format!("授权服务器返回错误: {err}")),
                    )
                } else {
                    (
                        Err(anyhow::anyhow!("missing code")),
                        callback_page(false, "请求无效", "缺少授权码。请重新登录。"),
                    )
                };
                *result.lock().await = Some(outcome);
                // Fire shutdown after a brief delay so axum has time to
                // flush the HTML response to the browser.
                let shutdown_slot = shutdown_slot.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    if let Some(tx) = shutdown_slot.lock().await.take() {
                        let _ = tx.send(());
                    }
                });
                Html(page)
            }
        }),
    );

    let socket = tokio::net::TcpSocket::new_v4().context("create tcp socket")?;
    socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;
    let bind_addr: std::net::SocketAddr = (std::net::Ipv4Addr::LOCALHOST, CALLBACK_PORT).into();
    socket
        .bind(bind_addr)
        .with_context(|| format!("bind callback port {CALLBACK_PORT}"))?;
    let listener = socket
        .listen(128)
        .with_context(|| format!("listen callback port {CALLBACK_PORT}"))?;

    let (tx, rx) = oneshot::channel::<()>();
    *shutdown_slot.lock().await = Some(tx);

    tokio::select! {
        _ = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        }) => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
    }

    let mut guard = result.lock().await;
    guard
        .take()
        .unwrap_or_else(|| Err(anyhow::anyhow!("no callback received")))
}

#[derive(Deserialize)]
struct TokenResponse {
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

async fn exchange_code(code: &str, pkce: &Pkce) -> Result<TokenResult> {
    let client = reqwest::Client::new();
    let resp = client
        .post(atman_runtime::codex_token::TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", atman_runtime::codex_token::CLIENT_ID),
            ("code_verifier", &pkce.code_verifier),
        ])
        .send()
        .await
        .context("token exchange request")?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        anyhow::bail!("token exchange failed (HTTP {status}): {body}");
    }

    let data: TokenResponse = serde_json::from_str(&body).context("parse token response")?;

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

fn callback_page(ok: bool, title: &str, message: &str) -> String {
    let icon = if ok { "✓" } else { "✗" };
    let color = if ok { "#0078a0" } else { "#c0392b" };
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
    background: #fff; border-radius: 12px; padding: 48px 56px;
    box-shadow: 0 2px 8px rgba(0,0,0,.06);
    text-align: center; max-width: 520px;
    border-top: 3px solid {color};
  }}
  .logo {{ margin-bottom: 24px; }}
  .logo pre {{ 
    font-size: 8px; line-height: 1.1; color: #0078a0; 
    font-family: "JetBrains Mono","Fira Code",Menlo,Consolas,monospace;
  }}
  .icon {{
    font-size: 40px; color: {color}; margin-bottom: 20px;
    display: inline-block; width: 64px; height: 64px; line-height: 64px;
    border-radius: 50%; background: {acetate};
  }}
  h1 {{ font-size: 20px; font-weight: 600; color: #1e1e1e; margin-bottom: 12px; }}
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
</html>"#,
        acetate = if ok {
            "rgba(0,120,160,0.10)"
        } else {
            "rgba(192,57,43,0.10)"
        },
    )
}
