use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Default)]
pub struct ModelManager {
    pub open: bool,
    models: Vec<ModelRow>,
    selected: usize,
    show_form: bool,
    form_field: usize,
    editing: Option<String>,
    name_editor: InputEditor,
    model_editor: InputEditor,
    provider_editor: InputEditor,
    context_budget_editor: InputEditor,
    thinking_editor: InputEditor,
    max_tokens_editor: InputEditor,
}

#[derive(Clone)]
struct ModelRow {
    name: String,
    model: String,
    provider: String,
    context_budget: u64,
    thinking: bool,
    max_tokens: Option<u32>,
    enabled: bool,
}

impl ModelManager {
    pub fn open(&mut self) {
        self.open = true;
        self.refresh();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
    }

    pub fn refresh(&mut self) {
        let entries = atman_runtime::model_registry::all_model_entries();
        self.models = entries
            .iter()
            .map(|(name, e)| ModelRow {
                name: name.clone(),
                model: if e.model.is_empty() {
                    name.clone()
                } else {
                    e.model.clone()
                },
                provider: e.provider.clone().unwrap_or_default(),
                context_budget: e.context_budget.unwrap_or(0),
                thinking: e.thinking.unwrap_or(false),
                max_tokens: e.max_tokens,
                enabled: e.enabled.unwrap_or(true),
            })
            .collect();
        self.models
            .sort_by(|a, b| a.provider.cmp(&b.provider).then(a.name.cmp(&b.name)));
        if self.selected >= self.models.len() && !self.models.is_empty() {
            self.selected = 0;
        }
    }

    fn open_form(&mut self) {
        self.show_form = true;
        self.form_field = 0;
        self.editing = None;
        self.name_editor = InputEditor::default();
        self.model_editor = InputEditor::default();
        self.provider_editor = InputEditor::default();
        self.context_budget_editor = InputEditor::default();
        self.thinking_editor = InputEditor::default();
        self.thinking_editor.insert_str("false");
        self.max_tokens_editor = InputEditor::default();
        let providers = atman_runtime::model_registry::all_provider_entries();
        if let Some((name, _)) = providers.first() {
            self.provider_editor.insert_str(name);
        }
    }

    fn open_edit(&mut self) {
        let Some(row) = self.models.get(self.selected).cloned() else {
            return;
        };
        self.show_form = true;
        self.form_field = 0;
        self.editing = Some(row.name.clone());
        self.name_editor = InputEditor::default();
        self.name_editor.insert_str(&row.name);
        self.model_editor = InputEditor::default();
        self.model_editor.insert_str(&row.model);
        self.provider_editor = InputEditor::default();
        self.provider_editor.insert_str(&row.provider);
        self.context_budget_editor = InputEditor::default();
        self.context_budget_editor
            .insert_str(&row.context_budget.to_string());
        self.thinking_editor = InputEditor::default();
        self.thinking_editor
            .insert_str(if row.thinking { "true" } else { "false" });
        self.max_tokens_editor = InputEditor::default();
        if let Some(mt) = row.max_tokens {
            self.max_tokens_editor.insert_str(&mt.to_string());
        }
    }

