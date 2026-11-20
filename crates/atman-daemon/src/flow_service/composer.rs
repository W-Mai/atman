use std::sync::Arc;

use anyhow::Context;
use atman_proto::{JsonRpcError, composer::*};
use atman_runtime::{
    provider::{ReasoningExecutionMode, ReasoningSelection},
    trust::{EscalationPolicy, ModeColor, Theme, TrustConfig, TrustMode},
    user_input::{
        controls::{
            InputControlSource, InputModelControl, InputModelUnavailable, InputReasoningControl,
        },
        preference_authority::{PreferenceOutcome, PreferenceRejection, read_preference_outcome},
        preferences::{PreferenceEdit, PreferenceOrigin},
    },
};
use serde_json::Value;

use crate::DaemonState;

pub(super) fn validate_edit(request: &SetComposerPreferenceRequest) -> Result<(), JsonRpcError> {
    if request.expected_revision.len() != 64
        || !request
            .expected_revision
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        return Err(JsonRpcError::invalid_params(
            "invalid composer preference revision",
        ));
    }
    let (model, reasoning) = match &request.edit {
        ComposerPreferenceEdit::Model { key } => (Some(key.clone()), None),
        ComposerPreferenceEdit::Reasoning {
            value: ComposerReasoningPreference::Selection { selection },
        } => (None, Some(selection.clone())),
        _ => (None, None),
    };
    validate(&FlowComposerRequest {
        flow: request.flow.clone(),
        model,
        reasoning,
        catalog_query: None,
    })
}

pub(super) async fn set(
    state: &DaemonState,
    request: SetComposerPreferenceRequest,
) -> anyhow::Result<Value> {
    use atman_runtime::user_input::preferences::ComposerReasoning as SavedReasoning;
    let edit = match request.edit {
        ComposerPreferenceEdit::Model { key } => PreferenceEdit::Model(key),
        ComposerPreferenceEdit::Reasoning { value } => PreferenceEdit::Reasoning(match value {
            ComposerReasoningPreference::ModelDefault => SavedReasoning::ModelDefault,
            ComposerReasoningPreference::Selection { selection } => match core_reasoning(selection)
            {
                Ok(value) => SavedReasoning::Selection(value),
                Err(_) => {
                    return response(
                        request.flow,
                        request.request_id,
                        PreferenceRejection::InvalidSelection.into(),
                    );
                }
            },
        }),
        ComposerPreferenceEdit::TrustMode { value } => PreferenceEdit::TrustMode(match value {
            ComposerTrustMode::Calm => TrustMode::Calm,
            ComposerTrustMode::Steady => TrustMode::Steady,
            ComposerTrustMode::Eager => TrustMode::Eager,
            ComposerTrustMode::Reckless => TrustMode::Reckless,
        }),
        ComposerPreferenceEdit::Escalation { value } => PreferenceEdit::Escalation(match value {
            ComposerEscalationPolicy::Deny => EscalationPolicy::Deny,
            ComposerEscalationPolicy::Ask => EscalationPolicy::Ask,
            ComposerEscalationPolicy::Allow => EscalationPolicy::Allow,
        }),
    };
    let flow = super::core_address(request.flow.clone());
    let outcome = if let Some(session) = state
        .session_service
        .get()
        .and_then(|svc| svc.host.session(flow.session_id.0).ok())
    {
        session
            .set_bound_input_preference(
                flow.instance_id,
                request.request_id,
                request.expected_revision,
                edit,
            )
            .await?
    } else {
        PreferenceRejection::InvalidTarget.into()
    };
    response(request.flow, request.request_id, outcome)
}

pub(super) async fn status(
    state: &DaemonState,
    request: ComposerPreferenceStatusRequest,
) -> anyhow::Result<Value> {
    let flow = super::core_address(request.flow.clone());
    let outcome = if let Some(session) = state
        .session_service
        .get()
        .and_then(|svc| svc.host.session(flow.session_id.0).ok())
    {
        session.input_preference_outcome(&flow.instance_id, request.request_id)
    } else {
        read_preference_outcome(state.data_dir().to_path_buf(), flow, request.request_id).await?
    };
    response(request.flow, request.request_id, outcome)
}

