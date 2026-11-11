use std::{io, num::NonZeroUsize};

use atman_proto::flows::{FlowAddress, FlowCursor};
use axum::{
    http::{HeaderMap, StatusCode},
    response::sse::Event,
};
use futures::StreamExt;
use uuid::Uuid;

use super::{EventStream, SseQuery};
use crate::{DaemonState, flow_service};

pub(super) async fn subscribe(
    daemon: &DaemonState,
    principal: &str,
    query: SseQuery,
    instance_id: Uuid,
    headers: &HeaderMap,
) -> Result<EventStream, (StatusCode, String)> {
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
    let after = encoded
        .map(str::parse::<FlowCursor>)
        .transpose()
        .map_err(|error| (StatusCode::BAD_REQUEST, error.into()))?;
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
        .subscribe_updates(
            after.map(|position| flow_service::events::receipt(position).into()),
            NonZeroUsize::new(32).unwrap(),
        )
        .await
        .map_err(|error| {
            let status = if error.kind() == io::ErrorKind::InvalidInput {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (status, error.to_string())
        })?;
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
