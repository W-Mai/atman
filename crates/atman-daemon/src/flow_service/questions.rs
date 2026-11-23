//! Quick Ask initialization uses the existing Session's operator and configuration.

use std::{io, num::NonZeroUsize, sync::Arc};

use anyhow::{Result, ensure};
use atman_proto::{
    JsonRpcError, execution::FlowExecutionPhase, flows::SubmittedUserContent,
    history::HistorySource, questions::*,
};
use atman_rt::{InstanceActivity, InstanceState, TurnPhase};
use atman_runtime::{
    flow_runtime::question::{self as runtime, QuestionPreparation, QuestionRequest},
    flow_store::{
        control::{ControlAction, ExecutionControlObservation},
        input_observation::{InputDisposition, InputObservation},
        local_operator::LocalOperator,
        position::{FlowPosition, TransientPosition},
        source_capture::SourceSelection,
    },
    provider::ReasoningSelection,
};
use serde_json::Value;
use uuid::Uuid;

use super::{SessionService, address, conversation, core_address, events, execution, human, wire};
use crate::DaemonState;

const TRANSCRIPT_WIRE_BYTES: usize = 768 * 1024;
static TRANSCRIPT_READERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

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
    validate_prompt(&request.user, &request.model, request.effort.as_deref())
}

fn validate_prompt(
    user: &SubmittedUserContent,
    model: &str,
    effort: Option<&str>,
) -> Result<(), JsonRpcError> {
    if model.trim().is_empty() || model.len() > 512 {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask model must contain 1 to 512 bytes",
        ));
    }
    if let Some(effort) = effort
        && (effort.len() > 128 || effort.parse::<ReasoningSelection>().is_err())
    {
        return Err(JsonRpcError::invalid_params("invalid Quick Ask effort"));
    }
    if user.text.trim().is_empty() && user.images.is_empty() && user.quote.is_none() {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask requires authored content",
        ));
    }
    Ok(())
}

pub(super) fn validate_identity(
    identity: &QuestionIdentity,
    owner: Option<Uuid>,
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

pub(super) fn validate_transcript(
    request: &ReadQuestionTranscriptRequest,
) -> Result<(), JsonRpcError> {
    validate_identity(&request.identity, Some(request.owner_generation))?;
    if request.host_generation.is_nil()
        || !(1..=32).contains(&request.limit)
        || request.after.as_ref().is_some_and(|position| {
            !matches_position(&request.identity, request.host_generation, position)
        })
    {
        return Err(JsonRpcError::invalid_params(
            "invalid temporary Quick Ask transcript boundary",
        ));
    }
    Ok(())
}

pub(super) fn validate_target(
    question: &QuestionTarget,
    request: Option<Uuid>,
) -> Result<(), JsonRpcError> {
    validate_identity(&question.identity, Some(question.owner_generation))?;
    if question.host_generation.is_nil()
        || request.is_some_and(|request| {
            request.is_nil()
                || request == question.identity.request_id
                || request == question.identity.question_id
        })
    {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask operation identity",
        ));
    }
    Ok(())
}

pub(super) fn validate_followup(request: &QuestionFollowupRequest) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, Some(request.followup_request_id))?;
    if !matches_position(
        &question.identity,
        question.host_generation,
        &request.observed,
    ) {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask follow-up position",
        ));
    }
    validate_prompt(&request.user, &request.model, request.effort.as_deref())
}

pub(super) fn validate_stop(request: &QuestionStopRequest) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, Some(request.stop_request_id))?;
    let lease = &request.lease;
    if lease.flow.session_id != question.identity.source.session_id
        || lease.flow.instance_id != question.identity.question_id
        || lease.turn.is_nil()
        || lease.run.is_nil()
    {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask stop requires the observed question turn",
        ));
    }
    Ok(())
}

pub(super) fn validate_discard(request: &QuestionDiscardRequest) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, Some(request.discard_request_id))?;
    if !matches_position(
        &question.identity,
        question.host_generation,
        &request.observed,
    ) {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask discard position",
        ));
    }
    Ok(())
}