    pub fn handle_key(&mut self, action: &KeyAction) {
        if self.show_form {
            self.handle_form_key(action);
            return;
        }
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                if !self.models.is_empty() {
                    self.selected = (self.selected + 1) % self.models.len();
                }
            }
            KeyAction::Char('a') => self.open_form(),
            KeyAction::Submit => self.open_edit(),
            _ => {}
        }
    }

    fn handle_form_key(&mut self, action: &KeyAction) {
        let editor = match self.form_field {
            0 => &mut self.name_editor,
            1 => &mut self.model_editor,
            2 => &mut self.provider_editor,
            3 => &mut self.context_budget_editor,
            4 => &mut self.thinking_editor,
            _ => &mut self.max_tokens_editor,
        };
        match action {
            KeyAction::Escape => {
                self.show_form = false;
                self.editing = None;
            }
            KeyAction::Submit => {
                self.commit_form();
            }
            KeyAction::Tab => {
                self.form_field = (self.form_field + 1) % 6;
            }
            KeyAction::BackTab => {
                self.form_field = if self.form_field == 0 {
                    5
                } else {
                    self.form_field - 1
                };
            }
            KeyAction::CursorLeft if self.form_field == 4 => {
                let new = if self.thinking_editor.buf().trim() == "true" {
                    "false"
                } else {
                    "true"
                };
                let mut ed = InputEditor::default();
                ed.insert_str(new);
                self.thinking_editor = ed;
            }
            KeyAction::CursorRight if self.form_field == 4 => {
                let new = if self.thinking_editor.buf().trim() == "true" {
                    "false"
                } else {
                    "true"
                };
                let mut ed = InputEditor::default();
                ed.insert_str(new);
                self.thinking_editor = ed;
            }
            KeyAction::Backspace if self.form_field == 4 => {}
            KeyAction::Char(_) if self.form_field == 4 => {}
            KeyAction::Backspace => {
                editor.backspace();
            }
            KeyAction::Char(c) => {
                editor.insert_char(*c);
            }
            _ => {}
        }
    }

    fn commit_form(&mut self) {
        let name = self.name_editor.buf().trim().to_string();
        if name.is_empty() {
            return;
        }
        let model = self.model_editor.buf().trim().to_string();
        let provider = self.provider_editor.buf().trim().to_string();
        let context_budget: u64 = self
            .context_budget_editor
            .buf()
            .trim()
            .parse()
            .unwrap_or(32768);
        let thinking = self.thinking_editor.buf().trim() == "true";
        let max_tokens: Option<u32> = self.max_tokens_editor.buf().trim().parse().ok();

        let entry = atman_runtime::model_registry::ModelEntry {
            model: if model.is_empty() {
                name.clone()
            } else {
                model
            },
            provider: if provider.is_empty() {
                None
            } else {
                Some(provider)
            },
            context_budget: Some(context_budget),
            thinking: Some(thinking),
            max_tokens,
            enabled: Some(true),
            ..Default::default()
        };

        let mut entries = atman_runtime::model_registry::all_model_entries();
        if let Some(ref editing) = self.editing {
            entries.retain(|(n, _)| n != editing);
        }
        entries.push((name, entry));
        let cfg = atman_runtime::model_registry::ModelConfig {
            models: entries.into_iter().collect(),
            providers: atman_runtime::model_registry::all_provider_entries()
                .into_iter()
                .collect(),
            aliases: atman_runtime::model_registry::all_aliases()
                .into_iter()
                .map(|(name, model)| (name, atman_runtime::model_registry::AliasEntry { model }))
                .collect(),
        };
        atman_runtime::model_registry::set_model_config(cfg);
        self.show_form = false;
        self.editing = None;
        self.refresh();
    }
}

impl crate::wm::modal::ModalOverlay for ModelManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        if self.show_form {
            self.render_form(f, area, t);
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let main = rows[0];
        let footer_area = rows[1];
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(main);
        let left_col = columns[0];
        let right_col = Rect {
            x: columns[1].x + 1,
            width: columns[1].width.saturating_sub(1),
            ..columns[1]
        };

