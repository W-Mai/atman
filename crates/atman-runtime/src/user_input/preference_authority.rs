//! Source-bound revisions and receipts for ordinary Session composer edits.

use std::io;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    config_hub::ConfigHub, flow_runtime::FlowWorker, model_registry::ModelRegistrySnapshot,
    provider::ProviderRegistry, trust::TrustMode,
};

use super::{
    controls::DEFAULT_INPUT_MODEL,
    preferences::{PreferenceEdit, SessionInputPreferences},
};

#[derive(Debug, Serialize)]
pub struct ComposerWriteRevisions {
    pub model: String,
    pub reasoning: String,
    pub trust_mode: String,
    pub escalation: String,
}

impl ComposerWriteRevisions {
    fn for_edit(&self, edit: &PreferenceEdit) -> Result<&str, PreferenceRejection> {
        Ok(match edit {
            PreferenceEdit::Model(_) => &self.model,
            PreferenceEdit::Reasoning(_) => &self.reasoning,
            PreferenceEdit::TrustMode(_) => &self.trust_mode,
            PreferenceEdit::Escalation(_) => &self.escalation,
            _ => return Err(PreferenceRejection::InvalidSelection),
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceRejection {
    InvalidSelection,
    Unavailable,
    NotEager,
    InvalidTarget,
    DifferentIntent,
    Storage,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceOutcome {
    Pending,
    Applied,
    Conflict,
    NotFound,
    Rejected(PreferenceRejection),
}

impl std::fmt::Display for PreferenceOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "composer preference: {self:?}")
    }
}

impl std::error::Error for PreferenceOutcome {}

impl PreferenceOutcome {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Conflict | Self::Rejected(_))
    }
}

impl From<PreferenceOutcome> for io::Error {
    fn from(outcome: PreferenceOutcome) -> Self {
        Self::other(outcome)
    }
}

