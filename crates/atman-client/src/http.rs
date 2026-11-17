//! Authenticated HTTP transport; write failures retain an unknown outcome for the caller.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use atman_proto::{
    JsonRpcRequest, JsonRpcResponse,
    capabilities::{CapabilitiesRequest, CapabilitiesResponse},
    client::{CapabilitySet, decode_response},
    flows::{
        ControlFlowRequest, FlowRequest, InputStatusRequest, InvokeFlowRequest, ListFlowsRequest,
        ReadFlowEventsRequest, SessionRequest,
    },
    methods::Method,
    responses::{
        ControlFlowResponse, FlowEventBatch, FlowEventPage, FlowListPage, InputStatusResponse,
        InvokeFlowResponse, SessionView,
    },
};
use eventsource_stream::Eventsource;
use futures::{StreamExt, TryStreamExt};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::OnceCell;

use crate::{ClientError, EventSubscription, FlowObservation, Result, flow_identity, limit};

const RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// HTTP credentials stay in the Authorization header and are excluded from Debug output.
#[derive(Clone)]
pub struct HttpClient {
    client: reqwest::Client,
    rpc: reqwest::Url,
    events: reqwest::Url,
    token: Arc<str>,
    next_id: Arc<AtomicU64>,
    capabilities: Arc<OnceCell<CapabilitiesResponse>>,
}

impl HttpClient {
    pub fn new(endpoint: &str, token: impl Into<Arc<str>>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Self::with_http_client(client, endpoint, token)
    }

