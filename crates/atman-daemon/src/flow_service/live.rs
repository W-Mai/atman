//! Authenticated display snapshots from an existing execution owner.

use std::time::Duration;

use atman_proto::{
    JsonRpcError,
    conversation::ConversationItemId,
    live::{
        DraftText, FlowDraftCall, FlowDraftEdition, FlowDraftHandoff, FlowDraftHandoffPart,
        FlowDraftId, FlowDraftPart, FlowDraftPartBody, FlowDraftSnapshot, FlowDraftWork,
        FlowFinalBoundary, ReadFlowDraftsRequest, ReadFlowDraftsResponse,
    },
    responses::InputTurn,
};
use atman_rt::TurnLease;
use atman_runtime::{
    event::AtmanUuid,
    flow_runtime::live::{
        BoundedDraftText, DraftDisplayId, DraftHandoff, DraftObservation, DraftWorkScope,
        ObservedDraftCall, ObservedDraftPart, ObservedDraftPartBody,
    },
};
use serde_json::Value;

use super::{address, core_address, wire};
use crate::DaemonState;

const MAX_RESPONSE_BYTES: usize = 512 * 1024;
const OBSERVATION_WAIT: Duration = Duration::from_secs(8);
static OBSERVATION_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(32);

pub(super) async fn read(
    state: &DaemonState,
    request: ReadFlowDraftsRequest,
) -> Result<Value, JsonRpcError> {
    let flow = core_address(request.flow.clone());
    let reader = state
        .session_service
        .get()
        .and_then(|service| service.host.session(flow.session_id.0).ok())
        .and_then(|session| session.loaded_flow(&flow.instance_id))
        .and_then(|worker| worker.observe_workbench_drafts());
    let response = match reader {
        None => ReadFlowDraftsResponse::Unavailable { flow: request.flow },
        Some(reader) => wait_for_snapshot(state, request, reader).await?,
    };
    let value = serde_json::to_value(response).map_err(|_| encode_error())?;
    if serde_json::to_vec(&value)
        .map_err(|_| encode_error())?
        .len()
        > MAX_RESPONSE_BYTES
    {
        return Err(encode_error());
    }
    Ok(value)
}

async fn wait_for_snapshot(
    state: &DaemonState,
    request: ReadFlowDraftsRequest,
    mut reader: atman_runtime::flow_runtime::live::DraftObservationReader,
) -> Result<ReadFlowDraftsResponse, JsonRpcError> {
    let mut observation = reader.snapshot();
    if let Some(known) = request.known
        && observation.as_ref().is_some_and(|snapshot| {
            wire::head(&snapshot.source.head) == request.head && edition(snapshot) == known
        })
    {
        let _slot = OBSERVATION_SLOTS
            .try_acquire()
            .map_err(|_| JsonRpcError::application("Draft observers are busy"))?;
        drop(observation);
        let shutdown = state.shutdown_signal();
        let changed = tokio::select! {
            biased;
            _ = shutdown.cancelled() => None,
            changed = tokio::time::timeout(OBSERVATION_WAIT, reader.changed()) => Some(changed),
        };
        observation = match changed {
            Some(Ok(true)) => reader.snapshot(),
            Some(Err(_)) => {
                return Ok(ReadFlowDraftsResponse::Unchanged {
                    flow: request.flow,
                    head: request.head,
                    known,
                });
            }
            _ => None,
        };
    }
    let response = match observation {
        None => ReadFlowDraftsResponse::Unavailable { flow: request.flow },
        Some(observation) if wire::head(&observation.source.head) != request.head => {
            ReadFlowDraftsResponse::HistoryChanged {
                flow: request.flow,
                head: wire::head(&observation.source.head),
            }
        }
        Some(observation) => ReadFlowDraftsResponse::Ready {
            snapshot: Box::new(project(observation).map_err(|_| encode_error())?),
        },
    };
    Ok(response)
}

