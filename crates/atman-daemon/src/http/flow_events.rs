use std::{io, num::NonZeroUsize, sync::Arc};

use atman_proto::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse,
    flows::{FlowAddress, FlowCursor, ReadFlowEventsRequest},
    methods::Method,
    responses::{FlowEventBatch, FlowEventPage},
};
use atman_runtime::{
    flow_runtime::catalog::SessionCatalog,
    flow_store::position::{FlowBatch, FlowPage, FlowPosition},
};
use axum::{
    extract::{Extension, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response, sse::Event},
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{EventStream, HttpState, ListenerShutdown, SseQuery};
use crate::{
    DaemonState, flow_service,
    host_gateway::{self, EventProjection, FORWARDED_HEADER},
};

pub(super) async fn subscribe(
    daemon: &DaemonState,
    principal: &str,
    query: SseQuery,
    instance_id: Uuid,
    headers: &HeaderMap,
) -> Result<EventStream, (StatusCode, String)> {
    let after = resume_cursor(&query, headers)?;
    let worker = flow_service::worker(
        daemon,
        FlowAddress {
            session_id: query.session_id,
            instance_id,
        },
        principal,
    )
    .await
    .map_err(|error| (StatusCode::BAD_REQUEST, error.message))?;
    // Complete registration and cursor validation before sending successful SSE headers.
    let mut updates = worker
        .subscribe_updates(after, NonZeroUsize::new(32).unwrap())
        .await
        .map_err(read_error)?;
    Ok(async_stream::try_stream! {
        while let Some(page) = updates.next().await {
            for batch in page?.batches {
                let id = flow_service::events::cursor(batch.receipt.require_durable()?).to_string();
                yield Event::default().event("flow_update").id(id)
                    .json_data(batch).map_err(io::Error::other)?;
            }
        }
    }
    .boxed())
}

fn resume_cursor(
    query: &SseQuery,
    headers: &HeaderMap,
) -> Result<Option<FlowPosition>, (StatusCode, String)> {
    if query.since_seq.is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "instance events require a complete flow cursor, not since_seq".into(),
        ));
    }
    // EventSource reuses its initial URL on reconnect; the later header must supersede that URL's starting cursor.
    let encoded = headers
        .get("last-event-id")
        .map(|value| value.to_str())
        .transpose()
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?
        .or(query.after.as_deref());
    encoded
        .map(str::parse::<FlowCursor>)
        .transpose()
        .map(|after| after.map(|cursor| flow_service::events::receipt(cursor).into()))
        .map_err(|error| (StatusCode::BAD_REQUEST, error.into()))
}

fn read_error(error: io::Error) -> (StatusCode, String) {
    let status = if error.kind() == io::ErrorKind::InvalidInput {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, error.to_string())
}

fn receipt_batch(batch: FlowBatch<()>) -> io::Result<FlowEventBatch> {
    Ok(FlowEventBatch {
        receipt: flow_service::events::cursor(batch.receipt.require_durable()?),
        facts: vec![],
    })
}

fn receipt_page(page: FlowPage<()>) -> io::Result<FlowEventPage> {
    Ok(FlowEventPage {
        batches: page
            .batches
            .into_iter()
            .map(receipt_batch)
            .collect::<io::Result<_>>()?,
        next: page
            .next
            .as_ref()
            .map(|position| position.require_durable().map(flow_service::events::cursor))
            .transpose()?,
        has_more: page.has_more,
    })
}

async fn receipt_stream(
    daemon: &DaemonState,
    address: FlowAddress,
    after: Option<FlowPosition>,
) -> io::Result<EventStream> {
    let flow = flow_service::core_address(address);
    let host = daemon.runtime_host();
    let limit = NonZeroUsize::new(32).unwrap();
    let mut updates = if let Some(worker) = host
        .session(flow.session_id.0)
        .ok()
        .and_then(|session| session.loaded_flow(&flow.instance_id))
    {
        worker.subscribe_event_receipts(after, limit).await?
    } else {
        // A recorded Flow can return a bounded catch-up page without starting an owner.
        let page = SessionCatalog::new(Some(host), Some(daemon.data_dir().to_owned()))
            .event_receipts_page(&flow, after, limit)
            .await?;
        futures::stream::iter([Ok(page)]).boxed()
    };
    Ok(async_stream::try_stream! {
        while let Some(page) = updates.next().await {
            for batch in page?.batches {
                let batch = receipt_batch(batch)?;
                yield Event::default().event("flow_update").id(batch.receipt.to_string())
                    .json_data(batch).map_err(io::Error::other)?;
            }
        }
    }
    .boxed())
}

