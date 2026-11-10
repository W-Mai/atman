use crate::app::{AppState, NoteLevel, ToastPosition};
use crate::wm::WindowManager;

pub struct UiState {
    pub app: AppState,
    pub wm: WindowManager,
    pub selection_menu: Option<crate::selection_menu::SelectionMenu>,
    pub(crate) suspended_navigation: Option<crate::flow_hub::FlowHub>,
    pub(crate) navigation_reply: Option<(Option<crate::control::ControlTicket>, crate::TuiCommand)>,
}

impl std::ops::Deref for UiState {
    type Target = AppState;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

impl std::ops::DerefMut for UiState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.app
    }
}

impl UiState {
    pub(crate) fn composer_visible(&self) -> bool {
        !self.app.focus_rail.page_open() && !self.wm.has_navigation_page()
    }
    pub(crate) fn layout_event(&mut self, event: &crossterm::event::Event) -> bool {
        use crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
        if self.wm.focused_id().is_some()
            || self.wm.modals.any_open()
            || self.selection_menu.is_some()
            || self.app.modal_notification.is_some()
        {
            return false;
        }
        let retry = match event {
            Event::Key(key) if key.kind != KeyEventKind::Release && key.code == KeyCode::F(5) => {
                self.app.layout_cache.preparation_error().is_some()
            }
            Event::Mouse(mouse) => {
                let hit = self
                    .app
                    .layout_retry_rect
                    .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()))
                    && !self
                        .app
                        .composer_rect
                        .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()));
                if mouse.kind == MouseEventKind::Moved {
                    self.app.layout_retry_hover = hit;
                }
                hit && mouse.kind == MouseEventKind::Down(MouseButton::Left)
            }
            _ => false,
        };
        if retry {
            self.app.layout_cache.retry_preparation();
            self.app.mark_visual_dirty();
        }
        retry
    }

    pub(crate) fn has_geometry_animation(&self) -> bool {
        (self.reasoning_visible() && self.app.reasoning_meter.motion_pending())
            || self.wm.has_geometry_animation()
            || (self.wm.top_kind().is_none()
                && self.composer_visible()
                && self
                    .app
                    .flow_loading
                    .or(self.app.navigation_loading)
                    .is_none()
                && !self
                    .app
                    .presentation_settings
                    .as_ref()
                    .is_some_and(crate::settings::SettingsSource::initial_loading)
                && !self
                    .app
                    .last_transcript_rect
                    .is_some_and(|area| self.wm.covers_region(area))
                && (self.app.items.work_fold_animates()
                    || (!self.app.items.is_empty()
                        && self.app.layout_cache.work_fold_needs_frame())))
    }

    pub(crate) fn has_active_animation(&self) -> bool {
        (self.composer_visible()
            && self.wm.top_kind() == Some(crate::wm::ModalKind::InputConsole)
            && self.app.modal_notification.is_none()
            && self.wm.modals.input_console.animates())
            || (self.reasoning_visible() && self.app.reasoning_meter.feedback_pending())
            || self.navigation_animates(self.suspended_navigation.as_ref())
            || (self.app.layout_cache.preparing()
                && !self
                    .app
                    .last_transcript_rect
                    .is_some_and(|area| self.wm.covers_region(area)))
            || self.app.has_active_animation()
            || (self.app.image_paste.is_waiting()
                && self
                    .app
                    .composer_rect
                    .is_some_and(|rect| !self.wm.covers_region(rect)))
            || (!self.app.last_full_rect.is_some_and(|area| {
                self.wm
                    .covers_region(ratatui::layout::Rect::new(area.x, area.y, area.width, 1))
            }) && self
                .app
                .preferences
                .as_ref()
                .is_some_and(|binding| binding.store.is_waiting()))
            || ((self.app.settings.is_waiting()
                || self
                    .app
                    .presentation_settings
                    .as_ref()
                    .is_some_and(crate::settings::SettingsSource::is_waiting))
                && !self.app.last_full_rect.is_some_and(|area| {
                    self.wm
                        .covers_region(ratatui::layout::Rect::new(area.x, area.y, area.width, 1))
                }))
            || (self.app.should_quit
                && self.app.image_paste.work.is_publishing()
                && !self.app.last_full_rect.is_some_and(|area| {
                    self.wm
                        .covers_region(ratatui::layout::Rect::new(area.x, area.y, area.width, 1))
                }))
            || ((self.app.host_controls.is_waiting() || self.app.flow_refreshing)
                && !self.app.last_full_rect.is_some_and(|area| {
                    self.wm
                        .covers_region(ratatui::layout::Rect::new(area.x, area.y, area.width, 1))
                }))
            || ((self.app.clipboard_copies.is_waiting()
                || (self.app.should_quit
                    && self
                        .app
                        .clipboard_copies
                        .service
                        .as_ref()
                        .is_some_and(|service| service.work.is_waiting())))
                && !self.app.last_full_rect.is_some_and(|area| {
                    self.wm
                        .covers_region(ratatui::layout::Rect::new(area.x, area.y, area.width, 1))
                }))
            || (self.wm.top_kind().is_none()
                && (self.app.flow_loading.is_some() || self.app.popup.is_waiting()))
            || self.wm.has_active_animation()
            || self
                .app
                .project_sidebar
                .lock()
                .unwrap()
                .has_active_animation()
    }

    pub(crate) fn navigation_animates(
        &self,
        navigation: Option<&crate::flow_hub::FlowHub>,
    ) -> bool {
        self.navigation_reply.is_none()
            && navigation
                .and_then(|hub| hub.opening_started_at())
                .is_some()
            && !self
                .app
                .last_transcript_rect
                .or(self.app.last_full_rect)
                .is_some_and(|area| self.wm.covers_region(area))
    }

    fn reasoning_visible(&self) -> bool {
        self.composer_visible()
            && !self.wm.modals.any_open()
            && self.app.modal_notification.is_none()
            && self
                .app
                .input_controls
                .iter()
                .any(|(action, _)| *action == crate::input::InputAction::Reasoning)
            && self
                .app
                .input_rect
                .is_some_and(|rect| !self.wm.covers_region(rect))
    }

    pub(crate) fn navigation_text_animates(
        &self,
        navigation: Option<&crate::flow_hub::FlowHub>,
    ) -> bool {
        navigation.is_some_and(|hub| hub.visible() && hub.preparing_text())
            && !self.wm.modals.any_open()
            && self.app.modal_notification.is_none()
            && !self
                .app
                .last_full_rect
                .is_some_and(|area| self.wm.covers_region(area))
    }

    pub(crate) fn navigation_can_open(
        &self,
        navigation: Option<&crate::flow_hub::FlowHub>,
    ) -> bool {
        !self.wm.modals.any_open()
            && self.selection_menu.is_none()
            && self.app.modal_notification.is_none()
            && !self.wm.has_pointer_capture()
            && self.wm.focused_id().is_none_or(|focused| {
                navigation.is_some_and(|hub| hub.opening_focus == Some(focused))
            })
    }

    pub fn new(app: AppState) -> Self {
        let mut wm = WindowManager::default();
        wm.modals.bind_settings(app.settings.clone());
        Self {
            app,
            wm,
            selection_menu: None,
            suspended_navigation: None,
            navigation_reply: None,
        }
    }

    pub fn open_task_panel(&mut self, handle: &str, canvas: ratatui::layout::Rect) {
        self.open_task_panel_impl(handle, canvas, false, false);
    }

    pub fn open_task_panel_maximized(&mut self, handle: &str) {
        let canvas = self.app.maximized_canvas();
        self.open_task_panel_impl(handle, canvas, true, false);
    }

    pub fn open_task_panel_background(&mut self, handle: &str) {
        let canvas = self.app.maximized_canvas();
        let focused_before = self.wm.focused_id();
        let opened = self.open_task_panel_impl(handle, canvas, false, true);
        if focused_before.is_some() && opened {
            self.app.push_toast(
                format!("background task ready — see panel {handle}"),
                NoteLevel::Info,
                std::time::Duration::from_secs(4),
                ToastPosition::TopRight,
            );
        }
    }

    fn open_task_panel_impl(
        &mut self,
        handle: &str,
        canvas: ratatui::layout::Rect,
        maximized: bool,
        background: bool,
    ) -> bool {
        self.app
            .open_task_panel(&mut self.wm, handle, canvas, maximized, background)
    }

    pub fn open_mermaid_panel(&mut self, item_idx: usize, canvas: ratatui::layout::Rect) {
        let id = format!("mermaid:{item_idx}");
        let source = match self.app.items.get(item_idx) {
            Some(crate::app::OutputItem::MermaidDiagram { source }) => source.as_str(),
            _ => "",
        };
        let lines = crate::mermaid::render_mermaid(source, canvas.width.saturating_sub(8));
        let preferred_width = lines
            .iter()
            .map(|l| crate::width::spans_width(&l.spans))
            .max()
            .unwrap_or(80)
            .max(80);
        let preferred_width = u16::try_from(preferred_width).unwrap_or(u16::MAX);
        let preferred_height = u16::try_from(lines.len().saturating_add(5)).unwrap_or(u16::MAX);
        self.wm.open_with_size(
            &id,
            crate::wm::ContentKey::Mermaid(id.clone()),
            crate::wm::OpenPolicy::ReuseExisting,
            crate::wm::WindowContent::Mermaid {
                item_id: id.clone(),
            },
            "Mermaid Diagram",
            canvas,
            preferred_width,
            preferred_height,
            false,
        );
        if let Some(p) = self
            .wm
            .panels
            .iter_mut()
            .find(|p| p.content_key == crate::wm::ContentKey::Mermaid(id.clone()))
        {
            p.content = Some(Box::new(
                crate::window::mermaid_panel::MermaidPanelContent::new(id.clone()),
            ));
        }
    }

    pub fn open_tool_output_panel(&mut self, item_idx: usize, tool_use_id: &str) -> bool {
        let Some(item) = self.app.items.tool_detail(item_idx, tool_use_id) else {
            return false;
        };
        let title = match &item {
            crate::app::OutputItem::DiffPreview { title, .. } => title.clone(),
            crate::app::OutputItem::FsDetail { view, .. } => view.title(),
            _ => "Tool output".to_string(),
        };
        let item_id = format!("tool-output:{item_idx}:{tool_use_id}");
        let canvas = self.app.maximized_canvas();
        let window_id = self.wm.open_with_size(
            &item_id,
            crate::wm::ContentKey::Output(item_id.clone()),
            crate::wm::OpenPolicy::ReuseExisting,
            crate::wm::WindowContent::Output {
                item_id: item_id.clone(),
            },
            &title,
            canvas,
            canvas.width,
            canvas.height,
            true,
        );
        if let Some(panel) = self
            .wm
            .panels
            .iter_mut()
            .find(|panel| panel.id == window_id)
        {
            panel.content = Some(Box::new(
                crate::window::output_panel::OutputPanelContent::new(
                    item_idx,
                    tool_use_id.to_string(),
                ),
            ));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_fold_geometry_ticks_follow_visible_animation_lifetime() {
        let mut ui = UiState::new(AppState::new("session".into(), None));
        ui.app.items.push(crate::app::OutputItem::AssistantMd {
            md: "body".into(),
            streaming: false,
            retried: false,
        });
        ui.app.items.upsert_work_fold(0..1, None, false);
        assert!(ui.has_geometry_animation());
        let loading = Some(crate::loading::ViewLoading {
            phase: crate::loading::ViewPhase::Reading,
            started_at: std::time::Instant::now(),
        });
        for navigation in [false, true] {
            ui.app.flow_loading = if navigation { None } else { loading };
            ui.app.navigation_loading = if navigation { loading } else { None };
            assert!(
                !ui.has_geometry_animation(),
                "loading hides document geometry"
            );
        }
        ui.app.flow_loading = None;
        ui.app.navigation_loading = None;
        assert!(ui.has_geometry_animation());
        ui.wm.modals.palette.open = true;
        ui.wm.sync_modals();
        assert!(!ui.has_geometry_animation());
        ui.wm.modals.palette.open = false;
        ui.wm.sync_modals();
        let canvas = ratatui::layout::Rect::new(0, 0, 120, 40);
        ui.app.last_transcript_rect = Some(canvas);
        let cover = ui.wm.open(
            "cover",
            crate::wm::ContentKey::Cheatsheet,
            crate::wm::WindowContent::Cheatsheet,
            "Cover",
            canvas,
        );
        ui.wm
            .panels
            .iter_mut()
            .find(|panel| panel.id == cover)
            .unwrap()
            .rect = canvas;
        assert!(!ui.has_geometry_animation());
        ui.wm.close(cover);
        assert!(ui.has_geometry_animation());
        let fold = &mut ui.app.items.work_folds[0];
        fold.started_at = std::time::Instant::now() - fold.duration;
        assert!(!ui.has_geometry_animation());
    }
}
