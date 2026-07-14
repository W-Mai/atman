use crate::tool::ApprovalLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustMode {
    Calm,
    #[default]
    Steady,
    EagerApprove,
    EagerDeny,
    Reckless,
}

impl TrustMode {
    pub fn auto_ceiling(self) -> ApprovalLevel {
        match self {
            Self::Calm => ApprovalLevel::Auto,
            Self::Steady => ApprovalLevel::Approve,
            Self::EagerApprove | Self::EagerDeny | Self::Reckless => ApprovalLevel::Dangerous,
        }
    }

    pub fn sandbox_enabled(self) -> bool {
        !matches!(self, Self::Reckless)
    }

    pub fn level(self) -> u8 {
        match self {
            Self::Calm => 1,
            Self::Steady => 2,
            Self::EagerApprove => 3,
            Self::EagerDeny => 4,
            Self::Reckless => 5,
        }
    }

    pub fn needs_warning(self) -> bool {
        matches!(self, Self::EagerApprove | Self::EagerDeny | Self::Reckless)
    }

    pub fn warning(self, display: &ModeDisplay) -> Option<String> {
        match self {
            Self::EagerApprove => Some(format!(
                "⚠ {} mode: sandbox guards bash/network. Workspace-internal ops auto-approved. \
                 Operations touching outside the workspace (fs writes, sandbox-blocked bash) \
                 will prompt for approval. Make sure you trust the current working directory.",
                display.name
            )),
            Self::EagerDeny => Some(format!(
                "⚠ {} mode: sandbox guards bash/network. Workspace-internal ops auto-approved. \
                 Operations touching outside the workspace are denied outright — no prompt. \
                 The agent cannot escape the workspace without you switching modes.",
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

    pub fn all() -> [TrustMode; 5] {
        [
            Self::Calm,
            Self::Steady,
            Self::EagerApprove,
            Self::EagerDeny,
            Self::Reckless,
        ]
    }
}

impl std::str::FromStr for TrustMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "calm" | "1" => Ok(Self::Calm),
            "steady" | "2" | "default" => Ok(Self::Steady),
            "eager-approve" | "eager" | "3" => Ok(Self::EagerApprove),
            "eager-deny" | "4" => Ok(Self::EagerDeny),
            "reckless" | "yolo" | "5" => Ok(Self::Reckless),
            other => Err(format!("unknown trust mode `{other}`")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Default,
    Wuxia,
    Animal,
    Weather,
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
    Orange,
    Red,
}

#[derive(Debug, Clone)]
pub struct ModeDisplay {
    pub name: &'static str,
    pub emoji: &'static str,
    pub color: ModeColor,
    pub description: &'static str,
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
                description: "free inside workspace, confirm outside",
            },
            (Theme::Default, TrustMode::EagerApprove) => ModeDisplay {
                name: "eager",
                emoji: "⚡",
                color: ModeColor::Yellow,
                description: "auto inside, prompt outside",
            },
            (Theme::Default, TrustMode::EagerDeny) => ModeDisplay {
                name: "strict-eager",
                emoji: "🔒",
                color: ModeColor::Orange,
                description: "auto inside, deny outside",
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
            (Theme::Wuxia, TrustMode::EagerApprove) => ModeDisplay {
                name: "破竹",
                emoji: "🎋",
                color: ModeColor::Yellow,
                description: "势如破竹，迎刃而解",
            },
            (Theme::Wuxia, TrustMode::EagerDeny) => ModeDisplay {
                name: "画地为牢",
                emoji: "⛩️",
                color: ModeColor::Orange,
                description: "画地为牢，不出此界",
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
            (Theme::Animal, TrustMode::EagerApprove) => ModeDisplay {
                name: "dog",
                emoji: "🐶",
                color: ModeColor::Yellow,
                description: "fence guards, charges ahead",
            },
            (Theme::Animal, TrustMode::EagerDeny) => ModeDisplay {
                name: "turtle",
                emoji: "🐢",
                color: ModeColor::Orange,
                description: "stays in its shell, won't leave",
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
            (Theme::Weather, TrustMode::EagerApprove) => ModeDisplay {
                name: "storm",
                emoji: "⛈",
                color: ModeColor::Yellow,
                description: "storm, coat on, push forward",
            },
            (Theme::Weather, TrustMode::EagerDeny) => ModeDisplay {
                name: "fog",
                emoji: "🌫",
                color: ModeColor::Orange,
                description: "fog, can't see beyond arm's reach",
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
            (Theme::Drink, TrustMode::EagerApprove) => ModeDisplay {
                name: "espresso",
                emoji: "☕",
                color: ModeColor::Yellow,
                description: "double shot, go fast",
            },
            (Theme::Drink, TrustMode::EagerDeny) => ModeDisplay {
                name: "lock-in",
                emoji: "🍺",
                color: ModeColor::Orange,
                description: "stays at the bar, no leaving",
            },
            (Theme::Drink, TrustMode::Reckless) => ModeDisplay {
                name: "bleach",
                emoji: "🧪",
                color: ModeColor::Red,
                description: "drink it and it's gone",
            },
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TrustConfig {
    #[serde(default)]
    pub mode: TrustMode,
    #[serde(default)]
    pub theme: Theme,
}

impl TrustConfig {
    pub fn display(&self) -> ModeDisplay {
        self.theme.display(self.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_ceiling_maps_correctly() {
        assert_eq!(TrustMode::Calm.auto_ceiling(), ApprovalLevel::Auto);
        assert_eq!(TrustMode::Steady.auto_ceiling(), ApprovalLevel::Approve);
        assert_eq!(
            TrustMode::EagerApprove.auto_ceiling(),
            ApprovalLevel::Dangerous
        );
        assert_eq!(
            TrustMode::EagerDeny.auto_ceiling(),
            ApprovalLevel::Dangerous
        );
        assert_eq!(TrustMode::Reckless.auto_ceiling(), ApprovalLevel::Dangerous);
    }

    #[test]
    fn sandbox_disabled_only_for_reckless() {
        assert!(TrustMode::Calm.sandbox_enabled());
        assert!(TrustMode::Steady.sandbox_enabled());
        assert!(TrustMode::EagerApprove.sandbox_enabled());
        assert!(TrustMode::EagerDeny.sandbox_enabled());
        assert!(!TrustMode::Reckless.sandbox_enabled());
    }

    #[test]
    fn needs_warning_for_eager_and_reckless() {
        assert!(!TrustMode::Calm.needs_warning());
        assert!(!TrustMode::Steady.needs_warning());
        assert!(TrustMode::EagerApprove.needs_warning());
        assert!(TrustMode::EagerDeny.needs_warning());
        assert!(TrustMode::Reckless.needs_warning());
    }

    #[test]
    fn warning_text_includes_mode_name() {
        let cfg = TrustConfig {
            mode: TrustMode::EagerApprove,
            theme: Theme::Default,
        };
        let display = cfg.display();
        let warning = TrustMode::EagerApprove.warning(&display).unwrap();
        assert!(warning.contains("eager"));

        let cfg2 = TrustConfig {
            mode: TrustMode::Reckless,
            theme: Theme::Animal,
        };
        let display2 = cfg2.display();
        let warning2 = TrustMode::Reckless.warning(&display2).unwrap();
        assert!(warning2.contains("honey-badger"));
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
        assert_eq!(
            "eager".parse::<TrustMode>().unwrap(),
            TrustMode::EagerApprove
        );
        assert_eq!(
            "eager-approve".parse::<TrustMode>().unwrap(),
            TrustMode::EagerApprove
        );
        assert_eq!(
            "eager-deny".parse::<TrustMode>().unwrap(),
            TrustMode::EagerDeny
        );
        assert_eq!(
            "reckless".parse::<TrustMode>().unwrap(),
            TrustMode::Reckless
        );
        assert_eq!("yolo".parse::<TrustMode>().unwrap(), TrustMode::Reckless);
        assert_eq!("1".parse::<TrustMode>().unwrap(), TrustMode::Calm);
        assert_eq!("5".parse::<TrustMode>().unwrap(), TrustMode::Reckless);
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
    fn default_mode_is_steady() {
        let cfg = TrustConfig::default();
        assert_eq!(cfg.mode, TrustMode::Steady);
        assert_eq!(cfg.theme, Theme::Default);
    }

    #[test]
    fn wuxia_descriptions_are_chinese() {
        let d = Theme::Wuxia.display(TrustMode::Calm);
        assert!(d.description.contains("拙"));
        let d = Theme::Wuxia.display(TrustMode::Steady);
        assert!(d.description.contains("行云"));
        let d = Theme::Wuxia.display(TrustMode::EagerDeny);
        assert!(d.description.contains("画地"));
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
    fn five_levels_ordered() {
        assert_eq!(TrustMode::Calm.level(), 1);
        assert_eq!(TrustMode::Steady.level(), 2);
        assert_eq!(TrustMode::EagerApprove.level(), 3);
        assert_eq!(TrustMode::EagerDeny.level(), 4);
        assert_eq!(TrustMode::Reckless.level(), 5);
    }
}
