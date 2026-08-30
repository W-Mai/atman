/// Trust mode controls how aggressively tools auto-approve.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    documented::DocumentedVariants,
)]
#[serde(rename_all = "lowercase")]
pub enum TrustMode {
    /// Auto-approve only Tier::Zero (read-only) tools; everything else needs manual approval.
    Calm,
    #[default]
    /// Auto-approve Tier::Zero and Tier::One; Tier::Two+ needs approval.
    Steady,
    /// Auto-approve up to Tier::Two; Tier::Three+ needs approval.
    Eager,
    /// Auto-approve everything including dangerous operations.
    Reckless,
}

impl TrustMode {
    pub fn sandbox_enabled(self) -> bool {
        !matches!(self, Self::Reckless)
    }

    pub fn level(self) -> u8 {
        match self {
            Self::Calm => 1,
            Self::Steady => 2,
            Self::Eager => 3,
            Self::Reckless => 4,
        }
    }

    pub fn needs_warning(self) -> bool {
        matches!(self, Self::Eager | Self::Reckless)
    }

    pub fn warning(self, display: &ModeDisplay) -> Option<String> {
        match self {
            Self::Eager => Some(format!(
                "⚠ {} mode: sandbox guards bash/fs. Workspace-internal ops auto-approved. \
                 Network is unrestricted. Escalated risks follow the configured \
                 escalation policy (deny / ask / allow).",
                display.name
            )),
            Self::Reckless => Some(format!(
                "⚠ {} mode: sandbox is off. The agent can read/write outside the workspace, \
                 run arbitrary commands, and access the network — all without confirmation. \
                 Make sure you understand the risk.\n\n\
                 Recommended only for one-off / sandbox projects, not for production repos \
                 or directories with sensitive data.",
                display.name
            )),
            _ => None,
        }
    }

    pub fn all() -> [TrustMode; 4] {
        [Self::Calm, Self::Steady, Self::Eager, Self::Reckless]
    }
}

impl std::str::FromStr for TrustMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "calm" | "1" => Ok(Self::Calm),
            "steady" | "2" | "default" => Ok(Self::Steady),
            "eager" | "3" => Ok(Self::Eager),
            "reckless" | "yolo" | "4" => Ok(Self::Reckless),
            other => Err(format!("unknown trust mode `{other}`")),
        }
    }
}

/// The action selected by controlled permission policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    Auto,
    Ask,
    Deny,
}

impl PolicyAction {
    pub fn most_restrictive(self, other: Self) -> Self {
        match (self, other) {
            (Self::Deny, _) | (_, Self::Deny) => Self::Deny,
            (Self::Ask, _) | (_, Self::Ask) => Self::Ask,
            (Self::Auto, Self::Auto) => Self::Auto,
        }
    }
}

/// How Eager mode handles risks that require escalation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscalationPolicy {
    Deny,
    #[default]
    Ask,
    Allow,
}

/// How Eager transformed an otherwise-Ask policy result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyEscalation {
    None,
    Denied,
    Pending,
    Allowed,
}

/// Policy result retaining whether an automatic decision came from Eager Allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyResolution {
    pub action: PolicyAction,
    pub escalation: PolicyEscalation,
}

impl EscalationPolicy {
    pub fn next(self) -> Self {
        match self {
            Self::Deny => Self::Ask,
            Self::Ask => Self::Allow,
            Self::Allow => Self::Deny,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

/// Whether Atman's permission controls apply to an execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionPolicy {
    Controlled,
    Unrestricted,
}

/// Structured resource risks combined with a tool's Tier policy.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RiskKind {
    WorkspaceExternal,
    Network,
    Irreversible,
    FilesystemWrite,
    ProcessSpawn,
    RepositoryMutation,
}

/// Optional Eager-mode overrides for each tool Tier.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TierPolicyOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier0: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier1: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier2: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier3: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier4: Option<PolicyAction>,
}

impl TierPolicyOverrides {
    fn resolve(&self, tier: crate::tool::Tier) -> PolicyAction {
        let configured = match tier {
            crate::tool::Tier::Zero => self.tier0,
            crate::tool::Tier::One => self.tier1,
            crate::tool::Tier::Two => self.tier2,
            crate::tool::Tier::Three => self.tier3,
            crate::tool::Tier::Four => self.tier4,
        };
        configured.unwrap_or_else(|| default_eager_tier_action(tier))
    }
}

