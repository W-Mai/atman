//! Session-owned overrides for the ordinary conversation composer.

use std::{collections::BTreeMap, io};

use serde::{Deserialize, Serialize};

use crate::{
    provider::ReasoningSelection,
    tool::Tier,
    trust::{EscalationPolicy, PolicyAction, RiskKind, TrustConfig, TrustMode},
};

/// A saved model-default choice is distinct from an absent Session override.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "selection", rename_all = "snake_case")]
pub enum ComposerReasoning {
    #[default]
    ModelDefault,
    Selection(ReasoningSelection),
}

impl ComposerReasoning {
    pub fn selection(&self) -> Option<&ReasoningSelection> {
        match self {
            Self::ModelDefault => None,
            Self::Selection(selection) => Some(selection),
        }
    }
}

/// One operator edit. Adjacent controls can retain their independent inheritance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceEdit {
    Model(String),
    Reasoning(ComposerReasoning),
    TrustMode(TrustMode),
    Escalation(EscalationPolicy),
    EagerTier {
        tier: Tier,
        action: Option<PolicyAction>,
    },
    EagerRisk {
        risk: RiskKind,
        action: Option<PolicyAction>,
    },
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct SessionInputPreferences {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reasoning: Option<ComposerReasoning>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) mode: Option<TrustMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) escalation: Option<EscalationPolicy>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    tiers: BTreeMap<Tier, Option<PolicyAction>>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    risks: BTreeMap<RiskKind, Option<PolicyAction>>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PreferenceOrigin {
    #[default]
    Global,
    Session,
}

impl PreferenceOrigin {
    fn resolve<T>(value: &Option<T>) -> Self {
        if value.is_some() {
            Self::Session
        } else {
            Self::Global
        }
    }
}

/// Source of the persisted choice, independent of model compatibility or preview values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct InputPreferenceOrigins {
    pub model: PreferenceOrigin,
    pub reasoning: PreferenceOrigin,
    pub trust_mode: PreferenceOrigin,
    pub escalation: PreferenceOrigin,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EffectiveInputPreferences {
    /// Concrete Session model key. An absent key follows the configuration's `smart` alias.
    pub model: Option<String>,
    pub reasoning: ComposerReasoning,
    pub trust: TrustConfig,
    pub origins: InputPreferenceOrigins,
    loaded: bool,
}

impl EffectiveInputPreferences {
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
}

impl SessionInputPreferences {
    pub(crate) fn concrete_model(&self) -> Option<&String> {
        self.model.as_ref()
    }
    pub(crate) fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn apply(&mut self, edit: &PreferenceEdit) -> io::Result<()> {
        match edit {
            PreferenceEdit::Model(model) => {
                if model.trim().is_empty() || model.len() > 512 {
                    return Err(invalid("invalid Session model key"));
                }
                self.model = Some(model.clone());
            }
            PreferenceEdit::Reasoning(reasoning) => {
                if reasoning.selection().is_some_and(|selection| {
                    selection
                        .effort()
                        .is_some_and(|effort| effort.as_str().len() > 256)
                        || selection
                            .execution_mode()
                            .is_some_and(|mode| mode.as_str().len() > 256)
                }) {
                    return Err(invalid("invalid Session reasoning selection"));
                }
                self.reasoning = Some(reasoning.clone());
            }
            PreferenceEdit::TrustMode(mode) => self.mode = Some(*mode),
            PreferenceEdit::Escalation(policy) => self.escalation = Some(*policy),
            PreferenceEdit::EagerTier { tier, action } => {
                self.tiers.insert(*tier, *action);
            }
            PreferenceEdit::EagerRisk { risk, action } => {
                self.risks.insert(*risk, *action);
            }
        }
        Ok(())
    }

    pub(crate) fn resolve(
        &self,
        reasoning: ComposerReasoning,
        mut trust: TrustConfig,
    ) -> EffectiveInputPreferences {
        trust.mode = self.mode.unwrap_or(trust.mode);
        trust.escalation = self.escalation.unwrap_or(trust.escalation);
        for (&tier, &action) in &self.tiers {
            set_tier(&mut trust, tier, action);
        }
        for (&risk, &action) in &self.risks {
            set_risk(&mut trust, risk, action);
        }
        EffectiveInputPreferences {
            model: self.model.clone(),
            reasoning: self.reasoning.clone().unwrap_or(reasoning),
            trust,
            origins: InputPreferenceOrigins {
                model: PreferenceOrigin::resolve(&self.model),
                reasoning: PreferenceOrigin::resolve(&self.reasoning),
                trust_mode: PreferenceOrigin::resolve(&self.mode),
                escalation: PreferenceOrigin::resolve(&self.escalation),
            },
            loaded: true,
        }
    }
}

impl PreferenceEdit {
    pub(crate) fn trust_field(&self) -> (&'static [&'static str], &'static str) {
        match self {
            Self::TrustMode(_) => (&[], "mode"),
            Self::Escalation(_) => (&[], "escalation"),
            Self::EagerTier { tier, .. } => (
                &["tiers", "eager"],
                match tier {
                    Tier::Zero => "tier0",
                    Tier::One => "tier1",
                    Tier::Two => "tier2",
                    Tier::Three => "tier3",
                    Tier::Four => "tier4",
                },
            ),
            Self::EagerRisk { risk, .. } => (
                &["risks", "eager"],
                match risk {
                    RiskKind::WorkspaceExternal => "outside_workspace",
                    RiskKind::Network => "network",
                    RiskKind::Irreversible => "irreversible",
                    RiskKind::FilesystemWrite => "filesystem_write",
                    RiskKind::ProcessSpawn => "process_spawn",
                    RiskKind::RepositoryMutation => "repository_mutation",
                },
            ),
            Self::Model(_) | Self::Reasoning(_) => unreachable!("requires a trust preference"),
        }
    }

    pub(crate) fn apply_to_trust(&self, trust: &mut TrustConfig) {
        match *self {
            Self::TrustMode(mode) => trust.mode = mode,
            Self::Escalation(policy) => trust.escalation = policy,
            Self::EagerTier { tier, action } => set_tier(trust, tier, action),
            Self::EagerRisk { risk, action } => set_risk(trust, risk, action),
            Self::Model(_) | Self::Reasoning(_) => unreachable!("requires a trust preference"),
        }
    }
}

fn set_tier(trust: &mut TrustConfig, tier: Tier, action: Option<PolicyAction>) {
    let tiers = &mut trust.tiers.eager;
    *match tier {
        Tier::Zero => &mut tiers.tier0,
        Tier::One => &mut tiers.tier1,
        Tier::Two => &mut tiers.tier2,
        Tier::Three => &mut tiers.tier3,
        Tier::Four => &mut tiers.tier4,
    } = action;
}

fn set_risk(trust: &mut TrustConfig, risk: RiskKind, action: Option<PolicyAction>) {
    let risks = &mut trust.risks.eager;
    *match risk {
        RiskKind::WorkspaceExternal => &mut risks.outside_workspace,
        RiskKind::Network => &mut risks.network,
        RiskKind::Irreversible => &mut risks.irreversible,
        RiskKind::FilesystemWrite => &mut risks.filesystem_write,
        RiskKind::ProcessSpawn => &mut risks.process_spawn,
        RiskKind::RepositoryMutation => &mut risks.repository_mutation,
    } = action;
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