fn response(
    flow: atman_proto::flows::FlowAddress,
    request_id: uuid::Uuid,
    outcome: PreferenceOutcome,
) -> anyhow::Result<Value> {
    let outcome = match outcome {
        PreferenceOutcome::Pending => ComposerPreferenceOutcome::Pending,
        PreferenceOutcome::Applied => ComposerPreferenceOutcome::Applied,
        PreferenceOutcome::Conflict => ComposerPreferenceOutcome::Conflict,
        PreferenceOutcome::NotFound => ComposerPreferenceOutcome::NotFound,
        PreferenceOutcome::Rejected(reason) => ComposerPreferenceOutcome::Rejected {
            reason: match reason {
                PreferenceRejection::InvalidSelection => {
                    ComposerPreferenceRejection::InvalidSelection
                }
                PreferenceRejection::Unavailable => ComposerPreferenceRejection::Unavailable,
                PreferenceRejection::NotEager => ComposerPreferenceRejection::NotEager,
                PreferenceRejection::InvalidTarget => ComposerPreferenceRejection::InvalidTarget,
                PreferenceRejection::DifferentIntent => {
                    ComposerPreferenceRejection::DifferentIntent
                }
                PreferenceRejection::Storage => ComposerPreferenceRejection::Storage,
            },
        },
    };
    Ok(serde_json::to_value(ComposerPreferenceResponse {
        flow,
        request_id,
        outcome,
    })?)
}

#[cfg(test)]
mod tests;

/// Explicitly open a composer's ordinary Flow owner and attach its model source.
/// The conversation's original entry, inbox, and scheduler remain untouched.
pub(super) async fn prepare(
    state: &Arc<DaemonState>,
    service: &super::SessionService,
    request: atman_proto::flows::FlowRequest,
) -> anyhow::Result<Value> {
    use atman_runtime::flow_store::SessionConfiguration;
    let _host_operation = service.host.retain_host_operation()?;
    let flow = super::core_address(request.flow.clone());
    let session = service.session(flow.session_id.0)?;
    let owner = session.snapshot().await?.catalog;
    anyhow::ensure!(
        matches!(
            owner.descriptor.configuration,
            SessionConfiguration::Atman(_)
        ) && !owner.archived_flows.contains(&flow.instance_id),
        "composer settings are not available for this Flow"
    );
    let row = owner
        .flows
        .iter()
        .find(|row| row.id == flow.instance_id && row.retention.is_none())
        .context("composer settings require a registered ordinary Flow")?;
    session.refresh_input_preferences().await?;
    let worker = service.host.flow(&flow).await?;
    anyhow::ensure!(!worker.is_transient(), "composer target was replaced");
    let executor = service
        .executor(
            state,
            &owner.descriptor,
            row.project.as_ref(),
            super::execution::launcher_home(state),
        )
        .await?;
    worker
        .context()
        .bind_provider_registry(&executor.providers)?;
    snapshot_worker(
        FlowComposerRequest {
            flow: request.flow,
            model: None,
            reasoning: None,
            catalog_query: None,
        },
        &session,
        &worker,
    )
    .await
}

pub(super) fn validate(request: &FlowComposerRequest) -> Result<(), JsonRpcError> {
    if let Some(model) = &request.model
        && (model.trim().is_empty() || model.len() > 512)
    {
        return Err(JsonRpcError::invalid_params(
            "composer model key must be non-empty and at most 512 bytes",
        ));
    }
    if request
        .catalog_query
        .as_ref()
        .is_some_and(|query| query.len() > 256)
    {
        return Err(JsonRpcError::invalid_params(
            "composer catalog query exceeds 256 bytes",
        ));
    }
    let valid = |text: &str| !text.trim().is_empty() && text.len() <= 256;
    let valid_mode = |mode: &Option<String>| mode.as_deref().is_none_or(valid);
    match &request.reasoning {
        Some(ComposerReasoning::Auto { execution_mode }) if !valid_mode(execution_mode) => {
            return Err(JsonRpcError::invalid_params("invalid reasoning mode"));
        }
        Some(ComposerReasoning::Effort {
            effort,
            execution_mode,
        }) if !valid(effort) || !valid_mode(execution_mode) => {
            return Err(JsonRpcError::invalid_params(
                "invalid reasoning effort or mode",
            ));
        }
        _ => {}
    }
    Ok(())
}

