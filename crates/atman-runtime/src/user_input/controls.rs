//! Read-only composer settings from the target's original configuration and context.

use std::{io, sync::Arc};

use serde::Serialize;
use uuid::Uuid;

use super::preference_authority::{ComposerWriteRevisions, revisions};

use crate::{
    config_hub::ConfigHub,
    flow_runtime::{FlowWorker, host::SessionHandle},
    flow_store::SessionConfiguration,
    model_registry::{ModelRegistry, ModelRegistrySnapshot},
    provider::{ReasoningSelection, ReasoningWireProfile},
    trust::TrustConfig,
    user_input::preferences::{
        EffectiveInputPreferences, InputPreferenceOrigins, SessionInputPreferences,
    },
};

/// Semantic model key used by ordinary conversation composers.
pub const DEFAULT_INPUT_MODEL: &str = "smart";
pub const COMPOSER_CATALOG_LIMIT: usize = 128;

/// Immutable original owner bindings; preparing a snapshot never starts execution.
pub struct InputControlSource {
    configuration: SessionConfiguration,
    models: Option<Arc<ModelRegistry>>,
    trust: TrustConfig,
    preferences: Option<EffectiveInputPreferences>,
    writable: Option<(Uuid, SessionInputPreferences)>,
}

#[derive(Debug, Serialize)]
pub struct InputControlsSnapshot {
    pub revision: String,
    pub model: InputModelControl,
    pub trust: TrustConfig,
    pub origins: Option<InputPreferenceOrigins>,
    pub catalog: Option<InputModelCatalog>,
    pub edit_revisions: Option<ComposerWriteRevisions>,
}

#[derive(Debug, Serialize)]
pub struct InputModelCatalog {
    pub rows: Vec<InputModelChoice>,
    pub truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct InputModelChoice {
    pub key: String,
    pub alias: bool,
    pub model: String,
    pub provider: String,
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
        let catalog = session.snapshot().await?.catalog;
        let configuration = catalog.descriptor.configuration;
        let writable = (!worker.is_transient()
            && catalog.flows.iter().any(|row| {
                row.id == worker.address().instance_id
                    && row.retention.is_none()
                    && !catalog.archived_flows.contains(&row.id)
            }))
        .then_some((worker.preference_binding, catalog.input_preferences));
        let context = worker.context();
        let preferences = context.input_preferences();
        Ok(Self {
            configuration,
            models: context.model_registry().cloned(),
            trust: preferences
                .as_ref()
                .map_or_else(|| context.trust_config(), |value| value.trust.clone()),
            preferences,
            writable,
        })
    }

