use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sidebar::SidebarMode;

static STATE_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedUiState {
    #[serde(default)]
    pub theme: atman_runtime::trust::Theme,
    #[serde(default)]
    pub sidebar_mode: SidebarMode,
    #[serde(default = "default_true")]
    pub sidebar_visible: bool,
    #[serde(default)]
    pub sidebar_collapse_locked: bool,
    #[serde(default = "default_true")]
    pub mouse_captured: bool,
    #[serde(default)]
    pub goal_collapsed: bool,
    #[serde(default)]
    pub plan_collapsed: bool,
    #[serde(default)]
    pub todo_collapsed: bool,
    #[serde(default)]
    pub context_collapsed: bool,
    #[serde(default)]
    pub meta_collapsed: bool,
    #[serde(default)]
    pub sidebar_upper_collapsed: bool,
    #[serde(default)]
    pub sidebar_lower_collapsed: bool,
    #[serde(default)]
    pub panel_sizes: std::collections::HashMap<String, (u16, u16)>,
    #[serde(default)]
    pub task_panel_collapsed: bool,
    #[serde(default)]
    pub onboarding_skipped: bool,
    #[serde(default)]
    pub hints_dismissed: bool,
    #[serde(default)]
    pub input_reasoning: Option<atman_runtime::provider::ReasoningSelection>,
}

impl Default for PersistedUiState {
    fn default() -> Self {
        Self {
            theme: atman_runtime::trust::Theme::default(),
            sidebar_mode: SidebarMode::default(),
            sidebar_visible: true,
            sidebar_collapse_locked: false,
            mouse_captured: true,
            goal_collapsed: false,
            plan_collapsed: false,
            todo_collapsed: false,
            context_collapsed: false,
            meta_collapsed: false,
            sidebar_upper_collapsed: false,
            sidebar_lower_collapsed: false,
            panel_sizes: std::collections::HashMap::new(),
            task_panel_collapsed: false,
            onboarding_skipped: false,
            hints_dismissed: false,
            input_reasoning: None,
        }
    }
}

