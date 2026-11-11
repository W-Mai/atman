use crate::wm::modal::ModalAction;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::keys::KeyAction;
use crate::model_browser::{BrowserAction, BrowserRow, BrowserRowKind, ModelBrowser};
use crate::settings_form::{Action, render_action, render_record};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModelSwitchRequest {
    pub request_id: u64,
    pub model: String,
}

#[derive(Default)]
pub struct ModelPicker {
    pub(crate) settings: crate::settings::SettingsSource,
    pub open: bool,
    browser: ModelBrowser,
    all_rows: Vec<BrowserRow>,
    search: crate::input::InputEditor,
    search_hit: Option<crate::settings_form::FieldHit>,
    cursor: Option<(u16, u16)>,
    picked: Option<String>,
    pending: Option<ModelSwitchRequest>,
    next_request_id: u64,
    records: Vec<(usize, Rect)>,
    list_rect: Rect,
    cancel_rect: Rect,
    switch_rect: Rect,
    hover: Option<Position>,
    composer: Option<crate::flow_view::DocumentIdentity>,
    more_rect: Rect,
}

impl ModelPicker {
    pub fn open(&mut self) {
        if self.pending.is_none() {
            self.composer = None;
        }
        let current = self.settings.models().model_info("smart").name;
        self.open_with_model(Some(&current));
    }

    pub(crate) fn open_compact(&mut self, app: &crate::app::AppState) {
        if app.items.is_archive() {
            return;
        }
        self.open_for(app);
        if self.pending.is_none() {
            self.composer = Some(app.items.identity());
        }
    }

    pub(crate) fn open_for(&mut self, app: &crate::app::AppState) {
        if self.pending.is_none() {
            self.composer = None;
        }
        let current = app.input_model().map(|(_, model)| model);
        self.open_with_model(current.as_deref());
    }

    pub(crate) fn is_compact(&self) -> bool {
        self.composer.is_some()
    }

    pub(crate) fn matches_view(&mut self, app: &crate::app::AppState) -> bool {
        if self
            .composer
            .as_ref()
            .is_some_and(|document| document != &app.items.identity() || app.items.is_archive())
        {
            self.open = false;
            self.clear_geometry();
            return false;
        }
        true
    }