/// Mode-specific Tier overrides. Calm and Steady intentionally have no configurable entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TierPolicyConfig {
    #[serde(default)]
    pub eager: TierPolicyOverrides,
}

/// Optional Eager-mode overrides for structured resource risks.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskPolicyOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_workspace: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub irreversible: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem_write: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_spawn: Option<PolicyAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_mutation: Option<PolicyAction>,
}

impl RiskPolicyOverrides {
    fn resolve(&self, risk: RiskKind) -> PolicyAction {
        match risk {
            RiskKind::WorkspaceExternal => self.outside_workspace,
            RiskKind::Network => self.network,
            RiskKind::Irreversible => self.irreversible,
            RiskKind::FilesystemWrite => self.filesystem_write,
            RiskKind::ProcessSpawn => self.process_spawn,
            RiskKind::RepositoryMutation => self.repository_mutation,
        }
        .unwrap_or(PolicyAction::Ask)
    }
}

/// Mode-specific risk overrides. Calm and Steady retain fixed safety floors.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskPolicyConfig {
    #[serde(default)]
    pub eager: RiskPolicyOverrides,
}

fn default_eager_tier_action(tier: crate::tool::Tier) -> PolicyAction {
    match tier {
        crate::tool::Tier::Zero | crate::tool::Tier::One => PolicyAction::Auto,
        crate::tool::Tier::Two | crate::tool::Tier::Three | crate::tool::Tier::Four => {
            PolicyAction::Ask
        }
    }
}

/// Display theme for trust mode labels in the TUI.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    Default,
    documented::DocumentedVariants,
)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    /// Standard English labels.
    Default,
    /// Wuxia (martial arts) themed labels.
    Wuxia,
    /// Animal-themed labels.
    Animal,
    /// Weather-themed labels.
    Weather,
    /// Drink-themed labels.
    Drink,
}

impl std::str::FromStr for Theme {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "wuxia" => Ok(Self::Wuxia),
            "animal" => Ok(Self::Animal),
            "weather" => Ok(Self::Weather),
            "drink" => Ok(Self::Drink),
            other => Err(format!("unknown theme `{other}`")),
        }
    }
}

