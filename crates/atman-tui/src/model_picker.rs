use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::keys::KeyAction;
use crate::model_browser::{BrowserAction, BrowserRow, BrowserRowKind, ModelBrowser};

#[derive(Default)]
pub struct ModelPicker {
    pub open: bool,
    browser: ModelBrowser,
    pub picked: Option<String>,
}

impl ModelPicker {
    pub fn open(&mut self) {
        self.open_with_model(None);
    }

    pub fn open_with_model(&mut self, current: Option<&str>) {
        self.open = true;
        self.refresh(current);
    }

    pub fn close(&mut self) {
        self.open = false;
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
        match self.browser.handle_key(action, 0) {
            BrowserAction::Cancelled => self.close(),
            BrowserAction::Selected => {
                self.picked = self.browser.selected().map(|row| row.value.clone());
                self.close();
            }
            BrowserAction::Consumed => {}
        }
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
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .split(area);

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("current: ", Style::default().fg(t.meta_fg.into())),
                Span::styled(&app.context.model, Style::default().fg(t.accent.into())),
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

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓/j/k navigate · PgUp/PgDn scroll · Enter select · Esc cancel",
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
