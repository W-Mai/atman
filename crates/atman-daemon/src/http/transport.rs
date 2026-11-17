use std::{
    fmt::Debug,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};

use axum::{
    Router,
    extract::{ConnectInfo, Request, connect_info::Connected},
    http::{Version, header},
    middleware::{self, Next},
    response::Response,
    serve::{IncomingStream, Listener},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

/// Serve RPC requests and retire HTTP/1 event observations when the listener stops. Accepted non-streaming handlers drain through Axum's graceful shutdown; cancelling this listener does not cancel the native execution host.
pub async fn serve<L>(listener: L, app: Router, shutdown: CancellationToken) -> io::Result<()>
where
    L: Listener,
    L::Addr: Debug,
{
    let listener = ObservationListener {
        listener,
        shutdown: shutdown.clone(),
    };
    let app = app
        .layer(middleware::from_fn(observation_connection))
        .layer(axum::Extension(super::ListenerShutdown(shutdown.clone())))
        .into_make_service_with_connect_info::<ObservationConnection>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
}

#[derive(Clone, Default)]
struct ObservationConnection(Arc<AtomicBool>);

impl<L: Listener> Connected<IncomingStream<'_, ObservationListener<L>>> for ObservationConnection {
    fn connect_info(stream: IncomingStream<'_, ObservationListener<L>>) -> Self {
        stream.io().observation.clone()
    }
}

async fn observation_connection(
    ConnectInfo(observation): ConnectInfo<ObservationConnection>,
    request: Request,
    next: Next,
) -> Response {
    // A reused connection must drain its new handler even if it previously carried SSE.
    observation.0.store(false, Ordering::Release);
    let http1 = matches!(request.version(), Version::HTTP_10 | Version::HTTP_11);
    let response = next.run(request).await;
    let events = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    // HTTP/1 has one active response; closing it cannot cancel another accepted request.
    observation.0.store(http1 && events, Ordering::Release);
    response
}

struct ObservationListener<L> {
    listener: L,
    shutdown: CancellationToken,
}

impl<L: Listener> Listener for ObservationListener<L> {
    type Io = ObservationIo<L::Io>;
    type Addr = L::Addr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let (io, address) = self.listener.accept().await;
        (
            ObservationIo {
                io,
                observation: ObservationConnection::default(),
                shutdown: Box::pin(self.shutdown.clone().cancelled_owned()),
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

struct ObservationIo<T> {
    io: T,
    observation: ObservationConnection,
    shutdown: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl<T> ObservationIo<T> {
    fn poll_observation(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if self.observation.0.load(Ordering::Acquire) && self.shutdown.as_mut().poll(cx).is_ready()
        {
            // Hyper may stop polling a body while its pending bytes cannot reach the client.
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "event listener stopped",
            ));
        }
        Ok(())
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ObservationIo<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.poll_observation(cx)?;
        Pin::new(&mut self.io).poll_read(cx, buffer)
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ObservationIo<T> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.poll_observation(cx)?;
        Pin::new(&mut self.io).poll_write(cx, buffer)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_observation(cx)?;
        Pin::new(&mut self.io).poll_write_vectored(cx, buffers)
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_observation(cx)?;
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_observation(cx)?;
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::Infallible, time::Duration};

    use axum::{response::Sse, response::sse::Event, routing::get};
    use futures::StreamExt;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt, DuplexStream},
        sync::{Notify, mpsc},
    };

    use super::*;

    struct Connections(mpsc::UnboundedReceiver<DuplexStream>);

    impl Listener for Connections {
        type Io = DuplexStream;
        type Addr = ();

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            match self.0.recv().await {
                Some(io) => (io, ()),
                None => std::future::pending().await,
            }
        }

        fn local_addr(&self) -> io::Result<Self::Addr> {
            Ok(())
        }
    }

    async fn request(client: &mut DuplexStream, path: &str) -> Vec<u8> {
        client
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(client.read_u8().await.unwrap());
        }
        assert!(headers.starts_with(b"HTTP/1.1 200"));
        headers
    }

    fn event_routes(shutdown: CancellationToken) -> Router {
        Router::new()
            .route(
                "/events",
                get(move || {
                    let stopped = shutdown.clone();
                    async move {
                        Sse::new(
                            futures::stream::repeat_with(|| {
                                Ok::<_, Infallible>(Event::default().data("x".repeat(8192)))
                            })
                            .take_until(stopped.cancelled_owned()),
                        )
                    }
                }),
            )
            .route(
                "/finite-events",
                get(|| async {
                    Sse::new(futures::stream::iter([Ok::<_, Infallible>(
                        Event::default().data("done"),
                    )]))
                }),
            )
    }

    #[tokio::test]
    async fn backpressured_sse_must_not_keep_listener_shutdown_waiting() {
        let shutdown = CancellationToken::new();
        let (connections, receiver) = mpsc::unbounded_channel();
        let (mut client, connection) = tokio::io::duplex(1024);
        connections.send(connection).unwrap();
        let app = event_routes(shutdown.clone()).layer(middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("Text/Event-Stream; charset=utf-8"),
                );
                response
            },
        ));
        let mut server = tokio::spawn(serve(Connections(receiver), app, shutdown.clone()));
        request(&mut client, "/events").await;
        shutdown.cancel();
        let stopped = tokio::time::timeout(Duration::from_secs(1), &mut server).await;
        if stopped.is_err() {
            server.abort();
        }
        stopped
            .expect("listener shutdown must not depend on an observer reading its response body")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn observation_shutdown_drains_a_reused_connections_new_request() {
        let shutdown = CancellationToken::new();
        let entered = Arc::new(Notify::new());
        let complete = Arc::new(Notify::new());
        let handler_entered = entered.clone();
        let handler_complete = complete.clone();
        let app = event_routes(shutdown.clone()).route(
            "/mutation",
            get(move || {
                let entered = handler_entered.clone();
                let complete = handler_complete.clone();
                async move {
                    entered.notify_one();
                    complete.notified().await;
                    "accepted complete"
                }
            }),
        );
        let (connections, receiver) = mpsc::unbounded_channel();
        let (mut observer, observation) = tokio::io::duplex(1024);
        let (mut client, connection) = tokio::io::duplex(1024);
        connections.send(observation).unwrap();
        connections.send(connection).unwrap();
        let mut server = tokio::spawn(serve(Connections(receiver), app, shutdown.clone()));
        request(&mut observer, "/events").await;
        request(&mut client, "/finite-events").await;
        let mut body = Vec::new();
        while !body.ends_with(b"0\r\n\r\n") {
            body.push(client.read_u8().await.unwrap());
        }
        client
            .write_all(b"GET /mutation HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        entered.notified().await;
        shutdown.cancel();
        tokio::select! {
            result = &mut server => panic!("an accepted request must finish before shutdown: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
        complete.notify_one();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), client.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.ends_with(b"accepted complete"));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