impl std::fmt::Display for Theme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => write!(f, "default"),
            Self::Wuxia => write!(f, "wuxia"),
            Self::Animal => write!(f, "animal"),
            Self::Weather => write!(f, "weather"),
            Self::Drink => write!(f, "drink"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeColor {
    Cyan,
    Green,
    Yellow,
    Red,
    Orange,
}

#[derive(Debug, Clone)]
pub struct ModeDisplay {
    pub name: &'static str,
    pub emoji: &'static str,
    pub color: ModeColor,
    pub description: &'static str,
}

#[derive(Debug, Clone)]
pub struct EscalationDisplay {
    pub name: &'static str,
    pub emoji: &'static str,
    pub color: ModeColor,
}

impl Theme {
    pub fn display(&self, mode: TrustMode) -> ModeDisplay {
        match (self, mode) {
            (Theme::Default, TrustMode::Calm) => ModeDisplay {
                name: "calm",
                emoji: "🌙",
                color: ModeColor::Cyan,
                description: "confirm every step",
            },
            (Theme::Default, TrustMode::Steady) => ModeDisplay {
                name: "steady",
                emoji: "✓",
                color: ModeColor::Green,
                description: "auto low-risk work, confirm escalation",
            },
            (Theme::Default, TrustMode::Eager) => ModeDisplay {
                name: "eager",
                emoji: "⚡",
                color: ModeColor::Yellow,
                description: "auto routine work, escalation policy controls risk",
            },
            (Theme::Default, TrustMode::Reckless) => ModeDisplay {
                name: "reckless",
                emoji: "🔥",
                color: ModeColor::Red,
                description: "all off, you decide",
            },

            (Theme::Wuxia, TrustMode::Calm) => ModeDisplay {
                name: "守拙",
                emoji: "🧘",
                color: ModeColor::Cyan,
                description: "大巧若拙，步步为营",
            },
            (Theme::Wuxia, TrustMode::Steady) => ModeDisplay {
                name: "行云",
                emoji: "☁️",
                color: ModeColor::Green,
                description: "行云流水，任意所至",
            },
            (Theme::Wuxia, TrustMode::Eager) => ModeDisplay {
                name: "破竹",
                emoji: "🎋",
                color: ModeColor::Yellow,
                description: "势如破竹，迎刃而解",
            },
            (Theme::Wuxia, TrustMode::Reckless) => ModeDisplay {
                name: "逍遥",
                emoji: "🕊️",
                color: ModeColor::Red,
                description: "逍遥御风，无招胜有招",
            },

            (Theme::Animal, TrustMode::Calm) => ModeDisplay {
                name: "hedgehog",
                emoji: "🦔",
                color: ModeColor::Cyan,
                description: "curls up, asks about everything",
            },
            (Theme::Animal, TrustMode::Steady) => ModeDisplay {
                name: "cat",
                emoji: "🐱",
                color: ModeColor::Green,
                description: "roams its territory, wary of strangers",
            },
            (Theme::Animal, TrustMode::Eager) => ModeDisplay {
                name: "dog",
                emoji: "🐶",
                color: ModeColor::Yellow,
                description: "fence guards, charges ahead",
            },
            (Theme::Animal, TrustMode::Reckless) => ModeDisplay {
                name: "honey-badger",
                emoji: "🦡",
                color: ModeColor::Red,
                description: "doesn't give a damn",
            },

            (Theme::Weather, TrustMode::Calm) => ModeDisplay {
                name: "drizzle",
                emoji: "🌧",
                color: ModeColor::Cyan,
                description: "light rain, step carefully",
            },
            (Theme::Weather, TrustMode::Steady) => ModeDisplay {
                name: "clear",
                emoji: "☀️",
                color: ModeColor::Green,
                description: "clear sky, normal pace",
            },
            (Theme::Weather, TrustMode::Eager) => ModeDisplay {
                name: "storm",
                emoji: "⛈",
                color: ModeColor::Yellow,
                description: "storm, coat on, push forward",
            },
            (Theme::Weather, TrustMode::Reckless) => ModeDisplay {
                name: "tornado",
                emoji: "🌪",
                color: ModeColor::Red,
                description: "tornado, hold nothing back",
            },

            (Theme::Drink, TrustMode::Calm) => ModeDisplay {
                name: "water",
                emoji: "💧",
                color: ModeColor::Cyan,
                description: "plain and safe",
            },
            (Theme::Drink, TrustMode::Steady) => ModeDisplay {
                name: "coffee",
                emoji: "☕",
                color: ModeColor::Green,
                description: "normal kick",
            },
            (Theme::Drink, TrustMode::Eager) => ModeDisplay {
                name: "espresso",
                emoji: "☕",
                color: ModeColor::Yellow,
                description: "double shot, go fast",
            },
            (Theme::Drink, TrustMode::Reckless) => ModeDisplay {
                name: "bleach",
                emoji: "🧪",
                color: ModeColor::Red,
                description: "drink it and it's gone",
            },
        }
    }

    pub fn escalation_display(&self, escalation: EscalationPolicy) -> EscalationDisplay {
        match (self, escalation) {
            (Theme::Default, EscalationPolicy::Deny) => EscalationDisplay {
                name: "deny",
                emoji: "🔒",
                color: ModeColor::Orange,
            },
            (Theme::Default, EscalationPolicy::Ask) => EscalationDisplay {
                name: "ask",
                emoji: "⚠️",
                color: ModeColor::Yellow,
            },
            (Theme::Default, EscalationPolicy::Allow) => EscalationDisplay {
                name: "allow",
                emoji: "✅",
                color: ModeColor::Green,
            },
            (Theme::Wuxia, EscalationPolicy::Deny) => EscalationDisplay {
                name: "画地为牢",
                emoji: "⛩️",
                color: ModeColor::Orange,
            },
            (Theme::Wuxia, EscalationPolicy::Ask) => EscalationDisplay {
                name: "请示",
                emoji: "📜",
                color: ModeColor::Yellow,
            },
            (Theme::Wuxia, EscalationPolicy::Allow) => EscalationDisplay {
                name: "放行",
                emoji: "🎋",
                color: ModeColor::Green,
            },
            (Theme::Animal, EscalationPolicy::Deny) => EscalationDisplay {
                name: "turtle",
                emoji: "🐢",
                color: ModeColor::Orange,
            },
            (Theme::Animal, EscalationPolicy::Ask) => EscalationDisplay {
                name: "owl",
                emoji: "🦉",
                color: ModeColor::Yellow,
            },
            (Theme::Animal, EscalationPolicy::Allow) => EscalationDisplay {
                name: "bird",
                emoji: "🐦",
                color: ModeColor::Green,
            },
            (Theme::Weather, EscalationPolicy::Deny) => EscalationDisplay {
                name: "fog",
                emoji: "🌫",
                color: ModeColor::Orange,
            },
            (Theme::Weather, EscalationPolicy::Ask) => EscalationDisplay {
                name: "cloud",
                emoji: "☁️",
                color: ModeColor::Yellow,
            },
            (Theme::Weather, EscalationPolicy::Allow) => EscalationDisplay {
                name: "clear",
                emoji: "☀️",
                color: ModeColor::Green,
            },
            (Theme::Drink, EscalationPolicy::Deny) => EscalationDisplay {
                name: "lock-in",
                emoji: "🍺",
                color: ModeColor::Orange,
            },
            (Theme::Drink, EscalationPolicy::Ask) => EscalationDisplay {
                name: "card",
                emoji: "💳",
                color: ModeColor::Yellow,
            },
            (Theme::Drink, EscalationPolicy::Allow) => EscalationDisplay {
                name: "open-tab",
                emoji: "🧾",
                color: ModeColor::Green,
            },
        }
    }
}

/// User-configurable trust settings.
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    documented::Documented,
    documented::DocumentedFields,
)]
#[serde(deny_unknown_fields)]
pub struct TrustConfig {
    /// How aggressively tools are auto-approved.
    #[serde(default)]
    pub mode: TrustMode,
    /// Display theme for trust mode labels.
    #[serde(default)]
    pub theme: Theme,
    /// How Eager mode handles policy decisions that require escalation.
    #[serde(default)]
    pub escalation: EscalationPolicy,
    /// Mode-specific Tier policy overrides.
    #[serde(default)]
    pub tiers: TierPolicyConfig,
    /// Mode-specific resource-risk policy overrides.
    #[serde(default)]
    pub risks: RiskPolicyConfig,
}