        crate::wm::shell::render_section_header(f, left_col, Line::from("Models"), t);
        let inner_left = Rect {
            x: left_col.x,
            y: left_col.y + 2,
            width: left_col.width,
            height: left_col.height.saturating_sub(2).saturating_sub(1),
        };
        let items: Vec<ListItem> = self
            .models
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let selected = i == self.selected;
                let style = if selected {
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let thinking = if m.thinking { " \u{1F9E0}" } else { "" };
                let budget = atman_runtime::humanize::format_count(m.context_budget);
                let disabled = if !m.enabled { " (off)" } else { "" };
                ListItem::new(Line::from(Span::styled(
                    format!(
                        " {}  {:<8}  {}{}{}",
                        m.provider, m.name, budget, thinking, disabled
                    ),
                    style,
                )))
            })
            .collect();
        let mut state = ListState::default().with_selected(Some(self.selected));
        f.render_stateful_widget(List::new(items), inner_left, &mut state);

        crate::wm::shell::render_section_header(f, right_col, Line::from("Details"), t);
        let inner_right = Rect {
            x: right_col.x,
            y: right_col.y + 2,
            width: right_col.width,
            height: right_col.height.saturating_sub(2).saturating_sub(1),
        };
        let mut lines = vec![];
        if let Some(m) = self.models.get(self.selected) {
            let label = Style::default().fg(t.meta_fg.into());
            let val = Style::default().fg(t.accent.into());
            lines.push(Line::from(vec![
                Span::styled(" Name:     ", label),
                Span::styled(&m.name, val),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" Model:    ", label),
                Span::styled(&m.model, val),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" Provider: ", label),
                Span::styled(&m.provider, val),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" Budget:   ", label),
                Span::styled(atman_runtime::humanize::format_count(m.context_budget), val),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" Thinking: ", label),
                Span::styled(if m.thinking { "yes" } else { "no" }, val),
            ]));
            if let Some(mt) = m.max_tokens {
                lines.push(Line::from(vec![
                    Span::styled(" Max Out:  ", label),
                    Span::styled(mt.to_string(), val),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled(" Enabled:  ", label),
                Span::styled(if m.enabled { "yes" } else { "no" }, val),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                " No models. Press a to add one.",
                Style::default().fg(t.meta_fg.into()),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner_right);

        crate::wm::shell::render_column_divider(
            f,
            columns[0].right(),
            columns[0].y,
            columns[0].height,
            t,
        );
        let footer = Paragraph::new(Line::from(Span::styled(
            "a:add  Enter:edit  Esc:close",
            Style::default().fg(t.meta_fg.into()),
        )))
        .alignment(ratatui::layout::Alignment::Right);
        f.render_widget(footer, footer_area);
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
        Line::from("Model Manager")
    }

    fn icon(&self) -> &str {
        "\u{25C6}"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

impl ModelManager {
    fn render_form(&mut self, f: &mut ratatui::Frame, area: Rect, t: &crate::theme::Theme) {
        crate::wm::shell::render_section_header(
            f,
            area,
            Line::from(if self.editing.is_some() {
                "Edit Model"
            } else {
                "Add Model"
            }),
            t,
        );
        let inner = Rect {
            x: area.x,
            y: area.y + 2,
            width: area.width,
            height: area.height.saturating_sub(2).saturating_sub(1),
        };
        let fields: [(&str, &str); 6] = [
            ("Name", self.name_editor.buf()),
            ("Model ID", self.model_editor.buf()),
            ("Provider", self.provider_editor.buf()),
            ("Context Budget", self.context_budget_editor.buf()),
            ("Thinking", self.thinking_editor.buf()),
            ("Max Tokens", self.max_tokens_editor.buf()),
        ];
        let label_style = Style::default().fg(t.meta_fg.into());
        let mut cursor_pos: Option<(u16, u16)> = None;
        let mut y = inner.y;
        for (i, (label, val)) in fields.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let active = i == self.form_field;
            let val_style = if active {
                Style::default().fg(t.accent.into())
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {label:<16}"), label_style),
                    Span::styled(val.to_string(), val_style),
                ])),
                Rect { y, ..inner },
            );
            if active {
                let prefix = format!(" {label:<16}");
                let prefix_w = crate::width::width(&prefix) as u16;
                let val_w = crate::width::width(val) as u16;
                cursor_pos = Some((inner.x + prefix_w + val_w, y));
            }
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    " Tab: next field  Enter: save  Esc: cancel  \u{2190}/\u{2192}: toggle thinking",
                    Style::default().fg(t.meta_fg.into()),
                ))),
                Rect { y, ..inner },
            );
        }
        if let Some((x, y)) = cursor_pos {
            f.set_cursor_position((x, y));
        }
    }
}
