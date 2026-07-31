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
    pub refresh_just_triggered: bool,
    pub test_just_triggered: bool,
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
    api_key_editor: InputEditor,
    base_url_editor: InputEditor,
    provider_type_editor: InputEditor,
    context_budget_editor: InputEditor,
    max_tokens_editor: InputEditor,
    thinking_editor: InputEditor,
    in_form: bool,
    form_field: usize,
    editing_provider: Option<String>,
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
                let kind = entry.provider.as_deref().unwrap_or("unknown");
                let detail = entry
                    .base_url
                    .as_deref()
                    .unwrap_or("")
                    .replace("https://", "")
                    .replace("http://", "");
                self.providers.push(ProviderEntry {
                    source: ProviderSource::Config,
                    name,
                    kind: kind.into(),
                    status: ProviderStatus::EnvKey,
                    detail,
                });
            }
        }
        self.groups = atman_runtime::model_registry::all_provider_groups();
    }

    fn open_add(&mut self) {
        self.show_add = true;
        self.editing_provider = None;
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

    fn open_edit(&mut self, name: &str) {
        let entry = atman_runtime::model_registry::model_entry(name);
        if entry.is_none() || entry.as_ref().and_then(|e| e.api_key.as_ref()).is_none() {
            return;
        }
        let entry = entry.unwrap();
        self.show_add = true;
        self.editing_provider = Some(name.to_string());
        self.in_form = true;
        self.form_field = 0;
        let mut name_ed = InputEditor::default();
        name_ed.insert_str(name);
        self.name_editor = name_ed;
        let mut key_ed = InputEditor::default();
        if let Some(k) = &entry.api_key {
            key_ed.insert_str(k);
        }
        self.api_key_editor = key_ed;
        let mut url_ed = InputEditor::default();
        if let Some(u) = &entry.base_url {
            url_ed.insert_str(u);
        }
        self.base_url_editor = url_ed;
        let mut pt_ed = InputEditor::default();
        if let Some(p) = &entry.provider {
            pt_ed.insert_str(p);
        } else {
            pt_ed.insert_str("openai-compat");
        }
        self.provider_type_editor = pt_ed;
        let mut ctx_ed = InputEditor::default();
        if let Some(c) = entry.context_budget {
            ctx_ed.insert_str(&c.to_string());
        }
        self.context_budget_editor = ctx_ed;
        let mut mt_ed = InputEditor::default();
        if let Some(m) = entry.max_tokens {
            mt_ed.insert_str(&m.to_string());
        }
        self.max_tokens_editor = mt_ed;
        let mut th_ed = InputEditor::default();
        th_ed.insert_str(if entry.thinking.unwrap_or(false) {
            "true"
        } else {
            "false"
        });
        self.thinking_editor = th_ed;
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
                KeyAction::Char('e') => {
                    let p = self.providers.get(self.selected).cloned();
                    if let Some(p) = p {
                        match p.source {
                            ProviderSource::Config => self.open_edit(&p.name),
                            ProviderSource::AuthStore { .. } => self.toggle_enabled(),
                            ProviderSource::Env => {}
                        }
                    }
                }
                KeyAction::Char('d') => self.request_confirm(ConfirmKind::Delete),
                KeyAction::Submit => self.request_confirm(ConfirmKind::Logout),
                KeyAction::Char('r') => {
                    self.refresh_selected(control_tx);
                }
                KeyAction::Char('t') => {
                    self.test_selected(control_tx);
                }
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
                self.refresh_just_triggered = true;
            }
        }
    }

    fn test_selected(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let p = self.providers.get(self.selected).cloned();
        if let Some(p) = p {
            if matches!(p.source, ProviderSource::Config) {
                if let Some(entry) = atman_runtime::model_registry::model_entry(&p.name) {
                    if let (Some(api_key), Some(base_url)) = (entry.api_key, entry.base_url) {
                        let provider_type =
                            entry.provider.unwrap_or_else(|| "openai-compat".into());
                        if let Some(tx) = control_tx {
                            let _ = tx.send(crate::TuiControl::TestProvider {
                                name: p.name,
                                provider_type,
                                api_key,
                                base_url,
                            });
                            self.test_just_triggered = true;
                        }
                    }
                }
            }
        }
    }

    fn test_form(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let name = self.name_editor.buf().trim().to_string();
        let api_key = self.api_key_editor.buf().trim().to_string();
        let base_url = self.base_url_editor.buf().trim().to_string();
        let provider_type = self.provider_type_editor.buf().trim().to_string();
        if name.is_empty() || api_key.is_empty() || base_url.is_empty() {
            return;
        }
        let provider_type = if provider_type.is_empty() {
            "openai-compat".into()
        } else {
            provider_type
        };
        if let Some(tx) = control_tx {
            let _ = tx.send(crate::TuiControl::TestProvider {
                name,
                provider_type,
                api_key,
                base_url,
            });
            self.test_just_triggered = true;
        }
    }

    fn handle_add_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.in_form {
            let editor = match self.form_field {
                0 => &mut self.name_editor,
                1 => &mut self.provider_type_editor,
                2 => &mut self.api_key_editor,
                3 => &mut self.base_url_editor,
                4 => &mut self.context_budget_editor,
                5 => &mut self.max_tokens_editor,
                _ => &mut self.thinking_editor,
            };
            match action {
                KeyAction::Escape => {
                    if self.editing_provider.is_some() {
                        self.show_add = false;
                        self.in_form = false;
                        self.editing_provider = None;
                    } else {
                        self.in_form = false;
                    }
                }
                KeyAction::Char('t') => {
                    self.test_form(control_tx);
                }
                KeyAction::Submit | KeyAction::Tab => {
                    if self.form_field < 6 {
                        self.form_field += 1;
                    } else {
                        self.commit_form(control_tx);
                    }
                }
                KeyAction::Backspace => {
                    editor.backspace();
                }
                KeyAction::CursorLeft => {
                    editor.move_left();
                }
                KeyAction::CursorRight => {
                    editor.move_right();
                }
                KeyAction::CursorHome => {
                    editor.move_home();
                }
                KeyAction::CursorEnd => {
                    editor.move_end();
                }
                KeyAction::Char(c) => {
                    editor.insert_char(*c);
                }
                _ => {}
            }
        } else if self.name_focused {
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
                    let kind = self.kinds[self.kind_selected].2.clone();
                    if kind == atman_runtime::auth_store::ProviderKind::Custom {
                        self.in_form = true;
                        self.form_field = 0;
                        self.name_editor = InputEditor::default();
                        self.api_key_editor = InputEditor::default();
                        self.base_url_editor = InputEditor::default();
                        let mut pt_ed = InputEditor::default();
                        pt_ed.insert_str("openai-compat");
                        self.provider_type_editor = pt_ed;
                        self.context_budget_editor = InputEditor::default();
                        self.max_tokens_editor = InputEditor::default();
                        let mut th_ed = InputEditor::default();
                        th_ed.insert_str("false");
                        self.thinking_editor = th_ed;
                    } else {
                        let (label, _, _) = self.kinds[self.kind_selected];
                        let mut ed = InputEditor::default();
                        ed.insert_str(label);
                        self.name_editor = ed;
                        self.name_focused = true;
                    }
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

    fn commit_form(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let name = self.name_editor.buf().trim().to_string();
        let api_key = self.api_key_editor.buf().trim().to_string();
        let base_url = self.base_url_editor.buf().trim().to_string();
        let provider_type = self.provider_type_editor.buf().trim().to_string();
        let context_budget: Option<u64> = self.context_budget_editor.buf().trim().parse().ok();
        let max_tokens: Option<u32> = self.max_tokens_editor.buf().trim().parse().ok();
        let thinking = matches!(
            self.thinking_editor.buf().trim().to_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
        if name.is_empty() || api_key.is_empty() || base_url.is_empty() {
            return;
        }
        let provider_type = if provider_type.is_empty() {
            "openai-compat".into()
        } else {
            provider_type
        };
        if let Some(tx) = control_tx {
            if self.editing_provider.is_some() {
                let _ = tx.send(crate::TuiControl::UpdateConfigProvider {
                    name,
                    provider_type,
                    api_key,
                    base_url,
                    context_budget,
                    max_tokens,
                    thinking,
                });
            } else {
                let _ = tx.send(crate::TuiControl::AddConfigProvider {
                    name,
                    provider_type,
                    api_key,
                    base_url,
                    context_budget,
                    max_tokens,
                    thinking,
                });
            }
        }
        self.show_add = false;
        self.in_form = false;
        self.editing_provider = None;
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
        .border_style(Style::default().fg(theme.accent.into()))
        .title(Span::styled(
            " Provider Manager ",
            Style::default()
                .fg(theme.accent.into())
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
            "a:add  e:edit  d:delete  r:refresh  t:test  Tab:models  Esc:close"
        }
        ProviderFocus::ModelList => "Tab:providers  a:alias  Esc:back",
    };
    let footer = Paragraph::new(Line::from(Span::styled(
        help,
        Style::default().fg(theme.meta_fg.into()),
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
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let (status_symbol, muted) = match p.status {
                ProviderStatus::Active => ("●", false),
                ProviderStatus::Cached => ("●", false),
                ProviderStatus::Refreshing => ("◐", false),
                ProviderStatus::Disabled => ("○", true),
                ProviderStatus::EnvKey => ("●", false),
                ProviderStatus::Inactive => ("✗", true),
                ProviderStatus::Error => ("✗", false),
            };
            let source_tag = match p.source {
                ProviderSource::Env => "env",
                ProviderSource::AuthStore { .. } => "oauth",
                ProviderSource::Config => "config",
            };
            let s = if muted {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            };
            let detail = if p.detail.is_empty() {
                String::new()
            } else {
                format!("  {}", p.detail)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", status_symbol), s),
                Span::styled(format!("{:<20}", p.name), s),
                Span::styled(
                    format!("[{}]", source_tag),
                    Style::default().fg(theme.meta_fg.into()),
                ),
                Span::styled(detail, Style::default().fg(theme.subtle_fg.into())),
            ]))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(mgr.selected));
    let block = Block::default()
        .borders(Borders::RIGHT)
        .border_style(Style::default().fg(theme.border.into()))
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
    let block = Block::default().borders(Borders::NONE).title(" Details ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let provider = mgr.providers.get(mgr.selected);
    let mut lines = vec![];

    if let Some(p) = provider {
        let label_style = Style::default()
            .fg(theme.subtle_fg.into())
            .add_modifier(Modifier::DIM);
        let val_style = Style::default().fg(theme.tinted_fg.into());

        lines.push(Line::from(vec![
            Span::styled(" Name:   ", label_style),
            Span::styled(p.name.clone(), val_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Kind:   ", label_style),
            Span::styled(p.kind.clone(), val_style),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Source: ", label_style),
            Span::styled(
                match p.source {
                    ProviderSource::Env => "environment variable",
                    ProviderSource::AuthStore { .. } => "OAuth (auth.json)",
                    ProviderSource::Config => "config.toml",
                },
                val_style,
            ),
        ]));

        if !p.detail.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" Info:   ", label_style),
                Span::styled(p.detail.clone(), val_style),
            ]));
        }

        // For config providers, show model entry details
        if matches!(p.source, ProviderSource::Config) {
            if let Some(entry) = atman_runtime::model_registry::model_entry(&p.name) {
                lines.push(Line::from(""));
                if let Some(ctx) = entry.context_budget {
                    lines.push(Line::from(vec![
                        Span::styled(" Context: ", label_style),
                        Span::styled(atman_runtime::humanize::format_count(ctx), val_style),
                    ]));
                }
                if let Some(mt) = entry.max_tokens {
                    lines.push(Line::from(vec![
                        Span::styled(" Max out: ", label_style),
                        Span::styled(format!("{}K", mt / 1000), val_style),
                    ]));
                }
                if let Some(thinking) = entry.thinking {
                    lines.push(Line::from(vec![
                        Span::styled(" Think:  ", label_style),
                        Span::styled(if thinking { "yes" } else { "no" }, val_style),
                    ]));
                }
            }
        }

        // For OAuth providers, show cached models
        if matches!(p.source, ProviderSource::AuthStore { .. }) {
            for g in &mgr.groups {
                if g.provider_name == p.name {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!(" {} cached models:", g.models.len()),
                        label_style,
                    )));
                    for m in &g.models {
                        let thinking = if m.thinking { " 🧠" } else { "" };
                        lines.push(Line::from(Span::styled(
                            format!(
                                "  {}  {}  {}{}",
                                m.slug,
                                atman_runtime::humanize::format_count(m.context_budget),
                                m.max_output_tokens
                                    .map(|n| format!("{}K out", n / 1000))
                                    .unwrap_or_else(|| "—".into()),
                                thinking
                            ),
                            val_style,
                        )));
                    }
                    break;
                }
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " Select a provider to view details",
            Style::default().fg(theme.meta_fg.into()),
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
    let mut lines = vec![];
    let mut cursor_pos: Option<(u16, u16)> = None;
    if mgr.in_form {
        let fields: [(&str, &str); 7] = [
            ("Name", mgr.name_editor.buf()),
            ("Type", mgr.provider_type_editor.buf()),
            ("API Key", mgr.api_key_editor.buf()),
            ("Base URL", mgr.base_url_editor.buf()),
            ("Context Budget", mgr.context_budget_editor.buf()),
            ("Max Output", mgr.max_tokens_editor.buf()),
            ("Thinking", mgr.thinking_editor.buf()),
        ];
        for (i, (label, val)) in fields.iter().enumerate() {
            let active = i == mgr.form_field;
            let style = if active {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.tinted_fg.into())
            };
            lines.push(Line::from(Span::styled(format!(" {label}:"), style)));
            let display_val = if *label == "API Key" && !val.is_empty() {
                "•".repeat(val.len().min(20))
            } else {
                (*val).to_string()
            };
            let val_line = Line::from(Span::styled(
                format!("  {display_val}"),
                Style::default().fg(theme.tinted_fg.into()),
            ));
            if active {
                let cur_buf = match i {
                    0 => mgr.name_editor.buf(),
                    1 => mgr.provider_type_editor.buf(),
                    2 => mgr.api_key_editor.buf(),
                    3 => mgr.base_url_editor.buf(),
                    4 => mgr.context_budget_editor.buf(),
                    5 => mgr.max_tokens_editor.buf(),
                    _ => mgr.thinking_editor.buf(),
                };
                let y = inner.y + (lines.len() as u16);
                let x = inner.x + 2 + cur_buf.len() as u16;
                cursor_pos = Some((x, y));
            }
            lines.push(val_line);
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Tab/Enter next · t:test · Enter on last to save · Esc cancel",
            Style::default().fg(theme.subtle_fg.into()),
        )));
    } else if mgr.name_focused {
        lines.push(Line::from("Name:"));
        lines.push(Line::from(Span::styled(
            format!("  {}", mgr.name_editor.buf()),
            Style::default().fg(theme.accent.into()),
        )));
        let y = inner.y + (lines.len() as u16) - 1;
        let x = inner.x + 2 + mgr.name_editor.buf().len() as u16;
        cursor_pos = Some((x, y));
        lines.push(Line::from("Enter to confirm, Esc to cancel"));
    } else {
        for (i, (label, desc, _)) in mgr.kinds.iter().enumerate() {
            let style = if i == mgr.kind_selected {
                Style::default()
                    .fg(theme.accent.into())
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
    if let Some((x, y)) = cursor_pos {
        f.set_cursor_position((x, y));
    }
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
        .border_style(Style::default().fg(theme.warn.into()))
        .title(format!(" {action}? "));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines = vec![
        Line::from(Span::styled(
            title.as_str(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            subtitle,
            Style::default().fg(theme.meta_fg.into()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  y / Enter: confirm    n / Esc: cancel",
            Style::default().fg(theme.meta_fg.into()),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center),
        inner,
    );
}
