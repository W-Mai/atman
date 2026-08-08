use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::AppState;
use crate::wm::layer::LayerKind;
use crate::wm::modal_wrappers::{
    AliasManagerWrapper, CompactReviewWrapper, FormModalWrapper, HistorySearchWrapper,
    ModelPickerWrapper, OnboardingWrapper, PaletteWrapper, ProviderManagerWrapper,
    SessionSwitcherWrapper, ThemePickerWrapper,
};
use crate::wm::{ModalComponent, ModalEntry, ModalKind, RenderCtx};

pub struct LayerStack {
    pub layers: Vec<LayerKind>,
    pub modal_stack: Vec<ModalEntry>,
}

impl LayerStack {
    pub fn new() -> Self {
        Self {
            layers: vec![
                LayerKind::Base,
                LayerKind::Docked,
                LayerKind::Floating,
                LayerKind::Modal,
                LayerKind::Blocking,
                LayerKind::Toast,
            ],
            modal_stack: Vec::new(),
        }
    }

    pub fn sync_modals(&mut self, app: &mut AppState) {
        let open = Self::open_modals(app);
        let open_set: HashSet<_> = open.iter().copied().collect();
        let saved_focus: HashMap<_, _> = self
            .modal_stack
            .iter()
            .map(|entry| (entry.kind, entry.pre_modal_focus))
            .collect();
        let restore_focus = self
            .modal_stack
            .first()
            .filter(|entry| !open_set.contains(&entry.kind))
            .and_then(|entry| entry.pre_modal_focus);

        self.modal_stack
            .retain(|entry| open_set.contains(&entry.kind));
        for kind in open {
            if self.modal_stack.iter().all(|entry| entry.kind != kind) {
                let pre_modal_focus = if self.modal_stack.is_empty() {
                    app.wm.focus.active
                } else {
                    None
                };
                self.modal_stack.push(ModalEntry {
                    kind,
                    pre_modal_focus,
                });
            }
        }
        if let Some(root) = self.modal_stack.first_mut()
            && root.pre_modal_focus.is_none()
        {
            root.pre_modal_focus = saved_focus.get(&root.kind).cloned().flatten();
        }

        if self.modal_stack.is_empty() {
            if let Some(id) = restore_focus
                && app.wm.panels.iter().any(|panel| panel.id == id)
            {
                app.wm.focus(id);
            }
        } else {
            app.wm.focus.blur();
        }
    }

    pub fn dispatch_key(&self) -> Option<ModalKind> {
        self.modal_stack.last().map(|entry| entry.kind)
    }

    pub fn has_floating(&self, app: &AppState) -> bool {
        app.wm.focused().is_some()
    }

    pub fn render_modals(&self, frame: &mut Frame, area: Rect, app: &mut AppState) {
        let snapshots = Vec::new();
        let items = Vec::new();
        let expanded_tools = HashSet::new();
        let activity_nodes = Vec::new();
        let mcp_servers = Vec::new();
        let expanded_mcp_servers = HashSet::new();
        let hovered_mcp_row = None;
        let hovered_history_row = None;
        let mcp_resources = HashMap::new();
        let mcp_prompts = HashMap::new();
        let mcp_browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            resources: &mcp_resources,
            prompts: &mcp_prompts,
        };
        let ctx = RenderCtx {
            window_id: app.wm.focused_id().unwrap_or(crate::wm::WindowId(0)),
            snapshots: &snapshots,
            items: &items,
            animation_frame: 0,
            expanded_tools: &expanded_tools,
            activity_nodes: &activity_nodes,
            items_version: 0,
            expanded_version: 0,
            mcp_servers: &mcp_servers,
            expanded_mcp_servers: &expanded_mcp_servers,
            mcp_selected: 0,
            hovered_mcp_row: &hovered_mcp_row,
            mcp_browser: &mcp_browser,
            hovered_history_row: &hovered_history_row,
        };
        for entry in &self.modal_stack {
            match entry.kind {
                ModalKind::Form => FormModalWrapper { app }.render(frame, area, &ctx),
                ModalKind::CompactReview => CompactReviewWrapper { app }.render(frame, area, &ctx),
                ModalKind::SessionSwitcher => {
                    SessionSwitcherWrapper { app }.render(frame, area, &ctx)
                }
                ModalKind::HistorySearch => HistorySearchWrapper { app }.render(frame, area, &ctx),
                ModalKind::ProviderManager => {
                    ProviderManagerWrapper { app }.render(frame, area, &ctx)
                }
                ModalKind::AliasManager => AliasManagerWrapper { app }.render(frame, area, &ctx),
                ModalKind::ModelPicker => ModelPickerWrapper { app }.render(frame, area, &ctx),
                ModalKind::Onboarding => OnboardingWrapper { app }.render(frame, area, &ctx),
                ModalKind::Palette => PaletteWrapper { app }.render(frame, area, &ctx),
                ModalKind::ThemePicker => ThemePickerWrapper { app }.render(frame, area, &ctx),
            }
        }
    }

    pub fn render_blocking(&self, f: &mut Frame, area: Rect, app: &AppState) {
        if let Some(ref msg) = app.modal_notification {
            crate::render_notify_modal(f, area, msg);
        }
        if let Some(form) = &app.mcp_add_form {
            let w = 60.min(area.width);
            let h = 22.min(area.height);
            let x = area.x + (area.width - w) / 2;
            let y = area.y + (area.height - h) / 2;
            let form_area = Rect {
                x,
                y,
                width: w,
                height: h,
            };
            crate::wm::render_shadow(f, form_area, &crate::theme::theme());
            crate::mcp_manager::render_mcp_add_form(f, form_area, form);
        }
    }

    pub fn render_toasts(&self, f: &mut Frame, area: Rect, app: &AppState) {
        crate::render_toast_notes(f, area, &app.toasts);
    }

    pub fn dispatch_mouse(&self) -> bool {
        false
    }

    fn open_modals(app: &AppState) -> Vec<ModalKind> {
        const MODALS: [ModalKind; 10] = [
            ModalKind::ThemePicker,
            ModalKind::Palette,
            ModalKind::SessionSwitcher,
            ModalKind::Onboarding,
            ModalKind::ProviderManager,
            ModalKind::ModelPicker,
            ModalKind::AliasManager,
            ModalKind::CompactReview,
            ModalKind::HistorySearch,
            ModalKind::Form,
        ];
        MODALS
            .into_iter()
            .filter(|kind| kind.is_open(app))
            .collect()
    }
}

impl Default for LayerStack {
    fn default() -> Self {
        Self::new()
    }
}
