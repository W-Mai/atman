//! Window Manager module — floating-panel implementation and type skeleton.

use std::collections::HashSet;

use crossterm::event::{MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear};
use tokio::sync::mpsc;

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
    HitTestResult, ModalComponent, ModalEntry, ModalKind, ModalManager, OutsideClickPolicy,
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
    pub scroll: u16,
    pub h_scroll: u16,
    pub split: bool,
    pub expanded_tools: HashSet<String>,
    pub render_cache: Option<PanelRenderCache>,
    /// Latest `WindowComponent::content_version()`, tracked each frame.
    /// Used for future background-update rendering decisions.
    pub content_version: u64,
}

/// Cached render output for sub-agent / workflow floating panels.
/// On a cache hit we skip workflow graph traversal, flatten_message, and
/// build_lines — just draw the stored lines and recompute hitmap rects from
/// the stored regions using the *current* scroll/area.
#[derive(Clone, Debug)]
pub struct PanelRenderCache {
    pub(crate) items_version: u64,
    pub(crate) expanded_version: u64,
    pub(crate) width: u16,
    /// None when the panel is done/idle → cache survives across frames.
    /// Some(frame) when running → invalidated every animation tick.
    pub(crate) animation_frame: Option<u32>,
    pub(crate) messages_len: usize,
    pub(crate) workflow_expanded: bool,
    pub(crate) expanded_tools_len: usize,

    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) regions: Vec<crate::output::NodeRegion>,
    pub(crate) tool_headers: Vec<crate::output::ToolHeaderSpot>,
    pub(crate) wf_offset: u32,
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
}

