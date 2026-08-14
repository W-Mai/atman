use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Default)]
pub struct ModelManager {
    pub open: bool,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    provider_idx: usize,
    model_idx: Vec<usize>,
    show_form: bool,
    form_field: usize,
    editing: Option<String>,
    locked_provider: Option<String>,
    pub open_alias_model: Option<String>,
    selected_provider: String,
    name_editor: InputEditor,
    model_editor: InputEditor,
    context_budget_editor: InputEditor,
    thinking_editor: InputEditor,
    max_tokens_editor: InputEditor,
}

impl ModelManager {
    pub fn open(&mut self) {
        self.open = true;
        self.locked_provider = None;
        self.refresh();
    }

    pub fn open_with_provider(&mut self, provider: &str) {
        self.open = true;
        self.locked_provider = Some(provider.to_string());
        self.refresh();
        self.select_provider(provider);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
    }

    pub fn refresh(&mut self) {
        self.groups = atman_runtime::model_registry::all_provider_groups();
        self.model_idx = vec![0; self.groups.len()];
        if self.provider_idx >= self.groups.len() {
            self.provider_idx = 0;
        }
    }

    fn select_provider(&mut self, name: &str) {
        if let Some(idx) = self.groups.iter().position(|g| g.provider_name == name) {
            self.provider_idx = idx;
        }
    }

    fn current_model(&self) -> Option<&atman_runtime::model_registry::ModelRow> {
        self.groups
            .get(self.provider_idx)
            .and_then(|g| g.models.get(self.model_idx[self.provider_idx]))
    }

    fn current_provider(&self) -> &str {
        self.groups
            .get(self.provider_idx)
            .map(|g| g.provider_name.as_str())
            .unwrap_or("")
    }

    fn open_form(&mut self) {
        self.show_form = true;
        self.form_field = 0;
        self.editing = None;
        self.selected_provider = self.current_provider().to_string();
        self.name_editor = InputEditor::default();
        self.model_editor = InputEditor::default();
        self.context_budget_editor = InputEditor::default();
        self.thinking_editor = InputEditor::default();
        self.thinking_editor.insert_str("false");
        self.max_tokens_editor = InputEditor::default();
    }

    fn open_edit(&mut self) {
        let model = match self.current_model() {
            Some(m) => m.clone(),
            None => return,
        };
        let provider = self.current_provider().to_string();
        let entries = atman_runtime::model_registry::all_model_entries();
        let entry = entries
            .iter()
            .find(|(n, _)| *n == model.slug)
            .map(|(_, e)| e.clone());
        let Some(entry) = entry else { return };

        self.show_form = true;
        self.form_field = 0;
        self.editing = Some(model.slug.clone());
        self.selected_provider = provider;
        self.name_editor = InputEditor::default();
        self.name_editor.insert_str(&model.slug);
        self.model_editor = InputEditor::default();
        self.model_editor.insert_str(if entry.model.is_empty() {
            &model.slug
        } else {
            &entry.model
        });
        self.context_budget_editor = InputEditor::default();
        self.context_budget_editor
            .insert_str(&model.context_budget.to_string());
        self.thinking_editor = InputEditor::default();
        self.thinking_editor
            .insert_str(if model.thinking { "true" } else { "false" });
        self.max_tokens_editor = InputEditor::default();
        if let Some(mt) = entry.max_tokens {
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
                if self.groups.is_empty() {
                    return;
                }
                if self.model_idx[self.provider_idx] > 0 {
                    self.model_idx[self.provider_idx] -= 1;
                } else if self.provider_idx > 0 {
                    self.provider_idx -= 1;
                    let prev = &self.groups[self.provider_idx];
                    self.model_idx[self.provider_idx] = prev.models.len().saturating_sub(1);
                }
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                if self.groups.is_empty() {
                    return;
                }
                let g = &self.groups[self.provider_idx];
                if self.model_idx[self.provider_idx] + 1 < g.models.len() {
                    self.model_idx[self.provider_idx] += 1;
                } else if self.provider_idx + 1 < self.groups.len() {
                    self.provider_idx += 1;
                    self.model_idx[self.provider_idx] = 0;
                }
            }
            KeyAction::Char('n') => self.open_form(),
            KeyAction::Char('a') => {
                if let Some(m) = self.current_model() {
                    self.open_alias_model = Some(m.slug.clone());
                }
            }
            KeyAction::Submit => self.open_edit(),
            _ => {}
        }
    }

    fn handle_form_key(&mut self, action: &KeyAction) {
        let editor = match self.form_field {
            0 => &mut self.name_editor,
            1 => &mut self.model_editor,
            3 => &mut self.context_budget_editor,
            5 => &mut self.max_tokens_editor,
            _ => &mut self.thinking_editor,
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
            KeyAction::Backspace if self.form_field == 2 => {}
            KeyAction::Char(_) if self.form_field == 2 => {}
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
        let provider = self.selected_provider.clone();
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

        crate::wm::shell::render_section_header(f, main, Line::from("Models"), t);
        let inner = Rect {
            x: main.x,
            y: main.y + 2,
            width: main.width,
            height: main.height.saturating_sub(2).saturating_sub(1),
        };

        let mut lines: Vec<Line> = vec![];
        for (pi, g) in self.groups.iter().enumerate() {
            let provider_active = pi == self.provider_idx;
            let p_style = if provider_active {
                Style::default()
                    .fg(t.heading.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.meta_fg.into())
            };
            lines.push(Line::from(Span::styled(
                format!(" {} [{} models]", g.provider_name, g.models.len()),
                p_style,
            )));
            for (mi, m) in g.models.iter().enumerate() {
                let is_current = pi == self.provider_idx && mi == self.model_idx[pi];
                let m_style = if is_current {
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.tinted_fg.into())
                };
                let thinking = if m.thinking { " \u{1F9E0}" } else { "" };
                let budget = atman_runtime::humanize::format_count(m.context_budget);
                lines.push(Line::from(Span::styled(
                    format!("   {}  {}{}", m.slug, budget, thinking),
                    m_style,
                )));
            }
            lines.push(Line::from(""));
        }

        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                " No models. Press n to add one.",
                Style::default().fg(t.meta_fg.into()),
            )));
        }

        f.render_widget(Paragraph::new(lines), inner);

        let footer = Paragraph::new(Line::from(Span::styled(
            "n:add  a:alias  Enter:edit  Esc:close",
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
        if let Some(model) = self.open_alias_model.take() {
            return Some(ModalAction::OpenAliasForModel(model));
        }
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
        let fields: [(&str, String); 6] = [
            ("Name", self.name_editor.buf().to_string()),
            ("Model ID", self.model_editor.buf().to_string()),
            ("Provider", self.selected_provider.clone()),
            (
                "Context Budget",
                self.context_budget_editor.buf().to_string(),
            ),
            ("Thinking", self.thinking_editor.buf().to_string()),
            ("Max Tokens", self.max_tokens_editor.buf().to_string()),
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
                    Span::styled(val.as_str(), val_style),
                ])),
                Rect { y, ..inner },
            );
            if active && i != 2 && i != 4 {
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
