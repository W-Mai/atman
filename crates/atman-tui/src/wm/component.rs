use std::collections::HashSet;

use crossterm::event::MouseEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::keys::KeyAction;
use crate::wm::PanelBtn;

use super::window::WindowId;

#[derive(Debug, Clone)]
pub enum WmEvent {
    Key(KeyAction),
    Mouse(MouseEvent),
    Scroll(u16, bool), // (delta, is_page)
}

#[derive(Debug)]
pub enum WmEventResult {
    Consumed(Vec<WmCommand>),
    Ignored,
}

#[derive(Debug, Clone)]
pub enum WmCommand {
    FocusWindow(WindowId),
    CloseWindow(WindowId),
    ToggleMaximize(WindowId),
    TermResize {
        handle: String,
        rows: u16,
        cols: u16,
    },
    OpenTaskPanel {
        handle: String,
        maximized: bool,
    },
    PushToast(String),
}

#[derive(Debug, Clone)]
pub enum CloseOutcome {
    Close,
    Block(String),
}

#[derive(Debug, Clone, Copy)]
pub struct SizeHint {
    pub min: (u16, u16),
    pub max: Option<(u16, u16)>,
    pub preferred: (u16, u16),
}

impl Default for SizeHint {
    fn default() -> Self {
        Self {
            min: (20, 6),
            max: None,
            preferred: (88, 29),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HitRegion {
    pub target: HitTarget,
    pub rect: Rect,
}

#[derive(Debug, Clone)]
pub enum HitTarget {
    Button(PanelBtn),
    Titlebar,
    ResizeHandle,
    Content,
    HistoryRow(String),
    WorkflowNode(usize, String),
    McpRow(String),
}

/// Immutable render data passed to `WindowComponent::render_content`.
pub struct RenderCtx<'a> {
    pub window_id: crate::wm::WindowId,
    pub snapshots: &'a [atman_runtime::TaskSnapshot],
    pub items: &'a [crate::app::OutputItem],
    pub animation_frame: u32,
    pub expanded_tools: &'a HashSet<String>,
    pub activity_nodes: &'a [crate::task_panel::ActivityNode],
    pub items_version: u64,
    pub expanded_version: u64,
    pub mcp_servers: &'a [atman_runtime::mcp::McpServerStatus],
    pub expanded_mcp_servers: &'a std::collections::HashSet<String>,
    pub mcp_selected: usize,
    pub hovered_mcp_row: &'a Option<String>,
    pub mcp_browser: &'a crate::mcp_manager::McpBrowserState<'a>,
    pub hovered_history_row: &'a Option<String>,
}

/// Mutable event context — allows components to send commands and mutate
/// their own scroll/state.
pub struct EventCtx<'a> {
    pub scroll: &'a mut u16,
    pub h_scroll: &'a mut u16,
}

/// Content contract for floating windows.
///
/// The WM shell owns border, title, buttons, drag, resize, and shadow.
/// Content rendering and event handling are delegated to this trait.
pub trait WindowComponent: Send {
    /// Render content into the given area (inside the shell border).
    /// Return hit regions for mouse dispatch.
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion>;

    /// Handle a key or mouse event. Return Consumed/Ignored.
    #[allow(dead_code)]
    fn handle_event(&mut self, event: &WmEvent, ctx: &mut EventCtx) -> WmEventResult;

    /// Preferred size for floating placement.
    #[allow(dead_code)]
    fn preferred_size(&self, viewport: Rect) -> SizeHint;

    /// Called when this window gains focus.
    #[allow(dead_code)]
    fn on_focus(&mut self) {}

    /// Called when this window loses focus.
    #[allow(dead_code)]
    fn on_blur(&mut self) {}

    /// Called when the window is about to close.
    /// Return `Block` to prevent closing (e.g., unsaved changes).
    #[allow(dead_code)]
    fn on_close(&mut self) -> CloseOutcome {
        CloseOutcome::Close
    }

    /// Called when the window's content area changes size.
    #[allow(dead_code)]
    fn on_resize(&mut self, _area: Rect, _ctx: &mut EventCtx) {}

    /// Whether this window's content should update while not focused.
    /// (e.g., live task output = true, static history = false)
    #[allow(dead_code)]
    fn wants_background_updates(&self) -> bool {
        false
    }

    /// Content version for cache invalidation. If this changes, the cache
    /// is invalidated and content is re-rendered.
    #[allow(dead_code)]
    fn content_version(&self) -> u64 {
        0
    }

    /// Optional suffix appended to the title (e.g., Mermaid's "Tab: split").
    #[allow(dead_code)]
    fn title_suffix(&self) -> Option<String> {
        None
    }

    /// Sync scroll/state from the Window shell into this content before
    /// rendering. Default no-op.
    fn sync_state(&mut self, _scroll: u16, _h_scroll: u16, _split: bool) {}

    /// Extract current scroll/state from this content after rendering.
    /// Returns `(scroll, h_scroll, split)`. Used to sync clamped values
    /// (e.g. scroll→max_scroll) back to the Window shell.
    fn extract_state(&self) -> (u16, u16, bool) {
        (0, 0, false)
    }
}
