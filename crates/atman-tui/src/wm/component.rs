use std::collections::HashSet;

use crossterm::event::MouseEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use crate::keys::KeyAction;
use crate::wm::PanelBtn;

use super::window::{ContentKey, WindowContent, WindowId};

#[derive(Debug, Clone)]
pub enum WmEvent {
    Key(KeyAction),
    Mouse(MouseEvent),
    Paste(String),
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
    OpenContentPanel {
        label: String,
        key: ContentKey,
        title: String,
        window_content: WindowContent,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpPanelAction {
    Add,
    Edit,
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
    McpAction(McpPanelAction),
    ToolHeader(String),
}

/// Immutable render data passed to `WindowComponent::render_content`.
pub struct RenderCtx<'a> {
    pub window_id: crate::wm::WindowId,
    pub snapshots: &'a [atman_runtime::TaskSnapshot],
    pub items: &'a [crate::app::OutputItem],
    pub item_revisions: &'a [crate::app::OutputRevision],
    pub handle_index: &'a std::collections::HashMap<String, usize>,
    pub detached_task_details:
        &'a std::collections::HashMap<String, crate::app::DetachedTaskDetail>,
    pub task_handle_index: &'a std::collections::HashMap<String, usize>,
    pub workflow_run_to_panel: &'a std::collections::HashMap<String, usize>,
    pub task_snapshots_revision: u64,
    pub interaction_revision: u64,
    pub animation_frame: u32,
    pub expanded_tools: &'a HashSet<String>,
    pub activity_nodes: &'a [crate::task_panel::ActivityNode],
    pub mcp_servers: &'a [atman_runtime::mcp::McpServerStatus],
    pub expanded_mcp_servers: &'a std::collections::HashSet<String>,
    pub mcp_selected: usize,
    pub hovered_mcp_row: &'a Option<String>,
    pub mcp_browser: &'a crate::mcp_manager::McpBrowserState<'a>,
    pub hovered_history_row: &'a Option<String>,
}

impl<'a> RenderCtx<'a> {
    pub fn task_detail(
        &self,
        handle: &str,
    ) -> Option<(
        usize,
        &'a crate::app::OutputItem,
        crate::app::OutputRevision,
    )> {
        let (index, item) = crate::app::resolve_task_detail(
            handle,
            self.items,
            self.handle_index,
            self.detached_task_details,
        )?;
        let revision = if index == usize::MAX {
            let revision = self.detached_task_details.get(handle)?.revision;
            crate::app::OutputRevision {
                id: revision,
                semantic: revision,
                layout: revision,
                source_generation: revision,
                ..Default::default()
            }
        } else {
            self.item_revisions.get(index).copied().unwrap_or_default()
        };
        Some((index, item, revision))
    }
}

/// Mutable event context — allows components to send commands and mutate
/// their own scroll/state.
pub struct EventCtx<'a> {
    pub scroll: &'a mut u32,
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
    fn handle_event(&mut self, event: &WmEvent, ctx: &mut EventCtx) -> WmEventResult;

    /// Whether the focused content placed a visible terminal cursor this frame.
    fn cursor_visible(&self) -> bool {
        false
    }

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

    /// Optional suffix appended to the title (e.g., Mermaid's "Tab: split").
    fn title_suffix(&self) -> Option<String> {
        None
    }

    /// Sync scroll/state from the Window shell into this content before
    /// rendering. Default no-op.
    fn sync_state(&mut self, _scroll: u32, _h_scroll: u16, _split: bool) {}

    /// Extract current scroll/state from this content after rendering.
    /// Returns `(scroll, h_scroll, split)`. Used to sync clamped values
    /// (e.g. scroll→max_scroll) back to the Window shell.
    fn extract_state(&self) -> (u32, u16, bool) {
        (0, 0, false)
    }
}