const DEFAULT_PANEL_W: u16 = 88;
const DEFAULT_PANEL_H: u16 = 29;
const MIN_PANEL_W: u16 = 20;
const MIN_PANEL_H: u16 = 6;

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
            (DEFAULT_PANEL_W, DEFAULT_PANEL_H)
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
            if !p.maximized && !maximized {
                let w = w.min(canvas.width.saturating_sub(4));
                let h = h.min(canvas.height.saturating_sub(4));
                p.rect.width = w;
                p.rect.height = h;
            }
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
        let offset = (self.panels.len() as u16) * 3;
        let rect = if maximized {
            maximized_rect(canvas)
        } else {
            let w = w.min(canvas.width.saturating_sub(4));
            let h = h.min(canvas.height.saturating_sub(4));
            Rect {
                x: canvas.x + 2 + offset,
                y: canvas.y + 1 + offset,
                width: w,
                height: h,
            }
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
            render_cache: None,
            content_version: 0,
        };
        self.panels.push(panel);
        if matches!(focus_policy, FocusPolicy::Steal) {
            self.focus.focus(id);
        }
        id
    }

    pub fn close(&mut self, id: WindowId) {
        if let Some(idx) = self.panels.iter().position(|p| p.id == id) {
            if let Some(ref mut content) = self.panels[idx].content {
                content.on_blur();
            }
            self.panels.remove(idx);
            self.focus.remove_and_refocus(id, &self.panels);
        }
    }

    pub fn focus(&mut self, id: WindowId) {
        self.focus.focus(id);
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
            self.bring_to_front(id);
        }
    }

    pub fn move_panel(&mut self, id: WindowId, x: u16, y: u16, _canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.rect.x = x;
            p.rect.y = y;
        }
    }

    pub fn resize_panel(&mut self, id: WindowId, w: u16, h: u16, _canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.rect.width = w.max(MIN_PANEL_W);
            p.rect.height = h.max(MIN_PANEL_H);
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
        let mut sorted: Vec<&WindowInstance> = self.panels.iter().collect();
        sorted.sort_by_key(|p| std::cmp::Reverse(p.z));
        let current_idx = sorted.iter().position(|p| Some(p.id) == self.focus.active);
        let next_idx = match current_idx {
            Some(i) => {
                if forward {
                    (i + 1) % sorted.len()
                } else {
                    (i + sorted.len() - 1) % sorted.len()
                }
            }
            None => 0,
        };
        if let Some(target) = sorted.get(next_idx) {
            let id = target.id;
            self.focus.focus(id);
            self.bring_to_front(id);
        }
    }

    pub fn dispatch_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        _control_tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> (bool, Vec<WmCommand>) {
        self.sync_modals();
        if let Some(kind) = self.layers.dispatch_key() {
            return (
                self.modals.handle_key_top(kind, action, _app, _control_tx),
                Vec::new(),
            );
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
                return (true, vec![WmCommand::CloseWindow(id)]);
            }
            _ => {}
        }

        let Some(content) = panel.content.as_mut() else {
            return (false, Vec::new());
        };
        let mut ctx = EventCtx {
            scroll: &mut panel.scroll,
            h_scroll: &mut panel.h_scroll,
        };
        match content.handle_event(&WmEvent::Key(action.clone()), &mut ctx) {
            WmEventResult::Consumed(commands) => (true, commands),
            WmEventResult::Ignored => (false, Vec::new()),
        }
    }

    pub fn dispatch_mouse(
        &mut self,
        event: &MouseEvent,
        _app: &mut crate::app::AppState,
        _control_tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> (bool, Vec<WmCommand>) {
        let Some(id) = self
            .hit_test_panel(event.column, event.row)
            .map(|panel| panel.id)
        else {
            return (false, Vec::new());
        };
        let Some(panel) = self.panels.iter_mut().find(|panel| panel.id == id) else {
            return (false, Vec::new());
        };
        match event.kind {
            MouseEventKind::ScrollUp => panel.scroll = panel.scroll.saturating_sub(3),
            MouseEventKind::ScrollDown => panel.scroll = panel.scroll.saturating_add(3),
            MouseEventKind::ScrollLeft => panel.h_scroll = panel.h_scroll.saturating_sub(3),
            MouseEventKind::ScrollRight => panel.h_scroll = panel.h_scroll.saturating_add(3),
            _ => return (false, Vec::new()),
        }
        (true, Vec::new())
    }

    pub fn apply_commands(
        &mut self,
        app: &mut crate::app::AppState,
        commands: Vec<WmCommand>,
        control_tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        for command in commands {
            match command {
                WmCommand::FocusWindow(id) => self.focus(id),
                WmCommand::CloseWindow(id) => self.close(id),
                WmCommand::ToggleMaximize(id) => {
                    self.toggle_maximize(id, app.maximized_canvas());
                }
                WmCommand::TermResize { handle, rows, cols } => {
                    if let Some(tx) = control_tx {
                        let _ = tx.send(crate::TuiControl::TermResize { handle, rows, cols });
                    }
                }
                WmCommand::OpenTaskPanel { handle, maximized } => {
                    app.open_task_panel(self, &handle, app.maximized_canvas(), maximized, false);
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
                        crate::wm::WindowContent::Mcp => {
                            Some(Box::new(crate::window::mcp_panel::McpPanelContent {
                                scroll: 0,
                            }))
                        }
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
                &app.mcp_browser_state(),
                app.items_version,
                app.expanded_version,
            );
        }

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
    _canvas: Rect,
    panels: &mut WindowManager,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
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
    items_version: u64,
    expanded_version: u64,
) -> WmHitmap {
    let mut all_hitmap = WmHitmap::default();
    let mut sorted: Vec<usize> = (0..panels.panels.len()).collect();
    sorted.sort_by_key(|&i| panels.panels[i].z);

    for panel in &mut panels.panels {
        if panel.maximized {
            panel.rect = maximized_rect(max_canvas);
        }
    }

    let t = crate::theme::theme();
    for &idx in &sorted {
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
            let content_bg: Color = t.code_bg.into();
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
                items_version,
                expanded_version,
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
        }

        shadow::render_shadow(f, panel.rect, &t);
    }

    all_hitmap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Rect {
        Rect::new(0, 0, 100, 40)
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
                    0, // items_version
                    0, // expanded_version
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
                    0, // items_version
                    0, // expanded_version
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