pub(super) async fn snapshot(
    state: &DaemonState,
    request: FlowComposerRequest,
) -> anyhow::Result<Value> {
    anyhow::ensure!(!state.is_stopping(), "daemon is stopping");
    let service = state
        .session_service
        .get()
        .context("composer target is not attached")?;
    let flow = super::core_address(request.flow.clone());
    let session = service
        .host
        .session(flow.session_id.0)
        .context("composer target is not attached")?;
    let worker = session
        .loaded_flow(&flow.instance_id)
        .context("composer flow is not loaded")?;
    snapshot_worker(request, &session, &worker).await
}

async fn snapshot_worker(
    request: FlowComposerRequest,
    session: &atman_runtime::flow_runtime::host::SessionHandle,
    worker: &atman_runtime::flow_runtime::FlowWorker,
) -> anyhow::Result<Value> {
    let reasoning = request
        .reasoning
        .map(core_reasoning)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let source = InputControlSource::for_flow(session, worker).await?;
    let settings = source
        .snapshot(request.model, reasoning, request.catalog_query)
        .await?;
    let original = worker.context();
    anyhow::ensure!(
        session
            .loaded_flow(&worker.address().instance_id)
            .is_some_and(|current| Arc::ptr_eq(&original, &current.context())),
        "composer target detached during observation"
    );
    Ok(serde_json::to_value(FlowComposerResponse {
        flow: request.flow,
        revision: settings.revision,
        model: model(settings.model),
        trust: trust(&settings.trust),
        origins: settings.origins.map(|value| ComposerPreferenceOrigins {
            model: origin(value.model),
            reasoning: origin(value.reasoning),
            trust_mode: origin(value.trust_mode),
            escalation: origin(value.escalation),
        }),
        trust_choices: settings
            .edit_revisions
            .as_ref()
            .map(|_| trust_choices(&settings.trust)),
        edit_revisions: settings
            .edit_revisions
            .map(|value| ComposerPreferenceRevisions {
                model: value.model,
                reasoning: value.reasoning,
                trust_mode: value.trust_mode,
                escalation: value.escalation,
            }),
        catalog: settings.catalog.map(|catalog| ComposerModelCatalog {
            rows: catalog
                .rows
                .into_iter()
                .map(|row| ComposerModelChoice {
                    key: row.key,
                    alias: row.alias,
                    model: row.model,
                    provider: row.provider,
                })
                .collect(),
            truncated: catalog.truncated,
        }),
    })?)
}

fn origin(value: PreferenceOrigin) -> ComposerPreferenceOrigin {
    match value {
        PreferenceOrigin::Global => ComposerPreferenceOrigin::Global,
        PreferenceOrigin::Session => ComposerPreferenceOrigin::Session,
    }
}

fn core_reasoning(value: ComposerReasoning) -> Result<ReasoningSelection, String> {
    let parse_mode = |mode: Option<String>| {
        mode.map(|text| text.parse::<ReasoningExecutionMode>())
            .transpose()
    };
    Ok(match value {
        ComposerReasoning::ProviderDefault => ReasoningSelection::ProviderDefault,
        ComposerReasoning::Disabled => ReasoningSelection::Disabled,
        ComposerReasoning::Auto { execution_mode } => ReasoningSelection::Auto {
            execution_mode: parse_mode(execution_mode)?,
        },
        ComposerReasoning::Effort {
            effort,
            execution_mode,
        } => ReasoningSelection::Effort {
            effort: effort.parse()?,
            execution_mode: parse_mode(execution_mode)?,
        },
        ComposerReasoning::BudgetTokens { tokens } => ReasoningSelection::BudgetTokens { tokens },
    })
}

fn reasoning(value: ReasoningSelection) -> ComposerReasoning {
    match value {
        ReasoningSelection::ProviderDefault => ComposerReasoning::ProviderDefault,
        ReasoningSelection::Disabled => ComposerReasoning::Disabled,
        ReasoningSelection::Auto { execution_mode } => ComposerReasoning::Auto {
            execution_mode: execution_mode.map(|mode| mode.to_string()),
        },
        ReasoningSelection::Effort {
            effort,
            execution_mode,
        } => ComposerReasoning::Effort {
            effort: effort.to_string(),
            execution_mode: execution_mode.map(|mode| mode.to_string()),
        },
        ReasoningSelection::BudgetTokens { tokens } => ComposerReasoning::BudgetTokens { tokens },
    }
}