impl TrustConfig {
    pub fn display(&self) -> ModeDisplay {
        self.theme.display(self.mode)
    }

    pub fn execution_policy(&self) -> ExecutionPolicy {
        match self.mode {
            TrustMode::Reckless => ExecutionPolicy::Unrestricted,
            TrustMode::Calm | TrustMode::Steady | TrustMode::Eager => ExecutionPolicy::Controlled,
        }
    }

    pub fn resolve_tier(&self, tier: crate::tool::Tier) -> PolicyAction {
        match self.mode {
            TrustMode::Calm => match tier {
                crate::tool::Tier::Zero => PolicyAction::Auto,
                crate::tool::Tier::One
                | crate::tool::Tier::Two
                | crate::tool::Tier::Three
                | crate::tool::Tier::Four => PolicyAction::Ask,
            },
            TrustMode::Steady => match tier {
                crate::tool::Tier::Zero | crate::tool::Tier::One => PolicyAction::Auto,
                crate::tool::Tier::Two | crate::tool::Tier::Three | crate::tool::Tier::Four => {
                    PolicyAction::Ask
                }
            },
            TrustMode::Eager => self.tiers.eager.resolve(tier),
            TrustMode::Reckless => PolicyAction::Auto,
        }
    }

    pub fn resolve_risk(&self, risk: RiskKind) -> PolicyAction {
        match self.mode {
            TrustMode::Calm | TrustMode::Steady => PolicyAction::Ask,
            TrustMode::Eager => self.risks.eager.resolve(risk),
            TrustMode::Reckless => PolicyAction::Auto,
        }
    }

    /// Resolves the controlled policy before grants or hard-boundary checks.
    pub fn resolve_policy(
        &self,
        tier: crate::tool::Tier,
        risks: impl IntoIterator<Item = RiskKind>,
    ) -> PolicyAction {
        self.resolve_policy_resolution(tier, risks).action
    }

