use std::collections::HashSet;

use ratatui::layout::Rect;

/// WM-internal window identity. Unique per window instance.
/// Uses a u64 counter — no external dependency required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u64);

impl std::fmt::Display for WindowId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "win-{}", self.0)
    }
}

/// Logical content identity — determines reuse when opening.
/// Two windows with the same `ContentKey` may be reused depending on `OpenPolicy`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ContentKey {
    Task(String),
    History,
    Activity(String),
    Mermaid(String),
    Output(String),
    Cheatsheet,
    Mcp,
    Knowledge,
    Projects,
}

/// Whether opening content with an existing key reuses or creates a new window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenPolicy {
    ReuseExisting,
    AlwaysNew,
}

/// Whether opening a window should steal floating-panel focus.
/// Used to distinguish user-initiated opens (always steal) from
/// background-completion opens (only steal when nothing is focused).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPolicy {
    /// Focus the opened (or reused) window.
    Steal,
    /// Leave the current floating focus untouched.
    Preserve,
}

/// What kind of content a window displays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowContent {
    Task {
        handle: String,
        kind: atman_runtime::TaskKind,
    },
    History,
    Activity {
        run_id: String,
    },
    Mermaid {
        item_id: String,
    },
    Output {
        item_id: String,
    },
    Cheatsheet,
    Mcp,
    Knowledge,
    Projects,
}

impl WindowContent {
    pub fn content_key(&self) -> ContentKey {
        match self {
            WindowContent::Task { handle, .. } => ContentKey::Task(handle.clone()),
            WindowContent::History => ContentKey::History,
            WindowContent::Activity { run_id } => ContentKey::Activity(run_id.clone()),
            WindowContent::Mermaid { item_id } => ContentKey::Mermaid(item_id.clone()),
            WindowContent::Output { item_id } => ContentKey::Output(item_id.clone()),
            WindowContent::Cheatsheet => ContentKey::Cheatsheet,
            WindowContent::Mcp => ContentKey::Mcp,
            WindowContent::Knowledge => ContentKey::Knowledge,
            WindowContent::Projects => ContentKey::Projects,
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            WindowContent::Task { kind, .. } => match kind {
                atman_runtime::TaskKind::Bash => "$",
                atman_runtime::TaskKind::Terminal => "▶",
                atman_runtime::TaskKind::Flow => "⬡",
            },
            WindowContent::History => "⊞",
            WindowContent::Activity { .. } => "▸",
            WindowContent::Mermaid { .. } => "◇",
            WindowContent::Output { .. } => "≡",
            WindowContent::Cheatsheet => "?",
            WindowContent::Mcp => "⚡",
            WindowContent::Knowledge => "§",
            WindowContent::Projects => "▦",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowMode {
    Floating,
    Maximized,
    Hidden,
}

#[derive(Debug, Clone)]
pub struct WindowState {
    pub mode: WindowMode,
    pub rect: Rect,
    pub restore_rect: Option<Rect>,
    pub scroll: u32,
    pub h_scroll: u16,
    pub z: u32,
    pub split: bool,
    pub expanded_tools: HashSet<String>,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            mode: WindowMode::Floating,
            rect: Rect::default(),
            restore_rect: None,
            scroll: 0,
            h_scroll: 0,
            z: 0,
            split: false,
            expanded_tools: HashSet::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WindowCapabilities {
    pub resizable: bool,
    pub maximizable: bool,
    pub closable: bool,
    pub splitable: bool,
    pub scrollable: bool,
}

/// A window instance — the WM's unit of management.
#[derive(Debug, Clone)]
pub struct WindowInstance {
    pub id: WindowId,
    pub title: String,
    pub content: WindowContent,
    pub state: WindowState,
    pub capabilities: WindowCapabilities,
}
