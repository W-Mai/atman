use std::collections::HashMap;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use futures::FutureExt;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::oauth::callback_page;

type ExchangeFuture = Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>;
type ExchangeFn = Box<dyn FnOnce(String) -> ExchangeFuture + Send>;
const CALLBACK_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

enum CallbackResult {
    Completed(Result<()>),
    Panicked(Box<dyn std::any::Any + Send + 'static>),
}

struct CallbackServerGuard {
    cancelled: CancellationToken,
    abort: Option<tokio::task::AbortHandle>,
}

impl CallbackServerGuard {
    fn disarm(&mut self) {
        self.abort = None;
    }
}

impl Drop for CallbackServerGuard {
    fn drop(&mut self) {
        self.cancelled.cancel();
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

pub async fn capture_oauth_callback(
    port: u16,
    expected_state: String,
    exchange_fn: ExchangeFn,
    timeout: Duration,
) -> Result<()> {
    let listener = bind_oauth_callback_listener(port)?;

    capture_oauth_callback_on_listener_impl(listener, expected_state, exchange_fn, timeout).await
}

/// Binds the local OAuth callback listener before an authorization page opens.
pub fn bind_oauth_callback_listener(port: u16) -> Result<tokio::net::TcpListener> {
    let socket = tokio::net::TcpSocket::new_v4().context("create tcp socket")?;
    socket.set_reuseaddr(true).context("set SO_REUSEADDR")?;
    socket
        .bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
        .with_context(|| format!("bind callback port {port}"))?;
    let listener = socket
        .listen(128)
        .with_context(|| format!("listen callback port {port}"))?;
    Ok(listener)
}

/// Captures one OAuth callback from an already-bound listener.
pub async fn capture_oauth_callback_on_listener<F, Fut>(
    listener: tokio::net::TcpListener,
    expected_state: String,
    exchange_fn: F,
    timeout: Duration,
) -> Result<()>
where
    F: FnOnce(String) -> Fut + Send + 'static,
    Fut: Future<Output = std::result::Result<(), String>> + Send + 'static,
{
    let exchange_fn: ExchangeFn =
        Box::new(move |code| Box::pin(exchange_fn(code)) as ExchangeFuture);
    capture_oauth_callback_on_listener_impl(listener, expected_state, exchange_fn, timeout).await
}

async fn capture_oauth_callback_on_listener_impl(
    listener: tokio::net::TcpListener,
    expected_state: String,
    exchange_fn: ExchangeFn,
    timeout: Duration,
) -> Result<()> {
    let exchange: Arc<Mutex<Option<ExchangeFn>>> = Arc::new(Mutex::new(Some(exchange_fn)));
    let claimed = Arc::new(AtomicBool::new(false));
    let cancelled = CancellationToken::new();
    let (result_tx, mut result_rx) = tokio::sync::oneshot::channel::<CallbackResult>();
    let result_tx = Arc::new(Mutex::new(Some(result_tx)));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let mut shutdown_tx = Some(shutdown_tx);

    let expected = expected_state;
    let exchange_for_handler = exchange.clone();
    let result_for_handler = result_tx.clone();
    let claimed_for_handler = claimed.clone();
    let cancelled_for_handler = cancelled.clone();

    let app = axum::Router::new().route(
        "/auth/callback",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let exchange = exchange_for_handler.clone();
            let result = result_for_handler.clone();
            let claimed = claimed_for_handler.clone();
            let cancelled = cancelled_for_handler.clone();
            let expected = expected.clone();
            async move {
                if claimed
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return Html(callback_page(false, "请求无效", "重复的回调请求。"));
                }

                let completed = async {
                    if params.get("state").map(|s| s != &expected).unwrap_or(true) {
                        return (
                            CallbackResult::Completed(Err(anyhow::anyhow!("state mismatch"))),
                            callback_page(
                                false,
                                "State Mismatch",
                                "The OAuth state parameter did not match.",
                            ),
                        );
                    }
                    if let Some(err) = params.get("error").cloned() {
                        return (
                            CallbackResult::Completed(Err(anyhow::anyhow!("oauth error: {err}"))),
                            callback_page(false, "授权被拒绝", &format!("授权服务器返回: {err}")),
                        );
                    }
                    let Some(code) = params.get("code").cloned() else {
                        return (
                            CallbackResult::Completed(Err(anyhow::anyhow!("missing code"))),
                            callback_page(false, "请求无效", "缺少授权码。"),
                        );
                    };
                    let exchange_fn = exchange.lock().await.take();
                    match exchange_fn {
                        Some(f) => match std::panic::AssertUnwindSafe(async move { f(code).await })
                            .catch_unwind()
                            .await
                        {
                            Ok(Ok(())) => (
                                CallbackResult::Completed(Ok(())),
                                callback_page(true, "认证成功", "已接入账户。您可以关闭此页面。"),
                            ),
                            Ok(Err(msg)) => (
                                CallbackResult::Completed(Err(anyhow::anyhow!(
                                    "token exchange failed: {msg}"
                                ))),
                                callback_page(false, "登录失败", &msg),
                            ),
                            Err(payload) => (
                                CallbackResult::Panicked(payload),
                                callback_page(false, "登录失败", "认证处理异常。"),
                            ),
                        },
                        None => (
                            CallbackResult::Completed(Err(anyhow::anyhow!("duplicate callback"))),
                            callback_page(false, "请求无效", "重复的回调请求。"),
                        ),
                    }
                };

                let Some((outcome, page)) = callback_completion(&cancelled, completed).await else {
                    return Html(callback_page(false, "登录超时", "OAuth 登录已超时。"));
                };
                if let Some(sender) = result.lock().await.take() {
                    let _ = sender.send(outcome);
                }

                Html(page)
            }
        }),
    );

    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let mut server_guard = CallbackServerGuard {
        cancelled: cancelled.clone(),
        abort: Some(server.abort_handle()),
    };

