//! Window Manager module — floating-panel implementation and type skeleton.

use std::collections::HashSet;

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear};

use atman_runtime::TaskSnapshot;

use crate::app::OutputItem;
use crate::task_panel::ActivityNode;

pub mod component;
pub mod content;
pub mod focus;
pub mod hitmap;
pub mod layer;
pub mod layer_stack;
pub mod modal;
pub mod shadow;
pub mod shell;
pub mod window;

pub use component::{
    CloseOutcome, EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmCommand,
    WmEvent, WmEventResult,
};
pub use focus::FocusState;
pub use hitmap::WmHitmap;
pub use layer::{Layer, LayerKind};
pub use layer_stack::LayerStack;
pub use modal::{
    HitTestResult, ModalAction, ModalEntry, ModalKind, ModalManager, OutsideClickPolicy,
};
pub use shadow::{
    lerp_color, multiply_color, render_bottom_fade, render_input_shadow, render_shadow,
    render_top_fade,
};
pub use window::{
    ContentKey, FocusPolicy, OpenPolicy, WindowCapabilities, WindowContent, WindowId, WindowMode,
    WindowState,
};
pub struct WindowInstance {
    pub id: WindowId,
    pub label: String,
    pub content_key: ContentKey,
    pub content_kind: WindowContent,
    pub content: Option<Box<dyn crate::wm::WindowComponent>>,
    pub title: String,
    pub rect: Rect,
    pub z: u32,
    pub maximized: bool,
    pub prev_rect: Option<Rect>,
    pub scroll: u32,
    pub h_scroll: u16,
    pub split: bool,
    pub expanded_tools: HashSet<String>,
    pub interaction_revision: u64,
}

/// Cached render output for sub-agent / workflow floating panels.
/// On a cache hit we skip workflow graph traversal, flatten_message, and
/// build_lines — just draw the stored lines and project visible hit regions
/// using the current scroll and area.
#[derive(Clone, Debug)]
pub struct PanelRenderCache {
    pub(crate) item_id: u64,
    pub(crate) content_revision: u64,
    pub(crate) interaction_revision: u64,
    pub(crate) width: u16,
    pub(crate) workflow_expanded: bool,

    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) dynamic_paint: crate::output::DynamicPaint,
    pub(crate) regions: Vec<crate::output::NodeRegion>,
    pub(crate) document_regions: Vec<crate::output::NodeRegion>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBtn {
    Minimize,
    Maximize,
    Close,
    Resize,
}

#[derive(Default)]
pub struct WmInteractionState {
    pub drag_target: Option<WindowId>,
    pub drag_offset: (u16, u16),
    pub last_titlebar_click: Option<(WindowId, std::time::Instant)>,
    pub resize_target: Option<WindowId>,
    pub resize_offset: (u16, u16),
    pub hovered_panel_btn: Option<(WindowId, PanelBtn)>,
    pub hovered_history_row: Option<String>,
    pub hovered_mcp_row: Option<String>,
    pub hovered_mcp_action: Option<component::McpPanelAction>,
    pub last_hitmap: WmHitmap,
    pub panel_close_armed_id: Option<String>,
    pub panel_close_armed_at: Option<std::time::Instant>,
}

#[derive(Default)]
pub struct WindowManager {
    pub panels: Vec<WindowInstance>,
    pub focus: FocusState,
    pub layers: LayerStack,
    pub interaction: WmInteractionState,
    pub modals: ModalManager,
    z_counter: u32,
    next_window_id: u64,
}

impl WindowManager {
    pub(crate) fn has_active_animation(&self) -> bool {
        if let Some(kind) = self.top_kind() {
            return self.modals.has_active_animation(kind);
        }
        self.panels.iter().enumerate().any(|(index, panel)| {
            panel.rect.width > 6
                && panel.rect.height > 3
                && !panel_is_covered(&self.panels, index)
                && panel
                    .content
                    .as_ref()
                    .is_some_and(|content| content.has_active_animation())
        })
    }

    pub fn arm_panel_close(&mut self, id: String) {
        self.interaction.panel_close_armed_id = Some(id);
        self.interaction.panel_close_armed_at = Some(std::time::Instant::now());
    }

    pub fn clear_panel_close_arm(&mut self) {
        self.interaction.panel_close_armed_id = None;
        self.interaction.panel_close_armed_at = None;
    }

    pub fn panel_close_arm_expired(&self) -> bool {
        match self.interaction.panel_close_armed_at {
            Some(t) => t.elapsed() > std::time::Duration::from_secs(2),
            None => true,
        }
    }

    pub fn sync_modals(&mut self) {
        let was_open = !self.layers.modal_stack.is_empty();
        let focused_id = self.focused_id();
        let restore_focus = self
            .layers
            .modal_stack
            .first()
            .and_then(|entry| entry.pre_modal_focus);
        self.layers.sync_from_kinds(&self.modals.open_kinds());
        if self.layers.modal_stack.is_empty() {
            if was_open
                && let Some(id) = restore_focus
                && self.panels.iter().any(|panel| panel.id == id)
            {
                self.focus(id);
            }
        } else {
            self.interaction.drag_target = None;
            self.interaction.resize_target = None;
            if !was_open && let Some(root) = self.layers.modal_stack.first_mut() {
                root.pre_modal_focus = focused_id;
            }
            self.focus.blur();
        }
    }

    pub fn top_kind(&self) -> Option<ModalKind> {
        self.layers.modal_stack.last().map(|entry| entry.kind)
    }

    pub fn any_modal_open(&self) -> bool {
        self.modals.any_open()
    }

    pub fn dispatch_paste(
        &mut self,
        text: &str,
        app: &mut crate::app::AppState,
        control_tx: Option<&crate::ControlSender>,
    ) -> bool {
        self.sync_modals();
        if app.modal_notification.is_some() {
            return true;
        }
        if let Some(kind) = self.top_kind() {
            self.modals.dispatch_paste(kind, text, app, control_tx);
            return true;
        }
        let Some(id) = self.focused_id() else {
            return false;
        };
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) else {
            return false;
        };
        let Some(content) = panel.content.as_mut() else {
            return false;
        };
        let mut ctx = EventCtx {
            scroll: &mut panel.scroll,
            h_scroll: &mut panel.h_scroll,
        };
        matches!(
            content.handle_event(&WmEvent::Paste(text.to_owned()), &mut ctx),
            WmEventResult::Consumed(_)
        )
    }
}

const DEFAULT_PANEL_W: u16 = 88;
const DEFAULT_PANEL_H: u16 = 29;
const MIN_PANEL_W: u16 = 20;
const MIN_PANEL_H: u16 = 6;

/// Place new windows from the upper left, wrapping the diagonal within bounds.
pub(crate) fn initial_panel_rect(canvas: Rect, preferred: (u16, u16), ordinal: usize) -> Rect {
    let width = preferred.0.min(canvas.width.saturating_sub(4));
    let height = preferred.1.min(canvas.height.saturating_sub(4));
    let max_x = canvas.right().saturating_sub(width);
    let max_y = canvas.bottom().saturating_sub(height);
    let x = canvas.x.saturating_add(2).min(max_x);
    let y = canvas.y.saturating_add(1).min(max_y);
    let steps = max_x.saturating_sub(x).min(max_y.saturating_sub(y)) / 3 + 1;
    let offset = (ordinal % usize::from(steps)) as u16 * 3;
    Rect::new(x + offset, y + offset, width, height)
}

impl WindowManager {
    pub fn focused(&self) -> Option<String> {
        self.focus.active.and_then(|id| {
            self.panels
                .iter()
                .find(|panel| panel.id == id)
                .map(|panel| panel.label.clone())
        })
    }

    pub fn focused_id(&self) -> Option<WindowId> {
        self.focus.active
    }

    pub fn focused_content_cursor_visible(&self) -> bool {
        self.focused_id()
            .and_then(|id| self.panels.iter().find(|panel| panel.id == id))
            .and_then(|panel| panel.content.as_deref())
            .is_some_and(WindowComponent::cursor_visible)
    }

    pub fn label(&self, id: WindowId) -> Option<&str> {
        self.panels
            .iter()
            .find(|panel| panel.id == id)
            .map(|panel| panel.label.as_str())
    }

    pub fn content_kind(&self, id: WindowId) -> Option<&WindowContent> {
        self.panels
            .iter()
            .find(|panel| panel.id == id)
            .map(|panel| &panel.content_kind)
    }

    pub fn open(
        &mut self,
        label: &str,
        content_key: ContentKey,
        content_kind: WindowContent,
        title: &str,
        canvas: Rect,
    ) -> WindowId {
        self.open_with_size(
            label,
            content_key,
            OpenPolicy::ReuseExisting,
            content_kind,
            title,
            canvas,
            0,
            0,
            false,
        )
    }

