//! Quick Ask initialization uses the existing Session's operator and configuration.

use std::{io, num::NonZeroUsize, sync::Arc};

use anyhow::{Context, Result, ensure};
use atman_proto::{
    JsonRpcError,
    composer::FlowComposerRequest,
    execution::FlowExecutionPhase,
    flows::FlowAddress,
    flows::SubmittedUserContent,
    history::{HistoryHead, HistorySource},
    questions::*,
};
use atman_rt::{InstanceActivity, InstanceState, TurnPhase};
use atman_runtime::{
    flow_runtime::question::{self as runtime, QuestionPreparation, QuestionRequest},
    flow_store::{
        FlowRef,
        control::{ControlAction, ExecutionControlObservation},
        input_observation::{InputDisposition, InputObservation},
        local_operator::LocalOperator,
        position::{FlowPosition, TransientPosition},
        retention::RetentionReceipt,
        source_capture::SourceSelection,
    },
    provider::ReasoningSelection,
    user_input::controls::InputControlSource,
};
use serde_json::Value;
use uuid::Uuid;

use super::{
    SessionService, address, composer, conversation, core_address, events, execution, human, wire,
};
use crate::DaemonState;

const TRANSCRIPT_WIRE_BYTES: usize = 768 * 1024;
static TRANSCRIPT_READERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(16);

pub(super) fn validate_start(request: &StartQuestionRequest) -> Result<(), JsonRpcError> {
    validate_identity(&request.identity, Some(request.owner_generation))?;
    validate_source(
        &request.identity.source,
        &request.expected_head,
        &request.observed,
    )?;
    validate_prompt(&request.user, &request.model, request.effort.as_deref())
}

fn validate_source(
    flow: &FlowAddress,
    head: &HistoryHead,
    source: &HistorySource,
) -> Result<(), JsonRpcError> {
    let position = &source.cursor;
    if &source.flow != flow
        || source.branch != head.branch
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
    Ok(())
}

fn validate_prompt(
    user: &SubmittedUserContent,
    model: &str,
    effort: Option<&str>,
) -> Result<(), JsonRpcError> {
    validate_model(model, effort)?;
    if user.text.trim().is_empty() && user.images.is_empty() && user.quote.is_none() {
        return Err(JsonRpcError::invalid_params(
            "Quick Ask requires authored content",
        ));
    }
    Ok(())
}

fn validate_model(model: &str, effort: Option<&str>) -> Result<(), JsonRpcError> {
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

pub(super) fn validate_refresh(request: &QuestionRefreshRequest) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, Some(request.refresh_request_id))?;
    if !matches_position(
        &question.identity,
        question.host_generation,
        &request.expected,
    ) {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask refresh position",
        ));
    }
    validate_source(
        &question.identity.source,
        &request.source_head,
        &request.observed_source,
    )?;
    validate_model(&request.model, request.effort.as_deref())
}

pub(super) fn validate_keep(request: &QuestionKeepRequest) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, Some(request.keep_request_id))?;
    if !matches_position(
        &question.identity,
        question.host_generation,
        &request.expected,
    ) {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask Keep position",
        ));
    }
    Ok(())
}

pub(super) fn validate_model_check(
    request: &QuestionModelCheckRequest,
) -> Result<(), JsonRpcError> {
    let question = &request.question;
    validate_target(question, None)?;
    if !matches_position(
        &question.identity,
        question.host_generation,
        &request.expected,
    ) {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask model check position",
        ));
    }
    validate_model(&request.model, request.effort.as_deref())
}

pub(super) async fn check_model(
    state: &DaemonState,
    request: QuestionModelCheckRequest,
) -> Result<Value> {
    let service = state
        .session_service
        .get()
        .ok_or_else(|| anyhow::anyhow!("Quick Ask host is unavailable"))?;
    let effort = parse_effort(request.effort.as_deref())?;
    let budget = service
        .host
        .check_question_model(
            &core_target(&request.question),
            LocalOperator::new(service.operator),
            core_position(request.expected.clone()),
            &request.model,
            effort.as_ref(),
        )
        .await?;
    Ok(serde_json::to_value(QuestionModelCheckResponse {
        question: request.question,
        checked: request.expected,
        model: budget.model,
    })?)
}

