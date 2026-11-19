//! Read-only composer settings for an explicit Flow.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::flows::FlowAddress;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowComposerRequest {
    pub flow: FlowAddress,
    /// Preview this model key in the target's catalog. Omit to use its normal input alias.
    #[schema(min_length = 1, max_length = 512)]
    pub model: Option<String>,
    /// The client's draft preference, resolved without editing any server settings.
    pub reasoning: Option<ComposerReasoning>,
    /// Omit for the lightweight settings query. An empty string searches the full catalog.
    #[schema(max_length = 256)]
    pub catalog_query: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComposerReasoning {
    ProviderDefault,
    Disabled,
    Auto {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(min_length = 1, max_length = 256)]
        execution_mode: Option<String>,
    },
    Effort {
        #[schema(min_length = 1, max_length = 256)]
        effort: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[schema(min_length = 1, max_length = 256)]
        execution_mode: Option<String>,
    },
    BudgetTokens {
        tokens: u32,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ComposerModelState {
    Pending,
    Unavailable {
        reason: ComposerModelUnavailable,
    },
    Ready {
        key: String,
        model: String,
        provider: String,
        context_budget: Option<u64>,
        reasoning: Box<ComposerReasoningState>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerModelUnavailable {
    ConfigurationUnavailable,
    ConfigurationChanged,
    ModelSourceMismatch,
    ModelUnavailable,
    ProviderUnavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerReasoningState {
    pub available: bool,
    pub choices: Vec<ComposerReasoning>,
    pub configured: ComposerReasoning,
    pub requested: Option<ComposerReasoning>,
    pub effective: Option<ComposerReasoning>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerTrustMode {
    Calm,
    Steady,
    Eager,
    Reckless,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerTrustTheme {
    Default,
    Wuxia,
    Animal,
    Weather,
    Drink,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerEscalationPolicy {
    Deny,
    Ask,
    Allow,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerTrustColor {
    Cyan,
    Green,
    Yellow,
    Red,
    Orange,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerTrust {
    pub mode: ComposerTrustMode,
    pub theme: ComposerTrustTheme,
    pub name: String,
    pub icon: String,
    pub color: ComposerTrustColor,
    pub description: String,
    /// The escalation setting applies while Eager is selected.
    pub escalation: Option<ComposerEscalation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerEscalation {
    pub policy: ComposerEscalationPolicy,
    pub name: String,
    pub icon: String,
    pub color: ComposerTrustColor,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct FlowComposerResponse {
    pub flow: FlowAddress,
    pub revision: String,
    pub model: ComposerModelState,
    pub trust: ComposerTrust,
    /// Absent when the target does not have loaded ordinary Session preferences.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origins: Option<ComposerPreferenceOrigins>,
    /// Present only when requested and the original source is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<ComposerModelCatalog>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ComposerPreferenceOrigin {
    Global,
    Session,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerPreferenceOrigins {
    pub model: ComposerPreferenceOrigin,
    pub reasoning: ComposerPreferenceOrigin,
    pub trust_mode: ComposerPreferenceOrigin,
    pub escalation: ComposerPreferenceOrigin,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerModelCatalog {
    #[schema(max_items = 128)]
    pub rows: Vec<ComposerModelChoice>,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ComposerModelChoice {
    #[schema(min_length = 1, max_length = 512)]
    pub key: String,
    pub alias: bool,
    #[schema(min_length = 1, max_length = 512)]
    pub model: String,
    #[schema(min_length = 1, max_length = 512)]
    pub provider: String,
}
