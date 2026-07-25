use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

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
    Disabled,
    Inactive,
    EnvKey,
    Cached,
    Refreshing,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmKind {
    Delete,
    Logout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ProviderFocus {
    #[default]
    ProviderList,
    ModelList,
}

#[derive(Default)]
pub struct ProviderManager {
    pub open: bool,
    pub providers: Vec<ProviderEntry>,
    pub selected: usize,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    model_selected: usize,
    pub open_alias_model: Option<String>,
    show_add: bool,
    focus: ProviderFocus,
    kinds: Vec<(
        &'static str,
        &'static str,
        atman_runtime::auth_store::ProviderKind,
    )>,
    kind_selected: usize,
    name_editor: InputEditor,
    name_focused: bool,
    /// Confirmation dialog state.
    show_confirm: bool,
    confirm_kind: Option<ConfirmKind>,
    confirm_provider_id: Option<String>,
    confirm_provider_name: String,
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
                let has_cache = p.model_cache.is_some();
                let status = if !p.enabled {
                    ProviderStatus::Disabled
                } else if has_cache {
                    ProviderStatus::Cached
                } else {
                    ProviderStatus::Active
                };
                self.providers.push(ProviderEntry {
                    source: ProviderSource::AuthStore { id: p.id.clone() },
                    name: p.name.clone(),
                    kind: format!("{:?}", p.kind).to_lowercase(),
                    status,
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
        self.groups = atman_runtime::model_registry::all_provider_groups();
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
        // Confirmation dialog gets first priority.
        if self.show_confirm {
            match action {
                KeyAction::Char('y') | KeyAction::Char('Y') | KeyAction::Submit => {
                    self.execute_confirm(control_tx);
                }
                KeyAction::Escape | KeyAction::Char('n') | KeyAction::Char('N') => {
                    self.show_confirm = false;
                }
                _ => {}
            }
            return;
        }
        if self.show_add {
            self.handle_add_key(action, control_tx);
            return;
        }
        self.refresh_list();
        match self.focus {
            ProviderFocus::ProviderList => match action {
                KeyAction::Escape => self.close(),
                KeyAction::Tab => {
                    if !self.groups.is_empty() {
                        self.focus = ProviderFocus::ModelList;
                        self.model_selected = 0;
                    }
                }
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
                KeyAction::Char('e') => self.toggle_enabled(),
                KeyAction::Char('d') => self.request_confirm(ConfirmKind::Delete),
                KeyAction::Submit => self.request_confirm(ConfirmKind::Logout),
                KeyAction::Char('r') => self.refresh_selected(control_tx),
                _ => {}
            },
            ProviderFocus::ModelList => {
                let g = match self.groups.get(self.selected) {
                    Some(g) => g,
                    None => {
                        self.focus = ProviderFocus::ProviderList;
                        return;
                    }
                };
                match action {
                    KeyAction::Escape => self.focus = ProviderFocus::ProviderList,
                    KeyAction::Tab => self.focus = ProviderFocus::ProviderList,
                    KeyAction::HistoryUp | KeyAction::Char('k') => {
                        if self.model_selected > 0 {
                            self.model_selected -= 1;
                        }
                    }
                    KeyAction::HistoryDown | KeyAction::Char('j') => {
                        if self.model_selected + 1 < g.models.len() {
                            self.model_selected += 1;
                        }
                    }
                    KeyAction::Char('a') => {
                        if let Some(m) = g.models.get(self.model_selected) {
                            self.open_alias_model = Some(m.slug.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn toggle_enabled(&mut self) {
        if let Some(ProviderEntry {
            source: ProviderSource::AuthStore { id },
            status,
            ..
        }) = self.providers.get_mut(self.selected)
        {
            let new_enabled = !matches!(status, ProviderStatus::Active | ProviderStatus::Cached);
            if let Ok(mut store) = atman_runtime::auth_store::AuthStore::load() {
                if let Some(p) = store.providers.iter_mut().find(|p| p.id == *id) {
                    p.enabled = new_enabled;
                    let _ = store.save();
                    self.refresh_list();
                }
            }
        }
    }

    fn request_confirm(&mut self, kind: ConfirmKind) {
        if let Some(ProviderEntry {
            source: ProviderSource::AuthStore { id },
            name,
            ..
        }) = self.providers.get(self.selected)
        {
            self.show_confirm = true;
            self.confirm_kind = Some(kind);
            self.confirm_provider_id = Some(id.clone());
            self.confirm_provider_name = name.clone();
        }
    }

    fn execute_confirm(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let provider_id = self.confirm_provider_id.take();
        let kind = self.confirm_kind.take();
        self.show_confirm = false;

        let Some(id) = provider_id else { return };

        match kind {
            Some(ConfirmKind::Delete) | Some(ConfirmKind::Logout) => {
                if let Some(tx) = control_tx {
                    let _ = tx.send(crate::TuiControl::AuthLogout { id: id.clone() });
                }
                if let Ok(mut store) = atman_runtime::auth_store::AuthStore::load() {
                    store.remove(&id);
                    let _ = store.save();
                }
                self.refresh_list();
            }
            None => {}
        }
    }

    fn refresh_selected(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if let Some(ProviderEntry {
            source: ProviderSource::AuthStore { id },
            ..
        }) = self.providers.get(self.selected)
        {
            if let Some(tx) = control_tx {
                let _ = tx.send(crate::TuiControl::RefreshProviderModels {
                    provider_id: id.clone(),
                });
            }
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
    let w = area.width.saturating_sub(4).clamp(60, 90);
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
            " Provider Manager ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    if inner.height < 4 {
        return;
    }

    if mgr.show_confirm {
        render_confirm_dialog(f, inner, mgr, &theme);
        return;
    }
    if mgr.show_add {
        render_add_dialog(f, inner, mgr, &theme);
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(inner);
    let main = rows[0];
    let footer_area = rows[1];

    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(main);

    render_provider_list(f, columns[0], mgr, &theme);
    render_model_detail(f, columns[1], mgr, &theme);

    // Footer help
    let help = match mgr.focus {
        ProviderFocus::ProviderList => {
            "e: enable/disable  |  d: delete  |  Enter: logout  |  r: refresh  |  a: add  |  Tab: models  |  Esc: close"
        }
        ProviderFocus::ModelList => "Tab: providers  |  a: alias this model  |  Esc: back",
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        help,
        Style::default().fg(theme.meta_fg),
    )))
    .alignment(ratatui::layout::Alignment::Right);
    f.render_widget(footer, footer_area);
}

fn render_provider_list(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &ProviderManager,
    theme: &crate::theme::Theme,
) {
    let items: Vec<ListItem> = mgr
        .providers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if i == mgr.selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let (status_symbol, muted) = match p.status {
                ProviderStatus::Active => ("●", false),
                ProviderStatus::Cached => ("●", false),
                ProviderStatus::Refreshing => ("◐", false),
                ProviderStatus::Disabled => ("○", true),
                ProviderStatus::EnvKey => ("○", false),
                ProviderStatus::Inactive => ("✗", true),
                ProviderStatus::Error => ("✗", false),
            };
            let s = if muted {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            };
            ListItem::new(Line::from(Span::styled(
                format!(" {} {}", status_symbol, p.name),
                s,
            )))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(mgr.selected));
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border))
        .title(" Providers ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_stateful_widget(List::new(items), inner, &mut state);
}

fn render_model_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &ProviderManager,
    theme: &crate::theme::Theme,
) {
    let block = Block::default().borders(Borders::NONE).title(" Models ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Match model groups by vendor name (e.g. "codex").
    let provider_name = mgr.providers.get(mgr.selected).map(|p| p.name.as_str());
    let mut lines = vec![];
    if let Some(name) = provider_name {
        for g in &mgr.groups {
            if g.provider_name == name {
                lines.push(Line::from(Span::styled(
                    format!(" {} models:", g.models.len()),
                    Style::default().add_modifier(Modifier::BOLD),
                )));
                for (mi, m) in g.models.iter().enumerate() {
                    let max_tok = m
                        .max_output_tokens
                        .map(|n| format!("{}K", n / 1000))
                        .unwrap_or_else(|| "—".to_string());
                    let thinking = if m.thinking { "🧠" } else { "" };
                    let is_sel = mgr.focus == ProviderFocus::ModelList && mgr.model_selected == mi;
                    let style = if is_sel {
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme.tinted_fg)
                    };
                    let prefix = if is_sel { "▶" } else { " " };
                    lines.push(Line::from(Span::styled(
                        format!(
                            " {} {}  {} ctx  {} out  {}",
                            prefix,
                            m.slug,
                            atman_runtime::humanize::format_count(m.context_budget),
                            max_tok,
                            thinking
                        ),
                        style,
                    )));
                }
                break;
            }
        }
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " Select a provider to view models",
            Style::default().fg(theme.meta_fg),
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_add_dialog(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &ProviderManager,
    theme: &crate::theme::Theme,
) {
    let block = Block::default()
        .borders(Borders::NONE)
        .title(" Add Provider ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    // Simplified add dialog rendering — keeping existing logic
    let mut lines = vec![];
    if mgr.name_focused {
        lines.push(Line::from("Name:"));
        lines.push(Line::from(Span::styled(
            format!("  {}", mgr.name_editor.buf()),
            Style::default().fg(theme.accent),
        )));
        lines.push(Line::from("Enter to confirm, Esc to cancel"));
    } else {
        for (i, (label, desc, _)) in mgr.kinds.iter().enumerate() {
            let style = if i == mgr.kind_selected {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                format!(" {} — {}", label, desc),
                style,
            )));
        }
        lines.push(Line::from("Enter to select, Esc to cancel"));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_confirm_dialog(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &ProviderManager,
    theme: &crate::theme::Theme,
) {
    let (action, subtitle) = match mgr.confirm_kind {
        Some(ConfirmKind::Delete) => ("Delete", "This removes credentials and cached models."),
        Some(ConfirmKind::Logout) => (
            "Log out of",
            "This will remove the saved session from auth.json.",
        ),
        None => ("Remove", ""),
    };
    let title = format!("{action} provider \"{}\"?", mgr.confirm_provider_name);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.warn))
        .title(format!(" {action}? "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(Span::styled(
            title.as_str(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(subtitle, Style::default().fg(theme.meta_fg))),
        Line::from(""),
        Line::from(Span::styled(
            "  y / Enter: confirm    n / Esc: cancel",
            Style::default().fg(theme.meta_fg),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center),
        inner,
    );
}