    pub(crate) fn render_compact(
        &mut self,
        frame: &mut ratatui::Frame,
        canvas: Rect,
        app: &crate::app::AppState,
    ) {
        if !self.matches_view(app) {
            return;
        }
        let Some(input) = app.input_rect else {
            self.open = false;
            self.clear_geometry();
            return;
        };
        let anchor = app
            .input_controls
            .iter()
            .find(|(action, _)| *action == crate::input::InputAction::Model)
            .map_or(input, |(_, rect)| Rect::new(rect.x, input.y, rect.width, 1));
        let rect = crate::input_reasoning::floating_rect(
            canvas,
            anchor,
            56,
            (self
                .all_rows
                .len()
                .max(self.browser.rows().len())
                .saturating_add(3)
                .clamp(4, 13)) as u16,
        );
        let hover = self.hover;
        self.clear_geometry();
        self.hover = hover;
        if rect.width < 4 || rect.height < 4 {
            self.open = false;
            return;
        }
        let t = crate::theme::theme();
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(crate::surface::block(crate::surface::Role::Overlay), rect);
        let body = Rect::new(rect.x + 1, rect.y, rect.width - 2, rect.height);
        let close = Rect::new(body.right() - 3, body.y, 3, 1);
        self.cancel_rect = close;
        render_action(
            frame,
            close,
            &Action {
                label: "×",
                ..Default::default()
            },
            &t,
        );
        let waiting = self.settings.initial_loading();
        let title = if waiting || self.pending.is_some() {
            format!(
                "{} {}",
                crate::loading::glyph(app.animation_frame),
                if waiting { "Models" } else { "Switching" }
            )
        } else {
            "Models".into()
        };
        frame.render_widget(
            Paragraph::new(title).style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(body.x + 1, body.y, body.width.saturating_sub(4), 1),
        );
        if waiting || self.settings.failure().is_some() {
            return;
        }
        self.render_search(frame, Rect::new(body.x, body.y + 1, body.width, 1), &t);
        self.list_rect = Rect::new(
            body.x,
            body.y + 2,
            body.width,
            body.height.saturating_sub(3),
        );
        let visible = self.list_rect.height as usize;
        self.browser.set_visible_rows(visible);
        for (offset, index) in self.browser.visible_rows(visible).take(visible).enumerate() {
            let row = Rect::new(body.x, self.list_rect.y + offset as u16, body.width, 1);
            let hovered = self.hover.is_some_and(|point| row.contains(point));
            let bg = render_record(
                frame,
                row,
                index == self.browser.selected_index(),
                hovered,
                &t,
            );
            let text = format!(
                " {} ",
                crate::width::middle_truncate(
                    &self.browser.rows()[index].label,
                    row.width.saturating_sub(2) as usize
                )
            );
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(t.work_action_fg.into()).bg(bg)),
                row,
            );
            self.records.push((index, row));
        }
        if self.browser.rows().is_empty() {
            frame.render_widget(
                Paragraph::new(" No matches").style(Style::default().fg(t.subtle_fg.into())),
                self.list_rect,
            );
        }
        self.more_rect = Rect::new(body.right() - 3, body.bottom() - 1, 3, 1);
        render_action(
            frame,
            self.more_rect,
            &Action {
                label: "⋯",
                ..Default::default()
            },
            &t,
        );
        let detail = self.browser.selected().map_or(
            if self.search.buf().is_empty() {
                "No models available"
            } else {
                "No matches"
            },
            |row| row.label.as_str(),
        );
        frame.render_widget(
            Paragraph::new(crate::width::truncate(
                detail,
                self.more_rect.x.saturating_sub(body.x + 1) as usize,
            ))
            .style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(
                body.x + 1,
                body.bottom() - 1,
                self.more_rect.x.saturating_sub(body.x + 1),
                1,
            ),
        );
    }

    fn open_with_model(&mut self, current: Option<&str>) {
        self.open = true;
        self.clear_geometry();
        if self.pending.is_none() {
            self.search.clear();
            self.refresh(current);
        }
    }

    pub fn close(&mut self) {
        if self.pending.is_none() {
            self.open = false;
            self.clear_geometry();
        }
    }

    pub(crate) fn catalog_changed(&mut self) {
        if self.open {
            let selected = self.browser.selected().map(|row| row.value.clone());
            self.refresh(selected.as_deref());
            self.clear_geometry();
        }
    }

    fn refresh(&mut self, current: Option<&str>) {
        let enabled = self.settings.enabled_provider_names();
        let mut rows = Vec::new();
        for (name, model) in self.settings.models().all_aliases() {
            rows.push(BrowserRow {
                kind: BrowserRowKind::Alias,
                label: format!("{name:<10} → {model}"),
                value: name,
                selectable: true,
            });
        }
        for group in self
            .settings
            .models()
            .all_provider_groups()
            .into_iter()
            .filter(|group| enabled.contains(&group.provider_name))
        {
            for model in group.models {
                rows.push(BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: format!(
                        "{} / {}",
                        self.settings.provider_display_name(&group.provider_name),
                        model.slug
                    ),
                    value: model.slug,
                    selectable: true,
                });
            }
        }
        rows.sort_by(|a, b| {
            (a.kind != BrowserRowKind::Alias, &a.label)
                .cmp(&(b.kind != BrowserRowKind::Alias, &b.label))
        });
        self.all_rows = rows;
        self.filter_rows(current);
        self.picked = None;
    }

    fn filter_rows(&mut self, current: Option<&str>) {
        let query = self.search.buf().trim().to_lowercase();
        let rows = self
            .all_rows
            .iter()
            .filter(|row| {
                query.is_empty()
                    || row.label.to_lowercase().contains(&query)
                    || row.value.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        self.browser.replace_rows(rows, current);
        self.picked = None;
        self.records.clear();
    }

    fn search_changed(&mut self) {
        let selected = self.browser.selected().map(|row| row.value.clone());
        self.filter_rows(selected.as_deref());
    }

    fn render_search(&mut self, frame: &mut ratatui::Frame, rect: Rect, t: &crate::theme::Theme) {
        let value = Rect::new(
            rect.x + rect.width.min(3),
            rect.y,
            rect.width.saturating_sub(4),
            rect.height,
        );
        let offset = self
            .search
            .cursor_display_col()
            .saturating_sub(value.width.saturating_sub(1) as usize);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" / ", Style::default().fg(t.accent.into())),
                Span::styled(
                    crate::width::trim_display_offset(
                        if self.search.buf().is_empty() {
                            "Search models"
                        } else {
                            self.search.buf()
                        },
                        offset,
                        value.width as usize,
                    ),
                    Style::default().fg(if self.search.buf().is_empty() {
                        t.subtle_fg.into()
                    } else {
                        t.tinted_fg.into()
                    }),
                ),
            ])),
            rect,
        );
        self.search_hit = Some(crate::settings_form::FieldHit {
            record: rect,
            value,
            offset,
            arrows: None,
        });
        self.cursor = (self.pending.is_none() && value.width > 0 && value.height > 0).then(|| {
            (
                value.x
                    + self
                        .search
                        .cursor_display_col()
                        .saturating_sub(offset)
                        .min(value.width.saturating_sub(1) as usize) as u16,
                value.y,
            )
        });
        if let Some(cursor) = self.cursor {
            frame.set_cursor_position(cursor);
        }
    }

    pub fn handle_key(&mut self, action: &KeyAction) {
        if self.pending.is_some() {
            return;
        }
        if self.is_compact() && *action == KeyAction::Tab {
            self.composer = None;
            self.clear_geometry();
            return;
        }
        let before_search = self.search.buf().to_owned();
        if *action == KeyAction::RemoveAttachment {
            self.search.clear();
            self.search_changed();
            return;
        }
        if *action != KeyAction::Newline && self.search.handle_key(action) {
            if before_search != self.search.buf() {
                self.search_changed();
            }
            return;
        }
        let before = self.browser.viewport();
        match self.browser.handle_key(action, 0) {
            BrowserAction::Cancelled => self.close(),
            BrowserAction::Selected => {
                self.picked = self.browser.selected().map(|row| row.value.clone());
            }
            BrowserAction::Consumed => {}
        }
        if before != self.browser.viewport() {
            self.records.clear();
        }
    }

    fn clear_geometry(&mut self) {
        self.records.clear();
        self.list_rect = Rect::default();
        self.cancel_rect = Rect::default();
        self.switch_rect = Rect::default();
        self.hover = None;
        self.more_rect = Rect::default();
        self.search_hit = None;
        self.cursor = None;
    }

    pub(crate) fn handle_mouse(&mut self, event: &MouseEvent) {
        if !self.open || self.pending.is_some() {
            return;
        }
        let point = Position::new(event.column, event.row);
        self.hover = Some(point);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = self.search_hit.filter(|hit| hit.record.contains(point)) {
                    self.search.set_cursor_by_display(
                        0,
                        event
                            .column
                            .saturating_sub(hit.value.x)
                            .saturating_add(hit.offset as u16),
                    );
                } else if self.more_rect.contains(point) {
                    self.composer = None;
                    self.clear_geometry();
                } else if self.cancel_rect.contains(point) {
                    self.close();
                } else if self.switch_rect.contains(point) {
                    self.handle_key(&KeyAction::Submit);
                } else if let Some((index, _)) =
                    self.records.iter().find(|(_, rect)| rect.contains(point))
                {
                    let row = index.saturating_sub(self.browser.viewport());
                    if self.browser.select_visible_row(row) {
                        self.handle_key(&KeyAction::Submit);
                    }
                } else if self.is_compact() && !self.list_rect.contains(point) {
                    self.close();
                }
            }
            MouseEventKind::ScrollUp if self.list_rect.contains(point) => {
                self.handle_key(&KeyAction::HistoryUp);
            }
            MouseEventKind::ScrollDown if self.list_rect.contains(point) => {
                self.handle_key(&KeyAction::HistoryDown);
            }
            _ => {}
        }
    }

    pub(crate) fn take_switch_request(&mut self) -> Option<ModelSwitchRequest> {
        let model = self.picked.take()?;
        self.begin_switch(model)
    }

    pub(crate) fn begin_switch(&mut self, model: String) -> Option<ModelSwitchRequest> {
        if self.pending.is_some() {
            return None;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request = ModelSwitchRequest {
            request_id: self.next_request_id,
            model,
        };
        self.pending = Some(request.clone());
        self.hover = None;
        Some(request)
    }

    pub(crate) fn finish_switch(&mut self, request_id: u64, model: &str, succeeded: bool) -> bool {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.request_id == request_id && pending.model == model)
        {
            return false;
        }
        self.pending = None;
        if succeeded {
            self.open = false;
            self.clear_geometry();
        }
        true
    }

    pub(crate) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

