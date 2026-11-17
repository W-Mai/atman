//! Authenticated RPC access to an existing native execution host.

use std::{io, net::SocketAddr, sync::Arc};

use atman_runtime::{
    flow_runtime::host::RuntimeHost, permission::PermissionClients, rendezvous::PromptResolver,
};
use futures::future::BoxFuture;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    DaemonState,
    http::{HttpState, router},
};

type PromptReply =
    dyn Fn(Uuid, Value, Option<Uuid>) -> BoxFuture<'static, io::Result<bool>> + Send + Sync;

/// Host-owned registration and reply paths for the same pending interactions.
#[derive(Clone)]
pub struct SharedPrompts {
    pub(crate) resolver: Arc<dyn PromptResolver>,
    reply: Arc<PromptReply>,
}

impl SharedPrompts {
    pub fn new(
        resolver: Arc<dyn PromptResolver>,
        reply: impl Fn(Uuid, Value, Option<Uuid>) -> BoxFuture<'static, io::Result<bool>>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            resolver,
            reply: Arc::new(reply),
        }
    }

    pub(crate) async fn reply(
        &self,
        id: Uuid,
        value: Value,
        session: Option<Uuid>,
    ) -> io::Result<bool> {
        (self.reply)(id, value, session).await
    }
}

/// Trusted native bindings; RPC callers cannot replace execution or sender authority.
pub struct SharedHostServices {
    pub host: RuntimeHost,
    pub permissions: Arc<PermissionClients>,
    pub operator: Uuid,
    pub prompts: Option<SharedPrompts>,
}

/// A private loopback RPC listener whose lifetime does not own execution.
pub struct EmbeddedRpcServer {
    endpoint: SocketAddr,
    shutdown: tokio_util::sync::CancellationToken,
    task: Option<tokio::task::JoinHandle<io::Result<()>>>,
}

impl EmbeddedRpcServer {
    pub async fn bind(state: Arc<DaemonState>, auth_token: String) -> io::Result<Self> {
        if auth_token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "RPC authentication token is empty",
            ));
        }
        if let Some(peer) = state.peer_transport.get() {
            peer.distinct_user_credential(&auth_token)?;
        }
        let endpoint = state
            .peer_endpoint()
            .unwrap_or_else(|| (std::net::Ipv4Addr::LOCALHOST, 0).into());
        let listener = tokio::net::TcpListener::bind(endpoint).await?;
        let endpoint = listener.local_addr()?;
        state.register_peer_endpoint(endpoint)?;
        let shutdown = state.shutdown_signal().child_token();
        let app = router(Arc::new(HttpState {
            daemon: state,
            auth_token,
        }));
        let stopped = shutdown.clone();
        let task = tokio::spawn(crate::http::serve(listener, app, stopped));
        Ok(Self {
            endpoint,
            shutdown,
            task: Some(task),
        })
    }

    pub fn endpoint(&self) -> SocketAddr {
        self.endpoint
    }

    /// Stop RPC admission and observation, leaving the native host and accepted work alive.
    pub async fn shutdown(mut self) -> io::Result<()> {
        self.shutdown.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)??;
        }
        Ok(())
    }
}

impl Drop for EmbeddedRpcServer {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches graceful cleanup; it must not abort accepted work.
        self.shutdown.cancel();
    }
}