fn matches_position(identity: &QuestionIdentity, host: Uuid, position: &QuestionPosition) -> bool {
    position.host_generation == host
        && position.flow_id == identity.question_id
        && position.sequence > 0
        && position.digest.len() == 64
        && position.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub(super) async fn transcript(
    state: &DaemonState,
    request: ReadQuestionTranscriptRequest,
) -> Result<Value> {
    let mut response = ReadQuestionTranscriptResponse {
        identity: request.identity,
        owner_generation: request.owner_generation,
        host_generation: request.host_generation,
        transcript: QuestionTranscriptState::Retired,
    };
    let Some(service) = state.session_service.get() else {
        return Ok(serde_json::to_value(response)?);
    };
    let Ok(_reader) = TRANSCRIPT_READERS.try_acquire() else {
        response.transcript = QuestionTranscriptState::ReadersBusy;
        return Ok(serde_json::to_value(response)?);
    };
    let shutdown = state.shutdown_signal();
    let request = runtime::QuestionTranscriptRequest {
        identity: core_identity(&response.identity),
        user: service.operator,
        owner_generation: response.owner_generation,
        host_generation: response.host_generation,
        after: request.after.map(core_position),
        limit: NonZeroUsize::new(request.limit as usize).expect("validated Quick Ask limit"),
        wait: request.wait,
    };
    let observed = tokio::select! {
        biased;
        _ = shutdown.cancelled() => runtime::QuestionTranscriptState::Retired,
        observed = service.host.read_question_transcript(request) => observed?,
    };
    response.transcript = match observed {
        runtime::QuestionTranscriptState::Unavailable => QuestionTranscriptState::Unavailable,
        runtime::QuestionTranscriptState::Retired => QuestionTranscriptState::Retired,
        runtime::QuestionTranscriptState::Capacity {
            required_at_least,
            allowed,
        } => QuestionTranscriptState::Capacity {
            required_at_least,
            allowed,
        },
        runtime::QuestionTranscriptState::Ready(page) => QuestionTranscriptState::Ready {
            batches: page
                .batches
                .into_iter()
                .map(|batch| {
                    Ok(QuestionTranscriptBatch {
                        receipt: transcript_position(batch.receipt)?,
                        items: batch
                            .facts
                            .into_iter()
                            .map(|item| conversation::project_item(item, true, None))
                            .collect(),
                    })
                })
                .collect::<Result<_>>()?,
            next: page.next.map(transcript_position).transpose()?,
            has_more: page.has_more,
        },
    };
    let bytes = serde_json::to_vec(&response)?;
    if bytes.len() > TRANSCRIPT_WIRE_BYTES {
        response.transcript = QuestionTranscriptState::Capacity {
            required_at_least: bytes.len(),
            allowed: TRANSCRIPT_WIRE_BYTES,
        };
        return Ok(serde_json::to_value(response)?);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn transcript_position(position: FlowPosition) -> Result<QuestionPosition> {
    match position {
        FlowPosition::Transient { transient } => Ok(question_position(transient)),
        FlowPosition::Durable(_) => {
            anyhow::bail!("Quick Ask transcript no longer has a temporary position")
        }
    }
}

pub(super) async fn status(state: &DaemonState, request: QuestionStatusRequest) -> Result<Value> {
    let identity = core_identity(&request.identity);
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

pub(super) async fn followup_status(
    state: &DaemonState,
    request: QuestionFollowupStatusRequest,
) -> Result<Value> {
    let followup = if let Some(service) = state.session_service.get() {
        service
            .host
            .observe_question_followup(
                &core_target(&request.question),
                LocalOperator::new(service.operator),
                request.followup_request_id,
            )
            .await?
    } else {
        runtime::QuestionFollowupState::Retired
    };
    let followup = match followup {
        runtime::QuestionFollowupState::Unavailable => QuestionFollowupState::Unavailable,
        runtime::QuestionFollowupState::Retired => QuestionFollowupState::Retired,
        runtime::QuestionFollowupState::Unseen => QuestionFollowupState::Unseen,
        runtime::QuestionFollowupState::Observed(input) => QuestionFollowupState::Observed {
            observation: Box::new(input_observation(*input)?),
        },
    };
    Ok(serde_json::to_value(QuestionFollowupStatusResponse {
        question: request.question,
        followup_request_id: request.followup_request_id,
        followup,
    })?)
}

pub(super) async fn observe_execution(
    state: &DaemonState,
    request: QuestionExecutionRequest,
) -> Result<Value> {
    let execution = if let Some(service) = state.session_service.get() {
        service
            .host
            .observe_question_execution(
                &core_target(&request.question),
                LocalOperator::new(service.operator),
                request.stop_request_id,
            )
            .await?
    } else {
        runtime::QuestionExecutionState::Retired
    };
    let execution = match execution {
        runtime::QuestionExecutionState::Unavailable => QuestionExecutionState::Unavailable,
        runtime::QuestionExecutionState::Retired => QuestionExecutionState::Retired,
        runtime::QuestionExecutionState::Observed(observed) => QuestionExecutionState::Observed {
            observation: Box::new(execution_observation(*observed, state.is_stopping())?),
        },
    };
    Ok(serde_json::to_value(QuestionExecutionResponse {
        question: request.question,
        stop_request_id: request.stop_request_id,
        execution,
    })?)
}

pub(super) fn discard_status(
    state: &DaemonState,
    request: QuestionDiscardStatusRequest,
) -> Result<Value> {
    let discard = if let Some(service) = state.session_service.get() {
        service.host.observe_question_discard(
            &core_target(&request.question),
            LocalOperator::new(service.operator),
            request.discard_request_id,
        )?
    } else {
        runtime::QuestionDiscardState::Retired
    };
    Ok(serde_json::to_value(QuestionDiscardResponse {
        question: request.question,
        discard_request_id: request.discard_request_id,
        discard: match discard {
            runtime::QuestionDiscardState::Unseen => QuestionDiscardState::Unseen,
            runtime::QuestionDiscardState::Draining => QuestionDiscardState::Draining,
            runtime::QuestionDiscardState::Retryable => QuestionDiscardState::Retryable,
            runtime::QuestionDiscardState::Discarded => QuestionDiscardState::Discarded,
            runtime::QuestionDiscardState::Retired => QuestionDiscardState::Retired,
        },
    })?)
}

fn execution_observation(
    observed: ExecutionControlObservation,
    stopping: bool,
) -> Result<QuestionExecutionObservation> {
    let execution = &observed.execution;
    let stoppable = match execution.state() {
        InstanceState::Open(InstanceActivity::Active(active))
            if !stopping && active.phase != TurnPhase::Cancelling =>
        {
            Some(wire::input_turn(&active.lease))
        }
        _ => None,
    };
    let stop = observed
        .control
        .filter(|record| record.action == ControlAction::CancelTurn)
        .map(|record| -> Result<_> {
            Ok(QuestionStopReceipt {
                accepted: transcript_position(record.accepted)?,
                applied: record.applied.map(transcript_position).transpose()?,
            })
        })
        .transpose()?;
    Ok(QuestionExecutionObservation {
        cursor: transcript_position(observed.cursor)?,
        revision: execution.revision(),
        phase: FlowExecutionPhase::observe(execution.state(), true),
        stoppable,
        stop,
    })
}

fn input_observation(input: InputObservation) -> Result<QuestionInputObservation> {
    Ok(QuestionInputObservation {
        message_id: input.message_id,
        accepted: transcript_position(input.accepted)?,
        observed_receipt: transcript_position(input.observed_receipt)?,
        input: match input.input {
            InputDisposition::Pending => QuestionInputConsumption::Pending,
            InputDisposition::Cancelled { receipt } => QuestionInputConsumption::Cancelled {
                receipt: transcript_position(receipt)?,
            },
            InputDisposition::Committed { receipt, lease } => QuestionInputConsumption::Committed {
                receipt: transcript_position(receipt)?,
                lease: wire::input_turn(&lease),
            },
        },
    })
}

fn core_identity(identity: &QuestionIdentity) -> runtime::QuestionIdentity {
    runtime::QuestionIdentity {
        source: core_address(identity.source.clone()),
        question: identity.question_id,
        request: identity.request_id,
    }
}

fn core_target(target: &QuestionTarget) -> runtime::QuestionTarget {
    runtime::QuestionTarget {
        identity: core_identity(&target.identity),
        owner_generation: target.owner_generation,
        host_generation: target.host_generation,
    }
}

impl SessionService {
    pub(super) async fn discard_question(&self, request: QuestionDiscardRequest) -> Result<Value> {
        self.host
            .discard_question(
                &core_target(&request.question),
                LocalOperator::new(self.operator),
                request.discard_request_id,
                core_position(request.observed),
            )
            .await?;
        Ok(serde_json::to_value(QuestionDiscardResponse {
            question: request.question,
            discard_request_id: request.discard_request_id,
            discard: QuestionDiscardState::Discarded,
        })?)
    }

    pub(super) async fn stop_question(&self, request: QuestionStopRequest) -> Result<Value> {
        let accepted = self
            .host
            .stop_question_turn(
                &core_target(&request.question),
                LocalOperator::new(self.operator),
                request.stop_request_id,
                request.expected_revision,
                wire::core_turn(request.lease),
            )
            .await?;
        Ok(serde_json::to_value(QuestionStopResponse {
            question: request.question,
            stop_request_id: request.stop_request_id,
            accepted: transcript_position(accepted)?,
        })?)
    }

    pub(super) async fn question_followup(
        &self,
        request: QuestionFollowupRequest,
    ) -> Result<Value> {
        let accepted = self
            .host
            .submit_question_followup(
                &core_target(&request.question),
                LocalOperator::new(self.operator),
                runtime::QuestionFollowupRequest {
                    request_id: request.followup_request_id,
                    observed: core_position(request.observed),
                    content: human::authored_content(request.user)?,
                    model: request.model,
                    effort: parse_effort(request.effort.as_deref())?,
                },
            )
            .await?;
        Ok(serde_json::to_value(QuestionFollowupResponse {
            question: request.question,
            followup_request_id: request.followup_request_id,
            message_id: accepted.message_id,
            accepted: transcript_position(accepted.receipt)?,
        })?)
    }

    pub(super) async fn start_question(
        &self,
        state: &Arc<DaemonState>,
        request: StartQuestionRequest,
    ) -> Result<Value> {
        let flow = core_address(request.identity.source.clone());
        let session = self.session(flow.session_id.0)?;
        let effort = parse_effort(request.effort.as_deref())?;
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

fn parse_effort(effort: Option<&str>) -> Result<Option<ReasoningSelection>> {
    effort
        .map(str::parse)
        .transpose()
        .map_err(anyhow::Error::msg)
}

fn start_receipt(receipt: runtime::QuestionStartReceipt) -> QuestionStartReceipt {
    QuestionStartReceipt {
        source: HistorySource {
            flow: address(&receipt.source.flow),
            branch: *receipt.source.branch.as_uuid(),
            cursor: events::cursor(&receipt.source.cursor),
        },
        launch: question_position(receipt.launch),
        accepted: question_position(receipt.accepted),
        message_id: receipt.message,
    }
}

fn question_position(position: TransientPosition) -> QuestionPosition {
    QuestionPosition {
        host_generation: position.host_generation,
        flow_id: position.flow_id,
        sequence: position.sequence,
        digest: position.digest,
    }
}

fn core_position(position: QuestionPosition) -> TransientPosition {
    TransientPosition {
        host_generation: position.host_generation,
        flow_id: position.flow_id,
        sequence: position.sequence,
        digest: position.digest,
    }
}