pub(super) fn validate_model_options(
    request: &QuestionModelOptionsRequest,
) -> Result<(), JsonRpcError> {
    if request.flow.session_id.0.is_nil() || request.flow.instance_id.is_nil() {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask model source",
        ));
    }
    if let Some(question) = &request.question {
        validate_target(question, None)?;
        if question.identity.source != request.flow {
            return Err(JsonRpcError::invalid_params(
                "Quick Ask model source changed",
            ));
        }
    }
    composer::validate(&FlowComposerRequest {
        flow: request.flow.clone(),
        model: Some(request.model.clone()),
        reasoning: request.reasoning.clone(),
        catalog_query: request.catalog_query.clone(),
    })
}

pub(super) async fn model_options(
    state: &DaemonState,
    request: QuestionModelOptionsRequest,
) -> Result<Value> {
    let service = state
        .session_service
        .get()
        .context("Quick Ask host is unavailable")?;
    let reasoning = request
        .reasoning
        .map(composer::core_reasoning)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let snapshot = if let Some(question) = &request.question {
        service
            .host
            .question_model_options(
                &core_target(question),
                LocalOperator::new(service.operator),
                request.model,
                reasoning,
                request.catalog_query,
            )
            .await?
    } else {
        let flow = core_address(request.flow.clone());
        let session = service.host.session(flow.session_id.0)?;
        let worker = session
            .loaded_flow(&flow.instance_id)
            .filter(|worker| !worker.is_transient())
            .context("Quick Ask model options require the loaded source Flow")?;
        let source = InputControlSource::for_flow(&session, &worker).await?;
        let snapshot = source
            .question_models(request.model, reasoning, request.catalog_query)
            .await?;
        let context = worker.context();
        ensure!(
            session
                .loaded_flow(&flow.instance_id)
                .is_some_and(|current| Arc::ptr_eq(&context, &current.context())),
            "Quick Ask source detached during model observation"
        );
        snapshot
    };
    Ok(serde_json::to_value(QuestionModelOptionsResponse {
        flow: request.flow,
        question: request.question,
        model: composer::model(snapshot.model),
        catalog: snapshot.catalog.map(composer::catalog),
    })?)
}

