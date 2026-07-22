use serde::{Deserialize, Serialize};

use crate::sidebar::SidebarMode;

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedUiState {
    #[serde(default)]
    pub trust: atman_runtime::trust::TrustConfig,
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
}

impl Default for PersistedUiState {
    fn default() -> Self {
        Self {
            trust: atman_runtime::trust::TrustConfig::default(),
            sidebar_mode: SidebarMode::default(),
            sidebar_visible: true,
            sidebar_collapse_locked: false,
            mouse_captured: true,
            goal_collapsed: false,
            plan_collapsed: false,
            todo_collapsed: false,
            context_collapsed: false,
            meta_collapsed: false,
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
                    eprintln!("[atman] states.json corrupt, using defaults: {e}");
                    Self::default()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                eprintln!("[atman] states.json read failed: {e}");
                Self::default()
            }
        }
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Snapshot relevant fields from an AppState for persistence.
    pub fn snapshot(app: &crate::app::AppState) -> Self {
        Self {
            trust: app.trust.clone(),
            sidebar_mode: app.sidebar_mode,
            sidebar_visible: !app.sidebar_collapsed,
            sidebar_collapse_locked: app.sidebar_collapse_locked,
            mouse_captured: app.mouse_captured,
            goal_collapsed: app.goal_collapsed,
            plan_collapsed: app.plan_collapsed,
            todo_collapsed: app.todo_collapsed,
            context_collapsed: app.context_collapsed,
            meta_collapsed: app.meta_collapsed,
        }
    }

    /// Apply persisted state onto an AppState.
    pub fn apply(&self, app: &mut crate::app::AppState) {
        app.trust = self.trust.clone();
        app.sidebar_mode = self.sidebar_mode;
        app.sidebar_collapsed = !self.sidebar_visible;
        app.sidebar_collapse_locked = self.sidebar_collapse_locked;
        app.mouse_captured = self.mouse_captured;
        app.goal_collapsed = self.goal_collapsed;
        app.plan_collapsed = self.plan_collapsed;
        app.todo_collapsed = self.todo_collapsed;
        app.context_collapsed = self.context_collapsed;
        app.meta_collapsed = self.meta_collapsed;
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
    fn snapshot_captures_app_state() {
        let app = crate::app::AppState::new("s".into(), None);
        let state = PersistedUiState::snapshot(&app);
        assert_eq!(state.sidebar_visible, !app.sidebar_collapsed);
        assert_eq!(state.mouse_captured, app.mouse_captured);
        assert_eq!(state.goal_collapsed, app.goal_collapsed);
    }

    #[test]
    fn apply_writes_all_fields() {
        let mut app = crate::app::AppState::new("s".into(), None);
        app.sidebar_collapsed = true;
        app.mouse_captured = false;

        let state = PersistedUiState {
            sidebar_visible: true,
            mouse_captured: true,
            goal_collapsed: true,
            ..PersistedUiState::default()
        };
        state.apply(&mut app);
        assert!(!app.sidebar_collapsed);
        assert!(app.mouse_captured);
        assert!(app.goal_collapsed);
    }
}