    tokio::select! {
        biased;
        outcome = &mut result_rx => {
            match outcome.context("OAuth callback result channel closed")? {
                CallbackResult::Panicked(payload) => {
                    cancelled.cancel();
                    server.abort();
                    let _ = server.await;
                    server_guard.disarm();
                    std::panic::resume_unwind(payload);
                }
                CallbackResult::Completed(outcome) => {
                    if let Some(shutdown) = shutdown_tx.take() {
                        let _ = shutdown.send(());
                    }
                    let served = match tokio::time::timeout(
                        CALLBACK_DRAIN_TIMEOUT,
                        &mut server,
                    )
                    .await
                    {
                        Ok(joined) => joined
                            .context("join OAuth callback server")?
                            .context("serve OAuth callback"),
                        Err(_) => {
                            cancelled.cancel();
                            server.abort();
                            let _ = server.await;
                            Ok(())
                        }
                    };
                    server_guard.disarm();
                    served?;
                    outcome
                }
            }
        }
        server_result = &mut server => {
            server_guard.disarm();
            server_result
                .context("join OAuth callback server")?
                .context("serve OAuth callback")?;
            Err(anyhow::anyhow!("no callback received"))
        }
        _ = tokio::time::sleep(timeout) => {
            cancelled.cancel();
            if let Some(shutdown) = shutdown_tx.take() {
                let _ = shutdown.send(());
            }
            server.abort();
            let _ = server.await;
            server_guard.disarm();
            Err(anyhow::anyhow!("no callback received"))
        }
    }
}

