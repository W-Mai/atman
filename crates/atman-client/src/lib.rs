//! Typed transports for the atman daemon protocol.

mod http;
mod session;
#[cfg(unix)]
mod unix;

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use atman_proto::{
    CapabilitiesRequest, CapabilitiesResponse, ClientId, CloseSessionRequest, CloseSessionResponse,
    CreateSessionRequest, DeleteSessionRequest, DeleteSessionResponse, EventCursor, JsonRpcRequest,
    JsonRpcResponse, ListProjectsRequest, ListProjectsResponse, ListSessionsRequest,
    PROTOCOL_VERSION, ProjectionEventEnvelope, RequestId, RpcKind, RpcMethod, RunFlowRequest,
    RunFlowResponse, SessionId, SessionSummary, rpc,
};
use futures::{future::BoxFuture, stream::BoxStream};

pub use http::HttpTransport;
pub use session::{
    AppliedUpdates, ReconcileError, RefreshOutcome, SessionClient, SessionClientError, SessionState,
};
#[cfg(unix)]
pub use unix::UnixTransport;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid endpoint: {0}")]
    InvalidEndpoint(String),
    #[error("transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTP transport failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("daemon returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("daemon closed the connection before responding")]
    Closed,
    #[error("invalid daemon response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
    #[error("invalid session event stream: {0}")]
    InvalidEventStream(String),
}

impl TransportError {
    fn is_retryable(&self) -> bool {
        match self {
            Self::Io(_) | Self::Http(_) | Self::Closed => true,
            Self::HttpStatus { status, .. } => matches!(*status, 408 | 425 | 429) || *status >= 500,
            Self::InvalidEndpoint(_) | Self::InvalidResponse(_) | Self::InvalidEventStream(_) => {
                false
            }
        }
    }
}

pub type SessionEventStream = BoxStream<'static, Result<ProjectionEventEnvelope, TransportError>>;

