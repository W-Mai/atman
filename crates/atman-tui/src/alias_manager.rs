use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Default)]
pub struct AliasManager {
    pub open: bool,
    pub aliases: Vec<(String, String)>,
    pub selected: usize,
    show_form: bool,
    form_title: String,
    editor: InputEditor,
    models: Vec<(String, String)>,
    model_index: usize,
    focused: usize,
    edit_original: Option<String>,
    is_edit: bool,
}

impl AliasManager {
    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn open(&mut self) {
        self.open = true;
        self.refresh_list();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
    }

    pub fn refresh_list(&mut self) {
        self.aliases = atman_runtime::model_registry::all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
    }

    fn refresh_models(&mut self) {
        let mut names: Vec<(String, String)> = atman_runtime::model_registry::all_model_entries()
            .into_iter()
            .map(|(k, _)| (k.clone(), k))
            .collect();
        for id in atman_runtime::model_registry::discovered_models() {
            if !names.iter().any(|(k, _)| *k == id) {
                names.push((id.clone(), id));
            }
        }
        for (_, model) in atman_runtime::model_registry::all_aliases() {
            if !names.iter().any(|(k, _)| *k == model) {
                names.push((model.clone(), model));
            }
        }
        names.sort_by(|a, b| a.0.cmp(&b.0));
        if names.is_empty() {
            names = vec![
                ("gpt-5.2-codex".into(), "gpt-5.2-codex".into()),
                ("gpt-5.1-codex".into(), "gpt-5.1-codex".into()),
                ("gpt-4o-mini".into(), "gpt-4o-mini".into()),
                (
                    "deepseek/deepseek-v4-pro".into(),
                    "deepseek/deepseek-v4-pro".into(),
                ),
            ];
        }
        self.models = names;
    }

