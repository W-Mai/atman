use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use tokio::sync::{Mutex, oneshot};

use crate::oauth::callback_page;

pub async fn capture_oauth_callback(
    port: u16,
    expected_state: String,
    timeout: Duration,
) -> Result<String> {
    let result: Arc<Mutex<Option<Result<String>>>> = Arc::new(Mutex::new(None));
    let shutdown_tx: Arc<Mutex<Option<oneshot::Sender<()>>>> = Arc::new(Mutex::new(None));

    let expected = expected_state;
    let result_for_handler = result.clone();
    let shutdown_for_handler = shutdown_tx.clone();

    let app = axum::Router::new().route(
        "/auth/callback",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let result = result_for_handler.clone();
            let shutdown = shutdown_for_handler.clone();
            let expected = expected.clone();
            async move {
                let state_ok = params.get("state").map(|s| s == &expected).unwrap_or(false);
                let (outcome, page) = if !state_ok {
                    (
                        Err(anyhow::anyhow!("state mismatch")),
                        callback_page(
                            false,
                            "State Mismatch",
                            "The OAuth state parameter did not match.",
                        ),
                    )
                } else if let Some(code) = params.get("code").cloned() {
                    (
                        Ok(code),
                        callback_page(true, "认证成功", "已接入账户。您可以关闭此页面。"),
                    )
                } else if let Some(err) = params.get("error").cloned() {
                    (
                        Err(anyhow::anyhow!("oauth error: {err}")),
                        callback_page(false, "授权被拒绝", &format!("授权服务器返回: {err}")),
                    )
                } else {
                    (
                        Err(anyhow::anyhow!("missing code")),
                        callback_page(false, "请求无效", "缺少授权码。"),
                    )
                };
                *result.lock().await = Some(outcome);
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if let Some(tx) = shutdown.lock().await.take() {
                        let _ = tx.send(());
                    }
                });
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

    let (tx, rx) = oneshot::channel();
    *shutdown_tx.lock().await = Some(tx);

    tokio::select! {
        _ = axum::serve(listener, app).with_graceful_shutdown(async {
            let _ = rx.await;
        }) => {}
        _ = tokio::time::sleep(timeout) => {}
    }

    result
        .lock()
        .await
        .take()
        .unwrap_or_else(|| Err(anyhow::anyhow!("no callback received")))
}
