use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::keys::KeyAction;

#[derive(Default)]
pub struct ModelPicker {
    pub open: bool,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    aliases: Vec<(String, String)>,
    selected: usize,
    pub picked: Option<String>,
}

#[derive(Debug, Clone)]
enum PickerRow {
    Alias { name: String, model: String },
    Model { slug: String },
}

impl ModelPicker {
    pub fn open(&mut self) {
        self.open = true;
        self.refresh();
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    fn refresh(&mut self) {
        self.groups = atman_runtime::model_registry::all_provider_groups();
        self.aliases = atman_runtime::model_registry::all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
        self.picked = None;
    }

    fn rows(&self) -> Vec<PickerRow> {
        let mut rows = Vec::new();
        for (name, model) in &self.aliases {
            rows.push(PickerRow::Alias {
                name: name.clone(),
                model: model.clone(),
            });
        }
        for group in &self.groups {
            for model in &group.models {
                rows.push(PickerRow::Model {
                    slug: model.slug.clone(),
                });
            }
        }
        rows
    }

    pub fn handle_key(&mut self, action: &KeyAction) {
        let len = self.rows().len();
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp | KeyAction::Char('k') if len > 0 => {
                self.selected = self.selected.checked_sub(1).unwrap_or(len - 1);
            }
            KeyAction::HistoryDown | KeyAction::Char('j') if len > 0 => {
                self.selected = (self.selected + 1) % len;
            }
            KeyAction::Submit if len > 0 => {
                let rows = self.rows();
                self.picked = rows.get(self.selected).map(|row| match row {
                    PickerRow::Alias { name, .. } => name.clone(),
                    PickerRow::Model { slug } => slug.clone(),
                });
                self.close();
            }
            _ => {}
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

        let picker_rows = self.rows();
        let items: Vec<ListItem> = picker_rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                let selected = i == self.selected;
                let style = if selected {
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let line = match row {
                    PickerRow::Alias { name, model } => Line::from(vec![
                        Span::styled(format!(" {name:<10}"), style),
                        Span::styled(" → ", Style::default().fg(t.meta_fg.into())),
                        Span::styled(model.clone(), style),
                    ]),
                    PickerRow::Model { slug } => {
                        Line::from(Span::styled(format!(" {slug}"), style))
                    }
                };
                ListItem::new(line)
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        f.render_stateful_widget(List::new(items), rows[1], &mut state);

        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "↑↓/j/k navigate · Enter select · Esc cancel",
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
    ) -> bool {
        self.handle_key(action);
        true
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