impl crate::wm::modal::ModalOverlay for ModelPicker {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.records.clear();
        self.search_hit = None;
        self.cursor = None;
        let inset = 1.min(area.width / 2);
        let top = u16::from(area.height >= 16);
        let content = Rect::new(
            area.x + inset,
            area.y + top,
            area.width.saturating_sub(inset * 2),
            area.height.saturating_sub(top * 2),
        );
        let action_height = 3.min(content.height);
        let help_height = u16::from(content.height >= 8);
        let status_height = u16::from(content.height >= 7);
        let count_height = u16::from(content.height >= 9);
        let search_height = u16::from(content.height >= 2);
        let status_gap = u16::from(content.height >= 13);
        let footer_height = action_height + help_height + count_height;
        let body_height = content
            .height
            .saturating_sub(footer_height + status_height + status_gap + search_height);
        self.list_rect = Rect::new(
            content.x,
            content.y + status_height + status_gap + search_height,
            content.width,
            body_height,
        );
        let action_y = content.bottom().saturating_sub(action_height + help_height);
        let gap = 2.min(content.width);
        let button_width = content.width.saturating_sub(gap) / 2;
        self.cancel_rect = Rect::new(content.x, action_y, button_width, action_height);
        self.switch_rect = Rect::new(
            content.x + button_width + gap,
            action_y,
            content.width.saturating_sub(button_width + gap),
            action_height,
        );