async fn callback_completion<F, T>(cancelled: &CancellationToken, completed: F) -> Option<T>
where
    F: Future<Output = T>,
{
    tokio::select! {
        biased;
        _ = cancelled.cancelled() => None,
        completed = completed => Some(completed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use tokio::io::AsyncWriteExt;

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    async fn listener() -> (tokio::net::TcpListener, std::net::SocketAddr) {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    #[tokio::test]
    async fn successful_callback_returns_without_waiting_for_timeout() {
        let (listener, address) = listener().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_exchange = calls.clone();
        let exchange: ExchangeFn = Box::new(move |_| {
            calls_for_exchange.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        });
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));

        let response = reqwest::get(format!(
            "http://{address}/auth/callback?state=expected&code=code"
        ))
        .await
        .unwrap();
        assert!(response.status().is_success());
        assert!(response.text().await.unwrap().contains("认证成功"));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("callback server did not stop after success")
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_callback_returns_without_waiting_for_timeout() {
        let (listener, address) = listener().await;
        let exchange: ExchangeFn =
            Box::new(|_| Box::pin(async { Err("exchange rejected".into()) }));
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));

        let response = reqwest::get(format!(
            "http://{address}/auth/callback?state=expected&code=code"
        ))
        .await
        .unwrap();
        assert!(response.status().is_success());
        assert!(response.text().await.unwrap().contains("登录失败"));
        let error = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("callback server did not stop after failure")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("exchange rejected"));
    }

    #[tokio::test]
    async fn panicking_exchange_factory_is_propagated_after_shutdown() {
        let (listener, address) = listener().await;
        let exchange: ExchangeFn = Box::new(|_| panic!("exchange panic fixture"));
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));

        let response = reqwest::get(format!(
            "http://{address}/auth/callback?state=expected&code=code"
        ))
        .await
        .unwrap();
        assert!(response.text().await.unwrap().contains("登录失败"));
        let error = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("panicking exchange did not resolve the callback")
            .unwrap_err();
        assert!(error.is_panic());
    }

    #[tokio::test]
    async fn cancellation_wins_without_polling_ready_completion() {
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let polls = Arc::new(AtomicUsize::new(0));
        let polls_for_completion = polls.clone();

        let result = callback_completion(&cancelled, async move {
            polls_for_completion.fetch_add(1, Ordering::SeqCst);
            "completed"
        })
        .await;

        assert_eq!(result, None);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn slow_connection_cannot_block_callback_server_shutdown() {
        let (listener, address) = listener().await;
        let exchange: ExchangeFn = Box::new(|_| Box::pin(async { Ok(()) }));
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));
        let mut slow = tokio::net::TcpStream::connect(address).await.unwrap();
        slow.write_all(
            b"GET /auth/callback?state=expected&code=slow HTTP/1.1\r\nHost: localhost\r\n",
        )
        .await
        .unwrap();
        tokio::task::yield_now().await;

        let response = reqwest::get(format!(
            "http://{address}/auth/callback?state=expected&code=code"
        ))
        .await
        .unwrap();
        assert!(response.text().await.unwrap().contains("认证成功"));
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("slow connection blocked callback shutdown")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn duplicate_callback_cannot_overwrite_first_result() {
        let (listener, address) = listener().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_exchange = calls.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
        let exchange: ExchangeFn = Box::new(move |_| {
            calls_for_exchange.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let _ = started_tx.send(());
                let _ = proceed_rx.await;
                Ok(())
            })
        });
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));
        let client = reqwest::Client::new();
        let callback_url = format!("http://{address}/auth/callback?state=expected&code=code");
        let first_client = client.clone();
        let first_url = callback_url.clone();
        let first = tokio::spawn(async move { first_client.get(first_url).send().await.unwrap() });
        started_rx.await.unwrap();

        let duplicate = client.get(callback_url).send().await.unwrap();
        assert!(duplicate.text().await.unwrap().contains("重复的回调请求"));
        proceed_tx.send(()).unwrap();
        assert!(
            first
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("认证成功")
        );
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("callback server did not stop after first result")
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn hanging_exchange_is_cancelled_by_outer_timeout() {
        let (listener, address) = listener().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let exchange: ExchangeFn = Box::new(move |_| {
            Box::pin(async move {
                let _drop_signal = DropSignal(Some(dropped_tx));
                let _ = started_tx.send(());
                std::future::pending::<std::result::Result<(), String>>().await
            })
        });
        let server = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(1),
        ));
        let request = tokio::spawn(async move {
            reqwest::get(format!(
                "http://{address}/auth/callback?state=expected&code=code"
            ))
            .await
        });
        tokio::time::timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("exchange did not start")
            .unwrap();

        let error = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("outer timeout did not cancel the callback server")
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("no callback received"));
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("timed-out exchange future was not dropped")
            .unwrap();
        request.abort();
    }

    #[tokio::test]
    async fn cancelling_capture_drops_a_hanging_exchange() {
        let (listener, address) = listener().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let exchange: ExchangeFn = Box::new(move |_| {
            Box::pin(async move {
                let _drop_signal = DropSignal(Some(dropped_tx));
                let _ = started_tx.send(());
                std::future::pending::<std::result::Result<(), String>>().await
            })
        });
        let capture = tokio::spawn(capture_oauth_callback_on_listener(
            listener,
            "expected".into(),
            exchange,
            Duration::from_secs(30),
        ));
        let request = tokio::spawn(async move {
            reqwest::get(format!(
                "http://{address}/auth/callback?state=expected&code=code"
            ))
            .await
        });
        started_rx.await.unwrap();

        capture.abort();
        let _ = capture.await;
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .expect("cancelled callback exchange was not dropped")
            .unwrap();
        request.abort();
    }
}