    /// Resolve client draft preferences without installing them in the Session or providers.
    pub async fn snapshot(
        self,
        model: Option<String>,
        requested: Option<ReasoningSelection>,
        catalog_query: Option<String>,
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
        if key.trim().is_empty()
            || key.len() > 512
            || too_long
            || catalog_query
                .as_ref()
                .is_some_and(|query| query.len() > 256)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "composer model or reasoning selection exceeds its bounds",
            ));
        }
        crate::read_work::capture(move || {
            let source = self;
            let (model, catalog, edit_revisions) = match source.model_source() {
                Ok((config, models)) => {
                    let edit_revisions = source.writable.as_ref().and_then(|(binding, sparse)| {
                        let observed = source.preferences.as_ref()?;
                        let (reasoning, trust) = config.input_defaults().ok()?;
                        // An inactive Session retains its effective defaults until activated again.
                        // Do not attach a write seal for newer defaults to an older display.
                        if sparse.resolve(reasoning.clone(), trust.clone()) != *observed {
                            return None;
                        }
                        revisions(*binding, sparse, &models, &reasoning, &trust).ok()
                    });
                    let catalog = catalog_query.as_deref().and_then(|query| {
                        source.preferences.as_ref()?;
                        let auth = config.load_auth().ok()?;
                        Some(source.project_catalog(&models, &auth, query))
                    });
                    (
                        source.project_model(&models, key, requested),
                        catalog,
                        edit_revisions,
                    )
                }
                Err(model) => (model, None, None),
            };
            let origins = source.preferences.as_ref().map(|value| value.origins);
            let encoded = serde_json::to_vec(&(
                &source.configuration,
                &model,
                &source.trust,
                &origins,
                &catalog,
            ))
            .map_err(io::Error::other)?;
            Ok(InputControlsSnapshot {
                revision: blake3::hash(&encoded).to_hex().to_string(),
                model,
                trust: source.trust,
                origins,
                catalog,
                edit_revisions,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    fn model_source(&self) -> Result<(ConfigHub, ModelRegistrySnapshot), InputModelControl> {
        let SessionConfiguration::Atman(source) = &self.configuration else {
            return Err(unavailable(InputModelUnavailable::ConfigurationUnavailable));
        };
        let Some(models) = &self.models else {
            return Err(InputModelControl::Pending);
        };
        let Ok(config) = ConfigHub::from_source(source) else {
            return Err(unavailable(InputModelUnavailable::ConfigurationChanged));
        };
        if !Arc::ptr_eq(config.models(), models) {
            return Err(unavailable(InputModelUnavailable::ModelSourceMismatch));
        }
        Ok((config, models.snapshot()))
    }

    fn project_catalog(
        &self,
        models: &ModelRegistrySnapshot,
        auth: &crate::auth_store::AuthStore,
        query: &str,
    ) -> InputModelCatalog {
        let query = query.trim().to_lowercase();
        let matches = |value: &str| query.is_empty() || value.to_lowercase().contains(&query);
        let valid = |value: &str| {
            !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control)
        };
        let allowed = models.enabled_provider_names(auth);
        let input_model = self
            .preferences
            .as_ref()
            .and_then(|value| value.model.as_deref());
        let mut rows = Vec::new();
        let mut truncated = false;
        models.visit_input_models(input_model, |key, alias, model, provider| {
            if ![key, model, provider].into_iter().all(valid)
                || !allowed.contains(provider)
                || ![key, model, provider].into_iter().any(matches)
            {
                return true;
            }
            if rows.len() == COMPOSER_CATALOG_LIMIT {
                truncated = true;
                return false;
            }
            rows.push(InputModelChoice {
                key: key.to_owned(),
                alias,
                model: model.to_owned(),
                provider: provider.to_owned(),
            });
            true
        });
        InputModelCatalog { rows, truncated }
    }

    fn project_model(
        &self,
        models: &ModelRegistrySnapshot,
        key: String,
        requested: Option<ReasoningSelection>,
    ) -> InputModelControl {
        let input_model = self
            .preferences
            .as_ref()
            .and_then(|settings| settings.model.as_deref());
        let Some(entry) = models.model_entry_for_input(&key, input_model) else {
            return unavailable(InputModelUnavailable::ModelUnavailable);
        };
        if entry.enabled == Some(false) {
            return unavailable(InputModelUnavailable::ModelUnavailable);
        }
        let call = models.model_call_config_for_input(&key, input_model);
        let Some(provider) = call.provider().filter(|provider| !provider.is_empty()) else {
            return unavailable(InputModelUnavailable::ProviderUnavailable);
        };
        if !call.provider_enabled() {
            return unavailable(InputModelUnavailable::ProviderUnavailable);
        }
        let info = &call.info;
        let concrete = &info.name;
        let available = models.reasoning_wire_profile_for_model(concrete)
            != ReasoningWireProfile::Unknown
            || !info.capabilities.reasoning_efforts.is_empty()
            || !info.capabilities.reasoning_modes.is_empty()
            || info.capabilities.default_reasoning_effort.is_some()
            || info.capabilities.default_reasoning_mode.is_some()
            || !matches!(info.reasoning, ReasoningSelection::ProviderDefault);
        let explicitly_requested = requested.is_some();
        let requested =
            requested.or_else(|| self.preferences.as_ref()?.reasoning.selection().cloned());
        let (effective, error) = if available {
            match models.effective_reasoning_for_model(concrete, requested.as_ref()) {
                Ok(effective) => (effective, None),
                Err(error) if !explicitly_requested => (
                    models
                        .effective_reasoning_for_model(concrete, None)
                        .ok()
                        .flatten(),
                    Some(error),
                ),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, Some("Reasoning capabilities are unavailable".into()))
        };
        InputModelControl::Ready {
            model: concrete.clone(),
            provider: provider.to_owned(),
            context_budget: (info.context_budget > 0).then_some(info.context_budget),
            reasoning: Box::new(InputReasoningControl {
                choices: if available {
                    models.reasoning_selections_for_model(concrete)
                } else {
                    Vec::new()
                },
                configured: info.reasoning.clone(),
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
