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
        preferences::PreferenceOrigin,
    },
};
use serde_json::Value;

use crate::DaemonState;

#[cfg(test)]
mod tests;

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
    let reasoning = request
        .reasoning
        .map(core_reasoning)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let source = InputControlSource::for_flow(&session, &worker).await?;
    let settings = source
        .snapshot(request.model, reasoning, request.catalog_query)
        .await?;
    let original = worker.context();
    anyhow::ensure!(
        session
            .loaded_flow(&flow.instance_id)
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
        escalation: (value.mode == TrustMode::Eager).then(|| {
            let display = value.theme.escalation_display(value.escalation);
            ComposerEscalation {
                policy: match value.escalation {
                    EscalationPolicy::Deny => ComposerEscalationPolicy::Deny,
                    EscalationPolicy::Ask => ComposerEscalationPolicy::Ask,
                    EscalationPolicy::Allow => ComposerEscalationPolicy::Allow,
                },
                name: display.name.into(),
                icon: display.emoji.into(),
                color: color(display.color),
            }
        }),
    }
}