fn reasoning_state(value: InputReasoningControl) -> ComposerReasoningState {
    ComposerReasoningState {
        available: value.available,
        choices: value.choices.into_iter().map(reasoning).collect(),
        configured: reasoning(value.configured),
        requested: value.requested.map(reasoning),
        effective: value.effective.map(reasoning),
        error: value.error,
    }
}

fn model(value: InputModelControl) -> ComposerModelState {
    match value {
        InputModelControl::Pending => ComposerModelState::Pending,
        InputModelControl::Unavailable { reason } => ComposerModelState::Unavailable {
            reason: match reason {
                InputModelUnavailable::ConfigurationUnavailable => {
                    ComposerModelUnavailable::ConfigurationUnavailable
                }
                InputModelUnavailable::ConfigurationChanged => {
                    ComposerModelUnavailable::ConfigurationChanged
                }
                InputModelUnavailable::ModelSourceMismatch => {
                    ComposerModelUnavailable::ModelSourceMismatch
                }
                InputModelUnavailable::ModelUnavailable => {
                    ComposerModelUnavailable::ModelUnavailable
                }
                InputModelUnavailable::ProviderUnavailable => {
                    ComposerModelUnavailable::ProviderUnavailable
                }
            },
        },
        InputModelControl::Ready {
            key,
            model,
            provider,
            context_budget,
            reasoning,
        } => ComposerModelState::Ready {
            key,
            model,
            provider,
            context_budget,
            reasoning: Box::new(reasoning_state(*reasoning)),
        },
    }
}

fn color(value: ModeColor) -> ComposerTrustColor {
    match value {
        ModeColor::Cyan => ComposerTrustColor::Cyan,
        ModeColor::Green => ComposerTrustColor::Green,
        ModeColor::Yellow => ComposerTrustColor::Yellow,
        ModeColor::Red => ComposerTrustColor::Red,
        ModeColor::Orange => ComposerTrustColor::Orange,
    }
}

fn escalation(theme: Theme, policy: EscalationPolicy) -> ComposerEscalation {
    let display = theme.escalation_display(policy);
    ComposerEscalation {
        policy: match policy {
            EscalationPolicy::Deny => ComposerEscalationPolicy::Deny,
            EscalationPolicy::Ask => ComposerEscalationPolicy::Ask,
            EscalationPolicy::Allow => ComposerEscalationPolicy::Allow,
        },
        name: display.name.into(),
        icon: display.emoji.into(),
        color: color(display.color),
    }
}

fn trust_choices(value: &TrustConfig) -> ComposerTrustChoices {
    let modes = [
        TrustMode::Calm,
        TrustMode::Steady,
        TrustMode::Eager,
        TrustMode::Reckless,
    ]
    .into_iter()
    .map(|mode| {
        trust(&TrustConfig {
            mode,
            ..value.clone()
        })
    })
    .collect();
    let escalations = if value.mode == TrustMode::Eager {
        [
            EscalationPolicy::Deny,
            EscalationPolicy::Ask,
            EscalationPolicy::Allow,
        ]
        .into_iter()
        .map(|policy| escalation(value.theme, policy))
        .collect()
    } else {
        Vec::new()
    };
    ComposerTrustChoices { modes, escalations }
}

fn trust(value: &TrustConfig) -> ComposerTrust {
    let display = value.display();
    ComposerTrust {
        mode: match value.mode {
            TrustMode::Calm => ComposerTrustMode::Calm,
            TrustMode::Steady => ComposerTrustMode::Steady,
            TrustMode::Eager => ComposerTrustMode::Eager,
            TrustMode::Reckless => ComposerTrustMode::Reckless,
        },
        theme: match value.theme {
            Theme::Default => ComposerTrustTheme::Default,
            Theme::Wuxia => ComposerTrustTheme::Wuxia,
            Theme::Animal => ComposerTrustTheme::Animal,
            Theme::Weather => ComposerTrustTheme::Weather,
            Theme::Drink => ComposerTrustTheme::Drink,
        },
        name: display.name.into(),
        icon: display.emoji.into(),
        color: color(display.color),
        description: display.description.into(),
        escalation: (value.mode == TrustMode::Eager)
            .then(|| escalation(value.theme, value.escalation)),
    }
}
