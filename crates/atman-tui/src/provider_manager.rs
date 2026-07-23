use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Debug, Clone)]
pub struct ProviderEntry {
    pub source: ProviderSource,
    pub name: String,
    pub kind: String,
    pub status: ProviderStatus,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub enum ProviderSource {
    AuthStore { id: String },
    Env,
    Config,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderStatus {
    Active,
    Inactive,
    EnvKey,
}

#[derive(Default)]
pub struct ProviderManager {
    pub open: bool,
    pub providers: Vec<ProviderEntry>,
    pub selected: usize,
    show_add: bool,
    kinds: Vec<(
        &'static str,
        &'static str,
        atman_runtime::auth_store::ProviderKind,
    )>,
    kind_selected: usize,
    name_editor: InputEditor,
    name_focused: bool,
}

impl ProviderManager {
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
        self.show_add = false;
    }

    pub fn refresh_list(&mut self) {
        self.providers.clear();
        if std::env::var("ANTHROPIC_API_KEY").is_ok() {
            self.providers.push(ProviderEntry {
                source: ProviderSource::Env,
                name: "Anthropic".into(),
                kind: "env".into(),
                status: ProviderStatus::EnvKey,
                detail: "ANTHROPIC_API_KEY".into(),
            });
        }
        if std::env::var("OPENAI_API_KEY").is_ok() {
            self.providers.push(ProviderEntry {
                source: ProviderSource::Env,
                name: "OpenAI".into(),
                kind: "env".into(),
                status: ProviderStatus::EnvKey,
                detail: "OPENAI_API_KEY".into(),
            });
        }
        if let Ok(store) = atman_runtime::auth_store::AuthStore::load() {
            for p in &store.providers {
                self.providers.push(ProviderEntry {
                    source: ProviderSource::AuthStore { id: p.id.clone() },
                    name: p.name.clone(),
                    kind: format!("{:?}", p.kind).to_lowercase(),
                    status: ProviderStatus::Active,
                    detail: p.account.clone().unwrap_or_default(),
                });
            }
        }
        for (name, entry) in atman_runtime::model_registry::all_model_entries() {
            if entry.api_key.is_some() {
                self.providers.push(ProviderEntry {
                    source: ProviderSource::Config,
                    name: format!("config:{}", name),
                    kind: "config".into(),
                    status: ProviderStatus::EnvKey,
                    detail: "config.toml".into(),
                });
            }
        }
    }

    fn selected_provider_auth_id(&self) -> Option<String> {
        self.providers
            .get(self.selected)
            .and_then(|e| match &e.source {
                ProviderSource::AuthStore { id } => Some(id.clone()),
                _ => None,
            })
    }

    fn open_add(&mut self) {
        self.show_add = true;
        self.kinds = vec![
            (
                "Codex",
                "ChatGPT Plus/Pro OAuth",
                atman_runtime::auth_store::ProviderKind::Codex,
            ),
            (
                "Claude",
                "Anthropic OAuth",
                atman_runtime::auth_store::ProviderKind::AnthropicOauth,
            ),
            (
                "GitHub Copilot",
                "GitHub OAuth",
                atman_runtime::auth_store::ProviderKind::GitHubCopilot,
            ),
            (
                "Custom",
                "API Key / OpenAI-compatible",
                atman_runtime::auth_store::ProviderKind::Custom,
            ),
        ];
        self.kind_selected = 0;
        self.name_editor = InputEditor::default();
        self.name_focused = false;
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.show_add {
            self.handle_add_key(action, control_tx);
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
                if self.selected + 1 < self.providers.len() {
                    self.selected += 1;
                }
            }
            KeyAction::Char('a') => self.open_add(),
            KeyAction::Char('e') | KeyAction::Submit => {
                if let Some(id) = self.selected_provider_auth_id() {
                    if let Some(tx) = control_tx {
                        let _ = tx.send(crate::TuiControl::AuthLogout { id });
                    }
                    self.refresh_list();
                }
            }
            _ => {}
        }
    }

