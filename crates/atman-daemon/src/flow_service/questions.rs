//! Quick Ask initialization uses the existing Session's operator and configuration.

use std::{io, sync::Arc};

use anyhow::{Result, ensure};
use atman_proto::{JsonRpcError, history::HistorySource, questions::*};
use atman_runtime::{
    flow_runtime::question::{self as runtime, QuestionPreparation, QuestionRequest},
    flow_store::{position::TransientPosition, source_capture::SourceSelection},
    provider::ReasoningSelection,
};
use serde_json::Value;

use super::{SessionService, address, core_address, events, execution, human, wire};
use crate::DaemonState;

pub(super) fn validate_start(request: &StartQuestionRequest) -> Result<(), JsonRpcError> {
    validate_identity(&request.identity, Some(request.owner_generation))?;
    let source = &request.observed;
    let position = &source.cursor;
    if source.flow != request.identity.source
        || source.branch != request.expected_head.branch
        || source.branch.is_nil()
        || position.journal_id != source.flow.instance_id
        || position.sequence == 0
        || position.digest.len() != 64
        || !position.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask requires the exact displayed source",
        ));
    }
    if request.model.trim().is_empty() || request.model.len() > 512 {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask model must contain 1 to 512 bytes",
        ));
    }
    if let Some(effort) = &request.effort
        && (effort.len() > 128 || effort.parse::<ReasoningSelection>().is_err())
    {
        return Err(JsonRpcError::invalid_params("invalid Quick Ask effort"));
    }
    if request.user.text.trim().is_empty()
        && request.user.images.is_empty()
        && request.user.quote.is_none()
    {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask requires authored content",
        ));
    }
    Ok(())
}

pub(super) fn validate_identity(
    identity: &QuestionIdentity,
    owner: Option<uuid::Uuid>,
) -> Result<(), JsonRpcError> {
    if identity.source.session_id.0.is_nil()
        || identity.source.instance_id.is_nil()
        || identity.question_id.is_nil()
        || identity.request_id.is_nil()
        || identity.question_id == identity.request_id
        || identity.question_id == identity.source.instance_id
        || owner.is_some_and(|owner| owner.is_nil())
    {
        return Err(JsonRpcError::invalid_params("invalid Quick Ask identity"));
    }
    Ok(())
}

pub(super) async fn status(state: &DaemonState, request: QuestionStatusRequest) -> Result<Value> {
    let identity = runtime::QuestionIdentity {
        source: core_address(request.identity.source.clone()),
        question: request.identity.question_id,
        request: request.identity.request_id,
    };
    let observed = if let Some(service) = state.session_service.get() {
        service
            .host
            .observe_question_start(&identity, service.operator, request.owner_generation)
            .await?
    } else {
        runtime::QuestionStartObservation {
            owner_generation: None,
            state: if request.owner_generation.is_some() {
                runtime::QuestionStartState::Retired
            } else {
                runtime::QuestionStartState::Unseen
            },
        }
    };
    let attempt = match observed.state {
        runtime::QuestionStartState::Unseen => QuestionStartState::Unseen,
        runtime::QuestionStartState::Preparing => QuestionStartState::Preparing,
        runtime::QuestionStartState::Retryable => QuestionStartState::Retryable,
        runtime::QuestionStartState::Accepted(receipt) => QuestionStartState::Accepted {
            receipt: Box::new(start_receipt(*receipt)),
        },
        runtime::QuestionStartState::Retired => QuestionStartState::Retired,
    };
    Ok(serde_json::to_value(QuestionStatusResponse {
        identity: request.identity,
        owner_generation: observed.owner_generation,
        attempt,
    })?)
}

impl SessionService {
    pub(super) async fn start_question(
        &self,
        state: &Arc<DaemonState>,
        request: StartQuestionRequest,
    ) -> Result<Value> {
        let flow = core_address(request.identity.source.clone());
        let session = self.session(flow.session_id.0)?;
        let effort = request
            .effort
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(anyhow::Error::msg)?;
        let ask = QuestionRequest {
            id: request.identity.question_id,
            request_id: request.identity.request_id,
            source: SourceSelection {
                flow: flow.clone(),
                branch: request.observed.branch.into(),
                cursor: events::receipt(request.observed.cursor),
            },
            user: self.operator,
            content: human::authored_content(request.user)?,
            model: request.model,
            effort,
        };
        let authorized = flow.clone();
        let receipt = self
            .host
            .start_observed_question(
                request.owner_generation,
                ask,
                wire::core_head(request.expected_head),
                async {
                    let snapshot = session.snapshot().await?;
                    let worker = self.host.flow(&flow).await?;
                    ensure!(
                        !worker.is_transient() && !worker.is_stopped(),
                        "Quick Ask source is unavailable"
                    );
                    let project = worker.project().await?;
                    let descriptor = &snapshot.catalog.descriptor;
                    let config = execution::config(descriptor)?;
                    let executor = self
                        .executor(
                            state,
                            descriptor,
                            project.as_ref(),
                            execution::launcher_home(state),
                        )
                        .await?;
                    QuestionPreparation::for_installed_entry(
                        config,
                        project,
                        executor,
                        execution::YIELD_INTERVAL,
                    )
                    .await
                },
                move |candidate| {
                    if candidate != &authorized {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "Quick Ask source changed",
                        ));
                    }
                    Ok(())
                },
            )
            .await?;
        Ok(serde_json::to_value(StartQuestionResponse {
            identity: request.identity,
            owner_generation: request.owner_generation,
            receipt: start_receipt(receipt),
        })?)
    }
}

fn start_receipt(receipt: runtime::QuestionStartReceipt) -> QuestionStartReceipt {
    let position = |p: TransientPosition| QuestionPosition {
        host_generation: p.host_generation,
        flow_id: p.flow_id,
        sequence: p.sequence,
        digest: p.digest,
    };
    QuestionStartReceipt {
        source: HistorySource {
            flow: address(&receipt.source.flow),
            branch: *receipt.source.branch.as_uuid(),
            cursor: events::cursor(&receipt.source.cursor),
        },
        launch: position(receipt.launch),
        accepted: position(receipt.accepted),
        message_id: receipt.message,
    }
}