    fn open_form(&mut self, is_edit: bool, alias: Option<&str>, model: Option<&str>) {
        self.show_form = true;
        self.is_edit = is_edit;
        self.form_title = if is_edit {
            "Edit Alias".into()
        } else {
            "Add Alias".into()
        };
        let mut ed = InputEditor::default();
        if let Some(a) = alias {
            ed.insert_str(a);
        }
        self.editor = ed;
        self.refresh_models();
        self.model_index = model
            .and_then(|m| self.models.iter().position(|(k, _)| k == m))
            .unwrap_or(0);
        self.focused = 0;
        self.edit_original = alias.map(|s| s.to_string());
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        _control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
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
                if self.selected + 1 < self.aliases.len() {
                    self.selected += 1;
                }
            }
            KeyAction::Char('a') => self.open_form(false, None, None),
            KeyAction::Char('e') | KeyAction::Submit => {
                if let Some((a, m)) = self.aliases.get(self.selected) {
                    let (a, m) = (a.clone(), m.clone());
                    self.open_form(true, Some(&a), Some(&m));
                }
            }
            KeyAction::Char('d') => {
                if let Some((a, _)) = self.aliases.get(self.selected) {
                    let a = a.clone();
                    atman_runtime::model_registry::remove_alias_from_config(&a).ok();
                    self.refresh_list();
                }
            }
            _ => {}
        }
    }

    fn handle_form_key(&mut self, action: &KeyAction) {
        match action {
            KeyAction::Escape => {
                self.show_form = false;
            }
            KeyAction::Tab => {
                self.focused = if self.focused == 0 { 1 } else { 0 };
            }
            KeyAction::Submit => self.commit_form(),
            _ => {
                if self.focused == 0 {
                    match action {
                        KeyAction::Backspace => {
                            self.editor.backspace();
                        }
                        KeyAction::DeleteWordBackward => {
                            self.editor.delete_word_backward();
                        }
                        KeyAction::CursorLeft => {
                            self.editor.move_left();
                        }
                        KeyAction::CursorRight => {
                            self.editor.move_right();
                        }
                        KeyAction::CursorHome => {
                            self.editor.move_home();
                        }
                        KeyAction::CursorEnd => {
                            self.editor.move_end();
                        }
                        KeyAction::Char(c) => {
                            self.editor.insert_char(*c);
                        }
                        _ => {}
                    }
                } else {
                    match action {
                        KeyAction::HistoryUp | KeyAction::Char('k') => {
                            if self.model_index > 0 {
                                self.model_index -= 1;
                            }
                        }
                        KeyAction::HistoryDown | KeyAction::Char('j') => {
                            if self.model_index + 1 < self.models.len() {
                                self.model_index += 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn commit_form(&mut self) {
        let alias = self.editor.buf().trim().to_string();
        if alias.is_empty() {
            return;
        }
        let model = self
            .models
            .get(self.model_index)
            .map(|(k, _)| k.clone())
            .unwrap_or_default();
        if model.is_empty() {
            return;
        }
        if self.is_edit {
            let old = self.edit_original.as_deref().unwrap_or(&alias);
            atman_runtime::model_registry::update_alias_in_config(old, &alias, &model).ok();
        } else {
            atman_runtime::model_registry::add_alias_to_config(&alias, &model).ok();
        }
        self.refresh_list();
        self.show_form = false;
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, mgr: &AliasManager) {
    let w = area.width.saturating_sub(4).clamp(50, 78);
    let h = area.height.saturating_sub(2).clamp(10, 24);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);

    let theme = crate::theme::theme();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(Span::styled(
            " Aliases (Esc to close) ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    if inner.height < 3 {
        return;
    }

    let content = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };

    if mgr.show_form {
        render_alias_form(f, content, mgr);
    } else {
        let items: Vec<ListItem> = mgr
            .aliases
            .iter()
            .map(|(a, m)| {
                let line = Line::from(vec![
                    Span::styled(
                        format!("  {:<20}", a),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("→ "),
                    Span::styled(m.clone(), Style::default().fg(theme.subtle_fg)),
                ]);
                ListItem::new(line)
            })
            .collect();
        let hl = Style::default()
            .bg(theme.subtle_fg)
            .add_modifier(Modifier::BOLD);
        let mut state = ListState::default();
        if !mgr.aliases.is_empty() {
            state.select(Some(mgr.selected));
        }
        f.render_stateful_widget(List::new(items).highlight_style(hl), content, &mut state);
    }

    let help_rect = Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    let help = if mgr.show_form {
        "Tab switch field  j/k pick model  Enter save  Esc cancel"
    } else {
        "j/k select  a add  e edit  d delete  Esc close"
    };
    f.render_widget(
        Paragraph::new(Span::styled(help, Style::default().fg(theme.subtle_fg))),
        help_rect,
    );
}

fn render_alias_form(f: &mut ratatui::Frame, area: Rect, mgr: &AliasManager) {
    let theme = crate::theme::theme();

    let buf = mgr.editor.buf();
    let cur = mgr.editor.cursor();
    let name_line = if mgr.focused == 0 {
        Line::from(render_cursor("  Name: ", buf, cur, theme.accent))
    } else {
        Line::from(vec![
            Span::raw("  Name: "),
            if buf.is_empty() {
                Span::styled("_", Style::default().fg(theme.subtle_fg))
            } else {
                Span::raw(buf.to_string())
            },
        ])
    };

    let model_header = if mgr.focused == 1 {
        Span::styled(
            "  Model:  (j/k select)",
            Style::default().add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  Model:", Style::default().fg(theme.subtle_fg))
    };

    let mut lines = vec![
        Line::from(Span::styled(
            format!("  {}", mgr.form_title),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::raw("")),
        name_line,
        Line::from(Span::raw("")),
        Line::from(model_header),
    ];

    let available = area.height.saturating_sub(lines.len() as u16 + 2);
    for (i, (_k, display)) in mgr.models.iter().take(available as usize).enumerate() {
        let (pfx, style) = if mgr.focused == 1 && i == mgr.model_index {
            ("▶ ", Style::default().add_modifier(Modifier::BOLD))
        } else {
            ("  ", Style::default().fg(theme.subtle_fg))
        };
        lines.push(Line::from(vec![
            Span::raw(format!("    {pfx}")),
            Span::styled(display.clone(), style),
        ]));
    }

    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled(
        "  Enter → save   Esc → cancel",
        Style::default().fg(theme.subtle_fg),
    )));
    f.render_widget(Paragraph::new(lines), area);
}

fn render_cursor<'a>(
    prefix: &str,
    text: &'a str,
    cursor_byte: usize,
    accent: ratatui::style::Color,
) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = vec![Span::raw(prefix.to_string())];
    let cursor = cursor_byte.min(text.len());
    let before = &text[..cursor];
    let ch = text[cursor..].chars().next();
    let after = &text[cursor + ch.map(|c| c.len_utf8()).unwrap_or(0)..];
    if !before.is_empty() {
        spans.push(Span::raw(before.to_string()));
    }
    if let Some(ch) = ch {
        spans.push(Span::styled(
            ch.to_string(),
            Style::default().bg(accent).fg(ratatui::style::Color::Black),
        ));
    } else {
        spans.push(Span::styled(" ", Style::default().bg(accent)));
    }
    if !after.is_empty() {
        spans.push(Span::raw(after.to_string()));
    }
    spans
}
