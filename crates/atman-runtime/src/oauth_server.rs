use std::collections::HashMap;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use tokio::sync::Mutex;

use crate::oauth::callback_page;

type ExchangeFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;
type ExchangeFn = Box<dyn FnOnce(String) -> ExchangeFuture + Send>;

pub async fn capture_oauth_callback(
    port: u16,
    expected_state: String,
    exchange_fn: ExchangeFn,
    timeout: Duration,
) -> Result<()> {
    let exchange: Arc<Mutex<Option<ExchangeFn>>> = Arc::new(Mutex::new(Some(exchange_fn)));
    let result: Arc<Mutex<Option<Result<()>>>> = Arc::new(Mutex::new(None));

    let expected = expected_state;
    let exchange_for_handler = exchange.clone();
    let result_for_handler = result.clone();

    let app = axum::Router::new().route(
        "/auth/callback",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let exchange = exchange_for_handler.clone();
            let result = result_for_handler.clone();
            let expected = expected.clone();
            async move {
                let (outcome, page) = if params.get("state").map(|s| s != &expected).unwrap_or(true)
                {
                    (
                        Err(anyhow::anyhow!("state mismatch")),
                        callback_page(
                            false,
                            "State Mismatch",
                            "The OAuth state parameter did not match.",
                        ),
                    )
                } else if let Some(err) = params.get("error").cloned() {
                    (
                        Err(anyhow::anyhow!("oauth error: {err}")),
                        callback_page(false, "授权被拒绝", &format!("授权服务器返回: {err}")),
                    )
                } else if let Some(code) = params.get("code").cloned() {
                    let exchange_fn = exchange.lock().await.take();
                    match exchange_fn {
                        Some(f) => match f(code).await {
                            Ok(()) => (
                                Ok(()),
                                callback_page(true, "认证成功", "已接入账户。您可以关闭此页面。"),
                            ),
                            Err(msg) => (
                                Err(anyhow::anyhow!("token exchange failed: {msg}")),
                                callback_page(false, "登录失败", &msg),
                            ),
                        },
                        None => (
                            Err(anyhow::anyhow!("duplicate callback")),
                            callback_page(false, "请求无效", "重复的回调请求。"),
                        ),
                    }
                } else {
                    (
                        Err(anyhow::anyhow!("missing code")),
                        callback_page(false, "请求无效", "缺少授权码。"),
                    )
                };
                *result.lock().await = Some(outcome);

                Html(page)
            }
        }),
    );

    let socket = tokio::net::TcpSocket::new_v4().context("create tcp socket")?;
    socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;
    socket
        .bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
        .with_context(|| format!("bind callback port {port}"))?;
    let listener = socket
        .listen(128)
        .with_context(|| format!("listen callback port {port}"))?;

    tokio::select! {
        _ = axum::serve(listener, app) => {}
        _ = tokio::time::sleep(timeout) => {}
    }

    result
        .lock()
        .await
        .take()
        .unwrap_or_else(|| Err(anyhow::anyhow!("no callback received")))
}
