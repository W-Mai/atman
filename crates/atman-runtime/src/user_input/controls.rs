//! Read-only composer settings from the target's original configuration and context.

use std::{io, sync::Arc};

use serde::Serialize;

use crate::{
    config_hub::ConfigHub,
    flow_runtime::{FlowWorker, host::SessionHandle},
    flow_store::SessionConfiguration,
    model_registry::ModelRegistry,
    provider::{ReasoningSelection, ReasoningWireProfile},
    trust::TrustConfig,
};

/// Semantic model key used by ordinary conversation composers.
pub const DEFAULT_INPUT_MODEL: &str = "smart";

/// Immutable original owner bindings; preparing a snapshot never starts execution.
pub struct InputControlSource {
    configuration: SessionConfiguration,
    models: Option<Arc<ModelRegistry>>,
    trust: TrustConfig,
}

#[derive(Debug, Serialize)]
pub struct InputControlsSnapshot {
    pub revision: String,
    pub model: InputModelControl,
    pub trust: TrustConfig,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InputModelControl {
    Pending,
    Unavailable {
        reason: InputModelUnavailable,
    },
    Ready {
        key: String,
        model: String,
        provider: String,
        context_budget: Option<u64>,
        reasoning: Box<InputReasoningControl>,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputModelUnavailable {
    ConfigurationUnavailable,
    ConfigurationChanged,
    ModelSourceMismatch,
    ModelUnavailable,
    ProviderUnavailable,
}

#[derive(Debug, Serialize)]
pub struct InputReasoningControl {
    pub available: bool,
    pub choices: Vec<ReasoningSelection>,
    pub configured: ReasoningSelection,
    pub requested: Option<ReasoningSelection>,
    pub effective: Option<ReasoningSelection>,
    pub error: Option<String>,
}

impl InputControlSource {
    /// Capture only an existing Session and loaded Flow; no writer or executor is opened.
    pub async fn for_flow(session: &SessionHandle, worker: &FlowWorker) -> io::Result<Self> {
        if worker.address().session_id.0 != session.id()
            || session
                .loaded_flow(&worker.address().instance_id)
                .is_none_or(|loaded| !Arc::ptr_eq(&loaded.context(), &worker.context()))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "composer settings require their original Session and loaded Flow",
            ));
        }
        let configuration = session.snapshot().await?.catalog.descriptor.configuration;
        let context = worker.context();
        Ok(Self {
            configuration,
            models: context.model_registry().cloned(),
            trust: context.trust_config(),
        })
    }

    /// Resolve client draft preferences without installing them in the Session or providers.
    pub async fn snapshot(
        self,
        model: Option<String>,
        requested: Option<ReasoningSelection>,
    ) -> io::Result<InputControlsSnapshot> {
        let key = model.unwrap_or_else(|| DEFAULT_INPUT_MODEL.into());
        let too_long = requested.as_ref().is_some_and(|selection| {
            selection
                .effort()
                .is_some_and(|effort| effort.as_str().len() > 256)
                || selection
                    .execution_mode()
                    .is_some_and(|mode| mode.as_str().len() > 256)
        });
        if key.trim().is_empty() || key.len() > 512 || too_long {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "composer model or reasoning selection exceeds its bounds",
            ));
        }
        crate::read_work::capture(move || {
            let model = self.project_model(key, requested);
            let encoded = serde_json::to_vec(&(&self.configuration, &model, &self.trust))
                .map_err(io::Error::other)?;
            Ok(InputControlsSnapshot {
                revision: blake3::hash(&encoded).to_hex().to_string(),
                model,
                trust: self.trust,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    fn project_model(
        &self,
        key: String,
        requested: Option<ReasoningSelection>,
    ) -> InputModelControl {
        let SessionConfiguration::Atman(source) = &self.configuration else {
            return unavailable(InputModelUnavailable::ConfigurationUnavailable);
        };
        let Some(models) = &self.models else {
            return InputModelControl::Pending;
        };
        let Ok(config) = ConfigHub::from_source(source) else {
            return unavailable(InputModelUnavailable::ConfigurationChanged);
        };
        if !Arc::ptr_eq(config.models(), models) {
            return unavailable(InputModelUnavailable::ModelSourceMismatch);
        }
        let models = models.snapshot();
        let Some(entry) = models.model_entry(&key) else {
            return unavailable(InputModelUnavailable::ModelUnavailable);
        };
        if entry.enabled == Some(false) {
            return unavailable(InputModelUnavailable::ModelUnavailable);
        }
        let call = models.model_call_config(&key);
        let Some(provider) = call.provider().filter(|provider| !provider.is_empty()) else {
            return unavailable(InputModelUnavailable::ProviderUnavailable);
        };
        if !call.provider_enabled() {
            return unavailable(InputModelUnavailable::ProviderUnavailable);
        }
        let info = models.model_info(&key);
        let available = models.reasoning_wire_profile_for_model(&key)
            != ReasoningWireProfile::Unknown
            || !info.capabilities.reasoning_efforts.is_empty()
            || !info.capabilities.reasoning_modes.is_empty()
            || info.capabilities.default_reasoning_effort.is_some()
            || info.capabilities.default_reasoning_mode.is_some()
            || !matches!(info.reasoning, ReasoningSelection::ProviderDefault);
        let (effective, error) = if available {
            match models.effective_reasoning_for_model(&key, requested.as_ref()) {
                Ok(effective) => (effective, None),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, Some("Reasoning capabilities are unavailable".into()))
        };
        InputModelControl::Ready {
            model: models.resolve_alias(&key),
            provider: provider.to_owned(),
            context_budget: (info.context_budget > 0).then_some(info.context_budget),
            reasoning: Box::new(InputReasoningControl {
                choices: if available {
                    models.reasoning_selections_for_model(&key)
                } else {
                    Vec::new()
                },
                configured: info.reasoning,
                requested,
                effective,
                error,
                available,
            }),
            key,
        }
    }
}

fn unavailable(reason: InputModelUnavailable) -> InputModelControl {
    InputModelControl::Unavailable { reason }
}
