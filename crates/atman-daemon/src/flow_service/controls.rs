//! Operator controls share identities and target receipts across individual and batch requests.

use std::collections::HashSet;

use anyhow::{Result, anyhow};
use atman_proto::{JsonRpcError, flows::*};
use atman_runtime::flow_store::{
    control::{ControlAction, ControlRequest},
    inbox::SenderIdentity,
    profile::ControlBatchRequest,
};
use serde_json::Value;
use uuid::Uuid;

use super::{SessionService, core_address};

fn operator() -> SenderIdentity {
    SenderIdentity::User(Uuid::from_bytes(*b"atman-local-user"))
}

pub(super) fn validate_identity(id: Uuid) -> Result<(), JsonRpcError> {
    if id.is_nil() {
        return Err(JsonRpcError::invalid_params(
            "control identity must not be nil",
        ));
    }
    Ok(())
}

pub(super) fn validate_control(request: &ControlFlowRequest) -> Result<(), JsonRpcError> {
    for id in [
        request.request_id,
        request.flow.session_id.0,
        request.flow.instance_id,
    ] {
        validate_identity(id)?;
    }
    Ok(())
}

pub(super) fn validate_batch(request: &ControlFlowBatchRequest) -> Result<(), JsonRpcError> {
    validate_identity(request.request_id)?;
    if request.commands.is_empty() {
        return Err(JsonRpcError::invalid_params(
            "control batch must contain targets",
        ));
    }
    let mut targets = HashSet::new();
    for command in &request.commands {
        validate_control(command)?;
        if !targets.insert((command.flow.session_id.0, command.flow.instance_id)) {
            return Err(JsonRpcError::invalid_params(
                "control batch contains a duplicate target",
            ));
        }
    }
    Ok(())
}

pub(super) fn operator_control(request: ControlFlowRequest) -> ControlRequest {
    ControlRequest {
        id: request.request_id,
        sender: operator(),
        target: core_address(request.flow),
        expected_revision: request.expected_revision,
        // Shared with terminal controls, stable across restarts and distinct from peer keys.
        pause_key: Uuid::from_bytes(*b"atman-user-pause").into(),
        action: match request.action {
            FlowControl::Pause => ControlAction::Pause,
            FlowControl::Resume => ControlAction::Resume,
            FlowControl::CancelTurn => ControlAction::CancelTurn,
            FlowControl::Recover => ControlAction::Recover,
            FlowControl::Close => ControlAction::Close,
        },
    }
}

impl SessionService {
    pub(super) async fn submit_controls(&self, request: ControlFlowBatchRequest) -> Result<Value> {
        self.profile().await?;
        let record = self
            .host
            .submit_controls(
                ControlBatchRequest {
                    id: request.request_id,
                    sender: operator(),
                    commands: request.commands.into_iter().map(operator_control).collect(),
                },
                |_| Ok(()),
            )
            .await?;
        Ok(serde_json::to_value(record)?)
    }

    pub(super) async fn control_status(&self, request: ControlFlowStatusRequest) -> Result<Value> {
        let record = self
            .profile()
            .await?
            .control_batch(operator(), request.request_id)
            .await?
            .ok_or_else(|| anyhow!("control batch not found"))?;
        Ok(serde_json::to_value(record)?)
    }
}
