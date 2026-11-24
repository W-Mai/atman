use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use axum::serve::Listener;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig,
        crypto::ring,
        pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    },
    server::TlsStream,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HANDSHAKES: usize = 32;

type Incoming = io::Result<(TlsStream<TcpStream>, SocketAddr)>;

pub(super) struct TlsListener {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    pending: FuturesUnordered<BoxFuture<'static, Incoming>>,
}

impl TlsListener {
    pub(super) async fn bind(
        address: SocketAddr,
        certificate: &Path,
        private_key: &Path,
    ) -> Result<Self> {
        let certificate = CertificateDer::pem_file_iter(certificate)
            .context("read remote TLS certificate")?
            .collect::<Result<Vec<_>, _>>()
            .context("decode remote TLS certificate")?;
        let private_key =
            PrivateKeyDer::from_pem_file(private_key).context("read remote TLS private key")?;
        let mut config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(certificate, private_key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            listener: TcpListener::bind(address)
                .await
                .context("bind remote HTTPS listener")?,
            acceptor: TlsAcceptor::from(Arc::new(config)),
            pending: FuturesUnordered::new(),
        })
    }
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                biased;
                completed = self.pending.next(), if !self.pending.is_empty() => {
                    if let Some(Ok(connection)) = completed {
                        return connection;
                    }
                }
                connection = self.listener.accept(), if self.pending.len() < MAX_HANDSHAKES => {
                    match connection {
                        Ok((stream, peer)) => {
                            let acceptor = self.acceptor.clone();
                            self.pending.push(Box::pin(async move {
                                let stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await??;
                                Ok((stream, peer))
                            }));
                        }
                        Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