    pub fn with_http_client(
        client: reqwest::Client,
        endpoint: &str,
        token: impl Into<Arc<str>>,
    ) -> Result<Self> {
        let mut base = reqwest::Url::parse(endpoint)
            .map_err(|_| ClientError::InvalidEndpoint("expected an absolute HTTP URL"))?;
        if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
            return Err(ClientError::InvalidEndpoint(
                "expected an absolute HTTP URL",
            ));
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::InvalidEndpoint(
                "credentials, queries and fragments must not appear in the endpoint",
            ));
        }
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let rpc = base
            .join("rpc")
            .map_err(|_| ClientError::InvalidEndpoint("invalid RPC URL"))?;
        let events = base
            .join("events")
            .map_err(|_| ClientError::InvalidEndpoint("invalid event URL"))?;
        Ok(Self {
            client,
            rpc,
            events,
            token: token.into(),
            next_id: Arc::new(AtomicU64::new(1)),
            capabilities: Arc::new(OnceCell::new()),
        })
    }

    pub async fn capabilities(&self, request: CapabilitiesRequest) -> Result<CapabilitiesResponse> {
        let response: CapabilitiesResponse = self.call(Method::Capabilities, &request).await?;
        CapabilitySet::new(response.clone()).require(&request)?;
        let _ = self.capabilities.set(response.clone());
        Ok(response)
    }

    pub async fn open_session(&self, request: SessionRequest) -> Result<SessionView> {
        if request.session_id.0.is_nil() {
            return Err(ClientError::InvalidRequest(
                "session identity must not be nil",
            ));
        }
        let response: SessionView = self.typed(Method::SessionOpen, &request).await?;
        if response.session_id != request.session_id
            || response.catalog.journal_id != request.session_id.0
        {
            return Err(ClientError::ResponseSource);
        }
        Ok(response)
    }

    pub async fn list_flows(&self, request: ListFlowsRequest) -> Result<FlowListPage> {
        limit(request.limit, 50)?;
        if request.session_id.0.is_nil() {
            return Err(ClientError::InvalidRequest(
                "session identity must not be nil",
            ));
        }
        let response: FlowListPage = self.typed(Method::SessionFlows, &request).await?;
        if response.catalog.journal_id != request.session_id.0
            || response
                .instances
                .iter()
                .any(|row| row.flow.session_id != request.session_id)
            || response.instances.len() > request.limit
        {
            return Err(ClientError::ResponseSource);
        }
        Ok(response)
    }

    pub async fn get_flow(&self, request: FlowRequest) -> Result<FlowObservation> {
        flow_identity(&request.flow)?;
        let response: FlowObservation = self.typed(Method::FlowGet, &request).await?;
        if response.flow != request.flow || response.cursor.journal_id != request.flow.instance_id {
            return Err(ClientError::ResponseSource);
        }
        Ok(response)
    }

    pub async fn invoke(&self, request: InvokeFlowRequest) -> Result<InvokeFlowResponse> {
        flow_identity(&request.flow)?;
        if request.request_id.is_nil() {
            return Err(ClientError::InvalidRequest(
                "request identity must not be nil",
            ));
        }
        let response: InvokeFlowResponse = self.typed(Method::FlowInvoke, &request).await?;
        if response.acceptance.receipt.journal_id != request.flow.instance_id {
            return Err(ClientError::ResponseSource);
        }
        Ok(response)
    }

    pub async fn input_status(&self, request: InputStatusRequest) -> Result<InputStatusResponse> {
        flow_identity(&request.flow)?;
        if request.request_id.is_nil() {
            return Err(ClientError::InvalidRequest(
                "request identity must not be nil",
            ));
        }
        let response: InputStatusResponse = self.typed(Method::FlowInputStatus, &request).await?;
        if response.flow != request.flow || response.request_id != request.request_id {
            return Err(ClientError::ResponseSource);
        }
        if let Some(observation) = &response.observation {
            if observation.accepted.journal_id != request.flow.instance_id
                || observation.observed_receipt.journal_id != request.flow.instance_id
            {
                return Err(ClientError::ResponseSource);
            }
            match &observation.input {
                atman_proto::responses::InputConsumption::Committed { receipt, lease } => {
                    if receipt.journal_id != request.flow.instance_id || lease.flow != request.flow
                    {
                        return Err(ClientError::ResponseSource);
                    }
                }
                atman_proto::responses::InputConsumption::Cancelled { receipt } => {
                    if receipt.journal_id != request.flow.instance_id {
                        return Err(ClientError::ResponseSource);
                    }
                }
                atman_proto::responses::InputConsumption::Pending => (),
            }
        }
        Ok(response)
    }

    pub async fn control(&self, request: ControlFlowRequest) -> Result<ControlFlowResponse> {
        flow_identity(&request.flow)?;
        if request.request_id.is_nil() {
            return Err(ClientError::InvalidRequest(
                "request identity must not be nil",
            ));
        }
        let response: ControlFlowResponse = self.typed(Method::FlowControl, &request).await?;
        if response.request_id != request.request_id
            || response.receipt.journal_id != request.flow.instance_id
        {
            return Err(ClientError::ResponseSource);
        }
        Ok(response)
    }

    pub async fn events(&self, request: ReadFlowEventsRequest) -> Result<FlowEventPage> {
        flow_identity(&request.flow)?;
        limit(request.limit, 128)?;
        let page: FlowEventPage = self.typed(Method::FlowEvents, &request).await?;
        crate::subscription::validate_page(&request.flow, &page)?;
        Ok(page)
    }

    /// Subscribe with a retained durable cursor. Reconnect explicitly with `subscription.last()`.
    ///
    /// No write is retried when this observer disconnects. A restarted host must be renegotiated by constructing a client for the newly authenticated endpoint.
    pub async fn subscribe_events(
        &self,
        request: ReadFlowEventsRequest,
    ) -> Result<EventSubscription> {
        flow_identity(&request.flow)?;
        limit(request.limit, 128)?;
        self.require(Method::FlowEvents).await?;
        let mut url = self.events.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("session_id", &request.flow.session_id.to_string());
            query.append_pair("instance_id", &request.flow.instance_id.to_string());
            if let Some(after) = &request.after {
                query.append_pair("after", &after.to_string());
            }
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(self.token.as_ref())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .send()
            .await?
            .error_for_status()?;
        if !response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|value| value.trim() == "text/event-stream")
            })
        {
            return Err(ClientError::EventStream(
                "expected text/event-stream".into(),
            ));
        }
        let input = response.bytes_stream();
        // Bound the parser's pending event before it can accumulate an oversized data field.
        let input = futures::stream::try_unfold(
            (Box::pin(input), 0usize, 0usize),
            |(mut input, mut bytes, mut line)| async move {
                let Some(chunk) = input.next().await else {
                    return Ok(None);
                };
                let chunk = chunk.map_err(ClientError::from)?;
                for byte in chunk.iter().copied() {
                    bytes += 1;
                    if bytes > RESPONSE_BYTES {
                        return Err(ClientError::EventStream(
                            "event byte budget exceeded".into(),
                        ));
                    }
                    if byte == b'\n' {
                        if line == 0 {
                            bytes = 0;
                        }
                        line = 0;
                    } else if byte != b'\r' {
                        line += 1;
                    }
                }
                Ok(Some((chunk, (input, bytes, line))))
            },
        );
        let events = input
            .eventsource()
            .map_err(|error| ClientError::EventStream(error.to_string()));
        let stream = events.try_filter_map(|event| async move {
            if event.event != "flow_update" {
                return Ok(None);
            }
            let batch: FlowEventBatch =
                serde_json::from_str(&event.data).map_err(ClientError::from)?;
            if event.id != batch.receipt.to_string() {
                return Err(ClientError::EventStream(
                    "event ID differs from its durable receipt".into(),
                ));
            }
            let next = Some(batch.receipt.clone());
            Ok(Some(FlowEventPage {
                batches: vec![batch],
                next,
                has_more: false,
            }))
        });
        let stream = stream.boxed();
        Ok(EventSubscription::new(request.flow, request.after, stream))
    }

    async fn typed<P: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        params: &P,
    ) -> Result<T> {
        self.require(method).await?;
        self.call(method, params).await
    }

    async fn require(&self, method: Method) -> Result<()> {
        let capabilities = self
            .capabilities
            .get_or_try_init(|| async {
                self.call::<_, CapabilitiesResponse>(
                    Method::Capabilities,
                    &CapabilitiesRequest::default(),
                )
                .await
            })
            .await?;
        let request = CapabilitiesRequest {
            protocol_revision: Some(atman_proto::capabilities::PROTOCOL_REVISION),
            schema_revision: Some(atman_proto::capabilities::SCHEMA_REVISION),
            methods: [(method.name().to_owned(), method.capability().revision)]
                .into_iter()
                .collect(),
        };
        CapabilitySet::new(capabilities.clone()).require(&request)?;
        Ok(())
    }

    async fn call<P: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        params: &P,
    ) -> Result<T> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if id == u64::MAX {
            return Err(ClientError::InvalidRequest(
                "RPC request identity exhausted",
            ));
        }
        let id = serde_json::Value::from(id);
        let request = JsonRpcRequest::new(id.clone(), method.name(), serde_json::to_value(params)?);
        let response = self
            .client
            .post(self.rpc.clone())
            .bearer_auth(self.token.as_ref())
            .timeout(Duration::from_secs(30))
            .json(&request)
            .send()
            .await?
            .error_for_status()?;
        let mut chunks = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            if body.len().saturating_add(chunk.len()) > RESPONSE_BYTES {
                return Err(ClientError::InvalidRequest(
                    "RPC response byte budget exceeded",
                ));
            }
            body.extend_from_slice(&chunk);
        }
        let response: JsonRpcResponse = serde_json::from_slice(&body)?;
        Ok(decode_response(response, Some(&id))?)
    }
}
