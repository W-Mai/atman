use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::AppState;
use crate::wm::WindowId;
use crate::wm::layer::LayerKind;
use crate::wm::modal_wrappers::{
    AliasManagerWrapper, CompactReviewWrapper, FormModalWrapper, HistorySearchWrapper,
    ModelPickerWrapper, OnboardingWrapper, PaletteWrapper, ProviderManagerWrapper,
    SessionSwitcherWrapper, ThemePickerWrapper, TrustModePickerWrapper,
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

    pub fn sync_from_kinds(&mut self, open: &[ModalKind]) {
        let open_set: HashSet<_> = open.iter().copied().collect();
        let saved_focus: HashMap<_, _> = self
            .modal_stack
            .iter()
            .map(|entry| (entry.kind, entry.pre_modal_focus))
            .collect();

        self.modal_stack
            .retain(|entry| open_set.contains(&entry.kind));
        for &kind in open {
            if self.modal_stack.iter().all(|entry| entry.kind != kind) {
                self.modal_stack.push(ModalEntry {
                    kind,
                    pre_modal_focus: None,
                });
            }
        }
        if let Some(root) = self.modal_stack.first_mut()
            && root.pre_modal_focus.is_none()
        {
            root.pre_modal_focus = saved_focus.get(&root.kind).cloned().flatten();
        }
    }

    pub fn dispatch_key(&self) -> Option<ModalKind> {
        self.modal_stack.last().map(|entry| entry.kind)
    }

    pub fn render_modals(
        &self,
        frame: &mut Frame,
        area: Rect,
        app: &mut AppState,
        modals: &mut crate::wm::ModalManager,
        focused_id: WindowId,
    ) {
        let snapshots = Vec::new();
        let items = Vec::new();
        let expanded_tools = HashSet::new();
        let activity_nodes = Vec::new();
        let mcp_servers = Vec::new();
        let expanded_mcp_servers = HashSet::new();
        let hovered_mcp_row = None;
        let hovered_history_row = None;
        let mcp_resources = app.mcp_resources_cache.clone();
        let mcp_prompts = app.mcp_prompts_cache.clone();
        let mcp_browser = crate::mcp_manager::McpBrowserState {
            tab: app.mcp_browser_tab,
            resources: &mcp_resources,
            prompts: &mcp_prompts,
        };
        let ctx = RenderCtx {
            window_id: focused_id,
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
                ModalKind::Form => FormModalWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::CompactReview => {
                    CompactReviewWrapper { app, modals }.render(frame, area, &ctx)
                }
                ModalKind::SessionSwitcher => {
                    SessionSwitcherWrapper { app, modals }.render(frame, area, &ctx)
                }
                ModalKind::HistorySearch => {
                    HistorySearchWrapper { app, modals }.render(frame, area, &ctx)
                }
                ModalKind::ProviderManager => {
                    ProviderManagerWrapper { app, modals }.render(frame, area, &ctx)
                }
                ModalKind::AliasManager => AliasManagerWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::ModelPicker => ModelPickerWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::Onboarding => OnboardingWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::Palette => PaletteWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::ThemePicker => ThemePickerWrapper { app, modals }.render(frame, area, &ctx),
                ModalKind::TrustModePicker => {
                    TrustModePickerWrapper { app, modals }.render(frame, area, &ctx)
                }
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
}

impl Default for LayerStack {
    fn default() -> Self {
        Self::new()
    }
}
