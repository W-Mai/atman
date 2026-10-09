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
    pub open: bool,
    browser: ModelBrowser,
    picked: Option<String>,
    pending: Option<ModelSwitchRequest>,
    next_request_id: u64,
    records: Vec<(usize, Rect)>,
    list_rect: Rect,
    cancel_rect: Rect,
    switch_rect: Rect,
    hover: Option<Position>,
}

impl ModelPicker {
    pub fn open(&mut self) {
        let current = atman_runtime::model_registry::model_info("smart").name;
        self.open_with_model(Some(&current));
    }

    fn open_with_model(&mut self, current: Option<&str>) {
        self.open = true;
        self.clear_geometry();
        if self.pending.is_none() {
            self.refresh(current);
        }
    }

    pub fn close(&mut self) {
        if self.pending.is_none() {
            self.open = false;
            self.clear_geometry();
        }
    }

    fn refresh(&mut self, current: Option<&str>) {
        let enabled = atman_runtime::model_registry::enabled_provider_names();
        let mut rows = Vec::new();
        for (name, model) in atman_runtime::model_registry::all_aliases() {
            rows.push(BrowserRow {
                kind: BrowserRowKind::Alias,
                label: format!("{name:<10} → {model}"),
                value: name,
                selectable: true,
            });
        }
        for group in atman_runtime::model_registry::all_provider_groups()
            .into_iter()
            .filter(|group| enabled.contains(&group.provider_name))
        {
            for model in group.models {
                rows.push(BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: format!(
                        "{} / {}",
                        atman_runtime::model_registry::provider_display_name(&group.provider_name),
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
        self.browser.replace_rows(rows, current);
        self.picked = None;
    }

    pub fn handle_key(&mut self, action: &KeyAction) {
        if self.pending.is_some() {
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
    }

    pub(crate) fn handle_mouse(&mut self, event: &MouseEvent) {
        if !self.open || self.pending.is_some() {
            return;
        }
        let point = Position::new(event.column, event.row);
        self.hover = Some(point);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.cancel_rect.contains(point) {
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
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.records.clear();
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
        let status_gap = u16::from(content.height >= 13);
        let footer_height = action_height + help_height + count_height;
        let body_height = content
            .height
            .saturating_sub(footer_height + status_height + status_gap);
        self.list_rect = Rect::new(
            content.x,
            content.y + status_height + status_gap,
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
                atman_runtime::model_registry::model_info("smart").name,
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
                Paragraph::new("  No models available")
                    .style(Style::default().fg(t.subtle_fg.into())),
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
            "↑↓/j/k: select · PgUp/PgDn: page · Click/Enter: switch"
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
        _tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
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