fn edition(observation: &DraftObservation) -> FlowDraftEdition {
    FlowDraftEdition {
        host_generation: observation.source.host_generation,
        worker_incarnation: observation.source.worker_incarnation,
        revision: observation.revision,
    }
}

fn encode_error() -> JsonRpcError {
    JsonRpcError::internal("Draft observation could not be encoded within its limits")
}

fn project(observation: DraftObservation) -> std::io::Result<FlowDraftSnapshot> {
    let source = observation.source;
    Ok(FlowDraftSnapshot {
        flow: address(&source.flow),
        head: wire::head(&source.head),
        host_generation: source.host_generation,
        worker_incarnation: source.worker_incarnation,
        after: wire::cursor(&observation.after)?,
        revision: observation.revision,
        active: observation.active.map(turn),
        work: observation.work.map(work).transpose()?,
        calls: observation.calls.into_iter().map(call).collect(),
        final_answer: observation.final_answer.map(call),
        handoffs: observation
            .handoffs
            .into_iter()
            .map(handoff)
            .collect::<Result<_, _>>()?,
        truncated: observation.truncated,
    })
}

pub(super) fn turn(value: TurnLease<AtmanUuid>) -> InputTurn {
    InputTurn {
        flow: address(&value.flow),
        turn: value.turn.0.0,
        run: value.run.0.0,
        epoch: value.epoch,
    }
}

fn work(value: DraftWorkScope) -> std::io::Result<FlowDraftWork> {
    let latest_final = match value.latest_final {
        Some(final_answer) => Some(FlowFinalBoundary {
            publication: final_answer.publication,
            receipt: wire::cursor(&final_answer.receipt)?,
        }),
        None => None,
    };
    Ok(FlowDraftWork {
        lease: turn(value.lease),
        admission: value.admission.as_ref().map(wire::cursor).transpose()?,
        latest_final,
    })
}

fn call(value: ObservedDraftCall) -> FlowDraftCall {
    FlowDraftCall {
        id: value.id,
        lease: turn(value.lease),
        run: value.run.0.0,
        attempt: value.attempt,
        revision: value.revision,
        publication_pending: value.publication_pending,
        parts: value.parts.into_iter().map(part).collect(),
    }
}

fn display_id(value: DraftDisplayId) -> FlowDraftId {
    FlowDraftId {
        call: value.call,
        attempt: value.attempt,
        part: value.part,
    }
}

fn text(value: BoundedDraftText) -> DraftText {
    DraftText {
        text: value.text,
        truncated: value.truncated,
    }
}

fn part(value: ObservedDraftPart) -> FlowDraftPart {
    let body = match value.body {
        ObservedDraftPartBody::Text(value) => FlowDraftPartBody::Text { value: text(value) },
        ObservedDraftPartBody::Thinking(value) => {
            FlowDraftPartBody::Thinking { value: text(value) }
        }
        ObservedDraftPartBody::FinalAnswer(value) => {
            FlowDraftPartBody::FinalAnswer { value: text(value) }
        }
        ObservedDraftPartBody::Tool {
            index,
            call_id,
            name,
        } => FlowDraftPartBody::Tool {
            index,
            call_id: text(call_id),
            name: text(name),
        },
    };
    FlowDraftPart {
        id: display_id(value.id),
        body,
    }
}

fn handoff(value: DraftHandoff) -> std::io::Result<FlowDraftHandoff> {
    Ok(FlowDraftHandoff {
        lease: turn(value.lease),
        run: value.run.0.0,
        publication: value.publication,
        receipt: wire::cursor(&value.receipt)?,
        // A MessagePublished fact contributes exactly one shared message.
        item: ConversationItemId {
            commit: value.record.commit,
            fact: value.record.fact,
            item: value.record.item,
            message: 0,
        },
        parts: value
            .parts
            .into_iter()
            .map(|part| FlowDraftHandoffPart {
                live: display_id(part.live),
                published_parts: part.published_parts,
            })
            .collect(),
    })
}