    fn handle_add_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.name_focused {
            match action {
                KeyAction::Escape => self.name_focused = false,
                KeyAction::Submit => self.commit_add(control_tx),
                KeyAction::Backspace => {
                    self.name_editor.backspace();
                }
                KeyAction::CursorLeft => {
                    self.name_editor.move_left();
                }
                KeyAction::CursorRight => {
                    self.name_editor.move_right();
                }
                KeyAction::CursorHome => {
                    self.name_editor.move_home();
                }
                KeyAction::CursorEnd => {
                    self.name_editor.move_end();
                }
                KeyAction::Char(c) => {
                    self.name_editor.insert_char(*c);
                }
                _ => {}
            }
        } else {
            match action {
                KeyAction::Escape => {
                    self.show_add = false;
                }
                KeyAction::Submit => {
                    let (label, _, _) = self.kinds[self.kind_selected];
                    let mut ed = InputEditor::default();
                    ed.insert_str(label);
                    self.name_editor = ed;
                    self.name_focused = true;
                }
                KeyAction::HistoryUp | KeyAction::Char('k') => {
                    if self.kind_selected > 0 {
                        self.kind_selected -= 1;
                    }
                }
                KeyAction::HistoryDown | KeyAction::Char('j') => {
                    if self.kind_selected + 1 < self.kinds.len() {
                        self.kind_selected += 1;
                    }
                }
                _ => {}
            }
        }
    }

    fn commit_add(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let name = self.name_editor.buf().trim().to_string();
        if name.is_empty() {
            return;
        }
        let kind = self.kinds[self.kind_selected].2.clone();
        if let Some(tx) = control_tx {
            let _ = tx.send(crate::TuiControl::AuthLogin { kind, name });
        }
        self.show_add = false;
        self.name_focused = false;
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, mgr: &ProviderManager) {
    let w = area.width.saturating_sub(4).clamp(50, 78);
    let h = area.height.saturating_sub(2).clamp(8, 22);
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
            " Providers (Esc to close) ",
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

    if mgr.show_add {
        if mgr.name_focused {
            let buf = mgr.name_editor.buf();
            let cur = mgr.name_editor.cursor();
            let lines = vec![
                Line::from(Span::styled(
                    "  Add Provider",
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::raw("")),
                Line::from(Span::raw("  Name:")),
                Line::from(render_cursor("  ", buf, cur, theme.accent)),
                Line::from(Span::raw("")),
                Line::from(Span::styled(
                    "  Enter → confirm   Esc → back",
                    Style::default().fg(theme.subtle_fg),
                )),
            ];
            f.render_widget(Paragraph::new(lines), content);
        } else {
            let items: Vec<ListItem> = mgr
                .kinds
                .iter()
                .enumerate()
                .map(|(i, (l, d, _))| {
                    let pfx = if i == mgr.kind_selected { "▶ " } else { "  " };
                    let line = Line::from(vec![
                        Span::raw(pfx),
                        Span::styled(
                            format!("{:<18}", *l),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(*d, Style::default().fg(theme.subtle_fg)),
                    ]);
                    ListItem::new(line)
                })
                .collect();
            let mut state = ListState::default();
            state.select(Some(mgr.kind_selected));
            f.render_stateful_widget(List::new(items), content, &mut state);
        }
    } else {
        let items: Vec<ListItem> = mgr
            .providers
            .iter()
            .map(|p| {
                let icon = match p.status {
                    ProviderStatus::Active => "✅",
                    ProviderStatus::Inactive => "❌",
                    ProviderStatus::EnvKey => "🔑",
                };
                let line = Line::from(vec![
                    Span::raw(format!(" {icon} ")),
                    Span::styled(
                        p.name.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("[{}]", p.kind),
                        Style::default().fg(theme.subtle_fg),
                    ),
                    Span::raw("  "),
                    Span::styled(p.detail.clone(), Style::default().fg(theme.subtle_fg)),
                ]);
                ListItem::new(line)
            })
            .collect();
        let hl = Style::default()
            .bg(theme.subtle_fg)
            .add_modifier(Modifier::BOLD);
        let mut state = ListState::default();
        if !mgr.providers.is_empty() {
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
    let help = if mgr.show_add {
        if mgr.name_focused {
            "Enter confirm  Esc back to list"
        } else {
            "j/k select  Enter pick type  Esc cancel"
        }
    } else {
        "j/k select  a add provider  Enter login/logout  Esc close"
    };
    f.render_widget(
        Paragraph::new(Span::styled(help, Style::default().fg(theme.subtle_fg))),
        help_rect,
    );
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