pub trait RpcTransport: Send + Sync {
    fn send(
        &self,
        request: JsonRpcRequest,
    ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>>;

    fn session_events(
        &self,
        _session_id: SessionId,
        _after_cursor: EventCursor,
    ) -> BoxFuture<'_, Result<Option<SessionEventStream>, TransportError>> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error(transparent)]
    Rpc(#[from] atman_proto::JsonRpcError),
    #[error("could not encode {method} request: {source}")]
    Encode {
        method: &'static str,
        source: serde_json::Error,
    },
    #[error("daemon response correlation mismatch: expected {expected}, received {received}")]
    Correlation { expected: u64, received: String },
    #[error("daemon protocol version {daemon} is incompatible with client version {client}")]
    ProtocolVersion { client: u32, daemon: u32 },
    #[error("daemon snapshot schema version {daemon} is incompatible with client version {client}")]
    SnapshotSchemaVersion { client: u32, daemon: u32 },
    #[error("daemon event schema version {daemon} is incompatible with client version {client}")]
    EventSchemaVersion { client: u32, daemon: u32 },
    #[error("daemon does not support {method} revision {revision}")]
    UnsupportedMethod { method: &'static str, revision: u32 },
}

impl ClientError {
    fn is_retryable(&self) -> bool {
        matches!(self, Self::Transport(error) if error.is_retryable())
    }
}

#[derive(Debug, Clone)]
pub struct ClientIdentity {
    pub id: ClientId,
    pub name: String,
    pub version: String,
}

impl ClientIdentity {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            id: ClientId::now(),
            name: name.into(),
            version: version.into(),
        }
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    transport: Arc<dyn RpcTransport>,
    next_request_id: AtomicU64,
    identity: ClientIdentity,
    capabilities: std::sync::RwLock<CapabilitiesResponse>,
    handshake_lock: tokio::sync::Mutex<()>,
}

impl Client {
    pub async fn connect<T>(transport: T, identity: ClientIdentity) -> Result<Self, ClientError>
    where
        T: RpcTransport + 'static,
    {
        let transport: Arc<dyn RpcTransport> = Arc::new(transport);
        let request_id = 1;
        let capabilities = invoke::<rpc::DaemonCapabilities>(
            transport.as_ref(),
            request_id,
            &CapabilitiesRequest {
                client_id: Some(identity.id.clone()),
                client_name: Some(identity.name.clone()),
                client_version: Some(identity.version.clone()),
                protocol_version: Some(PROTOCOL_VERSION),
            },
        )
        .await?;
        validate_capabilities(&capabilities)?;
        Ok(Self {
            inner: Arc::new(ClientInner {
                transport,
                next_request_id: AtomicU64::new(request_id + 1),
                identity,
                capabilities: std::sync::RwLock::new(capabilities),
                handshake_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    pub fn capabilities(&self) -> CapabilitiesResponse {
        self.inner.capabilities.read().unwrap().clone()
    }

    pub async fn refresh_capabilities(&self) -> Result<CapabilitiesResponse, ClientError> {
        let _guard = self.inner.handshake_lock.lock().await;
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let capabilities = invoke::<rpc::DaemonCapabilities>(
            self.inner.transport.as_ref(),
            request_id,
            &CapabilitiesRequest {
                client_id: Some(self.inner.identity.id.clone()),
                client_name: Some(self.inner.identity.name.clone()),
                client_version: Some(self.inner.identity.version.clone()),
                protocol_version: Some(PROTOCOL_VERSION),
            },
        )
        .await?;
        validate_capabilities(&capabilities)?;
        *self.inner.capabilities.write().unwrap() = capabilities.clone();
        Ok(capabilities)
    }

    pub async fn call<M: RpcMethod>(&self, params: &M::Params) -> Result<M::Output, ClientError> {
        if !self.capabilities().supports::<M>() {
            return Err(ClientError::UnsupportedMethod {
                method: M::NAME,
                revision: M::REVISION,
            });
        }
        let request_id = self.inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        invoke::<M>(self.inner.transport.as_ref(), request_id, params).await
    }

    pub(crate) async fn command<M: RpcMethod>(
        &self,
        params: &M::Params,
    ) -> Result<M::Output, ClientError> {
        debug_assert_eq!(M::KIND, RpcKind::Command);
        match self.call::<M>(params).await {
            Err(error) if error.is_retryable() => self.call::<M>(params).await,
            result => result,
        }
    }

    pub async fn attach_session(
        &self,
        session_id: atman_proto::SessionId,
    ) -> Result<SessionClient, SessionClientError> {
        SessionClient::attach(self.clone(), session_id).await
    }

    pub async fn create_session(
        &self,
        project_root: Option<String>,
        title: Option<String>,
    ) -> Result<SessionClient, SessionClientError> {
        let snapshot = self
            .command::<rpc::CreateSession>(&CreateSessionRequest {
                request_id: Some(RequestId::now()),
                project_root,
                title,
            })
            .await?;
        SessionClient::from_snapshot(self.clone(), snapshot)
    }

    pub async fn list_sessions(
        &self,
        project_root: Option<String>,
        search: Option<String>,
        limit: Option<usize>,
    ) -> Result<Vec<SessionSummary>, ClientError> {
        self.call::<rpc::ListSessions>(&ListSessionsRequest {
            project_root,
            search,
            limit,
        })
        .await
    }

    pub async fn list_projects(
        &self,
        search: Option<String>,
        limit: Option<usize>,
    ) -> Result<ListProjectsResponse, ClientError> {
        self.call::<rpc::ListProjects>(&ListProjectsRequest { search, limit })
            .await
    }

    pub async fn close_session(
        &self,
        session_id: SessionId,
    ) -> Result<CloseSessionResponse, ClientError> {
        self.command::<rpc::CloseSession>(&CloseSessionRequest {
            request_id: Some(RequestId::now()),
            session_id,
        })
        .await
    }

    pub async fn delete_session(
        &self,
        session_id: SessionId,
    ) -> Result<DeleteSessionResponse, ClientError> {
        self.command::<rpc::DeleteSession>(&DeleteSessionRequest {
            request_id: Some(RequestId::now()),
            session_id,
        })
        .await
    }

    pub async fn run_flow(
        &self,
        flow_path: impl Into<String>,
        args: serde_json::Map<String, serde_json::Value>,
        reasoning: Option<String>,
        images: Vec<atman_proto::InlineImage>,
    ) -> Result<RunFlowResponse, ClientError> {
        self.command::<rpc::RunFlow>(&RunFlowRequest {
            request_id: Some(RequestId::now()),
            flow_path: flow_path.into(),
            args,
            reasoning,
            images,
        })
        .await
    }

    pub async fn session_events(
        &self,
        session_id: SessionId,
        after_cursor: EventCursor,
    ) -> Result<Option<SessionEventStream>, TransportError> {
        self.inner
            .transport
            .session_events(session_id, after_cursor)
            .await
    }
}

fn validate_capabilities(capabilities: &CapabilitiesResponse) -> Result<(), ClientError> {
    if capabilities.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::ProtocolVersion {
            client: PROTOCOL_VERSION,
            daemon: capabilities.protocol_version,
        });
    }
    if capabilities.supports::<rpc::GetSessionSnapshot>()
        && capabilities.snapshot_schema_version != atman_proto::SNAPSHOT_SCHEMA_VERSION
    {
        return Err(ClientError::SnapshotSchemaVersion {
            client: atman_proto::SNAPSHOT_SCHEMA_VERSION,
            daemon: capabilities.snapshot_schema_version,
        });
    }
    if capabilities.supports::<rpc::GetSessionUpdates>()
        && capabilities.event_schema_version != atman_proto::PROJECTION_EVENT_SCHEMA_VERSION
    {
        return Err(ClientError::EventSchemaVersion {
            client: atman_proto::PROJECTION_EVENT_SCHEMA_VERSION,
            daemon: capabilities.event_schema_version,
        });
    }
    Ok(())
}

async fn invoke<M: RpcMethod>(
    transport: &dyn RpcTransport,
    request_id: u64,
    params: &M::Params,
) -> Result<M::Output, ClientError> {
    let request = JsonRpcRequest::for_method::<M>(request_id, params).map_err(|source| {
        ClientError::Encode {
            method: M::NAME,
            source,
        }
    })?;
    let response = transport.send(request).await?;
    let received = response
        .id
        .as_ref()
        .map(serde_json::Value::to_string)
        .unwrap_or_else(|| "notification".into());
    if response.id.as_ref().and_then(serde_json::Value::as_u64) != Some(request_id) {
        return Err(ClientError::Correlation {
            expected: request_id,
            received,
        });
    }
    Ok(response.into_method_output::<M>()?)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use atman_proto::{
        DaemonGeneration, EmptyParams, FlowRunId, JsonRpcResponse, MethodCapability,
        PROJECTION_EVENT_SCHEMA_VERSION, PingResponse, ProtocolLimits, Revision, RpcKind,
        SNAPSHOT_SCHEMA_VERSION, method_descriptor, methods,
    };

    use super::*;

    struct FakeTransport {
        requests: Mutex<Vec<JsonRpcRequest>>,
        protocol_version: u32,
        snapshot_schema_version: u32,
        event_schema_version: u32,
    }

    struct RetryRunFlowTransport {
        attempts: AtomicUsize,
        requests: Arc<Mutex<Vec<JsonRpcRequest>>>,
    }

    impl RpcTransport for RetryRunFlowTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request.clone());
                match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => {
                        let methods = [
                            method_descriptor::<rpc::DaemonCapabilities>(),
                            method_descriptor::<rpc::RunFlow>(),
                        ]
                        .into_iter()
                        .map(|method| MethodCapability {
                            name: method.name.into(),
                            kind: method.kind,
                            revision: method.revision,
                        })
                        .collect();
                        Ok(JsonRpcResponse::ok(
                            request.id,
                            serde_json::to_value(CapabilitiesResponse {
                                protocol_version: PROTOCOL_VERSION,
                                daemon_version: "test".into(),
                                daemon_generation: DaemonGeneration("generation".into()),
                                snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
                                event_schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
                                methods,
                                limits: ProtocolLimits {
                                    max_event_page_size: 100,
                                    subscriber_buffer: 256,
                                },
                            })?,
                        ))
                    }
                    methods::RUN_FLOW if self.attempts.fetch_add(1, Ordering::Relaxed) == 0 => {
                        Err(TransportError::Closed)
                    }
                    methods::RUN_FLOW => Ok(JsonRpcResponse::ok(
                        request.id,
                        serde_json::to_value(RunFlowResponse {
                            session_id: SessionId(uuid::Uuid::nil()),
                            run_id: FlowRunId(uuid::Uuid::nil()),
                            revision: Revision(3),
                            cursor: EventCursor(4),
                        })?,
                    )),
                    method => panic!("unexpected method {method}"),
                }
            })
        }
    }

    impl FakeTransport {
        fn new(protocol_version: u32) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                protocol_version,
                snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
                event_schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
            }
        }
    }

    impl RpcTransport for FakeTransport {
        fn send(
            &self,
            request: JsonRpcRequest,
        ) -> BoxFuture<'_, Result<JsonRpcResponse, TransportError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request.clone());
                let result = match request.method.as_str() {
                    methods::DAEMON_CAPABILITIES => serde_json::to_value(CapabilitiesResponse {
                        protocol_version: self.protocol_version,
                        daemon_version: "test".into(),
                        daemon_generation: DaemonGeneration("generation".into()),
                        snapshot_schema_version: self.snapshot_schema_version,
                        event_schema_version: self.event_schema_version,
                        methods: vec![
                            method_descriptor::<rpc::DaemonCapabilities>(),
                            method_descriptor::<rpc::Ping>(),
                            method_descriptor::<rpc::GetSessionSnapshot>(),
                            method_descriptor::<rpc::GetSessionUpdates>(),
                        ]
                        .into_iter()
                        .map(|method| MethodCapability {
                            name: method.name.into(),
                            kind: method.kind,
                            revision: method.revision,
                        })
                        .collect(),
                        limits: ProtocolLimits {
                            max_event_page_size: 100,
                            subscriber_buffer: 256,
                        },
                    })?,
                    methods::PING => serde_json::to_value(PingResponse {
                        pong: true,
                        version: "test".into(),
                    })?,
                    _ => unreachable!(),
                };
                Ok(JsonRpcResponse::ok(request.id, result))
            })
        }
    }

    #[tokio::test]
    async fn handshake_gates_typed_calls_and_correlates_requests() {
        let client = Client::connect(
            FakeTransport::new(PROTOCOL_VERSION),
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();
        let pong = client.call::<rpc::Ping>(&EmptyParams {}).await.unwrap();
        assert!(pong.pong);
        assert!(client.capabilities().supports::<rpc::Ping>());
    }

    #[tokio::test]
    async fn handshake_rejects_incompatible_protocol_versions() {
        let error = Client::connect(
            FakeTransport::new(PROTOCOL_VERSION + 1),
            ClientIdentity::new("test", "1"),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(error, ClientError::ProtocolVersion { .. }));
    }

    #[tokio::test]
    async fn handshake_rejects_incompatible_projection_schema_versions() {
        let snapshot_error = Client::connect(
            FakeTransport {
                snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION + 1,
                ..FakeTransport::new(PROTOCOL_VERSION)
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            snapshot_error,
            ClientError::SnapshotSchemaVersion { .. }
        ));

        let event_error = Client::connect(
            FakeTransport {
                event_schema_version: PROJECTION_EVENT_SCHEMA_VERSION + 1,
                ..FakeTransport::new(PROTOCOL_VERSION)
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            event_error,
            ClientError::EventSchemaVersion { .. }
        ));
    }

    #[tokio::test]
    async fn flow_start_retry_reuses_the_business_request_id() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let client = Client::connect(
            RetryRunFlowTransport {
                attempts: AtomicUsize::new(0),
                requests: requests.clone(),
            },
            ClientIdentity::new("test", "1"),
        )
        .await
        .unwrap();

        let response = client
            .run_flow("agent.at", serde_json::Map::new(), None, Vec::new())
            .await
            .unwrap();
        assert_eq!(response.revision, Revision(3));
        assert_eq!(response.cursor, EventCursor(4));

        let request_ids = requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.method == methods::RUN_FLOW)
            .map(|request| request.params.as_ref().unwrap()["request_id"].clone())
            .collect::<Vec<_>>();
        assert_eq!(request_ids.len(), 2);
        assert_eq!(request_ids[0], request_ids[1]);
        assert!(!request_ids[0].is_null());
    }

    #[test]
    fn rpc_kinds_remain_part_of_capability_negotiation() {
        assert_eq!(method_descriptor::<rpc::Ping>().kind, RpcKind::Query);
    }
}