impl From<PreferenceRejection> for PreferenceOutcome {
    fn from(value: PreferenceRejection) -> Self {
        Self::Rejected(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PreferenceIntent {
    pub flow: crate::flow_store::FlowInstanceId,
    pub expected: String,
    pub edit: PreferenceEdit,
}

pub(crate) struct BoundPreferenceEdit {
    pub worker: FlowWorker,
    pub intent: PreferenceIntent,
    pub providers: Option<ProviderRegistry>,
}

/// Inspect previously recorded acknowledgements without attaching or repairing a Session.
pub async fn read_preference_outcome(
    data_dir: std::path::PathBuf,
    flow: crate::flow_store::FlowRef,
    request: Uuid,
) -> io::Result<PreferenceOutcome> {
    crate::read_work::capture(move || {
        match crate::flow_store::RuntimeStore::inspect_session(&data_dir, flow.session_id.0) {
            Ok(session) => Ok(session
                .projection
                .preference_outcome(&flow.instance_id, request)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(PreferenceOutcome::NotFound)
            }
            Err(error) => Err(error),
        }
    })
    .await
    .map_err(io::Error::other)?
}

impl BoundPreferenceEdit {
    /// Validate against current config and connection identity; repeat under the global file lock.
    pub(crate) fn normalize(
        &self,
        sparse: &SessionInputPreferences,
        text: &str,
    ) -> Result<PreferenceEdit, PreferenceOutcome> {
        if self.worker.is_stopping() || self.worker.is_stopped() {
            return Err(PreferenceRejection::InvalidTarget.into());
        }
        let context = self.worker.context();
        let models = context
            .model_registry()
            .ok_or(PreferenceRejection::Unavailable)?
            .snapshot()
            .with_config_text(text)
            .map_err(|_| PreferenceRejection::Unavailable)?;
        let (reasoning, trust) =
            ConfigHub::input_defaults_from_text(text).map_err(|_| PreferenceRejection::Storage)?;
        let revisions = revisions(
            self.worker.preference_binding,
            sparse,
            &models,
            &reasoning,
            &trust,
        )
        .map_err(|_| PreferenceRejection::Storage)?;
        let edit = &self.intent.edit;
        if revisions.for_edit(edit)? != self.intent.expected {
            return Err(PreferenceOutcome::Conflict);
        }
        Ok(match edit {
            PreferenceEdit::Model(key) => PreferenceEdit::Model(resolve_input_model(
                &models,
                self.providers
                    .as_ref()
                    .ok_or(PreferenceRejection::Unavailable)?,
                key,
                sparse.model.as_deref(),
            )?),
            PreferenceEdit::Reasoning(value) => {
                if let Some(selection) = value.selection() {
                    let call = models
                        .model_call_config_for_input(DEFAULT_INPUT_MODEL, sparse.model.as_deref());
                    if call.provider().is_none() || !call.provider_enabled() {
                        return Err(PreferenceRejection::Unavailable.into());
                    }
                    // Keep the authored typed choice; resolution is checked for this target only.
                    call.resolve_reasoning(selection)
                        .map_err(|_| PreferenceRejection::InvalidSelection)?;
                }
                edit.clone()
            }
            PreferenceEdit::Escalation(_)
                if sparse.mode.unwrap_or(trust.mode) != TrustMode::Eager =>
            {
                return Err(PreferenceRejection::NotEager.into());
            }
            _ => edit.clone(),
        })
    }
}

/// Resolve a picker key and require an enabled connection in the original registry scope.
pub fn resolve_input_model(
    models: &ModelRegistrySnapshot,
    providers: &ProviderRegistry,
    key: &str,
    saved: Option<&str>,
) -> Result<String, PreferenceRejection> {
    if models
        .model_entry_for_input(key, saved)
        .is_none_or(|entry| entry.enabled == Some(false))
    {
        return Err(PreferenceRejection::InvalidSelection);
    }
    let call = models.model_call_config_for_input(key, saved);
    if providers
        .resolve_call(&call)
        .map_err(|_| PreferenceRejection::Unavailable)?
        .is_none()
    {
        return Err(PreferenceRejection::Unavailable);
    }
    Ok(call.info.name)
}

fn revision(binding: Uuid, field: &str, value: impl Serialize) -> io::Result<String> {
    let bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    let mut hash = blake3::Hasher::new_derive_key("atman composer preference revision");
    hash.update(binding.as_bytes());
    hash.update(field.as_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().to_hex().to_string())
}

pub(super) fn revisions(
    binding: Uuid,
    sparse: &SessionInputPreferences,
    models: &ModelRegistrySnapshot,
    reasoning: &super::preferences::ComposerReasoning,
    trust: &crate::trust::TrustConfig,
) -> io::Result<ComposerWriteRevisions> {
    let mut choices: Vec<(String, String, String)> = Vec::new();
    models.visit_input_models(sparse.model.as_deref(), |key, _, model, provider| {
        choices.push((key.to_owned(), model.to_owned(), provider.to_owned()));
        true
    });
    let aliases = models.all_aliases();
    let targets: Vec<_> = models
        .all_model_entries()
        .into_iter()
        .map(|(key, entry)| (key, entry.model, entry.provider, entry.api, entry.enabled))
        .collect();
    let providers: Vec<_> = models
        .all_provider_entries()
        .into_iter()
        .map(|(key, entry)| (key, entry.kind, entry.enabled, entry.api, entry.base_url))
        .collect();
    let call = models.model_call_config_for_input(DEFAULT_INPUT_MODEL, sparse.model.as_deref());
    let reasoning_choices = models.reasoning_selections_for_model(&call.info.name);
    Ok(ComposerWriteRevisions {
        model: revision(
            binding,
            "model",
            (&sparse.model, aliases, choices, targets, providers),
        )?,
        reasoning: revision(
            binding,
            "reasoning",
            (
                &sparse.reasoning,
                reasoning,
                &call.info.name,
                &call.api_model,
                call.provider(),
                call.api,
                call.provider_enabled(),
                &call.info.reasoning,
                reasoning_choices,
            ),
        )?,
        trust_mode: revision(binding, "trust_mode", (sparse.mode, trust.mode))?,
        escalation: revision(
            binding,
            "escalation",
            (
                sparse.escalation,
                trust.escalation,
                sparse.mode.unwrap_or(trust.mode),
            ),
        )?,
    })
}