    pub(crate) fn open_project_hub(
        &mut self,
        canvas: Rect,
        sidebar: std::sync::Arc<std::sync::Mutex<crate::project_sidebar::ProjectSidebar>>,
        project_root: Option<&str>,
        control_tx: Option<&crate::control::ControlSender>,
    ) {
        let id = self.open(
            "project-hub",
            ContentKey::Projects,
            WindowContent::Projects,
            "Projects",
            canvas,
        );
        if let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id)
            && panel.content.is_none()
        {
            panel.content = Some(Box::new(
                crate::window::project_panel::ProjectPanelContent::new_selected(
                    sidebar.lock().unwrap().directory.projects.clone(),
                    project_root.map(|root| {
                        atman_runtime::session_meta::fingerprint_from_root(std::path::Path::new(
                            root,
                        ))
                    }),
                    None,
                ),
            ));
        }
        if self
            .panels
            .iter()
            .find(|panel| panel.id == id)
            .is_some_and(|panel| !panel.maximized)
        {
            self.toggle_maximize(id, canvas);
        }
        self.request_projects(control_tx, None, &sidebar);
    }

    pub(crate) fn show_project_sessions(&mut self, fingerprint: &str) {
        if let Some(component) = self
            .panels
            .iter_mut()
            .find(|p| p.content_kind == WindowContent::Projects)
            .and_then(|p| p.content.as_mut())
        {
            component.show_project_sessions(fingerprint);
        }
    }

    fn request_projects(
        &mut self,
        control_tx: Option<&crate::control::ControlSender>,
        mutation: Option<crate::ProjectMutation>,
        sidebar: &std::sync::Arc<std::sync::Mutex<crate::project_sidebar::ProjectSidebar>>,
    ) {
        let Some(component) = self
            .panels
            .iter_mut()
            .find(|p| p.content_kind == WindowContent::Projects)
            .and_then(|p| p.content.as_mut())
        else {
            return;
        };
        let Some(id) = component.begin_project_request() else {
            return;
        };
        sidebar.lock().unwrap().track(id);
        if control_tx.is_none_or(|tx| {
            tx.send(crate::TuiControl::Projects(
                crate::project_hub::ProjectRequest { id, mutation },
            ))
            .is_err()
        }) {
            sidebar
                .lock()
                .unwrap()
                .result(id, &Err("Project directory is unavailable".into()));
            component.project_result(id, &Err("Project directory is unavailable".into()));
        }
    }

    pub fn project_result(
        &mut self,
        request: uuid::Uuid,
        result: &Result<crate::project_hub::ProjectDirectory, String>,
    ) -> bool {
        self.panels
            .iter_mut()
            .filter_map(|p| p.content.as_mut())
            .any(|content| content.project_result(request, result))
    }

    pub(crate) fn apply_project_sort(
        &mut self,
        window: WindowId,
        request: &crate::project_hub::ProjectSortRequest,
        sort: crate::project_hub::ProjectSort,
    ) {
        if let Some(content) = self
            .panels
            .iter_mut()
            .find(|panel| panel.id == window)
            .and_then(|panel| panel.content.as_mut())
        {
            content.apply_project_sort(request, sort);
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_with_size(
        &mut self,
        label: &str,
        content_key: ContentKey,
        policy: OpenPolicy,
        content_kind: WindowContent,
        title: &str,
        canvas: Rect,
        w: u16,
        h: u16,
        maximized: bool,
    ) -> WindowId {
        self.open_with_policy(
            FocusPolicy::Steal,
            label,
            content_key,
            policy,
            content_kind,
            title,
            canvas,
            w,
            h,
            maximized,
        )
    }

    /// Focus-preserving open. Steals focus only when no floating panel is
    /// currently focused (used for background task completions). When a
    /// panel is already focused, the new panel still opens
    /// and comes to the front, but the user's current focus is untouched.
    #[allow(clippy::too_many_arguments)]
    pub fn open_background_with_size(
        &mut self,
        label: &str,
        content_key: ContentKey,
        policy: OpenPolicy,
        content_kind: WindowContent,
        title: &str,
        canvas: Rect,
        w: u16,
        h: u16,
        maximized: bool,
    ) -> WindowId {
        self.open_with_policy(
            if self.focused_id().is_some() {
                FocusPolicy::Preserve
            } else {
                FocusPolicy::Steal
            },
            label,
            content_key,
            policy,
            content_kind,
            title,
            canvas,
            w,
            h,
            maximized,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn open_with_policy(
        &mut self,
        focus_policy: FocusPolicy,
        label: &str,
        content_key: ContentKey,
        policy: OpenPolicy,
        content_kind: WindowContent,
        title: &str,
        canvas: Rect,
        w: u16,
        h: u16,
        maximized: bool,
    ) -> WindowId {
        let (w, h) = if w == 0 || h == 0 {
            let vp = Rect::new(0, 0, canvas.width, canvas.height);
            let hint = self
                .panels
                .iter()
                .find(|p| p.content_key == content_key)
                .and_then(|p| p.content.as_ref())
                .map(|c| c.preferred_size(vp))
                .unwrap_or_default();
            (
                hint.preferred.0.max(DEFAULT_PANEL_W),
                hint.preferred.1.max(DEFAULT_PANEL_H),
            )
        } else {
            (w, h)
        };
        let reuse = match policy {
            OpenPolicy::ReuseExisting => self
                .panels
                .iter()
                .find(|p| p.content_key == content_key)
                .map(|p| p.id),
            OpenPolicy::AlwaysNew => None,
        };
        if let Some(existing_id) = reuse {
            let p = self
                .panels
                .iter_mut()
                .find(|p| p.id == existing_id)
                .unwrap();
            p.label = label.to_string();
            p.content_kind = content_kind;
            p.title = title.to_string();
            if maximized && !p.maximized {
                p.prev_rect = Some(p.rect);
                p.rect = maximized_rect(canvas);
                p.maximized = true;
            }
            self.bring_to_front(existing_id);
            if matches!(focus_policy, FocusPolicy::Steal) {
                self.focus.focus(existing_id);
            }
            return existing_id;
        }
        self.z_counter += 1;
        let rect = if maximized {
            maximized_rect(canvas)
        } else if matches!(content_kind, WindowContent::Mcp | WindowContent::Knowledge)
            && (canvas.width < 55 || canvas.height < 22)
        {
            canvas
        } else {
            initial_panel_rect(canvas, (w, h), self.panels.len())
        };
        let prev_rect = if maximized {
            Some(default_rect(canvas))
        } else {
            None
        };
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        let panel = WindowInstance {
            id,
            label: label.to_string(),
            content_key,
            content_kind,
            content: None,
            title: title.to_string(),
            rect,
            z: self.z_counter,
            maximized,
            prev_rect,
            scroll: 0,
            h_scroll: 0,
            split: false,
            expanded_tools: HashSet::new(),
            interaction_revision: 0,
        };
        self.panels.push(panel);
        if matches!(focus_policy, FocusPolicy::Steal) {
            self.focus.focus(id);
        }
        id
    }

    pub fn close(&mut self, id: WindowId) -> component::CloseOutcome {
        if let Some(idx) = self.panels.iter().position(|p| p.id == id) {
            if let Some(ref mut content) = self.panels[idx].content {
                if let blocked @ component::CloseOutcome::Block(_) = content.on_close() {
                    return blocked;
                }
                content.on_blur();
            }
            self.panels.remove(idx);
            self.focus.remove_and_refocus(id, &self.panels);
        }
        component::CloseOutcome::Close
    }

    pub fn request_close(&mut self, id: WindowId, app: &mut crate::app::AppState) {
        if let component::CloseOutcome::Block(message) = self.close(id) {
            app.push_toast(
                message,
                crate::app::NoteLevel::Warn,
                std::time::Duration::from_secs(4),
                crate::app::ToastPosition::TopRight,
            );
        }
    }

    pub fn focus(&mut self, id: WindowId) {
        if let Some(old) = self.focus.active {
            if old != id {
                if let Some(p) = self.panels.iter_mut().find(|p| p.id == old) {
                    if let Some(ref mut content) = p.content {
                        content.on_blur();
                    }
                }
            }
        }
        self.focus.focus(id);
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            if let Some(ref mut content) = p.content {
                content.on_focus();
            }
        }
        self.bring_to_front(id);
    }

    fn bring_to_front(&mut self, id: WindowId) {
        self.z_counter += 1;
        let z = self.z_counter;
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.z = z;
        }
    }

    pub fn toggle_maximize(&mut self, id: WindowId, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            if p.maximized {
                p.rect = p.prev_rect.take().unwrap_or_else(|| default_rect(canvas));
                p.maximized = false;
            } else {
                p.prev_rect = Some(p.rect);
                p.rect = maximized_rect(canvas);
                p.maximized = true;
            }
            if let Some(ref mut content) = p.content {
                let area = content_area(p.rect);
                let mut ctx = EventCtx {
                    scroll: &mut p.scroll,
                    h_scroll: &mut p.h_scroll,
                };
                content.on_resize(area, &mut ctx);
            }
            self.bring_to_front(id);
        }
    }

    pub fn move_panel(&mut self, id: WindowId, x: u16, y: u16, _canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.rect.x = x;
            p.rect.y = y;
        }
    }

    pub fn resize_panel(&mut self, id: WindowId, w: u16, h: u16, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            let min = p
                .content
                .as_ref()
                .map(|content| content.preferred_size(canvas).min)
                .unwrap_or((MIN_PANEL_W, MIN_PANEL_H));
            p.rect.width = w.max(min.0).max(MIN_PANEL_W);
            p.rect.height = h.max(min.1).max(MIN_PANEL_H);
        }
    }

    /// Clamp every floating panel rect so it stays inside (and within the max
    /// size of) the terminal canvas. Guarantees a panel never renders larger
    /// than the usable area nor off-canvas after a terminal resize.
    pub fn clamp_to_canvas(&mut self, canvas: Rect) {
        let max_w = canvas.width.saturating_sub(4).max(MIN_PANEL_W);
        let max_h = canvas.height.saturating_sub(4).max(MIN_PANEL_H);
        for panel in &mut self.panels {
            panel.rect.width = panel.rect.width.min(max_w).max(MIN_PANEL_W);
            panel.rect.height = panel.rect.height.min(max_h).max(MIN_PANEL_H);
            if panel.rect.x + panel.rect.width > canvas.x + canvas.width {
                panel.rect.x = canvas.x + canvas.width.saturating_sub(panel.rect.width);
            }
            if panel.rect.y + panel.rect.height > canvas.y + canvas.height {
                panel.rect.y = canvas.y + canvas.height.saturating_sub(panel.rect.height);
            }
        }
    }

    pub fn unmaximize(&mut self, id: WindowId, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            if p.maximized {
                p.rect = p.prev_rect.take().unwrap_or_else(|| default_rect(canvas));
                p.maximized = false;
            }
        }
    }

    pub fn cycle_focus(&mut self, forward: bool) {
        if self.panels.len() < 2 {
            return;
        }
        // Z order changes when focus moves, so traversal uses stable creation order.
        let current_idx = self
            .panels
            .iter()
            .position(|p| Some(p.id) == self.focus.active);
        let next_idx = match current_idx {
            Some(i) => {
                if forward {
                    (i + 1) % self.panels.len()
                } else {
                    (i + self.panels.len() - 1) % self.panels.len()
                }
            }
            None => 0,
        };
        if let Some(target) = self.panels.get(next_idx) {
            let id = target.id;
            self.focus(id);
        }
    }

    pub fn dispatch_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        control_tx: Option<&crate::ControlSender>,
    ) -> (bool, Vec<WmCommand>) {
        self.sync_modals();
        if app.modal_notification.is_some() {
            if matches!(action, crate::keys::KeyAction::Escape) {
                app.modal_notification = None;
            }
            return (true, Vec::new());
        }
        if let Some(kind) = self.layers.dispatch_key() {
            let (consumed, carried) = self.modals.handle_key_top(kind, action, app, control_tx);
            let commands = carried
                .and_then(|a| match a {
                    crate::wm::ModalAction::Dispatched(id) => {
                        Some(self.apply_palette_action(id, app, control_tx))
                    }
                    crate::wm::ModalAction::Consumed
                    | crate::wm::ModalAction::OpenModelManager(_)
                    | crate::wm::ModalAction::OpenAliasManager
                    | crate::wm::ModalAction::OpenAliasForModel(_) => None,
                })
                .unwrap_or_default();
            return (consumed, commands);
        }
        if !app.submission_focus
            && (!app.pending_permissions.is_empty() || !app.pending_permission_groups.is_empty())
            && crate::key_handler::is_approval_key(action)
        {
            return (false, Vec::new());
        }
        match action {
            crate::keys::KeyAction::CyclePanelForward => {
                self.cycle_focus(true);
                return (true, Vec::new());
            }
            crate::keys::KeyAction::CyclePanelBackward => {
                self.cycle_focus(false);
                return (true, Vec::new());
            }
            _ => {}
        }

        let Some(id) = self.focused_id() else {
            return (false, Vec::new());
        };
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) else {
            return (false, Vec::new());
        };

        if let Some(content) = panel.content.as_mut() {
            let mut ctx = EventCtx {
                scroll: &mut panel.scroll,
                h_scroll: &mut panel.h_scroll,
            };
            if let WmEventResult::Consumed(commands) =
                content.handle_event(&WmEvent::Key(action.clone()), &mut ctx)
            {
                return (true, commands);
            }
        }

        match action {
            crate::keys::KeyAction::ScrollUp | crate::keys::KeyAction::PageUp => {
                panel.scroll = panel.scroll.saturating_sub(
                    if matches!(action, crate::keys::KeyAction::PageUp) {
                        10
                    } else {
                        3
                    },
                );
                return (true, Vec::new());
            }
            crate::keys::KeyAction::ScrollDown | crate::keys::KeyAction::PageDown => {
                panel.scroll = panel.scroll.saturating_add(
                    if matches!(action, crate::keys::KeyAction::PageDown) {
                        10
                    } else {
                        3
                    },
                );
                return (true, Vec::new());
            }
            crate::keys::KeyAction::Escape => {
                if matches!(panel.content_kind, WindowContent::Mcp)
                    && app.mcp_remove_armed.take().is_some()
                {
                    return (true, Vec::new());
                }
                return (true, vec![WmCommand::CloseWindow(id)]);
            }
            _ => {}
        }

        (false, Vec::new())
    }

    /// Execute a colon command through the same action path as the palette.
    pub(crate) fn dispatch_meta_command(
        &mut self,
        input: &str,
        app: &mut crate::app::AppState,
        control_tx: Option<&crate::ControlSender>,
    ) -> bool {
        let Some(command) = input.trim().strip_prefix(':').map(str::trim) else {
            return false;
        };
        if matches!(command, "exit" | "quit" | "q") {
            app.should_quit = true;
            return true;
        }
        if command == "panel" {
            self.modals.palette.open_for(app);
            return true;
        }
        let (name, args) = command
            .split_once(char::is_whitespace)
            .unwrap_or((command, ""));
        if !crate::panel_registry::command_enabled(name) {
            app.push_note(
                "This panel is not enabled in this build",
                crate::app::NoteLevel::Info,
            );
            return true;
        }
        if name == "sidebar" {
            match crate::sidebar::SidebarMode::parse_command(args) {
                Ok(Some(mode)) => app.set_focus_sidebar(mode),
                Ok(None) => app.toggle_focus_sidebar(),
                Err(error) => app.push_note(error, crate::app::NoteLevel::Error),
            }
            return true;
        }
        if name == "copy" {
            match args.trim() {
                "" | "last" | "last-message" => crate::key_handler::copy_last_message(app),
                "last-tool" => crate::key_handler::copy_last_tool(app),
                value => app.push_note(
                    format!("unknown copy target `{value}`; use last-message | last-tool"),
                    crate::app::NoteLevel::Error,
                ),
            }
            return true;
        }
        let Some(id) = crate::palette::PaletteEntryId::from_command(command) else {
            return false;
        };
        // Compaction requires the bound host's context and approval path.
        if id == crate::palette::PaletteEntryId::CompactNow {
            return false;
        }
        let commands = self.apply_palette_action(id, app, control_tx);
        self.apply_commands(app, commands, control_tx);
        true
    }

    /// Execute a picked command-palette entry. Called right after the palette
    /// closes on Submit; opens the target modal, mutates app state, and emits
    /// control/session commands.
    fn apply_palette_action(
        &mut self,
        id: crate::palette::PaletteEntryId,
        app: &mut crate::app::AppState,
        control_tx: Option<&crate::ControlSender>,
    ) -> Vec<WmCommand> {
        use crate::palette::PaletteEntryId;
        if !crate::panel_registry::command_enabled(id.command()) {
            return Vec::new();
        }
        if app.items.is_archive()
            && !matches!(
                id,
                PaletteEntryId::OpenFlowHub
                    | PaletteEntryId::OpenFlowLinks
                    | PaletteEntryId::OpenWatchHub
                    | PaletteEntryId::OpenInbox
                    | PaletteEntryId::OpenWorktrees
                    | PaletteEntryId::OpenWorkspace
                    | PaletteEntryId::ToggleActivity
                    | PaletteEntryId::OpenManagement
                    | PaletteEntryId::SwitchSession
                    | PaletteEntryId::NewSession
                    | PaletteEntryId::DeleteSession
                    | PaletteEntryId::OpenProjectHub
                    | PaletteEntryId::SetFormulaRendering
                    | PaletteEntryId::YankMode
                    | PaletteEntryId::CopyLastMessage
                    | PaletteEntryId::CopyLastTool
                    | PaletteEntryId::SetModeTheme
                    | PaletteEntryId::ShowHelp
            )
        {
            app.push_toast(
                "Archived flow is read-only; select an executable flow for this operation",
                crate::app::NoteLevel::Info,
                std::time::Duration::from_secs(6),
                crate::app::ToastPosition::BottomRight,
            );
            return Vec::new();
        }
        match id {
            PaletteEntryId::YankMode => {
                crate::key_handler::enter_selection_mode(app);
            }
            PaletteEntryId::CopyLastMessage => crate::key_handler::copy_last_message(app),
            PaletteEntryId::CopyLastTool => crate::key_handler::copy_last_tool(app),
            PaletteEntryId::CompactNow => {
                if let Some(tx) = control_tx {
                    let _ = tx.send(crate::TuiControl::CompactNow);
                    app.push_note(
                        "requested transcript compaction",
                        crate::app::NoteLevel::Info,
                    );
                }
            }
            PaletteEntryId::SwitchSession => {
                self.modals.session_switcher.open_for(app, control_tx);
            }
            PaletteEntryId::NewSession => {
                if control_tx.is_some_and(|tx| tx.binding().is_some()) {
                    app.open_flow_hub = Some(crate::flow_hub::Action::NewSession);
                } else if let Some(tx) = control_tx {
                    let _ = tx.send(crate::TuiControl::NewSession);
                }
            }
            PaletteEntryId::MoveSession => {
                if let Some(binding) = control_tx.and_then(|tx| tx.binding())
                    && let Some(flow) = binding.flow.instance()
                {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        crate::flow_hub::HubRequest::SessionProject {
                            session: flow.session_id.0,
                        },
                    ));
                } else if let (Some(tx), Some(session)) = (control_tx, app.session.as_ref()) {
                    let form = atman_runtime::form::PendingForm {
                        form_id: "session_move_path".to_string(),
                        run_id: atman_rt::RunId::<atman_runtime::event::AtmanUuid>::now(),
                        tool_use_id: "session_move_path".to_string(),
                        kind: atman_runtime::form::FormKind::Text {
                            prompt: "New working directory:".to_string(),
                            placeholder: Some("/path/to/project".to_string()),
                            multiline: false,
                        },
                        form: atman_runtime::form::CompositeForm {
                            questions: vec![atman_runtime::form::FormQuestion {
                                id: "question".into(),
                                kind: atman_runtime::form::FormKind::Text {
                                    prompt: "New working directory:".to_string(),
                                    placeholder: Some("/path/to/project".to_string()),
                                    multiline: false,
                                },
                            }],
                        },
                        emitted_at: chrono::Utc::now(),
                    };
                    session.forms().request(form);
                    let _ = tx.send(crate::TuiControl::MoveSession);
                }
            }
            PaletteEntryId::DeleteSession => {
                if control_tx.is_some_and(|tx| tx.binding().is_some()) {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        crate::flow_hub::HubRequest::Sessions { before: None },
                    ));
                } else {
                    let scope = crate::session_switcher::SessionScope::Project;
                    let rows = crate::key_handler::enumerate_session_rows(app, scope);
                    self.modals.session_switcher.open_with(rows, scope);
                }
            }
            PaletteEntryId::DeleteOldSession => {
                if let Some(owner) = &app.continued_session
                    && let Some(current) = control_tx
                        .and_then(|tx| tx.binding())
                        .and_then(|binding| binding.flow.instance())
                    && current.session_id == owner.session_id
                {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        crate::flow_hub::HubRequest::ContinuationRemoval {
                            owner: owner.clone(),
                        },
                    ));
                } else {
                    app.push_note(
                        "This session has no inherited legacy history",
                        crate::app::NoteLevel::Info,
                    );
                }
            }
            PaletteEntryId::ToggleProjectSidebar => {
                app.project_sidebar.lock().unwrap().toggle(control_tx);
            }
            PaletteEntryId::OpenProjectHub => {
                let canvas = app.maximized_canvas();
                self.open_project_hub(
                    canvas,
                    app.project_sidebar.clone(),
                    app.project_root.as_deref(),
                    control_tx,
                );
            }
            PaletteEntryId::OpenWorktrees => {
                if control_tx.and_then(|tx| tx.binding()).is_some() {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        crate::flow_hub::HubRequest::Workspaces { after: None },
                    ));
                } else {
                    app.push_note(
                        "Worktrees requires a persistent session",
                        crate::app::NoteLevel::Info,
                    );
                }
            }
            PaletteEntryId::OpenFlowHub => {
                if let Some(binding) = control_tx.and_then(|tx| tx.binding()) {
                    app.open_flow_hub =
                        Some(crate::flow_hub::Action::Load(binding.flow.directory()));
                } else {
                    app.push_note(
                        "FlowHub requires a persistent session",
                        crate::app::NoteLevel::Info,
                    );
                }
            }
            PaletteEntryId::OpenFlowLinks
            | PaletteEntryId::OpenWatchHub
            | PaletteEntryId::OpenWorkspace
            | PaletteEntryId::OpenInbox => {
                if let Some(flow) = control_tx
                    .and_then(|tx| tx.binding())
                    .and_then(|binding| binding.flow.instance())
                {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        if id == PaletteEntryId::OpenWorkspace {
                            crate::flow_hub::HubRequest::FlowWorkspace {
                                owner: flow.clone(),
                            }
                        } else if id == PaletteEntryId::OpenInbox {
                            crate::flow_hub::HubRequest::Inbox {
                                owner: flow.clone(),
                                side: atman_runtime::flow_store::inbox_view::InboxSide::Received,
                                before: None,
                            }
                        } else if id == PaletteEntryId::OpenWatchHub {
                            crate::flow_hub::HubRequest::Watchers {
                                owner: flow.clone(),
                            }
                        } else {
                            crate::flow_hub::HubRequest::Relations {
                                session: flow.session_id.0,
                                focus: Some(flow.clone()),
                                range: crate::flow_hub::RelationRange::Session,
                            }
                        },
                    ));
                } else {
                    app.push_toast(
                        "Flow inspection requires a persistent instance",
                        crate::app::NoteLevel::Info,
                        std::time::Duration::from_secs(4),
                        crate::app::ToastPosition::BottomRight,
                    );
                }
            }
            PaletteEntryId::OpenManagement => {
                if control_tx.is_some_and(|tx| tx.binding().is_some()) {
                    app.open_flow_hub = Some(crate::flow_hub::Action::OpenManagement);
                } else {
                    app.push_note(
                        "Atman navigation requires a persistent workspace",
                        crate::app::NoteLevel::Info,
                    );
                }
            }
            PaletteEntryId::SetProjectStorageScope => {
                self.modals
                    .form_modal
                    .open_preference(crate::settings::Preference::ProjectStorage, app);
            }
            PaletteEntryId::SetFormulaRendering => {
                self.modals
                    .form_modal
                    .open_preference(crate::settings::Preference::MathRendering, app);
            }
            PaletteEntryId::SearchHistory => {
                self.modals.history_search.open();
            }
            PaletteEntryId::ToggleActivity => {
                app.toggle_focus_activity();
            }
            PaletteEntryId::ManageProviders => {
                self.modals.provider_manager.toggle();
            }
            PaletteEntryId::ManageAliases => {
                self.modals.alias_manager.toggle();
            }
            PaletteEntryId::ManageModels => {
                self.modals.model_manager.open();
            }
            PaletteEntryId::SwitchModel => {
                self.modals.model_picker.open();
            }
            PaletteEntryId::ManageMcp => {
                let canvas = app.last_transcript_rect.unwrap_or_default();
                self.open(
                    "mcp-manager",
                    crate::wm::ContentKey::Mcp,
                    crate::wm::WindowContent::Mcp,
                    "MCP Servers",
                    canvas,
                );
                if let Some(p) = self
                    .panels
                    .iter_mut()
                    .find(|p| p.content_key == crate::wm::ContentKey::Mcp)
                {
                    p.content = Some(Box::new(
                        crate::window::mcp_panel::McpPanelContent::default(),
                    ));
                }
            }
            PaletteEntryId::ManageGoal => {
                if let Some(session) = app.session.clone() {
                    if let Some(id) = self
                        .panels
                        .iter()
                        .find(|p| p.content_key == ContentKey::Goal)
                        .map(|p| p.id)
                    {
                        self.focus(id);
                        return Vec::new();
                    }
                    let canvas = app.maximized_canvas();
                    let id = self.open_with_size(
                        "goal",
                        ContentKey::Goal,
                        OpenPolicy::ReuseExisting,
                        WindowContent::Goal,
                        "Goal",
                        canvas,
                        92,
                        26,
                        false,
                    );
                    if let Some(panel) = self.panels.iter_mut().find(|p| p.id == id)
                        && panel.content.is_none()
                    {
                        panel.rect.x = canvas.x + canvas.width.saturating_sub(panel.rect.width) / 2;
                        panel.rect.y =
                            canvas.y + canvas.height.saturating_sub(panel.rect.height) / 2;
                        panel.content = Some(Box::new(
                            crate::window::goal_panel::GoalPanelContent::new(session),
                        ));
                        if let Some(snapshot) = app.items.snapshot() {
                            panel
                                .content
                                .as_mut()
                                .unwrap()
                                .set_flow_snapshot(snapshot.clone());
                        }
                    }
                }
            }
            PaletteEntryId::ManageKnowledge => {
                let canvas = app.last_transcript_rect.unwrap_or_default();
                self.open_with_size(
                    "memory-rules",
                    crate::wm::ContentKey::Knowledge,
                    OpenPolicy::ReuseExisting,
                    crate::wm::WindowContent::Knowledge,
                    "Memory & Rules",
                    canvas,
                    100,
                    36,
                    false,
                );
                if let Some(panel) = self
                    .panels
                    .iter_mut()
                    .find(|panel| panel.content_key == crate::wm::ContentKey::Knowledge)
                    && panel.content.is_none()
                {
                    panel.content = Some(Box::new(
                        crate::window::knowledge_panel::KnowledgePanelContent::new(
                            app.knowledge_state.clone(),
                            control_tx.cloned(),
                        ),
                    ));
                }
                if let Some(tx) = control_tx {
                    let _ = tx.send(crate::TuiControl::ListKnowledge);
                }
            }
            PaletteEntryId::ShowHelp => {
                let canvas = app.last_transcript_rect.unwrap_or_default();
                self.open(
                    "cheatsheet",
                    crate::wm::ContentKey::Cheatsheet,
                    crate::wm::WindowContent::Cheatsheet,
                    "Keybindings",
                    canvas,
                );
                if let Some(p) = self
                    .panels
                    .iter_mut()
                    .find(|p| p.content_key == crate::wm::ContentKey::Cheatsheet)
                {
                    p.content = Some(Box::new(
                        crate::window::cheatsheet_panel::CheatsheetPanelContent { scroll: 0 },
                    ));
                }
            }
            PaletteEntryId::SetTrustMode => {
                self.modals.open_trust_mode_picker(app);
            }
            PaletteEntryId::AutoNameSession => {
                if let Some(tx) = control_tx {
                    let _ = tx.send(crate::TuiControl::AutoNameSession);
                }
            }
            PaletteEntryId::SetModeTheme => {
                self.modals.theme_picker_open = true;
            }
        }
        Vec::new()
    }

    pub fn dispatch_mouse(
        &mut self,
        event: &MouseEvent,
        app: &mut crate::app::AppState,
        control_tx: Option<&crate::ControlSender>,
    ) -> (bool, Vec<WmCommand>) {
        self.sync_modals();
        if app.modal_notification.is_some() {
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::Palette) {
            if let Some(id) = self.modals.palette.handle_mouse(event) {
                return (true, self.apply_palette_action(id, app, control_tx));
            }
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::Form) {
            self.modals.form_modal.handle_mouse(event, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::CompactReview) {
            if let Some(modal) = self.modals.compact_review.as_mut() {
                modal.handle_mouse(event, app, control_tx);
            }
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::ProviderManager) {
            self.modals.handle_provider_mouse(event, app, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::ModelManager) {
            self.modals.handle_model_mouse(event, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::ModelPicker) {
            self.modals
                .handle_model_picker_mouse(event, app, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::Onboarding) {
            self.modals.handle_onboarding_mouse(event, app, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::AliasManager) {
            self.modals.handle_alias_mouse(event, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::McpEditor) {
            self.modals.mcp_editor.handle_mouse(event, app, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind() == Some(ModalKind::SessionSwitcher) {
            self.modals
                .session_switcher
                .interact_mouse(event, app, control_tx);
            return (true, Vec::new());
        }
        if let Some(kind @ (ModalKind::ThemePicker | ModalKind::TrustModePicker)) = self.top_kind()
        {
            self.modals
                .handle_picker_mouse(kind, event, app, control_tx);
            return (true, Vec::new());
        }
        if self.top_kind().is_some() {
            return (true, Vec::new());
        }
        // The shell owns pointer capture and chrome, regardless of how eagerly
        // a content component consumes mouse events.
        if matches!(event.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
            && (self.interaction.drag_target.is_some() || self.interaction.resize_target.is_some())
        {
            return (false, Vec::new());
        }
        if matches!(
            event.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::Drag(_)
                | MouseEventKind::Up(_)
                | MouseEventKind::Moved
        ) && (self.hit_test_btn(event.column, event.row).is_some()
            || self.hit_test_titlebar(event.column, event.row).is_some())
        {
            return (false, Vec::new());
        }
        let Some(id) = self
            .hit_test_panel(event.column, event.row)
            .map(|panel| panel.id)
        else {
            return (false, Vec::new());
        };
        if matches!(event.kind, MouseEventKind::Down(_)) {
            self.focus(id);
        }
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) else {
            return (false, Vec::new());
        };
        if let Some(content) = panel.content.as_mut() {
            let mut ctx = EventCtx {
                scroll: &mut panel.scroll,
                h_scroll: &mut panel.h_scroll,
            };
            if let WmEventResult::Consumed(commands) =
                content.handle_event(&WmEvent::Mouse(*event), &mut ctx)
            {
                return (true, commands);
            }
        }
        match event.kind {
            MouseEventKind::ScrollUp => panel.scroll = panel.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => panel.scroll = panel.scroll.saturating_add(3),
            MouseEventKind::ScrollLeft => panel.h_scroll = panel.h_scroll.saturating_sub(3),
            MouseEventKind::ScrollRight => panel.h_scroll = panel.h_scroll.saturating_add(3),
            _ => return (false, Vec::new()),
        }
        (true, Vec::new())
    }

    pub(crate) fn goal_saved(&mut self, request_id: u64, result: Result<(), String>) {
        for panel in &mut self.panels {
            if let Some(content) = panel.content.as_mut() {
                let mut ctx = EventCtx {
                    scroll: &mut panel.scroll,
                    h_scroll: &mut panel.h_scroll,
                };
                content.handle_event(
                    &WmEvent::GoalSaved {
                        request_id,
                        result: result.clone(),
                    },
                    &mut ctx,
                );
            }
        }
    }

    pub(crate) fn flow_snapshot(
        &mut self,
        snapshot: std::sync::Arc<atman_runtime::flow_runtime::FlowViewSnapshot>,
    ) {
        for panel in &mut self.panels {
            if let Some(content) = panel.content.as_mut() {
                content.set_flow_snapshot(snapshot.clone());
            }
        }
    }

    pub fn apply_commands(
        &mut self,
        app: &mut crate::app::AppState,
        commands: Vec<WmCommand>,
        control_tx: Option<&crate::ControlSender>,
    ) {
        for command in commands {
            let opens_session = matches!(
                command,
                WmCommand::SwitchSession { .. }
                    | WmCommand::OpenSession(_)
                    | WmCommand::BrowseIndexedHistory(_)
            );
            match command {
                WmCommand::FocusWindow(id) => self.focus(id),
                WmCommand::CloseWindow(id) => self.request_close(id, app),
                WmCommand::ToggleMaximize(id) => {
                    self.toggle_maximize(id, app.maximized_canvas());
                }
                WmCommand::TermResize { handle, rows, cols } => {
                    if let Some(tx) = control_tx {
                        let _ = tx.send(crate::TuiControl::TermResize { handle, rows, cols });
                    }
                }
                WmCommand::GoalControl {
                    request_id,
                    target,
                    goal_id,
                    revision,
                    action,
                } => {
                    let sent = control_tx.is_some_and(|tx| {
                        tx.send(crate::TuiControl::GoalControl {
                            request_id,
                            target,
                            goal_id,
                            revision,
                            action,
                        })
                        .is_ok()
                    });
                    if !sent && let Some(request_id) = request_id {
                        self.goal_saved(request_id, Err("Goal controls are unavailable".into()));
                    }
                }
                WmCommand::OpenTaskPanel { handle, maximized } => {
                    app.open_task_panel(self, &handle, app.maximized_canvas(), maximized, false);
                }
                WmCommand::OpenOutputSnapshot { id, item } => {
                    let title = match item.as_ref() {
                        crate::app::OutputItem::DiffPreview { title, .. } => title.clone(),
                        crate::app::OutputItem::FsDetail { view, .. } => view.title(),
                        _ => "Tool output".into(),
                    };
                    let canvas = app.maximized_canvas();
                    let window = self.open_with_size(
                        &id,
                        ContentKey::Output(id.clone()),
                        OpenPolicy::ReuseExisting,
                        WindowContent::Output {
                            item_id: id.clone(),
                        },
                        &title,
                        canvas,
                        canvas.width,
                        canvas.height,
                        true,
                    );
                    if let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == window) {
                        panel.content = Some(Box::new(
                            crate::window::output_panel::OutputPanelContent::snapshot(item),
                        ));
                    }
                }
                WmCommand::OpenContentPanel {
                    label,
                    key,
                    title,
                    window_content,
                } => {
                    let canvas = app
                        .last_transcript_rect
                        .unwrap_or_else(|| app.maximized_canvas());
                    let id = self.open(&label, key, window_content.clone(), &title, canvas);
                    let content: Option<Box<dyn crate::wm::WindowComponent>> = match window_content
                    {
                        crate::wm::WindowContent::Mcp => Some(Box::new(
                            crate::window::mcp_panel::McpPanelContent::default(),
                        )),
                        crate::wm::WindowContent::Cheatsheet => Some(Box::new(
                            crate::window::cheatsheet_panel::CheatsheetPanelContent { scroll: 0 },
                        )),
                        _ => None,
                    };
                    if let Some((p, content)) =
                        self.panels.iter_mut().find(|p| p.id == id).zip(content)
                    {
                        p.content = Some(content);
                    }
                }
                WmCommand::PushToast(message) => app.push_toast(
                    message,
                    crate::app::NoteLevel::Info,
                    std::time::Duration::from_secs(3),
                    crate::app::ToastPosition::TopRight,
                ),
                WmCommand::SwitchSession { sid, project_root } => {
                    crate::key_handler::request_project_session_switch(
                        app,
                        control_tx,
                        sid,
                        project_root,
                    );
                }
                WmCommand::MutateProject(mutation) => {
                    self.request_projects(control_tx, Some(mutation), &app.project_sidebar);
                }
                WmCommand::RefreshProjects => {
                    self.request_projects(control_tx, None, &app.project_sidebar)
                }
                WmCommand::SortProjects(request) => {
                    if let Some(panel) = self
                        .panels
                        .iter()
                        .find(|panel| panel.content_kind == WindowContent::Projects)
                    {
                        if let Err(error) =
                            self.modals.form_modal.open_project_sort(panel.id, request)
                        {
                            app.push_note(error, crate::app::NoteLevel::Error);
                        }
                        self.sync_modals();
                        app.mark_visual_dirty();
                    }
                }
                WmCommand::OpenSession(session) => {
                    app.open_flow_hub = Some(crate::flow_hub::Action::OpenSession(session));
                }
                WmCommand::BrowseIndexedHistory(source) => {
                    app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                        crate::flow_hub::HubRequest::IndexedHistory { source },
                    ));
                }
            }
            if opens_session {
                let projects: Vec<_> = self
                    .panels
                    .iter()
                    .filter(|panel| panel.content_kind == WindowContent::Projects)
                    .map(|panel| panel.id)
                    .collect();
                for id in projects {
                    self.request_close(id, app);
                }
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, canvas: Rect, app: &mut crate::app::AppState) {
        self.sync_modals();
        if self.panels.is_empty() {
            self.interaction.last_hitmap = WmHitmap::default();
        } else {
            let hovered_panel_btn = self.interaction.hovered_panel_btn;
            let hovered_history_row = self.interaction.hovered_history_row.clone();
            let hovered_mcp_row = self.interaction.hovered_mcp_row.clone();
            let mut mcp_browser = app.mcp_browser_state();
            mcp_browser.hovered_action = self.interaction.hovered_mcp_action;
            let close_armed_id = self.interaction.panel_close_armed_id.clone();
            let close_armed = close_armed_id
                .as_deref()
                .map(|id| (id, self.panel_close_arm_expired()));
            self.interaction.last_hitmap = render(
                frame,
                canvas,
                self,
                &app.task_snapshots,
                &app.items,
                app.items.revisions(),
                &app.handle_index,
                &app.detached_task_details,
                &app.task_handle_index,
                &app.workflow_run_to_panel,
                app.task_snapshots_revision,
                &app.activity_nodes,
                &hovered_panel_btn,
                &hovered_history_row,
                app.animation_frame,
                close_armed,
                app.maximized_canvas(),
                !self.layers.modal_stack.is_empty(),
                &app.context.mcp_servers,
                &app.expanded_mcp_servers,
                app.mcp_selected,
                &hovered_mcp_row,
                &mcp_browser,
            );
        }

        self.render_overlays(frame, canvas, app);
    }

    /// Compose the same modal layer over a conversation or a directory.
    pub(crate) fn render_overlays(
        &mut self,
        frame: &mut Frame,
        canvas: Rect,
        app: &mut crate::app::AppState,
    ) {
        self.sync_modals();
        let modal_open = app.modal_notification.is_some() || self.top_kind().is_some();
        if modal_open {
            crate::wm::shadow::render_backdrop(frame, &crate::theme::theme());
        }
        if let Some(kind) = self.top_kind() {
            let t = crate::theme::theme();
            let rect = self.modals.compute_rect(kind, canvas);
            let title = self.modals.title_for(kind);
            let icon = self.modals.icon_for(kind);
            let accent = self.modals.accent_for(kind, &t);
            let show_header = !matches!(kind, crate::wm::ModalKind::Form); // form_modal handles its own header
            let content = crate::wm::shell::render_overlay_shell(
                frame,
                rect,
                title,
                icon,
                accent,
                show_header,
                &t,
            );
            self.modals.render_top(kind, frame, content, app, &t);
            if let Some((cx, cy)) = self.modals.cursor_position(kind) {
                frame.set_cursor_position((cx, cy));
            }
        }
        let layers = std::mem::take(&mut self.layers);
        layers.render_blocking(frame, canvas, app);
        layers.render_toasts(frame, canvas, app);
        self.layers = layers;
    }
}

fn maximized_rect(canvas: Rect) -> Rect {
    let w = canvas.width.saturating_sub(8);
    let h = canvas.height.saturating_sub(4);
    let x = canvas.x + 4;
    let y = canvas.y + 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn content_area(rect: Rect) -> Rect {
    shell::content_area(rect)
}

fn default_rect(canvas: Rect) -> Rect {
    let w = DEFAULT_PANEL_W.min(canvas.width.saturating_sub(4));
    let h = DEFAULT_PANEL_H.min(canvas.height.saturating_sub(4));
    let x = canvas.x + (canvas.width.saturating_sub(w)) / 2;
    let y = canvas.y + (canvas.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    canvas: Rect,
    panels: &mut WindowManager,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    item_revisions: &[crate::app::OutputRevision],
    handle_index: &std::collections::HashMap<String, usize>,
    detached_task_details: &std::collections::HashMap<String, crate::app::DetachedTaskDetail>,
    task_handle_index: &std::collections::HashMap<String, usize>,
    workflow_run_to_panel: &std::collections::HashMap<String, usize>,
    task_snapshots_revision: u64,
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(WindowId, PanelBtn)>,
    hovered_history_row: &Option<String>,
    animation_frame: u32,
    panel_close_armed: Option<(&str, bool)>,
    max_canvas: Rect,
    modal_open: bool,
    mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    expanded_mcp_servers: &std::collections::HashSet<String>,
    mcp_selected: usize,
    hovered_mcp_row: &Option<String>,
    mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
) -> WmHitmap {
    let mut all_hitmap = WmHitmap::default();
    let mut sorted: Vec<usize> = (0..panels.panels.len()).collect();
    sorted.sort_by_key(|&i| panels.panels[i].z);

    for panel in &mut panels.panels {
        if panel.maximized {
            panel.rect = if matches!(panel.content_kind, WindowContent::Projects) {
                canvas
            } else {
                maximized_rect(max_canvas)
            };
        }
    }

    let t = crate::theme::theme();
    for &idx in &sorted {
        if panel_is_covered(&panels.panels, idx) {
            continue;
        }
        let panel = &mut panels.panels[idx];
        let is_focused = panels.focus.active == Some(panel.id);
        let btn_hover = hovered_btn
            .as_ref()
            .and_then(|(id, btn)| if id == &panel.id { Some(*btn) } else { None });

        shell::render_shell(
            f,
            panel,
            is_focused,
            btn_hover,
            panel_close_armed,
            snapshots,
            task_handle_index,
            &t,
        );

        let content_area = Rect {
            x: panel.rect.x + 3,
            y: panel.rect.y + 2,
            width: panel.rect.width.saturating_sub(6),
            height: panel.rect.height.saturating_sub(3),
        };

        let mut panel_hitmap = WmHitmap::default();
        if content_area.height > 0 && content_area.width > 0 {
            let content_bg: Color = if matches!(
                panel.content_kind,
                WindowContent::Mcp | WindowContent::Knowledge | WindowContent::Goal
            ) {
                shell::surface_bg(&t)
            } else {
                t.code_bg.into()
            };
            f.render_widget(Clear, content_area);
            f.render_widget(
                Block::default().style(Style::default().bg(content_bg)),
                content_area,
            );
            content::render_panel_content(
                f,
                content_area,
                panel,
                snapshots,
                items,
                item_revisions,
                handle_index,
                detached_task_details,
                task_handle_index,
                workflow_run_to_panel,
                task_snapshots_revision,
                activity_nodes,
                hovered_btn,
                hovered_history_row,
                &mut panel_hitmap,
                animation_frame,
                is_focused,
                modal_open,
                mcp_servers,
                expanded_mcp_servers,
                mcp_selected,
                hovered_mcp_row,
                mcp_browser,
            );
            all_hitmap
                .history_row_rects
                .append(&mut panel_hitmap.history_row_rects);
            all_hitmap
                .workflow_node_rects
                .append(&mut panel_hitmap.workflow_node_rects);
            all_hitmap
                .mcp_row_rects
                .append(&mut panel_hitmap.mcp_row_rects);
            all_hitmap
                .selection_projections
                .append(&mut panel_hitmap.selection_projections);
        }

        shadow::render_shadow(f, panel.rect, &t);
    }

    all_hitmap
}

fn panel_is_covered(panels: &[WindowInstance], index: usize) -> bool {
    let panel = &panels[index];
    panels.iter().enumerate().any(|(other_index, other)| {
        (other.z > panel.z || (other.z == panel.z && other_index > index))
            && other.rect.x <= panel.rect.x
            && other.rect.y <= panel.rect.y
            && other.rect.right() >= panel.rect.right()
            && other.rect.bottom() >= panel.rect.bottom()
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn continued_session_cleanup_uses_the_same_source_from_keyboard_mouse_and_peer_views() {
        use crate::{
            control::{ControlSender, ViewBinding},
            flow_hub::{Action, HubRequest},
            palette::PaletteEntryId,
            wm::modal::ModalOverlay,
        };
        use atman_runtime::flow_store::FlowRef;
        use uuid::Uuid;
        let owner = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        };
        for peer in [false, true] {
            for mouse in [false, true] {
                let current = if peer {
                    FlowRef {
                        instance_id: atman_rt::FlowInstanceId::now(),
                        ..owner.clone()
                    }
                } else {
                    owner.clone()
                };
                let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
                let (sender, _replies) = ControlSender::for_flow(
                    ViewBinding {
                        view_id: Uuid::now_v7(),
                        revision: 1,
                        flow: current.into(),
                    },
                    commands,
                );
                let mut app = crate::app::AppState::default();
                let mut wm = super::WindowManager::default();
                wm.dispatch_meta_command(":panel", &mut app, Some(&sender));
                assert!(
                    !wm.modals
                        .palette
                        .filtered
                        .contains(&PaletteEntryId::DeleteOldSession)
                );
                wm.modals.palette.close();
                wm.dispatch_meta_command(":delete-old-session", &mut app, Some(&sender));
                assert!(app.open_flow_hub.is_none());
                app.continued_session = Some(owner.clone());
                wm.dispatch_meta_command(":panel", &mut app, Some(&sender));
                assert!(
                    wm.modals
                        .palette
                        .filtered
                        .contains(&PaletteEntryId::DeleteOldSession)
                );
                for c in "delete-old-session".chars() {
                    wm.modals.palette.push_char(c);
                }
                if mouse {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 12))
                            .unwrap();
                    terminal
                        .draw(|frame| {
                            wm.modals.palette.render_content(
                                frame,
                                frame.area(),
                                &app,
                                &crate::theme::theme(),
                            )
                        })
                        .unwrap();
                    let buffer = terminal.backend().buffer();
                    let row = (0..12)
                        .find(|y| {
                            (0..120)
                                .map(|x| buffer[(x, *y)].symbol())
                                .collect::<String>()
                                .contains("Delete Old Session")
                        })
                        .unwrap();
                    wm.dispatch_mouse(
                        &crossterm::event::MouseEvent {
                            kind: crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ),
                            column: 5,
                            row,
                            modifiers: crossterm::event::KeyModifiers::NONE,
                        },
                        &mut app,
                        Some(&sender),
                    );
                } else {
                    wm.dispatch_key(&crate::keys::KeyAction::Submit, &mut app, Some(&sender));
                }
                assert!(
                    matches!(app.open_flow_hub.take(), Some(Action::Load(HubRequest::ContinuationRemoval { owner: selected })) if selected == owner)
                );
                wm.dispatch_meta_command(":delete-old-session", &mut app, Some(&sender));
                assert!(
                    matches!(app.open_flow_hub.take(), Some(Action::Load(HubRequest::ContinuationRemoval { owner: selected })) if selected == owner)
                );
            }
        }
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn escape_closes_task_windows_without_cancelling_execution() {
        for kind in [
            atman_runtime::TaskKind::Flow,
            atman_runtime::TaskKind::Bash,
            atman_runtime::TaskKind::Terminal,
        ] {
            let registry = atman_runtime::TaskRegistry::new();
            let token = tokio_util::sync::CancellationToken::new();
            registry.register(
                kind,
                "task".into(),
                "child".into(),
                "session".into(),
                token.clone(),
            );
            let mut app = crate::app::AppState::new("session".into(), None)
                .with_task_registry(registry.clone());
            let mut wm = WindowManager::default();
            let id = wm.open(
                "child",
                ContentKey::Task("child".into()),
                WindowContent::Task {
                    handle: "child".into(),
                    kind,
                },
                "Child",
                canvas(),
            );
            let (consumed, commands) =
                wm.dispatch_key(&crate::keys::KeyAction::Escape, &mut app, None);
            assert!(consumed);
            assert!(
                matches!(commands.as_slice(), [WmCommand::CloseWindow(window)] if *window == id)
            );
            wm.apply_commands(&mut app, commands, None);
            assert!(wm.panels.is_empty());
            assert!(!token.is_cancelled());
            assert!(
                registry
                    .lookup_by_handle_in_session("child", "session")
                    .unwrap()
                    .is_running()
            );
        }
    }

    #[cfg(feature = "extended-panels")]
    #[test]
    fn links_command_and_palette_use_the_requesting_view_address() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        let instance_id = atman_rt::FlowInstanceId::now();
        for _ in 0..2 {
            let flow = atman_runtime::flow_store::FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: instance_id.clone(),
            };
            let (host, mut incoming) = tokio::sync::mpsc::unbounded_channel();
            let (tx, _replies) = crate::ControlSender::for_flow(
                crate::control::ViewBinding {
                    view_id: uuid::Uuid::now_v7(),
                    revision: 0,
                    flow: flow.clone().into(),
                },
                host,
            );
            assert!(wm.dispatch_meta_command(":links", &mut app, Some(&tx)));
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Relations {focus:Some(target), ..})) if target == flow)
            );
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::OpenFlowLinks,
                &mut app,
                Some(&tx),
            );
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Relations {focus:Some(target), ..})) if target == flow)
            );
            assert!(incoming.try_recv().is_err());
            assert!(wm.dispatch_meta_command(":move-session", &mut app, Some(&tx)));
            assert!(matches!(app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::SessionProject { session }))
                if session == flow.session_id.0));
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::MoveSession,
                &mut app,
                Some(&tx),
            );
            assert!(matches!(app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::SessionProject { session }))
                if session == flow.session_id.0));
            assert!(wm.dispatch_meta_command(":watchers", &mut app, Some(&tx)));
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Watchers { owner })) if owner == flow)
            );
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::OpenWatchHub,
                &mut app,
                Some(&tx),
            );
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Watchers { owner })) if owner == flow)
            );
            assert!(incoming.try_recv().is_err());
            assert!(wm.dispatch_meta_command(":inbox", &mut app, Some(&tx)));
            app.open_flow_hub.take();
            assert!(wm.dispatch_meta_command(":workspace", &mut app, Some(&tx)));
            assert!(matches!(
                app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::FlowWorkspace { owner })) if owner == flow
            ));
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::OpenWorkspace,
                &mut app,
                Some(&tx),
            );
            assert!(matches!(
                app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::FlowWorkspace { owner })) if owner == flow
            ));
            assert!(incoming.try_recv().is_err());
            assert!(wm.dispatch_meta_command(":worktrees", &mut app, Some(&tx)));
            assert!(matches!(
                app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(
                    crate::flow_hub::HubRequest::Workspaces { after: None }
                ))
            ));
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::OpenWorktrees,
                &mut app,
                Some(&tx),
            );
            assert!(matches!(
                app.open_flow_hub.take(),
                Some(crate::flow_hub::Action::Load(
                    crate::flow_hub::HubRequest::Workspaces { after: None }
                ))
            ));
            assert!(wm.dispatch_meta_command(":inbox", &mut app, Some(&tx)));
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Inbox { owner, .. })) if owner == flow)
            );
            wm.apply_palette_action(
                crate::palette::PaletteEntryId::OpenInbox,
                &mut app,
                Some(&tx),
            );
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Inbox { owner, .. })) if owner == flow)
            );
            app.queue_action(0, crate::submission_queue::QueueAction::History, Some(&tx));
            assert!(
                matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::Inbox { owner, .. })) if owner == flow)
            );
            assert!(incoming.try_recv().is_err());
        }
        assert!(wm.dispatch_meta_command(":links", &mut app, None));
        assert!(app.open_flow_hub.is_none());
    }

    #[cfg(feature = "extended-panels")]
    #[test]
    fn management_command_and_palette_share_bound_navigation() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        let (host, mut incoming) = tokio::sync::mpsc::unbounded_channel();
        let binding = crate::control::ViewBinding {
            view_id: uuid::Uuid::now_v7(),
            revision: 0,
            flow: atman_runtime::flow_store::FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: atman_rt::FlowInstanceId(uuid::Uuid::now_v7().into()),
            }
            .into(),
        };
        let (tx, _replies) = crate::ControlSender::for_flow(binding, host);
        assert!(wm.dispatch_meta_command(":atman", &mut app, Some(&tx)));
        assert!(matches!(
            app.open_flow_hub.take(),
            Some(crate::flow_hub::Action::OpenManagement)
        ));
        wm.apply_palette_action(
            crate::palette::PaletteEntryId::OpenManagement,
            &mut app,
            Some(&tx),
        );
        assert!(matches!(
            app.open_flow_hub.take(),
            Some(crate::flow_hub::Action::OpenManagement)
        ));
        assert!(incoming.try_recv().is_err());
        assert!(!wm.modals.any_open());
    }

    #[test]
    fn transcript_commands_and_arguments_remain_runtime_owned() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        let (tx, mut rx) = crate::ControlSender::channel();
        for command in [
            ":compact",
            ":storage local",
            ":help",
            ":panel extra",
            ":unknown",
        ] {
            assert!(!wm.dispatch_meta_command(command, &mut app, Some(&tx)));
        }
        assert!(rx.try_recv().is_err());
        assert!(wm.panels.is_empty());
        assert!(!wm.modals.any_open());
    }

    #[tokio::test]
    async fn palette_commands_have_local_routes_except_host_compaction() {
        for id in crate::palette::PaletteEntryId::all() {
            let mut wm = WindowManager::default();
            let mut app = crate::app::AppState::default();
            let (tx, _rx) = crate::ControlSender::channel();
            assert_eq!(
                wm.dispatch_meta_command(&format!(":{}", id.command()), &mut app, Some(&tx)),
                id != crate::palette::PaletteEntryId::CompactNow,
                "{}",
                id.command()
            );
        }
    }

    #[test]
    fn view_commands_do_not_submit_or_change_the_other_view() {
        let mut main = crate::UiState::new(crate::app::AppState::default());
        let mut floating = crate::UiState::new(crate::app::AppState::default());
        let (tx, mut rx) = crate::ControlSender::channel();
        for view in [&mut main, &mut floating] {
            assert!(
                view.wm
                    .dispatch_meta_command(":sidebar off", &mut view.app, Some(&tx))
            );
            assert!(view.app.sidebar_collapsed);
            assert!(
                view.wm
                    .dispatch_meta_command(":sidebar on", &mut view.app, Some(&tx))
            );
            assert!(!view.app.sidebar_collapsed);
            for command in [
                ":copy",
                ":copy last",
                ":copy last-message",
                ":copy last-tool",
                ":copy invalid",
                ":sidebar auto",
            ] {
                assert!(
                    view.wm
                        .dispatch_meta_command(command, &mut view.app, Some(&tx))
                );
            }
        }
        main.wm
            .dispatch_meta_command(":sidebar off", &mut main.app, Some(&tx));
        assert!(main.app.sidebar_collapsed);
        assert!(!floating.app.sidebar_collapsed);
        assert!(rx.try_recv().is_err());
    }

    struct CountingContent(Arc<AtomicUsize>);

    struct MouseConsumingContent(Arc<AtomicUsize>);

    impl WindowComponent for CountingContent {
        fn render_content(
            &mut self,
            _area: Rect,
            _frame: &mut Frame,
            _ctx: &RenderCtx,
        ) -> Vec<HitRegion> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Vec::new()
        }

        fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
            WmEventResult::Ignored
        }

        fn preferred_size(&self, _viewport: Rect) -> SizeHint {
            SizeHint::default()
        }
    }

    impl WindowComponent for MouseConsumingContent {
        fn render_content(
            &mut self,
            _area: Rect,
            _frame: &mut Frame,
            _ctx: &RenderCtx,
        ) -> Vec<HitRegion> {
            Vec::new()
        }

        fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
            if matches!(
                event,
                WmEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::ScrollRight,
                    ..
                }) | WmEvent::Key(
                    crate::keys::KeyAction::PageDown | crate::keys::KeyAction::Escape
                )
            ) {
                self.0.fetch_add(1, Ordering::Relaxed);
                WmEventResult::Consumed(Vec::new())
            } else {
                WmEventResult::Ignored
            }
        }

        fn preferred_size(&self, _viewport: Rect) -> SizeHint {
            SizeHint::default()
        }
    }

    fn canvas() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn top_form_receives_paste_and_mouse_confirmation() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        wm.modals
            .form_modal
            .attach(atman_runtime::form::PendingForm {
                form_id: "confirm_test".into(),
                run_id: atman_rt::RunId::<atman_runtime::event::AtmanUuid>::now(),
                tool_use_id: "tool_test".into(),
                form: atman_runtime::form::CompositeForm {
                    questions: vec![atman_runtime::form::FormQuestion {
                        id: "question".into(),
                        kind: atman_runtime::form::FormKind::Confirm {
                            prompt: "Proceed?".into(),
                        },
                    }],
                },
                kind: atman_runtime::form::FormKind::Confirm {
                    prompt: "Proceed?".into(),
                },
                emitted_at: chrono::Utc::now(),
            });
        assert!(wm.dispatch_paste("ignored", &mut app, None));
        assert!(wm.modals.form_modal.text_editor.buf().is_empty());

        wm.modals.form_modal.yes_rect = Some(Rect::new(2, 3, 7, 1));
        let click = MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 4,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let (tx, mut rx) = crate::ControlSender::channel();
        assert!(wm.dispatch_mouse(&click, &mut app, Some(&tx)).0);
        assert!(wm.dispatch_mouse(&click, &mut app, Some(&tx)).0);
        assert!(rx.try_recv().is_err());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| {
                crate::wm::modal::ModalOverlay::render_content(
                    &mut wm.modals.form_modal,
                    frame,
                    frame.area(),
                    &app,
                    &crate::theme::theme(),
                );
            })
            .unwrap();
        let yes = wm.modals.form_modal.yes_rect.unwrap();
        let click = MouseEvent {
            column: yes.x + 2,
            row: yes.y + 1,
            ..click
        };
        assert!(wm.dispatch_mouse(&click, &mut app, Some(&tx)).0);
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::TuiControl::FormSubmit {
                submission: atman_runtime::form::FormSubmission::Submitted { .. },
                ..
            })
        ));

        wm.modals
            .form_modal
            .attach(atman_runtime::form::PendingForm {
                form_id: "text_test".into(),
                run_id: atman_rt::RunId::<atman_runtime::event::AtmanUuid>::now(),
                tool_use_id: "tool_text".into(),
                form: atman_runtime::form::CompositeForm {
                    questions: vec![atman_runtime::form::FormQuestion {
                        id: "path".into(),
                        kind: atman_runtime::form::FormKind::Text {
                            prompt: "Path".into(),
                            placeholder: None,
                            multiline: false,
                        },
                    }],
                },
                kind: atman_runtime::form::FormKind::Text {
                    prompt: "Path".into(),
                    placeholder: None,
                    multiline: false,
                },
                emitted_at: chrono::Utc::now(),
            });
        assert!(wm.dispatch_paste("one\r\ntwo", &mut app, None));
        assert_eq!(wm.modals.form_modal.text_editor.buf(), "one two");
    }

    fn task_content(handle: &str) -> WindowContent {
        WindowContent::Task {
            handle: handle.to_string(),
            kind: atman_runtime::TaskKind::Bash,
        }
    }

    fn id(fp: &WindowManager, label: &str) -> WindowId {
        fp.panels
            .iter()
            .find(|panel| panel.label == label)
            .unwrap()
            .id
    }

    #[test]
    fn paste_is_dispatched_to_the_actual_top_modal() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::new("session".into(), None);

        wm.modals.palette.open();
        wm.sync_modals();
        wm.modals.open_mcp_add();
        assert!(wm.dispatch_paste("server", &mut app, None));

        assert_eq!(wm.top_kind(), Some(ModalKind::McpEditor));
        assert_eq!(wm.modals.mcp_editor.name.buf(), "server");
        assert!(wm.modals.palette.input.buf().is_empty());
    }

    #[test]
    fn project_directory_replies_cannot_reopen_closed_windows_or_replace_new_requests() {
        let mut wm = WindowManager::default();
        let (sender, mut requests) = crate::control::ControlSender::channel();
        let canvas = Rect::new(0, 0, 130, 40);
        wm.open_project_hub(canvas, Default::default(), None, Some(&sender));
        let crate::TuiControl::Projects(first) = requests.try_recv().unwrap() else {
            panic!("directory request expected");
        };
        let original = wm.panels[0].id;
        wm.open_project_hub(canvas, Default::default(), None, Some(&sender));
        assert!(requests.try_recv().is_err());
        assert_eq!(wm.panels[0].id, original);
        wm.close(original);
        let data = Ok(crate::project_hub::ProjectDirectory::default());
        assert!(!wm.project_result(first.id, &data));
        assert!(wm.panels.is_empty());
        wm.open_project_hub(canvas, Default::default(), None, Some(&sender));
        let crate::TuiControl::Projects(second) = requests.try_recv().unwrap() else {
            panic!("directory request expected");
        };
        assert_ne!(first.id, second.id);
        assert!(!wm.project_result(first.id, &data));
        assert!(wm.project_result(second.id, &data));
    }

    #[test]
    fn loading_animation_follows_visible_pending_window_ownership() {
        let mut wm = WindowManager::default();
        let (sender, mut requests) = crate::control::ControlSender::channel();
        let canvas = Rect::new(0, 0, 130, 40);
        wm.open_project_hub(canvas, Default::default(), None, Some(&sender));
        let crate::TuiControl::Projects(request) = requests.try_recv().unwrap() else {
            panic!("directory request expected")
        };
        assert!(wm.has_active_animation());
        let cover = wm.open(
            "Cover",
            ContentKey::Cheatsheet,
            WindowContent::Cheatsheet,
            "Cover",
            canvas,
        );
        wm.panels
            .iter_mut()
            .find(|panel| panel.id == cover)
            .unwrap()
            .rect = canvas;
        assert!(
            !wm.has_active_animation(),
            "fully occluded loading must not wake frames"
        );
        wm.close(cover);
        assert!(wm.has_active_animation());
        wm.modals.palette.open = true;
        wm.sync_modals();
        assert!(
            !wm.has_active_animation(),
            "hidden panel content does not animate through a modal"
        );
        wm.modals.palette.open = false;
        wm.sync_modals();
        assert!(wm.project_result(request.id, &Ok(Default::default())));
        assert!(!wm.has_active_animation(), "completion stops loading ticks");
    }

    #[test]
    fn opening_a_project_session_dismisses_the_hub_before_bound_navigation() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::new("current".into(), None);
        app.input = "retained draft".into();
        let (sender, _requests) = crate::control::ControlSender::channel();
        wm.open_project_hub(
            Rect::new(0, 0, 130, 40),
            app.project_sidebar.clone(),
            None,
            Some(&sender),
        );
        let target = uuid::Uuid::now_v7();
        wm.apply_commands(
            &mut app,
            vec![WmCommand::OpenSession(target)],
            Some(&sender),
        );
        assert!(
            !wm.panels
                .iter()
                .any(|panel| panel.content_kind == WindowContent::Projects)
        );
        assert!(
            matches!(app.open_flow_hub, Some(crate::flow_hub::Action::OpenSession(session)) if session == target)
        );
        assert_eq!(app.input, "retained draft");
        assert!(!app.should_quit);
    }

    #[test]
    fn reopening_knowledge_panel_preserves_its_content() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::new("session".into(), None);
        wm.apply_palette_action(
            crate::palette::PaletteEntryId::ManageKnowledge,
            &mut app,
            None,
        );
        let original = wm.panels[0].content.as_deref().unwrap() as *const dyn WindowComponent;
        let adjusted = Rect::new(7, 5, 64, 19);
        wm.panels[0].rect = adjusted;
        wm.apply_palette_action(
            crate::palette::PaletteEntryId::ManageKnowledge,
            &mut app,
            None,
        );
        let reopened = wm.panels[0].content.as_deref().unwrap() as *const dyn WindowComponent;
        assert_eq!(wm.panels.len(), 1);
        assert!(std::ptr::addr_eq(original, reopened));
        assert_eq!(wm.panels[0].rect, adjusted);
    }

    #[test]
    fn floating_geometry_survives_render_and_reopen_for_every_content_kind() {
        let kinds = [
            WindowContent::Task {
                handle: "bash".into(),
                kind: atman_runtime::TaskKind::Bash,
            },
            WindowContent::Task {
                handle: "terminal".into(),
                kind: atman_runtime::TaskKind::Terminal,
            },
            WindowContent::Task {
                handle: "flow".into(),
                kind: atman_runtime::TaskKind::Flow,
            },
            WindowContent::History,
            WindowContent::Activity {
                run_id: "run".into(),
            },
            WindowContent::Mermaid {
                item_id: "diagram".into(),
            },
            WindowContent::Output {
                item_id: "output".into(),
            },
            WindowContent::Cheatsheet,
            WindowContent::Mcp,
            WindowContent::Knowledge,
            WindowContent::Goal,
            WindowContent::Projects,
        ];
        for kind in kinds {
            let mut wm = WindowManager::default();
            let mut app = crate::app::AppState::default();
            let canvas = Rect::new(0, 0, 50, 20);
            let id = wm.open("panel", kind.content_key(), kind.clone(), "Panel", canvas);
            wm.move_panel(id, 3, 2, canvas);
            wm.resize_panel(id, 40, 16, canvas);
            let rect = wm.panels[0].rect;
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(50, 20)).unwrap();
            terminal
                .draw(|frame| wm.render(frame, canvas, &mut app))
                .unwrap();
            wm.open("panel", kind.content_key(), kind.clone(), "Panel", canvas);
            assert_eq!(wm.panels[0].rect, rect, "{kind:?}");
            let count = Arc::new(AtomicUsize::new(0));
            wm.panels[0].content = Some(Box::new(MouseConsumingContent(count.clone())));
            for key in [
                crate::keys::KeyAction::PageDown,
                crate::keys::KeyAction::Escape,
            ] {
                let (consumed, commands) = wm.dispatch_key(&key, &mut app, None);
                assert!(consumed && commands.is_empty(), "{kind:?}");
            }
            assert_eq!(count.load(Ordering::Relaxed), 2);
            assert_eq!(wm.panels[0].scroll, 0);
            wm.interaction.drag_target = Some(id);
            wm.interaction.resize_target = Some(id);
            wm.modals.palette.open();
            wm.sync_modals();
            assert!(wm.interaction.drag_target.is_none());
            assert!(wm.interaction.resize_target.is_none());
        }
    }

    #[test]
    fn goal_window_centers_and_preserves_shell_pointer_control() {
        for (width, height) in [(140, 44), (50, 20)] {
            let canvas = Rect::new(0, 0, width, height);
            let mut wm = WindowManager::default();
            let mut app = crate::app::AppState::new("goal-window".into(), None);
            app.session = Some(std::sync::Arc::new(atman_runtime::Session::open_ephemeral()));
            app.last_full_rect = Some(canvas);
            app.last_transcript_rect = Some(canvas);
            assert!(wm.dispatch_meta_command(":goal", &mut app, None));
            let id = wm.panels[0].id;
            let rect = wm.panels[0].rect;
            assert_eq!(rect.x, (width - rect.width) / 2);
            assert_eq!(rect.y, (height - rect.height) / 2);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| wm.render(frame, canvas, &mut app))
                .unwrap();
            assert_eq!(wm.panels[0].rect, rect);
            for (column, row) in [
                (rect.x + 6, rect.y),
                (rect.right() - 1, rect.bottom() - 1),
                (rect.x, rect.y + 5),
            ] {
                let event = MouseEvent {
                    kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    column,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                };
                assert!(
                    !wm.dispatch_mouse(&event, &mut app, None).0,
                    "content must not consume window chrome"
                );
            }
            assert_eq!(
                wm.hit_test_titlebar(rect.x + 6, rect.y).map(|p| p.id),
                Some(id)
            );
            assert_eq!(
                wm.hit_test_resize(rect.right() - 1, rect.bottom() - 1),
                Some(id)
            );
            for resize in [false, true] {
                wm.interaction.drag_target = (!resize).then_some(id);
                wm.interaction.resize_target = resize.then_some(id);
                for kind in [
                    MouseEventKind::Drag(crossterm::event::MouseButton::Left),
                    MouseEventKind::Up(crossterm::event::MouseButton::Left),
                ] {
                    let event = MouseEvent {
                        kind,
                        column: rect.x + 5,
                        row: rect.y + 6,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    };
                    assert!(
                        !wm.dispatch_mouse(&event, &mut app, None).0,
                        "captured pointer must stay with the shell over content"
                    );
                }
            }
            wm.interaction.drag_target = None;
            wm.interaction.resize_target = None;
            wm.move_panel(id, rect.x + 1, rect.y + 1, canvas);
            wm.resize_panel(id, rect.width - 4, rect.height - 2, canvas);
            let adjusted = wm.panels[0].rect;
            terminal
                .draw(|frame| wm.render(frame, canvas, &mut app))
                .unwrap();
            assert_eq!(wm.panels[0].rect, adjusted);
            assert!(wm.dispatch_meta_command(":goal", &mut app, None));
            assert_eq!(wm.panels.len(), 1);
            assert_eq!(wm.panels[0].rect, adjusted);
            wm.resize_panel(id, 1, 1, canvas);
            assert_eq!(
                (wm.panels[0].rect.width, wm.panels[0].rect.height),
                (32, 14)
            );
        }
    }

    #[test]
    fn notifications_block_keys_and_paste_before_underlying_modals() {
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        wm.modals.palette.open();
        app.modal_notification = Some("Notice".into());

        assert!(wm.dispatch_paste("hidden input", &mut app, None));
        for action in [crate::keys::KeyAction::Save, crate::keys::KeyAction::Submit] {
            let (consumed, commands) = wm.dispatch_key(&action, &mut app, None);
            assert!(consumed && commands.is_empty());
            assert!(app.modal_notification.is_some());
        }
        assert!(wm.modals.palette.input.buf().is_empty());
        assert!(wm.modals.palette.open);

        assert!(
            wm.dispatch_key(&crate::keys::KeyAction::Escape, &mut app, None)
                .0
        );
        assert!(app.modal_notification.is_none());
        assert!(wm.modals.palette.open);
    }

    #[test]
    fn management_panels_fit_small_canvases_and_notifications_block_mouse() {
        for kind in [WindowContent::Mcp, WindowContent::Knowledge] {
            let mut wm = WindowManager::default();
            let mut app = crate::app::AppState::default();
            let canvas = Rect::new(0, 0, 40, 18);
            let id = wm.open("management", kind.content_key(), kind, "Management", canvas);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 18)).unwrap();
            terminal
                .draw(|frame| wm.render(frame, canvas, &mut app))
                .unwrap();
            assert_eq!(wm.panels[0].rect, canvas);
            app.modal_notification = Some("Notice".into());
            let event = MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 8,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            assert!(wm.dispatch_mouse(&event, &mut app, None).0);
            assert_eq!(wm.panels[0].scroll, 0);
            app.modal_notification = None;
            app.mcp_remove_armed = Some("server".into());
            if matches!(wm.content_kind(id), Some(WindowContent::Mcp)) {
                let (consumed, commands) =
                    wm.dispatch_key(&crate::keys::KeyAction::Escape, &mut app, None);
                assert!(consumed && commands.is_empty());
                assert!(app.mcp_remove_armed.is_none());
            }
        }
    }

    #[test]
    fn initial_window_placement_cascades_within_each_canvas() {
        for origin in [(0, 0), (17, 9)] {
            for (width, height) in [(40, 18), (64, 20), (200, 65)] {
                let canvas = Rect::new(origin.0, origin.1, width, height);
                for ordinal in [0, 1, 2, 100, usize::MAX] {
                    let rect = initial_panel_rect(canvas, (88, 29), ordinal);
                    assert_eq!(rect.intersection(canvas), rect);
                }
                let first = initial_panel_rect(canvas, (88, 29), 0);
                assert_eq!((first.x, first.y), (canvas.x + 2, canvas.y + 1));
                if width == 200 {
                    let next = initial_panel_rect(canvas, (88, 29), 1);
                    assert_eq!((next.x, next.y), (first.x + 3, first.y + 3));
                }
            }
        }
    }

    #[test]
    fn open_creates_panel_and_focuses() {
        let mut fp = WindowManager::default();
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            task_content("a"),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused(), Some("bg_1".to_string()));
    }

    #[test]
    fn open_existing_focuses_without_duplicate() {
        let mut fp = WindowManager::default();
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            task_content("a"),
            "cargo build",
            canvas(),
        );
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            task_content("a"),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
    }

    #[test]
    fn close_removes_and_refocuses() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            task_content("a"),
            "b",
            canvas(),
        );
        fp.close(id(&fp, "b"));
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused(), Some("a".to_string()));
    }

    #[test]
    fn window_shortcuts_visit_all_windows_in_both_directions() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut wm = WindowManager::default();
        let mut app = crate::app::AppState::default();
        let ids: Vec<_> = ["a", "b", "c"]
            .into_iter()
            .map(|name| {
                wm.open(
                    name,
                    ContentKey::Task(name.into()),
                    task_content(name),
                    name,
                    canvas(),
                )
            })
            .collect();
        for (modifiers, expected) in [
            (KeyModifiers::CONTROL, vec![ids[0], ids[1], ids[2]]),
            (
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                vec![ids[1], ids[0], ids[2]],
            ),
        ] {
            for id in expected {
                let action = crate::keys::map(KeyEvent::new(KeyCode::Tab, modifiers));
                assert!(wm.dispatch_key(&action, &mut app, None).0);
                assert_eq!(wm.focused_id(), Some(id));
            }
        }
    }

    #[test]
    fn focus_brings_to_front() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            task_content("a"),
            "b",
            canvas(),
        );
        let z_b_before = fp.panels.iter().find(|p| p.id == id(&fp, "b")).unwrap().z;
        fp.focus(id(&fp, "a"));
        let z_a = fp.panels.iter().find(|p| p.id == id(&fp, "a")).unwrap().z;
        assert!(z_a > z_b_before);
    }

    #[test]
    fn open_background_preserves_focus_when_panel_focused() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        assert_eq!(fp.focused(), Some("a".to_string()));

        // Background open while "a" is focused must NOT steal focus.
        fp.open_background_with_size(
            "b",
            ContentKey::Task("b".to_string()),
            OpenPolicy::AlwaysNew,
            task_content("b"),
            "b",
            canvas(),
            0,
            0,
            false,
        );
        assert_eq!(fp.panels.len(), 2);
        assert_eq!(fp.focused(), Some("a".to_string()), "must keep focus on a");
    }

    #[test]
    fn open_background_steals_focus_when_none_focused() {
        let mut fp = WindowManager::default();
        let id_a = fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.focus.blur();
        assert_eq!(fp.focused_id(), None);

        fp.open_background_with_size(
            "b",
            ContentKey::Task("b".to_string()),
            OpenPolicy::AlwaysNew,
            task_content("b"),
            "b",
            canvas(),
            0,
            0,
            false,
        );
        assert_eq!(fp.focused().as_deref(), Some("b"), "should focus b");
        fp.close(id_a);
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused().as_deref(), Some("b"));
    }

    #[test]
    fn toggle_maximize_swaps_rect() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let orig = fp.panels[0].rect;
        fp.toggle_maximize(id(&fp, "a"), canvas());
        assert!(fp.panels[0].maximized);
        assert_ne!(fp.panels[0].rect, orig);
        fp.toggle_maximize(id(&fp, "a"), canvas());
        assert!(!fp.panels[0].maximized);
        assert_eq!(fp.panels[0].rect, orig);
    }

    #[test]
    fn hit_test_titlebar_finds_topmost() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            task_content("a"),
            "b",
            canvas(),
        );
        let b = fp.panels.iter().find(|p| p.id == id(&fp, "b")).unwrap();
        let hit = fp.hit_test_titlebar(b.rect.x + 2, b.rect.y);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().id, id(&fp, "b"));
    }

    #[test]
    fn move_panel_no_clamp() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.move_panel(id(&fp, "a"), 200, 200, canvas());
        let p = &fp.panels[0];
        assert_eq!(p.rect.x, 200);
        assert_eq!(p.rect.y, 200);
    }

    #[test]
    fn hit_test_titlebar_includes_shadow_ring() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let p = &fp.panels[0];
        // shadow ring: 2 cols left of panel
        let hit = fp.hit_test_titlebar(p.rect.x - 1, p.rect.y + 1);
        assert!(hit.is_some(), "should hit shadow ring left of panel");
        // shadow ring: 1 row above panel
        let hit = fp.hit_test_titlebar(p.rect.x + 5, p.rect.y - 1);
        assert!(hit.is_some(), "should hit shadow ring above panel");
        // shadow ring: 2 cols right of panel
        let hit = fp.hit_test_titlebar(p.rect.x + p.rect.width, p.rect.y + 1);
        assert!(hit.is_some(), "should hit shadow ring right of panel");
    }

    #[test]
    fn hit_test_titlebar_excludes_content() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let p = &fp.panels[0];
        // content area: x+2..x+w-2, y+3..y+h-2
        let hit = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 4);
        assert!(hit.is_none(), "should not hit content area");
    }

    #[test]
    fn hit_test_titlebar_first_content_row_is_draggable() {
        // Content area starts at y+2 (panel.rect.y + 2). The titlebar
        // exclusion must cover y+2 so that clicks on the first content row
        // (e.g. first history row) are not captured as drag.
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::History,
            WindowContent::History,
            "History",
            canvas(),
        );
        let p = &fp.panels[0];
        // first content row is at y+2, x+3 (inside content x range)
        let hit = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 2);
        assert!(
            hit.is_none(),
            "first content row y+2 must be excluded from titlebar"
        );
        // second content row at y+3 is excluded
        let hit2 = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 3);
        assert!(hit2.is_none(), "second content row y+3 is excluded");
        // title bar at y+0 is draggable
        let hit3 = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y);
        assert!(hit3.is_some(), "title bar y+0 is draggable");
    }

    #[test]
    fn hit_test_close_at_correct_position() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let p = &fp.panels[0];
        let hit = fp.hit_test_close(p.rect.x + 1, p.rect.y + 5);
        assert!(hit.is_some());
        let hit_pad = fp.hit_test_close(p.rect.x, p.rect.y + 5);
        assert!(hit_pad.is_some(), "should hit at padding position too");
        let miss = fp.hit_test_close(p.rect.x + 3, p.rect.y + 5);
        assert!(miss.is_none(), "should not hit beyond 3-wide area");
    }

    #[test]
    fn hit_test_resize_at_correct_position() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let p = &fp.panels[0];
        // ⇲ at (x+w-1, y+h-1) — 3x3 area
        let cx = p.rect.x + p.rect.width - 1;
        let cy = p.rect.y + p.rect.height - 1;
        // center
        assert!(fp.hit_test_resize(cx, cy).is_some());
        // surrounding
        assert!(fp.hit_test_resize(cx - 1, cy).is_some());
        assert!(fp.hit_test_resize(cx + 1, cy).is_some());
        assert!(fp.hit_test_resize(cx, cy - 1).is_some());
        assert!(fp.hit_test_resize(cx, cy + 1).is_some());
        // too far
        assert!(fp.hit_test_resize(cx - 2, cy).is_none());
        assert!(fp.hit_test_resize(cx, cy + 2).is_none());
    }

    #[test]
    fn resize_panel_min_size() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let c = canvas();
        fp.resize_panel(id(&fp, "a"), 5, 3, c);
        assert_eq!(fp.panels[0].rect.width, 20);
        assert_eq!(fp.panels[0].rect.height, 6);
    }

    #[test]
    fn shadow_border_not_overlapped_by_panel() {
        // shadow border is at lx0 (rect.x - 2), which is outside panel.rect
        // panel.rect starts at rect.x, so rect.x - 2 is NOT inside panel
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        let p = &fp.panels[0];
        // shadow top border y = rect.y - 1, panel starts at rect.y
        // so shadow top is NOT inside panel
        assert!(p.rect.y > 0, "panel should not be at y=0 for shadow test");
        // shadow left border x = rect.x - 2, panel starts at rect.x
        assert!(p.rect.x >= 2, "panel should not be at x<2 for shadow test");
    }

    #[test]
    fn reuse_existing_preserves_window_id() {
        let mut fp = WindowManager::default();
        let first = fp.open_with_size(
            "a",
            ContentKey::Task("a".to_string()),
            OpenPolicy::ReuseExisting,
            task_content("a"),
            "a",
            canvas(),
            0,
            0,
            false,
        );
        let second = fp.open_with_size(
            "a",
            ContentKey::Task("a".to_string()),
            OpenPolicy::ReuseExisting,
            task_content("a"),
            "a",
            canvas(),
            0,
            0,
            false,
        );
        assert_eq!(
            first, second,
            "reopening the same handle with ReuseExisting must reuse the same WindowId"
        );
        assert_eq!(fp.panels.len(), 1, "no duplicate panel for reused handle");

        let other = fp.open_with_size(
            "b",
            ContentKey::Task("b".to_string()),
            OpenPolicy::ReuseExisting,
            task_content("b"),
            "b",
            canvas(),
            0,
            0,
            false,
        );
        assert_ne!(
            other, first,
            "a different handle must produce a distinct WindowId"
        );
        assert_eq!(fp.panels.len(), 2);
    }

    #[test]
    fn close_uses_history_not_panels_last() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            task_content("a"),
            "b",
            canvas(),
        );
        fp.open(
            "c",
            ContentKey::Task("c".to_string()),
            task_content("a"),
            "c",
            canvas(),
        );
        // focus order: a was focused first, then b, then c.
        // Now re-focus "a" so it's most-recent in history.
        fp.focus(id(&fp, "a"));
        // close "c" (the last-opened). Focus should go to "a" (most recent in
        // history), not "b" (panels.last()).
        fp.close(id(&fp, "c"));
        assert_eq!(fp.panels.len(), 2);
        assert_eq!(
            fp.focused(),
            Some("a".to_string()),
            "close should refocus by history, not panels.last()"
        );
    }

    #[test]
    fn layer_stack_layer_ordering() {
        let stack = LayerStack::new();
        assert_eq!(
            stack.layers,
            vec![
                LayerKind::Base,
                LayerKind::Docked,
                LayerKind::Floating,
                LayerKind::Modal,
                LayerKind::Blocking,
                LayerKind::Toast,
            ],
            "layers must be ordered lowest-priority (render) first"
        );
    }

    #[test]
    fn layer_stack_render_order_is_ascending_priority() {
        // Render order is lowest priority first; the vector must already be in
        // ascending render_order() order.
        let stack = LayerStack::new();
        let orders: Vec<u8> = stack.layers.iter().map(|l| l.render_order()).collect();
        let mut sorted = orders.clone();
        sorted.sort();
        assert_eq!(orders, sorted, "layers must be in ascending render order");
    }

    #[test]
    fn layer_stack_dispatch_key_none_without_modal() {
        let stack = LayerStack::new();
        assert!(
            stack.dispatch_key().is_none(),
            "with no modal open, dispatch_key must route to nothing"
        );
        assert!(stack.modal_stack.is_empty());
    }

    #[test]
    fn provider_mouse_feedback_is_drained_only_when_provider_is_topmost() {
        let mut wm = WindowManager::default();
        wm.modals.provider_manager.open();
        assert_eq!(
            wm.modals.provider_manager.begin_mutation(
                crate::ProviderMutation::Refresh {
                    provider_id: "provider-id".into(),
                },
                None,
            ),
            crate::provider_manager::ProviderDispatchOutcome::Unavailable
        );
        wm.sync_modals();
        wm.modals.alias_manager.open();
        wm.sync_modals();
        assert_eq!(wm.top_kind(), Some(ModalKind::AliasManager));

        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: 0,
            row: 0,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let mut app = crate::app::AppState::new("session".into(), None);
        let (consumed, _) = wm.dispatch_mouse(&event, &mut app, None);
        assert!(consumed);
        assert!(app.toasts.is_empty());

        wm.modals.alias_manager.close();
        let (consumed, _) = wm.dispatch_mouse(&event, &mut app, None);
        assert!(consumed);
        assert_eq!(wm.top_kind(), Some(ModalKind::ProviderManager));
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].level, crate::app::NoteLevel::Error);
    }

    #[test]
    fn session_switcher_consumes_mouse_before_panel_fallback() {
        let mut wm = WindowManager::default();
        let id = wm.open(
            "mouse",
            ContentKey::Task("mouse".into()),
            task_content("mouse"),
            "mouse",
            canvas(),
        );
        wm.modals.session_switcher.open = true;
        wm.sync_modals();
        let panel = wm.panels.iter().find(|panel| panel.id == id).unwrap();
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: panel.rect.x.saturating_add(3),
            row: panel.rect.y.saturating_add(3),
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let mut app = crate::app::AppState::new("session".into(), None);

        let (consumed, commands) = wm.dispatch_mouse(&event, &mut app, None);

        assert!(consumed);
        assert!(commands.is_empty());
        assert_eq!(wm.panels[0].scroll, 0);
    }

    #[test]
    fn modal_without_mouse_controls_still_blocks_the_underlying_panel() {
        let mut wm = WindowManager::default();
        let id = wm.open(
            "mouse",
            ContentKey::Task("mouse".into()),
            task_content("mouse"),
            "mouse",
            canvas(),
        );
        let consumed_count = Arc::new(AtomicUsize::new(0));
        let panel = wm.panels.iter_mut().find(|panel| panel.id == id).unwrap();
        panel.content = Some(Box::new(MouseConsumingContent(consumed_count.clone())));
        let event = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: panel.rect.x.saturating_add(3),
            row: panel.rect.y.saturating_add(3),
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        wm.modals.model_picker.open = true;
        let mut app = crate::app::AppState::default();
        let (consumed, commands) = wm.dispatch_mouse(&event, &mut app, None);
        assert!(consumed);
        assert!(commands.is_empty());
        assert_eq!(consumed_count.load(Ordering::Relaxed), 0);
        assert_eq!(wm.panels[0].scroll, 0);
    }

    #[test]
    fn component_can_consume_mouse_scroll_before_shell_fallback() {
        let mut wm = WindowManager::default();
        let id = wm.open(
            "mouse",
            ContentKey::Task("mouse".into()),
            task_content("mouse"),
            "mouse",
            canvas(),
        );
        let consumed_count = Arc::new(AtomicUsize::new(0));
        let panel = wm.panels.iter_mut().find(|panel| panel.id == id).unwrap();
        panel.content = Some(Box::new(MouseConsumingContent(consumed_count.clone())));
        let event = MouseEvent {
            kind: MouseEventKind::ScrollRight,
            column: panel.rect.x.saturating_add(3),
            row: panel.rect.y.saturating_add(3),
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let mut app = crate::app::AppState::new("session".into(), None);
        let (consumed, commands) = wm.dispatch_mouse(&event, &mut app, None);

        assert!(consumed);
        assert!(commands.is_empty());
        assert_eq!(consumed_count.load(Ordering::Relaxed), 1);
        assert_eq!(wm.panels[0].h_scroll, 0);
    }

    #[test]
    fn clamp_to_canvas_fits_panel_after_resize() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            canvas(),
        );
        fp.panels[0].rect = Rect::new(0, 0, 100, 50);
        fp.clamp_to_canvas(Rect::new(0, 0, 80, 40));
        let p = &fp.panels[0];
        assert!(p.rect.width <= 80);
        assert!(p.rect.height <= 40);
        assert!(
            p.rect.x + p.rect.width <= 80,
            "panel right edge must stay inside canvas"
        );
        assert!(
            p.rect.y + p.rect.height <= 40,
            "panel bottom edge must stay inside canvas"
        );
    }

    #[test]
    fn small_terminal_does_not_crash() {
        let mut wm = WindowManager::default();
        wm.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            Rect::new(0, 0, 50, 16),
        );
        assert_eq!(wm.panels.len(), 1);
        let p = &wm.panels[0];
        assert!(p.rect.width <= 50);
        assert!(p.rect.height <= 16);
    }

    #[test]
    fn fully_covered_panel_skips_content_projection() {
        let mut wm = WindowManager::default();
        let canvas = Rect::new(0, 0, 100, 40);
        let lower = wm.open(
            "lower",
            ContentKey::Task("lower".into()),
            task_content("lower"),
            "lower",
            canvas,
        );
        let upper = wm.open(
            "upper",
            ContentKey::Task("upper".into()),
            task_content("upper"),
            "upper",
            canvas,
        );
        let rect = Rect::new(10, 5, 60, 24);
        let lower_count = Arc::new(AtomicUsize::new(0));
        let upper_count = Arc::new(AtomicUsize::new(0));
        for panel in &mut wm.panels {
            panel.rect = rect;
            if panel.id == lower {
                panel.content = Some(Box::new(CountingContent(lower_count.clone())));
            } else if panel.id == upper {
                panel.content = Some(Box::new(CountingContent(upper_count.clone())));
            }
        }

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        let empty_set = std::collections::HashSet::new();
        let empty_map = std::collections::HashMap::new();
        let empty_details = std::collections::HashMap::new();
        let resources = std::collections::HashMap::new();
        let prompts = std::collections::HashMap::new();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            content_revision: 0,
            hovered_action: None,
            remove_armed: None,
            resources: &resources,
            prompts: &prompts,
        };
        terminal
            .draw(|frame| {
                render(
                    frame,
                    frame.area(),
                    &mut wm,
                    &[],
                    &[],
                    &[],
                    &empty_map,
                    &empty_details,
                    &empty_map,
                    &empty_map,
                    0,
                    &[],
                    &None,
                    &None,
                    0,
                    None,
                    frame.area(),
                    false,
                    &[],
                    &empty_set,
                    0,
                    &None,
                    &browser,
                );
            })
            .unwrap();

        assert_eq!(lower_count.load(Ordering::Relaxed), 0);
        assert_eq!(upper_count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn clamp_to_canvas_very_small() {
        let mut wm = WindowManager::default();
        wm.open(
            "a",
            ContentKey::Task("a".to_string()),
            task_content("a"),
            "a",
            Rect::new(0, 0, 200, 100),
        );
        wm.clamp_to_canvas(Rect::new(0, 0, 40, 10));
        let p = &wm.panels[0];
        assert!(p.rect.width <= 40);
        assert!(p.rect.height <= 10);
    }

    #[test]
    fn cheatsheet_panel_renders_content() {
        let mut wm = WindowManager::default();
        let canvas = Rect::new(0, 0, 100, 40);
        let id = wm.open(
            "cheatsheet",
            ContentKey::Cheatsheet,
            WindowContent::Cheatsheet,
            "Keybindings",
            canvas,
        );

        if let Some(p) = wm.panels.iter_mut().find(|p| p.id == id) {
            p.content = Some(Box::new(
                crate::window::cheatsheet_panel::CheatsheetPanelContent { scroll: 0 },
            ));
        }

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();

        let snapshots: &[atman_runtime::TaskSnapshot] = &[];
        let items: &[crate::app::OutputItem] = &[];
        let activity_nodes: &[crate::task_panel::ActivityNode] = &[];
        let hovered_btn: Option<(WindowId, PanelBtn)> = None;
        let hovered_history_row: Option<String> = None;
        let expanded_mcp_servers: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let hovered_mcp_row: Option<String> = None;
        let mcp_resources: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>> =
            std::collections::HashMap::new();
        let mcp_prompts: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>> =
            std::collections::HashMap::new();
        let mcp_browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            content_revision: 0,
            hovered_action: None,
            remove_armed: None,
            resources: &mcp_resources,
            prompts: &mcp_prompts,
        };

        terminal
            .draw(|f| {
                crate::wm::render(
                    f,
                    f.area(),
                    &mut wm,
                    snapshots,
                    items,
                    &[],
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    0,
                    activity_nodes,
                    &hovered_btn,
                    &hovered_history_row,
                    0,        // animation_frame
                    None,     // panel_close_armed
                    f.area(), // max_canvas
                    false,    // modal_open
                    &[],      // mcp_servers
                    &expanded_mcp_servers,
                    0, // mcp_selected
                    &hovered_mcp_row,
                    &mcp_browser,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("Ctrl")
                || content.contains("Enter")
                || content.contains("Keybindings"),
            "cheatsheet panel should render keybinding content, got empty or placeholder"
        );
    }

    #[test]
    fn panel_without_content_shows_placeholder() {
        let mut wm = WindowManager::default();
        let canvas = Rect::new(0, 0, 100, 40);
        wm.open(
            "empty",
            ContentKey::History,
            WindowContent::History,
            "History",
            canvas,
        );
        // Do NOT set content — the render path must fall back to the
        // placeholder rather than crashing (Bug 1 fix in content.rs).

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();

        let snapshots: &[atman_runtime::TaskSnapshot] = &[];
        let items: &[crate::app::OutputItem] = &[];
        let activity_nodes: &[crate::task_panel::ActivityNode] = &[];
        let hovered_btn: Option<(WindowId, PanelBtn)> = None;
        let hovered_history_row: Option<String> = None;
        let expanded_mcp_servers: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let hovered_mcp_row: Option<String> = None;
        let mcp_resources: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>> =
            std::collections::HashMap::new();
        let mcp_prompts: std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>> =
            std::collections::HashMap::new();
        let mcp_browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            content_revision: 0,
            hovered_action: None,
            remove_armed: None,
            resources: &mcp_resources,
            prompts: &mcp_prompts,
        };

        terminal
            .draw(|f| {
                crate::wm::render(
                    f,
                    f.area(),
                    &mut wm,
                    snapshots,
                    items,
                    &[],
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    &std::collections::HashMap::new(),
                    0,
                    activity_nodes,
                    &hovered_btn,
                    &hovered_history_row,
                    0,        // animation_frame
                    None,     // panel_close_armed
                    f.area(), // max_canvas
                    false,    // modal_open
                    &[],      // mcp_servers
                    &expanded_mcp_servers,
                    0, // mcp_selected
                    &hovered_mcp_row,
                    &mcp_browser,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content.iter().map(|c| c.symbol()).collect();
        assert!(
            content.contains("no data"),
            "panel without content should render the placeholder, got: {content:?}"
        );
    }
}
