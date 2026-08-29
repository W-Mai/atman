use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::keys::KeyAction;
use crate::model_browser::{BrowserAction, BrowserRow, BrowserRowKind, ModelBrowser};

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
}

impl ModelPicker {
    pub fn open(&mut self) {
        let current = atman_runtime::model_registry::model_info("smart").name;
        self.open_with_model(Some(&current));
    }

    fn open_with_model(&mut self, current: Option<&str>) {
        self.open = true;
        if self.pending.is_none() {
            self.refresh(current);
        }
    }

    pub fn close(&mut self) {
        if self.pending.is_none() {
            self.open = false;
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
        match self.browser.handle_key(action, 0) {
            BrowserAction::Cancelled => self.close(),
            BrowserAction::Selected => {
                self.picked = self.browser.selected().map(|row| row.value.clone());
            }
            BrowserAction::Consumed => {}
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
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

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
                Span::styled(status, Style::default().fg(t.meta_fg.into())),
                Span::styled(model, Style::default().fg(color.into())),
            ])),
            rows[0],
        );

        self.browser.set_visible_rows(rows[1].height as usize);
        let visible = self.browser.visible_rows(rows[1].height as usize);
        let lines = visible
            .map(|index| {
                let row = &self.browser.rows()[index];
                let style = if index == self.browser.selected_index() {
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                Line::from(Span::styled(format!(" {}", row.label), style))
            })
            .collect::<Vec<_>>();
        f.render_widget(Paragraph::new(lines), rows[1]);

        let help = if self.pending.is_some() {
            "Waiting for model switch…"
        } else {
            "↑↓/j/k navigate · PgUp/PgDn scroll · Enter select · Esc cancel"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                help,
                Style::default().fg(t.meta_fg.into()),
            )))
            .alignment(ratatui::layout::Alignment::Right),
            rows[2],
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
}