pub(crate) async fn receipt_response(
    daemon: &Arc<DaemonState>,
    query: SseQuery,
    headers: &HeaderMap,
    shutdown: CancellationToken,
) -> Result<Response, (StatusCode, String)> {
    let instance_id = query.instance_id.filter(|id| !id.is_nil()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "receipt observation requires a Flow".into(),
        )
    })?;
    if query.session_id.0.is_nil() {
        return Err((
            StatusCode::BAD_REQUEST,
            "receipt observation requires a Session".into(),
        ));
    }
    let after = resume_cursor(&query, headers)?;
    if let Some(response) = host_gateway::events(
        daemon,
        &query,
        headers,
        shutdown.clone(),
        EventProjection::Receipts,
    )
    .await?
    {
        return Ok(response);
    }
    let address = FlowAddress {
        session_id: query.session_id,
        instance_id,
    };
    let stream = receipt_stream(daemon, address, after)
        .await
        .map_err(read_error)?;
    Ok(super::event_stream(stream.take_until(shutdown.cancelled_owned()).boxed()).into_response())
}

pub(super) async fn sse_receipts(
    State(state): State<Arc<HttpState>>,
    listener: Option<Extension<ListenerShutdown>>,
    Query(query): Query<SseQuery>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let shutdown = listener.map_or_else(
        || state.daemon.shutdown_signal(),
        |Extension(listener)| listener.0,
    );
    receipt_response(&state.daemon, query, &headers, shutdown).await
}

pub(crate) async fn receipt_rpc(
    daemon: &DaemonState,
    request: JsonRpcRequest,
    forwarded: bool,
) -> JsonRpcResponse {
    let id = request.id.clone();
    if request.jsonrpc != atman_proto::JSONRPC_VERSION
        || request.method != Method::FlowEvents.name()
    {
        return JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("expected a Flow receipt request"),
        );
    }
    let Ok(target) = serde_json::from_value::<ReadFlowEventsRequest>(
        request.params.clone().unwrap_or(serde_json::Value::Null),
    ) else {
        return JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("invalid Flow receipt request"),
        );
    };
    let Some(limit) = NonZeroUsize::new(target.limit).filter(|limit| limit.get() <= 128) else {
        return JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("receipt page limit must be between 1 and 128"),
        );
    };
    if target.flow.session_id.0.is_nil() || target.flow.instance_id.is_nil() {
        return JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("receipt page requires a Flow"),
        );
    }
    let host = daemon.runtime_host();
    if forwarded {
        if !host.sessions().contains(&target.flow.session_id.0) {
            return JsonRpcResponse::err(
                id,
                JsonRpcError::application("execution owner changed; refresh host discovery"),
            );
        }
    } else {
        match host_gateway::event_receipts_page(daemon, target.flow.session_id.0, &request).await {
            Ok(Some(response)) => return response,
            Ok(None) => {}
            Err(error) => return JsonRpcResponse::err(id, error),
        }
    }
    let flow = flow_service::core_address(target.flow);
    let after = target
        .after
        .map(|cursor| flow_service::events::receipt(cursor).into());
    let result = SessionCatalog::new(Some(host), Some(daemon.data_dir().to_owned()))
        .event_receipts_page(&flow, after, limit)
        .await
        .and_then(receipt_page)
        .and_then(|page| serde_json::to_value(page).map_err(io::Error::other));
    match result {
        Ok(page) => JsonRpcResponse::ok(id, page),
        Err(error) if error.kind() == io::ErrorKind::InvalidInput => JsonRpcResponse::err(
            id,
            JsonRpcError::invalid_params("receipt is outside the Flow journal"),
        ),
        Err(_) => JsonRpcResponse::err(
            id,
            JsonRpcError::application("Flow receipts are unavailable"),
        ),
    }
}

pub(super) async fn rpc_receipts(
    State(state): State<Arc<HttpState>>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    Json(
        receipt_rpc(
            &state.daemon,
            request,
            headers.contains_key(FORWARDED_HEADER),
        )
        .await,
    )
}
