//! Authenticated host observations and explicit Session handoff requests.

use atman_proto::{
    JsonRpcError,
    hosts::{self, HandoffSessionRequest, HostDescription, SessionHandoffOutcome},
    methods::Method,
};
use atman_runtime::flow_runtime::host as core;
use serde_json::Value;
use std::sync::Arc;

use crate::DaemonState;

pub(crate) async fn dispatch(
    state: &Arc<DaemonState>,
    method: Method,
    params: Value,
    principal: &str,
) -> Result<Value, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let host = state.runtime_host();
    match method {
        Method::HostDescribe => serde_json::to_value(HostDescription {
            generation: state.daemon_generation().to_owned(),
            runtime_generation: host.generation(),
            role: state.host_role(),
            sessions: host
                .sessions()
                .into_iter()
                .map(atman_proto::SessionId)
                .collect(),
            profile_owner: host.profile_handle().is_some(),
            presentation: state.presentation(),
        })
        .map_err(|error| JsonRpcError::internal(error.to_string())),
        Method::HostHandoff => {
            let request: HandoffSessionRequest = serde_json::from_value(params)
                .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
            if request.source_generation != host.generation()
                || request.destination_generation.is_nil()
                || request.source_generation == request.destination_generation
                || request.request_id.is_nil()
                || request.session_id.0.is_nil()
            {
                return Err(JsonRpcError::invalid_params(
                    "handoff host or request identity is invalid",
                ));
            }
            {
                let mut requests = state.handoff_requests.lock().unwrap();
                if let Some(original) = requests.get(&request.request_id) {
                    if original != &request {
                        return Err(JsonRpcError::invalid_params(
                            "handoff request identity was reused with different data",
                        ));
                    }
                } else {
                    requests.insert(request.request_id, request.clone());
                }
            }
            let outcome = state
                .request_handoff(request.clone())
                .await
                .map_err(|error| JsonRpcError::application(error.to_string()))?;
            let outcome = match outcome {
                core::SessionHandoffOutcome::Pending {
                    request_id,
                    source_generation,
                    session_id,
                    blockers,
                } => SessionHandoffOutcome::Pending {
                    request_id,
                    source_generation,
                    session_id: atman_proto::SessionId(session_id),
                    blockers: blockers.into_iter().map(blocker).collect(),
                },
                core::SessionHandoffOutcome::Released { receipt } => {
                    SessionHandoffOutcome::Installed {
                        receipt: public_receipt(receipt, request.destination_generation)?,
                    }
                }
            };
            serde_json::to_value(outcome).map_err(|error| JsonRpcError::internal(error.to_string()))
        }
        _ => Err(JsonRpcError::method_not_found(method.name())),
    }
}

fn public_receipt(
    receipt: core::SessionHandoffReceipt,
    destination_generation: uuid::Uuid,
) -> Result<hosts::SessionHandoffReceipt, JsonRpcError> {
    Ok(hosts::SessionHandoffReceipt {
        session_id: atman_proto::SessionId(receipt.session_id),
        request_id: receipt.request_id,
        source_generation: receipt.source_generation,
        destination_generation,
        catalog: crate::flow_service::events::cursor(&receipt.catalog),
        flows: receipt
            .flows
            .into_iter()
            .map(|flow| {
                Ok(hosts::HandoffFlow {
                    instance_id: flow.instance_id.0.0,
                    cursor: crate::flow_service::events::cursor(
                        flow.cursor
                            .require_durable()
                            .map_err(|error| JsonRpcError::application(error.to_string()))?,
                    ),
                    scheduling: flow.scheduling.map(|scheduling| match scheduling {
                        core::HandoffScheduling::Running => hosts::HandoffScheduling::Running,
                        core::HandoffScheduling::Stopped => hosts::HandoffScheduling::Stopped,
                        core::HandoffScheduling::Failed { error } => {
                            hosts::HandoffScheduling::Failed { error }
                        }
                    }),
                })
            })
            .collect::<Result<Vec<_>, JsonRpcError>>()?,
    })
}

fn blocker(blocker: core::HandoffBlocker) -> hosts::HandoffBlocker {
    match blocker {
        core::HandoffBlocker::Admission => hosts::HandoffBlocker::Admission,
        core::HandoffBlocker::Active { instance_id } => hosts::HandoffBlocker::Active {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Resources { instance_id } => hosts::HandoffBlocker::Resources {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Interactions { instance_id } => hosts::HandoffBlocker::Interactions {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Transient { instance_id } => hosts::HandoffBlocker::Transient {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Retention => hosts::HandoffBlocker::Retention,
        core::HandoffBlocker::SourceCapture => hosts::HandoffBlocker::SourceCapture,
        core::HandoffBlocker::ProfileAuthority => hosts::HandoffBlocker::ProfileAuthority,
        core::HandoffBlocker::Controls => hosts::HandoffBlocker::Controls,
        core::HandoffBlocker::Observation { instance_id } => hosts::HandoffBlocker::Observation {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Continuation { instance_id } => hosts::HandoffBlocker::Continuation {
            instance_id: instance_id.0.0,
        },
        core::HandoffBlocker::Unavailable { message } => {
            hosts::HandoffBlocker::Unavailable { message }
        }
    }
}