        let (status, model, color) = if let Some(pending) = &self.pending {
            ("switching: ", pending.model.clone(), t.warn)
        } else {
            (
                "current: ",
                self.settings
                    .models()
                    .model_info(&app.input_model_key())
                    .name,
                t.accent,
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(status, Style::default().fg(t.subtle_fg.into())),
                Span::styled(model, Style::default().fg(color.into())),
            ])),
            Rect::new(
                content.x + 2.min(content.width),
                content.y,
                content.width.saturating_sub(4),
                status_height,
            ),
        );

        self.render_search(
            f,
            Rect::new(
                content.x,
                content.y + status_height,
                content.width,
                search_height,
            ),
            t,
        );
        let visible = (self.list_rect.height / 3) as usize;
        self.browser.set_visible_rows(visible);
        for (row, index) in self.browser.visible_rows(visible).take(visible).enumerate() {
            let rect = Rect::new(
                content.x,
                self.list_rect.y + row as u16 * 3,
                content.width,
                3,
            );
            let bg = render_record(
                f,
                rect,
                index == self.browser.selected_index(),
                self.pending.is_none() && self.hover.is_some_and(|point| rect.contains(point)),
                t,
            );
            f.render_widget(
                Paragraph::new(crate::width::truncate(
                    &self.browser.rows()[index].label,
                    rect.width.saturating_sub(4) as usize,
                ))
                .style(Style::default().fg(t.tinted_fg.into()).bg(bg)),
                Rect::new(
                    rect.x + 2.min(rect.width),
                    rect.y + 1,
                    rect.width.saturating_sub(4),
                    1,
                ),
            );
            self.records.push((index, rect));
        }
        if self.browser.rows().is_empty() && body_height > 0 {
            f.render_widget(
                Paragraph::new("  No matches").style(Style::default().fg(t.subtle_fg.into())),
                self.list_rect,
            );
        }
        let selected = if self.browser.rows().is_empty() {
            0
        } else {
            self.browser.selected_index() + 1
        };
        f.render_widget(
            Paragraph::new(format!(
                "  {selected} / {} models",
                self.browser.rows().len()
            ))
            .style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(
                content.x,
                action_y.saturating_sub(count_height),
                content.width,
                count_height,
            ),
        );
        for (rect, label, shortcut, disabled) in [
            (self.cancel_rect, "Cancel", "Esc", self.pending.is_some()),
            (
                self.switch_rect,
                if self.switch_rect.width >= 15 {
                    "Switch"
                } else {
                    "Use"
                },
                "Enter",
                self.pending.is_some() || self.browser.selected().is_none(),
            ),
        ] {
            render_action(
                f,
                rect,
                &Action {
                    label,
                    shortcut,
                    hovered: self.hover.is_some_and(|point| rect.contains(point)),
                    disabled,
                    ..Default::default()
                },
                t,
            );
        }

        let help = if self.pending.is_some() {
            "Waiting for model switch…"
        } else if content.width >= 58 {
            "Type to search · ↑↓: select · Click/Enter: switch"
        } else {
            "↑↓: select · PgUp/PgDn: page"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                help,
                Style::default().fg(t.subtle_fg.into()),
            )))
            .alignment(ratatui::layout::Alignment::Center),
            Rect::new(
                content.x,
                self.switch_rect.bottom(),
                content.width,
                help_height,
            ),
        );
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        _tx: Option<&crate::ControlSender>,
    ) -> Option<ModalAction> {
        self.handle_key(action);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        self.cursor.filter(|_| self.open && self.pending.is_none())
    }

    fn handle_paste(&mut self, text: &str) {
        if self.open && self.pending.is_none() {
            self.search.paste_single_line(text);
            self.search_changed();
        }
    }

    fn title(&self) -> Line<'static> {
        Line::from("Switch Model")
    }

    fn icon(&self) -> &str {
        "◆"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn search_and_expansion_keep_displayed_targets_and_modal_input_ownership() {
        use crate::wm::modal::{ModalKind, ModalManager, ModalOverlay};
        let mut settings = crate::settings::SettingsSource::for_models(std::sync::Arc::new(
            atman_runtime::model_registry::ModelRegistry::new(),
        ));
        settings.ready().await;
        let mut app = crate::app::AppState::new("search".into(), None);
        app.input = "Retained conversation draft".into();
        app.input_rect = Some(Rect::new(3, 20, 60, 6));
        let mut manager = ModalManager::default();
        manager.bind_settings(settings);
        let picker = &mut manager.model_picker;
        picker.open_compact(&app);
        picker.all_rows = vec![
            BrowserRow {
                kind: BrowserRowKind::Alias,
                label: "smart → vendor/reasoning".into(),
                value: "smart".into(),
                selectable: true,
            },
            BrowserRow {
                kind: BrowserRowKind::Model,
                label: "Gateway / mjk-model".into(),
                value: "exact-model".into(),
                selectable: true,
            },
            BrowserRow {
                kind: BrowserRowKind::Model,
                label: "Other / model".into(),
                value: "other-model".into(),
                selectable: true,
            },
        ];
        picker.filter_rows(None);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        terminal
            .draw(|f| picker.render_compact(f, f.area(), &app))
            .unwrap();
        let original = picker.search_hit.unwrap().record;
        let stale = picker.records[0].1;
        for c in "mjk".chars() {
            picker.handle_key(&KeyAction::Char(c));
        }
        assert!(
            picker.is_compact(),
            "search letters cannot trigger navigation shortcuts"
        );
        assert!(
            picker.records.is_empty(),
            "unpainted filtered rows cannot receive old clicks"
        );
        picker.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: stale.x,
            row: stale.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(picker.picked.is_none());
        terminal
            .draw(|f| picker.render_compact(f, f.area(), &app))
            .unwrap();
        assert_eq!(picker.search_hit.unwrap().record, original);
        assert_eq!(picker.browser.selected().unwrap().value, "exact-model");
        picker.handle_key(&KeyAction::Char('z'));
        assert!(picker.browser.rows().is_empty());
        picker.handle_key(&KeyAction::Submit);
        assert!(
            picker.take_switch_request().is_none(),
            "empty search cannot select an old model"
        );
        picker.handle_key(&KeyAction::RemoveAttachment);
        manager.dispatch_paste(ModalKind::ModelPicker, "GATEWAY", &mut app, None);
        assert_eq!(manager.model_picker.browser.rows().len(), 1);
        terminal
            .draw(|f| manager.model_picker.render_compact(f, f.area(), &app))
            .unwrap();
        assert!(manager.cursor_visible(ModalKind::ModelPicker));
        assert_eq!(
            manager.cursor_position(ModalKind::ModelPicker).unwrap().1,
            original.y
        );
        let picker = &mut manager.model_picker;
        let hit = picker.search_hit.unwrap();
        picker.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.value.x,
            row: hit.value.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(picker.open);
        assert_eq!(picker.search.cursor(), 0);
        picker.handle_key(&KeyAction::Tab);
        assert!(!picker.is_compact());
        assert_eq!(picker.search.buf(), "GATEWAY");
        terminal
            .draw(|f| picker.render_content(f, f.area(), &app, &crate::theme::theme()))
            .unwrap();
        let rect = picker.records[0].1;
        picker.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 1,
            row: rect.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let request = picker.take_switch_request().unwrap();
        assert_eq!(request.model, "exact-model");
        picker.handle_key(&KeyAction::Backspace);
        assert_eq!(picker.search.buf(), "GATEWAY");
        assert_eq!(app.input, "Retained conversation draft");
        assert!(picker.finish_switch(request.request_id, &request.model, true));
    }

    #[tokio::test]
    async fn catalog_refresh_keeps_the_pending_request_and_updates_retry_choices() {
        let temp = tempfile::tempdir().unwrap();
        let config = atman_runtime::config_hub::ConfigHub::from_config_dir(temp.path());
        config.add_alias("selected", "example").unwrap();
        let mut picker = ModelPicker {
            settings: config.clone().into(),
            ..Default::default()
        };
        picker.settings.ready().await;
        picker.open_with_model(Some("selected"));
        picker.handle_key(&KeyAction::Submit);
        let request = picker.take_switch_request().unwrap();
        config.add_alias("added", "another").unwrap();
        picker.settings.invalidate();
        picker.settings.ready().await;
        picker.catalog_changed();
        assert_eq!(picker.pending.as_ref(), Some(&request));
        assert_eq!(picker.browser.selected().unwrap().value, "selected");
        assert_eq!(picker.browser.rows().len(), 2);

        config.remove_alias("selected").unwrap();
        picker.settings.invalidate();
        picker.settings.ready().await;
        picker.catalog_changed();
        assert_eq!(picker.pending.as_ref(), Some(&request));
        assert_eq!(picker.browser.selected().unwrap().value, "added");
        picker.handle_key(&KeyAction::Submit);
        assert!(picker.take_switch_request().is_none());
        assert!(picker.finish_switch(request.request_id, &request.model, false));
        picker.handle_key(&KeyAction::Submit);
        assert_eq!(picker.take_switch_request().unwrap().model, "added");
    }

    fn picker() -> ModelPicker {
        ModelPicker {
            open: true,
            browser: ModelBrowser::new(
                vec![BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "Model".into(),
                    value: "provider:model".into(),
                    selectable: true,
                }],
                None,
            ),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn compact_models_keep_painted_identity_paging_and_source_binding() {
        use std::sync::Arc;
        let mut picker = picker();
        picker.settings = crate::settings::SettingsSource::for_models(Arc::new(
            atman_runtime::model_registry::ModelRegistry::new(),
        ));
        picker.settings.ready().await;
        let mut app = crate::app::AppState::new("compact".into(), None);
        app.input_rect = Some(Rect::new(3, 20, 60, 5));
        picker.composer = Some(app.items.identity());
        picker.browser = ModelBrowser::new(
            (0..20)
                .map(|index| BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: format!("gateway / model-{index}"),
                    value: format!("exact-model-{index}"),
                    selectable: true,
                })
                .collect(),
            None,
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        picker.handle_key(&KeyAction::End);
        terminal
            .draw(|frame| picker.render_compact(frame, frame.area(), &app))
            .unwrap();
        assert_eq!(picker.records.last().unwrap().0, 19);
        let (index, rect) = picker.records[0];
        assert_eq!(rect.height, 1);
        let event = MouseEvent {
            kind: MouseEventKind::Moved,
            column: rect.x + 1,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        picker.handle_mouse(&event);
        terminal
            .draw(|frame| picker.render_compact(frame, frame.area(), &app))
            .unwrap();
        assert_eq!(picker.hover, Some(Position::new(event.column, event.row)));
        picker.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            ..event
        });
        let request = picker.take_switch_request().unwrap();
        assert_eq!(request.model, format!("exact-model-{index}"));
        app.items = Default::default();
        assert!(!picker.matches_view(&app));
        assert!(!picker.open && picker.records.is_empty() && picker.is_pending());
        assert!(picker.finish_switch(request.request_id, &request.model, true));
    }

    #[test]
    fn selection_stays_open_until_matching_switch_result() {
        let mut picker = picker();
        picker.handle_key(&KeyAction::Submit);
        assert!(picker.open);

        let request = picker.take_switch_request().unwrap();
        assert!(picker.is_pending());
        picker.handle_key(&KeyAction::Escape);
        assert!(picker.open);

        assert!(!picker.finish_switch(request.request_id + 1, &request.model, true));
        assert!(picker.is_pending());
        assert!(picker.open);

        assert!(picker.finish_switch(request.request_id, &request.model, false));
        assert!(!picker.is_pending());
        assert!(picker.open);

        picker.handle_key(&KeyAction::Submit);
        let retry = picker.take_switch_request().unwrap();
        assert!(picker.finish_switch(retry.request_id, &retry.model, true));
        assert!(!picker.open);
    }

    #[test]
    fn padded_model_records_keep_mouse_identity_after_paging_and_lock_while_pending() {
        use crate::wm::modal::ModalOverlay;
        for (width, height) in [(36, 14), (76, 28)] {
            let mut picker = picker();
            picker.browser = ModelBrowser::new(
                (0..20)
                    .map(|index| BrowserRow {
                        kind: BrowserRowKind::Model,
                        label: format!("Provider / model-{index}"),
                        value: format!("model-{index}"),
                        selectable: true,
                    })
                    .collect(),
                None,
            );
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let app = crate::app::AppState::default();
            let t = crate::theme::theme();
            terminal
                .draw(|f| picker.render_content(f, f.area(), &app, &t))
                .unwrap();
            picker.handle_key(&KeyAction::End);
            assert!(picker.records.is_empty());
            terminal
                .draw(|f| picker.render_content(f, f.area(), &app, &t))
                .unwrap();
            assert_eq!(picker.records.last().unwrap().0, 19);
            let (index, rect) = picker.records[0];
            assert_eq!(rect.height, 3);
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y + 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            picker.handle_mouse(&event);
            let request = picker.take_switch_request().unwrap();
            assert_eq!(request.model, format!("model-{index}"));
            picker.handle_mouse(&event);
            assert!(picker.take_switch_request().is_none());
            assert!(picker.open);
            assert!(picker.finish_switch(request.request_id, &request.model, true));
            assert!(picker.records.is_empty());
        }
    }
}