    pub fn resolve_policy_resolution(
        &self,
        tier: crate::tool::Tier,
        risks: impl IntoIterator<Item = RiskKind>,
    ) -> PolicyResolution {
        if self.execution_policy() == ExecutionPolicy::Unrestricted {
            return PolicyResolution {
                action: PolicyAction::Auto,
                escalation: PolicyEscalation::None,
            };
        }

        let action = risks
            .into_iter()
            .fold(self.resolve_tier(tier), |action, risk| {
                action.most_restrictive(self.resolve_risk(risk))
            });

        if self.mode != TrustMode::Eager || action != PolicyAction::Ask {
            return PolicyResolution {
                action,
                escalation: PolicyEscalation::None,
            };
        }
        match self.escalation {
            EscalationPolicy::Deny => PolicyResolution {
                action: PolicyAction::Deny,
                escalation: PolicyEscalation::Denied,
            },
            EscalationPolicy::Ask => PolicyResolution {
                action: PolicyAction::Ask,
                escalation: PolicyEscalation::Pending,
            },
            EscalationPolicy::Allow => PolicyResolution {
                action: PolicyAction::Auto,
                escalation: PolicyEscalation::Allowed,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_disabled_only_for_reckless() {
        assert!(TrustMode::Calm.sandbox_enabled());
        assert!(TrustMode::Steady.sandbox_enabled());
        assert!(TrustMode::Eager.sandbox_enabled());
        assert!(!TrustMode::Reckless.sandbox_enabled());
    }

    #[test]
    fn needs_warning_for_eager_and_reckless() {
        assert!(!TrustMode::Calm.needs_warning());
        assert!(!TrustMode::Steady.needs_warning());
        assert!(TrustMode::Eager.needs_warning());
        assert!(TrustMode::Reckless.needs_warning());
    }

    #[test]
    fn warning_text_includes_mode_name() {
        let cfg = TrustConfig {
            mode: TrustMode::Eager,
            theme: Theme::Default,
            ..TrustConfig::default()
        };
        let display = cfg.display();
        let warning = TrustMode::Eager.warning(&display).unwrap();
        assert!(warning.contains("eager"));

        let cfg2 = TrustConfig {
            mode: TrustMode::Reckless,
            theme: Theme::Animal,
            ..TrustConfig::default()
        };
        let display2 = cfg2.display();
        let warning2 = TrustMode::Reckless.warning(&display2).unwrap();
        assert!(warning2.contains("honey-badger"));
    }

    #[test]
    fn wuxia_escalation_display_preserves_localized_labels() {
        assert_eq!(
            Theme::Wuxia.escalation_display(EscalationPolicy::Deny).name,
            "画地为牢"
        );
        assert_eq!(
            Theme::Wuxia.escalation_display(EscalationPolicy::Ask).name,
            "请示"
        );
        assert_eq!(
            Theme::Wuxia
                .escalation_display(EscalationPolicy::Allow)
                .name,
            "放行"
        );
    }

    #[test]
    fn all_themes_produce_displays() {
        let themes = [
            Theme::Default,
            Theme::Wuxia,
            Theme::Animal,
            Theme::Weather,
            Theme::Drink,
        ];
        for theme in &themes {
            for mode in &TrustMode::all() {
                let d = theme.display(*mode);
                assert!(!d.name.is_empty());
                assert!(!d.emoji.is_empty());
                assert!(!d.description.is_empty());
            }
        }
    }

    #[test]
    fn mode_from_str_parses_all_variants() {
        assert_eq!("calm".parse::<TrustMode>().unwrap(), TrustMode::Calm);
        assert_eq!("steady".parse::<TrustMode>().unwrap(), TrustMode::Steady);
        assert_eq!("eager".parse::<TrustMode>().unwrap(), TrustMode::Eager);
        assert_eq!(
            "reckless".parse::<TrustMode>().unwrap(),
            TrustMode::Reckless
        );
        assert_eq!("yolo".parse::<TrustMode>().unwrap(), TrustMode::Reckless);
        assert_eq!("1".parse::<TrustMode>().unwrap(), TrustMode::Calm);
        assert_eq!("4".parse::<TrustMode>().unwrap(), TrustMode::Reckless);
        assert!("unknown".parse::<TrustMode>().is_err());
    }

    #[test]
    fn theme_from_str_parses_all_variants() {
        assert_eq!("default".parse::<Theme>().unwrap(), Theme::Default);
        assert_eq!("wuxia".parse::<Theme>().unwrap(), Theme::Wuxia);
        assert_eq!("animal".parse::<Theme>().unwrap(), Theme::Animal);
        assert_eq!("weather".parse::<Theme>().unwrap(), Theme::Weather);
        assert_eq!("drink".parse::<Theme>().unwrap(), Theme::Drink);
        assert!("unknown".parse::<Theme>().is_err());
    }

    #[test]
    fn theme_display_roundtrip() {
        for theme in &[
            Theme::Default,
            Theme::Wuxia,
            Theme::Animal,
            Theme::Weather,
            Theme::Drink,
        ] {
            let s = theme.to_string();
            let back: Theme = s.parse().unwrap();
            assert_eq!(*theme, back);
        }
    }

    #[test]
    fn default_config_is_steady_approve() {
        let cfg = TrustConfig::default();
        assert_eq!(cfg.mode, TrustMode::Steady);
        assert_eq!(cfg.theme, Theme::Default);
        assert_eq!(cfg.escalation, EscalationPolicy::Ask);
    }

    #[test]
    fn wuxia_descriptions_are_chinese() {
        let d = Theme::Wuxia.display(TrustMode::Calm);
        assert!(d.description.contains("拙"));
        let d = Theme::Wuxia.display(TrustMode::Steady);
        assert!(d.description.contains("行云"));
    }

    #[test]
    fn non_wuxia_descriptions_are_english() {
        for mode in &TrustMode::all() {
            let d = Theme::Default.display(*mode);
            assert!(
                d.description.is_ascii(),
                "default theme should be ASCII: {}",
                d.description
            );
            let d = Theme::Animal.display(*mode);
            assert!(
                d.description.is_ascii(),
                "animal theme should be ASCII: {}",
                d.description
            );
        }
    }

    #[test]
    fn four_levels_ordered() {
        assert_eq!(TrustMode::Calm.level(), 1);
        assert_eq!(TrustMode::Steady.level(), 2);
        assert_eq!(TrustMode::Eager.level(), 3);
        assert_eq!(TrustMode::Reckless.level(), 4);
    }

    #[test]
    fn default_tier_matrix_matches_modes() {
        use crate::tool::Tier;

        let tiers = [Tier::Zero, Tier::One, Tier::Two, Tier::Three, Tier::Four];
        let cases = [
            (
                TrustMode::Calm,
                [
                    PolicyAction::Auto,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                ],
            ),
            (
                TrustMode::Steady,
                [
                    PolicyAction::Auto,
                    PolicyAction::Auto,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                ],
            ),
            (
                TrustMode::Eager,
                [
                    PolicyAction::Auto,
                    PolicyAction::Auto,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                    PolicyAction::Ask,
                ],
            ),
            (TrustMode::Reckless, [PolicyAction::Auto; 5]),
        ];

        for (mode, expected) in cases {
            let config = TrustConfig {
                mode,
                ..TrustConfig::default()
            };
            for (tier, action) in tiers.into_iter().zip(expected) {
                assert_eq!(
                    config.resolve_tier(tier),
                    action,
                    "mode={mode:?}, tier={tier:?}"
                );
            }
        }
    }

    #[test]
    fn calm_and_steady_keep_fixed_safety_floors() {
        use crate::tool::Tier;

        let configured = TierPolicyConfig {
            eager: TierPolicyOverrides {
                tier4: Some(PolicyAction::Auto),
                ..TierPolicyOverrides::default()
            },
        };
        for mode in [TrustMode::Calm, TrustMode::Steady] {
            let config = TrustConfig {
                mode,
                tiers: configured.clone(),
                ..TrustConfig::default()
            };
            assert_eq!(config.resolve_tier(Tier::Four), PolicyAction::Ask);
        }
    }

    #[test]
    fn eager_allow_never_overrides_explicit_deny() {
        use crate::tool::Tier;

        let config = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Allow,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier4: Some(PolicyAction::Deny),
                    ..TierPolicyOverrides::default()
                },
            },
            risks: RiskPolicyConfig {
                eager: RiskPolicyOverrides {
                    network: Some(PolicyAction::Deny),
                    ..RiskPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        };

        assert_eq!(config.resolve_policy(Tier::Four, []), PolicyAction::Deny);
        assert_eq!(
            config.resolve_policy(Tier::Zero, [RiskKind::Network]),
            PolicyAction::Deny
        );
        assert_eq!(config.resolve_policy(Tier::Three, []), PolicyAction::Auto);
    }

    #[test]
    fn eager_escalation_applies_only_to_ask() {
        use crate::tool::Tier;

        for (escalation, expected) in [
            (EscalationPolicy::Deny, PolicyAction::Deny),
            (EscalationPolicy::Ask, PolicyAction::Ask),
            (EscalationPolicy::Allow, PolicyAction::Auto),
        ] {
            let config = TrustConfig {
                mode: TrustMode::Eager,
                escalation,
                ..TrustConfig::default()
            };
            assert_eq!(config.resolve_policy(Tier::Three, []), expected);
            assert_eq!(config.resolve_policy(Tier::Zero, []), PolicyAction::Auto);
        }
    }

    #[test]
    fn every_risk_defaults_to_ask_in_controlled_modes() {
        let risks = [
            RiskKind::WorkspaceExternal,
            RiskKind::Network,
            RiskKind::Irreversible,
            RiskKind::FilesystemWrite,
            RiskKind::ProcessSpawn,
            RiskKind::RepositoryMutation,
        ];
        for mode in [TrustMode::Calm, TrustMode::Steady, TrustMode::Eager] {
            let config = TrustConfig {
                mode,
                ..TrustConfig::default()
            };
            for risk in risks {
                assert_eq!(config.resolve_risk(risk), PolicyAction::Ask);
            }
        }
    }

    #[test]
    fn reckless_is_explicitly_unrestricted() {
        use crate::tool::Tier;

        let config = TrustConfig {
            mode: TrustMode::Reckless,
            escalation: EscalationPolicy::Deny,
            ..TrustConfig::default()
        };
        assert_eq!(config.execution_policy(), ExecutionPolicy::Unrestricted);
        assert_eq!(
            config.resolve_policy(Tier::Four, [RiskKind::Network]),
            PolicyAction::Auto
        );
    }

    #[test]
    fn eager_resolution_preserves_how_ask_was_resolved() {
        use crate::tool::Tier;

        let allowed = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Allow,
            ..TrustConfig::default()
        }
        .resolve_policy_resolution(Tier::Two, [RiskKind::ProcessSpawn]);
        assert_eq!(allowed.action, PolicyAction::Auto);
        assert_eq!(allowed.escalation, PolicyEscalation::Allowed);

        let automatic = TrustConfig {
            mode: TrustMode::Eager,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier2: Some(PolicyAction::Auto),
                    ..TierPolicyOverrides::default()
                },
            },
            risks: RiskPolicyConfig {
                eager: RiskPolicyOverrides {
                    process_spawn: Some(PolicyAction::Auto),
                    ..RiskPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        }
        .resolve_policy_resolution(Tier::Two, [RiskKind::ProcessSpawn]);
        assert_eq!(automatic.action, PolicyAction::Auto);
        assert_eq!(automatic.escalation, PolicyEscalation::None);
    }

    #[test]
    fn trust_config_json_and_toml_round_trip() {
        let config = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Allow,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier4: Some(PolicyAction::Deny),
                    ..TierPolicyOverrides::default()
                },
            },
            risks: RiskPolicyConfig {
                eager: RiskPolicyOverrides {
                    network: Some(PolicyAction::Ask),
                    ..RiskPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        let from_json: TrustConfig = serde_json::from_str(&json).unwrap();
        let toml = toml::to_string(&config).unwrap();
        let from_toml: TrustConfig = toml::from_str(&toml).unwrap();
        for back in [from_json, from_toml] {
            assert_eq!(back.mode, config.mode);
            assert_eq!(back.escalation, config.escalation);
            assert_eq!(back.tiers, config.tiers);
            assert_eq!(back.risks, config.risks);
        }
    }
}
