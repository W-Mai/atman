//! Authenticated preview observation without opening a Session or Flow.

use std::time::Duration;

use atman_proto::{
    JsonRpcError,
    preview::{
        FlowPreviewCompletion, FlowPreviewEdition, FlowPreviewOutcome, FlowPreviewSnapshot,
        ReadFlowPreviewRequest, ReadFlowPreviewResponse,
    },
};
use atman_rt::{Interruption, TurnOutcome};
use atman_runtime::flow_runtime::live::{CompactPreview, CompactPreviewReader};
use serde_json::Value;

use super::{address, core_address, live, wire};
use crate::DaemonState;

const MAX_RESPONSE_BYTES: usize = 8 * 1024;
const OBSERVATION_WAIT: Duration = Duration::from_secs(8);
static OBSERVATION_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(32);

pub(super) async fn read(
    state: &DaemonState,
    request: ReadFlowPreviewRequest,
) -> Result<Value, JsonRpcError> {
    let flow = core_address(request.flow.clone());
    let shutdown = state.shutdown_signal();
    let reader = (!shutdown.is_cancelled())
        .then(|| state.session_service.get())
        .flatten()
        .and_then(|service| service.host.session(flow.session_id.0).ok())
        .and_then(|session| session.loaded_flow(&flow.instance_id))
        .and_then(|worker| worker.observe_compact_preview());
    let observation = match reader {
        None => None,
        Some(reader) => wait_for_preview(reader, request.known.as_ref(), &shutdown).await?,
    };
    let response = match observation {
        None => ReadFlowPreviewResponse::Unavailable { flow: request.flow },
        Some(observation) => {
            let edition = edition(&observation);
            if request.known.as_ref() == Some(&edition) {
                ReadFlowPreviewResponse::Unchanged {
                    flow: request.flow,
                    edition,
                }
            } else {
                ReadFlowPreviewResponse::Ready {
                    snapshot: Box::new(project(observation, edition)),
                }
            }
        }
    };
    let result = serde_json::to_value(response).map_err(|_| encode_error())?;
    if serde_json::to_vec(&result)
        .map_err(|_| encode_error())?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return Err(encode_error());
    }
    Ok(result)
}

async fn wait_for_preview(
    mut reader: CompactPreviewReader,
    known: Option<&FlowPreviewEdition>,
    shutdown: &tokio_util::sync::CancellationToken,
) -> Result<Option<CompactPreview>, JsonRpcError> {
    let observation = reader.snapshot();
    if !observation
        .as_ref()
        .is_some_and(|value| known == Some(&edition(value)))
    {
        return Ok(observation);
    }
    drop(observation);
    let _slot = OBSERVATION_SLOTS
        .try_acquire()
        .map_err(|_| JsonRpcError::application("Flow preview observers are busy"))?;
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => Ok(None),
        changed = reader.changed() => Ok(changed.then(|| reader.snapshot()).flatten()),
        _ = tokio::time::sleep(OBSERVATION_WAIT) => Ok(reader.snapshot()),
    }
}

fn edition(value: &CompactPreview) -> FlowPreviewEdition {
    FlowPreviewEdition {
        host_generation: value.host_generation,
        worker_incarnation: value.worker_incarnation,
        revision: value.revision,
    }
}

fn project(value: CompactPreview, edition: FlowPreviewEdition) -> FlowPreviewSnapshot {
    let snapshot = value.snapshot;
    FlowPreviewSnapshot {
        flow: address(&snapshot.flow),
        edition,
        head: wire::head(&value.head),
        text: snapshot.text,
        active: snapshot.active.map(live::turn),
        completion: snapshot.completion.map(|completion| FlowPreviewCompletion {
            lease: live::turn(completion.lease),
            outcome: match completion.outcome {
                TurnOutcome::Completed => FlowPreviewOutcome::Completed,
                TurnOutcome::Interrupted(Interruption::Cancelled) => FlowPreviewOutcome::Cancelled,
                TurnOutcome::Interrupted(Interruption::Failed) => FlowPreviewOutcome::Failed,
                TurnOutcome::Interrupted(Interruption::HostLost) => FlowPreviewOutcome::HostLost,
            },
        }),
    }
}

fn encode_error() -> JsonRpcError {
    JsonRpcError::internal("Flow preview could not be encoded within its limits")
}