pub(super) fn validate_keep_installed(
    request: &QuestionKeepInstalledRequest,
) -> Result<(), JsonRpcError> {
    validate_target(&request.question, Some(request.keep_request_id))?;
    let installed = &request.installed;
    if installed.journal_id != request.question.identity.question_id
        || installed.sequence == 0
        || installed.digest.len() != 64
        || !installed
            .digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(JsonRpcError::invalid_params(
            "invalid Quick Ask installed position",
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

pub(super) async fn refresh_status(
    state: &DaemonState,
    request: QuestionRefreshStatusRequest,
) -> Result<Value> {
    let refresh = if let Some(service) = state.session_service.get() {
        service
            .host
            .observe_question_refresh(
                &core_target(&request.question),
                LocalOperator::new(service.operator),
                request.refresh_request_id,
            )
            .await?
    } else {
        runtime::QuestionRefreshState::Retired
    };
    let refresh = match refresh {
        runtime::QuestionRefreshState::Unseen => QuestionRefreshState::Unseen,
        runtime::QuestionRefreshState::Unavailable => QuestionRefreshState::Unavailable,
        runtime::QuestionRefreshState::Retired => QuestionRefreshState::Retired,
        runtime::QuestionRefreshState::Applied(receipt) => QuestionRefreshState::Applied {
            receipt: Box::new(refresh_receipt(*receipt)?),
        },
    };
    Ok(serde_json::to_value(QuestionRefreshStatusResponse {
        question: request.question,
        refresh_request_id: request.refresh_request_id,
        refresh,
    })?)
}

pub(super) fn keep_status(
    state: &DaemonState,
    request: QuestionKeepStatusRequest,
) -> Result<Value> {
    let keep = if let Some(service) = state.session_service.get() {
        service.host.observe_question_keep(
            &core_target(&request.question),
            LocalOperator::new(service.operator),
            request.keep_request_id,
        )?
    } else {
        runtime::QuestionKeepState::Retired
    };
    keep_response(request.question, request.keep_request_id, keep)
}

impl SessionService {
    pub(super) async fn keep_question(&self, request: QuestionKeepRequest) -> Result<Value> {
        let keep = self
            .host
            .keep_question(
                &core_target(&request.question),
                LocalOperator::new(self.operator),
                request.keep_request_id,
                core_position(request.expected),
            )
            .await?;
        keep_response(request.question, request.keep_request_id, keep)
    }

    pub(super) async fn install_kept_question(
        &self,
        request: QuestionKeepInstalledRequest,
    ) -> Result<Value> {
        let keep = self
            .host
            .install_kept_question(
                &core_target(&request.question),
                LocalOperator::new(self.operator),
                request.keep_request_id,
                events::receipt(request.installed),
            )
            .await?;
        keep_response(request.question, request.keep_request_id, keep)
    }

    pub(super) async fn refresh_question(
        &self,
        state: &Arc<DaemonState>,
        request: QuestionRefreshRequest,
    ) -> Result<Value> {
        let target = core_target(&request.question);
        let receipt = self
            .host
            .refresh_observed_question(
                &target,
                LocalOperator::new(self.operator),
                runtime::QuestionRefreshRequest {
                    id: request.refresh_request_id,
                    question: target.address(),
                    expected: FlowPosition::Transient {
                        transient: core_position(request.expected),
                    },
                    source: core_source(request.observed_source),
                    model: request.model,
                    effort: parse_effort(request.effort.as_deref())?,
                },
                wire::core_head(request.source_head),
                self.question_preparation(state, &target.identity.source),
            )
            .await?;
        Ok(serde_json::to_value(QuestionRefreshResponse {
            question: request.question,
            refresh_request_id: request.refresh_request_id,
            receipt: refresh_receipt(receipt)?,
        })?)
    }

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
        let effort = parse_effort(request.effort.as_deref())?;
        let ask = QuestionRequest {
            id: request.identity.question_id,
            request_id: request.identity.request_id,
            source: core_source(request.observed),
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
                self.question_preparation(state, &flow),
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

    async fn question_preparation(
        &self,
        state: &Arc<DaemonState>,
        flow: &FlowRef,
    ) -> Result<QuestionPreparation> {
        let session = self.session(flow.session_id.0)?;
        let snapshot = session.snapshot().await?;
        let worker = self.host.flow(flow).await?;
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
    }
}

fn core_source(source: HistorySource) -> SourceSelection {
    SourceSelection {
        flow: core_address(source.flow),
        branch: source.branch.into(),
        cursor: events::receipt(source.cursor),
    }
}

fn history_source(source: SourceSelection) -> HistorySource {
    HistorySource {
        flow: address(&source.flow),
        branch: *source.branch.as_uuid(),
        cursor: events::cursor(&source.cursor),
    }
}

fn refresh_receipt(receipt: runtime::QuestionRefreshReceipt) -> Result<QuestionRefreshReceipt> {
    Ok(QuestionRefreshReceipt {
        source: history_source(receipt.source),
        applied: transcript_position(receipt.applied)?,
    })
}

fn keep_response(
    question: QuestionTarget,
    keep_request_id: Uuid,
    keep: runtime::QuestionKeepState,
) -> Result<Value> {
    Ok(serde_json::to_value(QuestionKeepResponse {
        question,
        keep_request_id,
        keep: match keep {
            runtime::QuestionKeepState::Unseen => QuestionKeepState::Unseen,
            runtime::QuestionKeepState::Pending => QuestionKeepState::Pending,
            runtime::QuestionKeepState::Published(receipt) => QuestionKeepState::Published {
                receipt: Box::new(keep_receipt(*receipt)),
            },
            runtime::QuestionKeepState::Installed(receipt) => QuestionKeepState::Installed {
                receipt: Box::new(keep_receipt(*receipt)),
            },
            runtime::QuestionKeepState::Retired => QuestionKeepState::Retired,
        },
    })?)
}

fn keep_receipt(receipt: RetentionReceipt) -> QuestionKeepReceipt {
    QuestionKeepReceipt {
        flow: address(&receipt.flow),
        source: question_position(receipt.source),
        destination: events::cursor(&receipt.destination),
        outputs_digest: receipt.outputs_digest,
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
        source: history_source(receipt.source),
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