impl PersistedUiState {
    fn path() -> Option<std::path::PathBuf> {
        atman_runtime::storage::config_dir()
            .ok()
            .map(|d| d.join("states.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(s) => s,
                Err(e) => {
                    atman_runtime::notify!(warn, "states.json corrupt, using defaults: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                atman_runtime::notify!(error, "states.json read failed: {e}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        self.save_to(&path)
    }

    fn save_to(&self, path: &std::path::Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("states.json");
        let tmp = parent.join(format!(
            ".{file_name}.{}.{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            STATE_TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Snapshot relevant fields from an AppState for persistence.
    pub fn snapshot(app: &crate::app::AppState) -> Self {
        Self {
            theme: app.trust.theme,
            sidebar_mode: app.sidebar_mode,
            sidebar_visible: !app.sidebar_collapsed,
            sidebar_collapse_locked: app.sidebar_collapse_locked,
            mouse_captured: app.mouse_captured,
            goal_collapsed: app.goal_collapsed,
            plan_collapsed: app.plan_collapsed,
            todo_collapsed: app.todo_collapsed,
            context_collapsed: app.context_collapsed,
            meta_collapsed: app.meta_collapsed,
            sidebar_upper_collapsed: app.sidebar_upper_collapsed,
            sidebar_lower_collapsed: app.sidebar_lower_collapsed,
            panel_sizes: app.panel_sizes.clone(),
            task_panel_collapsed: app.task_panel_collapsed,
            onboarding_skipped: app.onboarding_skipped,
            hints_dismissed: app.hints_dismissed,
            input_reasoning: app.input_reasoning.clone(),
        }
    }

    /// Apply persisted state onto an AppState.
    pub fn apply(&self, app: &mut crate::app::AppState) {
        app.trust.theme = self.theme;
        app.sidebar_mode = self.sidebar_mode;
        app.sidebar_collapsed = !self.sidebar_visible;
        app.sidebar_collapse_locked = self.sidebar_collapse_locked;
        app.mouse_captured = self.mouse_captured;
        app.goal_collapsed = self.goal_collapsed;
        app.plan_collapsed = self.plan_collapsed;
        app.todo_collapsed = self.todo_collapsed;
        app.context_collapsed = self.context_collapsed;
        app.meta_collapsed = self.meta_collapsed;
        app.sidebar_upper_collapsed = self.sidebar_upper_collapsed;
        app.sidebar_lower_collapsed = self.sidebar_lower_collapsed;
        app.panel_sizes = self.panel_sizes.clone();
        app.task_panel_collapsed = self.task_panel_collapsed;
        app.onboarding_skipped = self.onboarding_skipped;
        app.hints_dismissed = self.hints_dismissed;
        app.input_reasoning = self.input_reasoning.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_serializable() {
        let state = PersistedUiState::default();
        let json = serde_json::to_string(&state).unwrap();
        let back: PersistedUiState = serde_json::from_str(&json).unwrap();
        assert!(back.sidebar_visible);
        assert!(back.mouse_captured);
        assert_eq!(back.input_reasoning, None);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("states.json");

        let state = PersistedUiState {
            mouse_captured: false,
            goal_collapsed: true,
            ..PersistedUiState::default()
        };
        let json = serde_json::to_string_pretty(&state).unwrap();
        std::fs::write(&path, &json).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let back: PersistedUiState = serde_json::from_str(&text).unwrap();
        assert!(!back.mouse_captured);
        assert!(back.goal_collapsed);
        assert!(back.sidebar_visible);
    }

    #[test]
    fn save_replaces_state_atomically_without_leaving_temp_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("states.json");
        let state = PersistedUiState {
            input_reasoning: Some(atman_runtime::provider::ReasoningSelection::Effort {
                effort: atman_runtime::provider::ReasoningEffort::XHigh,
                execution_mode: Some(atman_runtime::provider::ReasoningExecutionMode::Pro),
            }),
            ..PersistedUiState::default()
        };

        state.save_to(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let back: PersistedUiState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.input_reasoning, state.input_reasoning);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_json_defaults_input_reasoning() {
        let state: PersistedUiState =
            serde_json::from_str(r#"{"sidebar_visible":true,"mouse_captured":true}"#).unwrap();

        assert_eq!(state.input_reasoning, None);
    }

    #[test]
    fn snapshot_captures_app_state() {
        let mut app = crate::app::AppState::new("s".into(), None);
        app.input_reasoning = Some(atman_runtime::provider::ReasoningSelection::Disabled);
        let state = PersistedUiState::snapshot(&app);
        assert_eq!(state.sidebar_visible, !app.sidebar_collapsed);
        assert_eq!(state.mouse_captured, app.mouse_captured);
        assert_eq!(state.goal_collapsed, app.goal_collapsed);
        assert_eq!(state.input_reasoning, app.input_reasoning);
    }

    #[test]
    fn apply_writes_ui_fields_without_changing_permissions() {
        use atman_runtime::trust::{EscalationPolicy, Theme, TrustMode};

        let mut app = crate::app::AppState::new("s".into(), None);
        app.sidebar_collapsed = true;
        app.mouse_captured = false;
        app.trust.mode = TrustMode::Calm;
        app.trust.escalation = EscalationPolicy::Deny;

        let state = PersistedUiState {
            theme: Theme::Weather,
            sidebar_visible: true,
            mouse_captured: true,
            goal_collapsed: true,
            input_reasoning: Some(atman_runtime::provider::ReasoningSelection::Auto {
                execution_mode: None,
            }),
            ..PersistedUiState::default()
        };
        state.apply(&mut app);
        assert_eq!(app.trust.mode, TrustMode::Calm);
        assert_eq!(app.trust.theme, Theme::Weather);
        assert_eq!(app.trust.escalation, EscalationPolicy::Deny);
        assert!(!app.sidebar_collapsed);
        assert!(app.mouse_captured);
        assert!(app.goal_collapsed);
        assert_eq!(app.input_reasoning, state.input_reasoning);
    }
}
