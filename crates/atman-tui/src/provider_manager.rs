use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

fn provider_types() -> Vec<&'static str> {
    atman_runtime::model_registry::config_provider_types()
}

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
}

#[derive(Debug, Clone)]
enum AddProviderKind {
    Preset(usize),
    OAuth(atman_runtime::auth_store::ProviderKind),
    Custom,
}

#[derive(Debug, Clone)]
struct AddProviderOption {
    label: &'static str,
    description: &'static str,
    kind: AddProviderKind,
}

#[derive(Default)]
pub struct ProviderManager {
    pub open: bool,
    pub last_input_rect: Option<ratatui::layout::Rect>,
    pub providers: Vec<ProviderEntry>,
    pub selected: usize,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    pub refresh_just_triggered: bool,
    pub test_just_triggered: bool,
    pub test_btn_rect: Option<Rect>,
    pub add_just_completed: bool,
    pub last_added_name: Option<String>,
    show_add: bool,
    focus: ProviderFocus,
    add_options: Vec<AddProviderOption>,
    kind_selected: usize,
    name_editor: InputEditor,
    name_focused: bool,
    api_key_editor: InputEditor,
    base_url_editor: InputEditor,
    provider_type_editor: InputEditor,
    api_key_env_editor: InputEditor,
    enabled_editor: InputEditor,
    pub in_form: bool,
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

    pub fn handle_mouse(
        &mut self,
        me: &crossterm::event::MouseEvent,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if !self.in_form {
            return;
        }
        let Some(rect) = self.test_btn_rect else {
            return;
        };
        let col = me.column;
        let row = me.row;
        let hit = col >= rect.x
            && col < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height;
        use crossterm::event::MouseEventKind;
        match me.kind {
            MouseEventKind::Moved if hit => {
                self.form_field = 7;
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) if hit => {
                self.test_form(control_tx);
            }
            _ => {}
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
        for (name, entry) in atman_runtime::model_registry::all_provider_entries() {
            let detail = entry
                .base_url
                .as_deref()
                .unwrap_or("")
                .replace("https://", "")
                .replace("http://", "");
            let status = if entry.enabled == Some(false) {
                ProviderStatus::Disabled
            } else {
                ProviderStatus::Active
            };
            self.providers.push(ProviderEntry {
                source: ProviderSource::Config,
                name,
                kind: entry.kind.clone(),
                status,
                detail,
            });
        }
        self.groups = atman_runtime::model_registry::all_provider_groups();
    }

    pub fn open_add(&mut self) {
        self.open = true;
        self.show_add = true;
        self.editing_provider = None;
        self.in_form = false;
        self.name_focused = false;
        self.add_options = atman_runtime::model_registry::PROVIDER_PRESETS
            .iter()
            .enumerate()
            .map(|(i, preset)| AddProviderOption {
                label: preset.name,
                description: preset.description,
                kind: AddProviderKind::Preset(i),
            })
            .chain([
                AddProviderOption {
                    label: "Claude OAuth",
                    description: "Anthropic OAuth",
                    kind: AddProviderKind::OAuth(
                        atman_runtime::auth_store::ProviderKind::AnthropicOauth,
                    ),
                },
                AddProviderOption {
                    label: "GitHub Copilot",
                    description: "GitHub OAuth",
                    kind: AddProviderKind::OAuth(
                        atman_runtime::auth_store::ProviderKind::GitHubCopilot,
                    ),
                },
                AddProviderOption {
                    label: "Custom",
                    description: "API Key / OpenAI-compatible",
                    kind: AddProviderKind::Custom,
                },
            ])
            .collect();
        self.kind_selected = 0;
        self.name_editor = InputEditor::default();
    }

    fn open_edit(&mut self, name: &str) {
        let providers = atman_runtime::model_registry::all_provider_entries();
        let Some(entry) = providers.iter().find(|(n, _)| n == name).map(|(_, e)| e) else {
            return;
        };
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
        let mut env_ed = InputEditor::default();
        if let Some(e) = &entry.api_key_env {
            env_ed.insert_str(e);
        }
        self.api_key_env_editor = env_ed;
        let mut url_ed = InputEditor::default();
        if let Some(u) = &entry.base_url {
            url_ed.insert_str(u);
        }
        self.base_url_editor = url_ed;
        let mut pt_ed = InputEditor::default();
        pt_ed.insert_str(&entry.kind);
        self.provider_type_editor = pt_ed;
        let mut en_ed = InputEditor::default();
        en_ed.insert_str(if entry.enabled.unwrap_or(true) {
            "true"
        } else {
            "false"
        });
        self.enabled_editor = en_ed;
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
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
            return None;
        }
        if self.show_add {
            return self
                .handle_add_key(action, control_tx)
                .or(Some(ModalAction::Consumed));
        }
        self.refresh_list();
        match self.focus {
            ProviderFocus::ProviderList => match action {
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
                KeyAction::Char('n') => self.open_add(),
                KeyAction::Char('m') => {
                    if let Some(p) = self.providers.get(self.selected) {
                        if matches!(p.source, ProviderSource::Config) {
                            return Some(ModalAction::OpenModelManager(p.name.clone()));
                        }
                    }
                }
                KeyAction::Char('e') => {
                    let p = self.providers.get(self.selected).cloned();
                    if let Some(p) = p {
                        match p.source {
                            ProviderSource::Env => {}
                            _ => self.toggle_enabled(control_tx),
                        }
                    }
                }
                KeyAction::Char('d') => self.request_confirm(ConfirmKind::Delete),
                KeyAction::Submit => {
                    let p = self.providers.get(self.selected).cloned();
                    if let Some(p) = p {
                        match p.source {
                            ProviderSource::Env => {}
                            ProviderSource::AuthStore { .. } => {
                                self.request_confirm(ConfirmKind::Logout);
                            }
                            ProviderSource::Config => self.open_edit(&p.name),
                        }
                    }
                }
                KeyAction::Char('r') => {
                    self.refresh_selected(control_tx);
                }
                KeyAction::Char('t') => {
                    self.test_selected(control_tx);
                }
                _ => {}
            },
        }
        None
    }

    fn toggle_enabled(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let p = self.providers.get(self.selected).cloned();
        if let Some(p) = p {
            match p.source {
                ProviderSource::AuthStore { id } => {
                    let new_enabled =
                        !matches!(p.status, ProviderStatus::Active | ProviderStatus::Cached);
                    if let Ok(mut store) = atman_runtime::auth_store::AuthStore::load() {
                        if let Some(stored) = store.providers.iter_mut().find(|x| x.id == id) {
                            stored.enabled = new_enabled;
                            let _ = store.save();
                            self.refresh_list();
                        }
                    }
                }
                ProviderSource::Config => {
                    let providers = atman_runtime::model_registry::all_provider_entries();
                    if let Some(entry) = providers
                        .iter()
                        .find(|(n, _)| *n == p.name)
                        .map(|(_, e)| e.clone())
                    {
                        let current_enabled = entry.enabled.unwrap_or(true);
                        if let Some(tx) = control_tx {
                            let _ = tx.send(crate::TuiControl::UpdateConfigProvider {
                                name: p.name.clone(),
                                provider_type: entry.kind.clone(),
                                api_key: entry.api_key.unwrap_or_default(),
                                api_key_env: entry.api_key_env.unwrap_or_default(),
                                base_url: entry.base_url.unwrap_or_default(),
                                max_tokens: entry.max_tokens,
                                enabled: !current_enabled,
                            });
                        }
                    }
                }
                ProviderSource::Env => {}
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
                let providers = atman_runtime::model_registry::all_provider_entries();
                if let Some(entry) = providers.iter().find(|(n, _)| *n == p.name).map(|(_, e)| e) {
                    if let (Some(api_key), Some(base_url)) = (&entry.api_key, &entry.base_url) {
                        let provider_type = entry.kind.clone();
                        if let Some(tx) = control_tx {
                            let _ = tx.send(crate::TuiControl::TestProvider {
                                name: p.name,
                                provider_type,
                                api_key: api_key.clone(),
                                base_url: base_url.clone(),
                            });
                            self.test_just_triggered = true;
                        }
                    }
                }
            }
        }
    }

    fn open_custom_form(&mut self) {
        self.in_form = true;
        self.form_field = 0;
        self.name_editor = InputEditor::default();
        self.api_key_editor = InputEditor::default();
        self.base_url_editor = InputEditor::default();
        let mut pt_ed = InputEditor::default();
        pt_ed.insert_str(atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE);
        self.provider_type_editor = pt_ed;
        self.api_key_env_editor = InputEditor::default();
        let mut en_ed = InputEditor::default();
        en_ed.insert_str("true");
        self.enabled_editor = en_ed;
    }

    fn open_preset_form(&mut self, preset_idx: usize) {
        let Some(preset) = atman_runtime::model_registry::PROVIDER_PRESETS.get(preset_idx) else {
            return;
        };
        self.open_custom_form();
        let mut name_ed = InputEditor::default();
        name_ed.insert_str(preset.name);
        self.name_editor = name_ed;
        let mut pt_ed = InputEditor::default();
        pt_ed.insert_str(preset.provider_type);
        self.provider_type_editor = pt_ed;
        let mut url_ed = InputEditor::default();
        url_ed.insert_str(preset.base_url);
        self.base_url_editor = url_ed;
        if !preset.needs_api_key {
            self.form_field = 2;
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
            atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE.into()
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
    ) -> Option<ModalAction> {
        if self.in_form {
            if self.form_field == 6 {
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
                    KeyAction::Submit => {
                        self.test_form(control_tx);
                    }
                    KeyAction::Tab => {
                        self.form_field = 0;
                    }
                    KeyAction::BackTab => {
                        self.form_field = 5;
                    }
                    _ => {}
                }
                return None;
            }
            let editor = match self.form_field {
                0 => &mut self.name_editor,
                1 => &mut self.provider_type_editor,
                2 => &mut self.api_key_editor,
                3 => &mut self.api_key_env_editor,
                4 => &mut self.base_url_editor,
                _ => &mut self.enabled_editor,
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
                KeyAction::Submit => {
                    return self.commit_form(control_tx);
                }
                KeyAction::Tab => {
                    self.form_field = (self.form_field + 1) % 7;
                }
                KeyAction::BackTab => {
                    self.form_field = if self.form_field == 0 {
                        6
                    } else {
                        self.form_field - 1
                    };
                }
                KeyAction::CursorLeft if self.form_field == 1 => {
                    let current = self.provider_type_editor.buf().trim();
                    let types = provider_types();
                    let idx = types.iter().position(|t| *t == current).unwrap_or(0);
                    let new_idx = if idx == 0 { types.len() - 1 } else { idx - 1 };
                    let mut ed = InputEditor::default();
                    ed.insert_str(types[new_idx]);
                    self.provider_type_editor = ed;
                }
                KeyAction::CursorRight if self.form_field == 1 => {
                    let current = self.provider_type_editor.buf().trim();
                    let types = provider_types();
                    let idx = types.iter().position(|t| *t == current).unwrap_or(0);
                    let new_idx = (idx + 1) % types.len();
                    let mut ed = InputEditor::default();
                    ed.insert_str(types[new_idx]);
                    self.provider_type_editor = ed;
                }
                KeyAction::Backspace if self.form_field == 1 => {}
                KeyAction::Char(_) if self.form_field == 1 => {}
                KeyAction::CursorLeft if self.form_field == 5 => {
                    let current = self.enabled_editor.buf().trim();
                    let new = if current == "true" { "false" } else { "true" };
                    let mut ed = InputEditor::default();
                    ed.insert_str(new);
                    self.enabled_editor = ed;
                }
                KeyAction::CursorRight if self.form_field == 5 => {
                    let current = self.enabled_editor.buf().trim();
                    let new = if current == "true" { "false" } else { "true" };
                    let mut ed = InputEditor::default();
                    ed.insert_str(new);
                    self.enabled_editor = ed;
                }
                KeyAction::Backspace if self.form_field == 5 => {}
                KeyAction::Char(_) if self.form_field == 5 => {}
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
                    let option = self.add_options.get(self.kind_selected).cloned()?;
                    match option.kind {
                        AddProviderKind::Preset(idx) => {
                            let preset = &atman_runtime::model_registry::PROVIDER_PRESETS[idx];
                            if preset.provider_type == "codex" {
                                if let Some(tx) = control_tx {
                                    let _ = tx.send(crate::TuiControl::AuthLogin {
                                        kind: atman_runtime::auth_store::ProviderKind::Codex,
                                        name: preset.name.to_string(),
                                    });
                                }
                                self.show_add = false;
                                self.open = false;
                                self.last_added_name = Some(preset.name.to_string());
                                self.add_just_completed = true;
                            } else {
                                self.open_preset_form(idx);
                            }
                        }
                        AddProviderKind::OAuth(kind) => {
                            let mut ed = InputEditor::default();
                            ed.insert_str(option.label);
                            self.name_editor = ed;
                            self.name_focused = true;
                            self.add_options[self.kind_selected].kind =
                                AddProviderKind::OAuth(kind);
                        }
                        AddProviderKind::Custom => self.open_custom_form(),
                    }
                }
                KeyAction::HistoryUp | KeyAction::Char('k') => {
                    if self.kind_selected > 0 {
                        self.kind_selected -= 1;
                    }
                }
                KeyAction::HistoryDown | KeyAction::Char('j') => {
                    if self.kind_selected + 1 < self.add_options.len() {
                        self.kind_selected += 1;
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn commit_form(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        let name = self.name_editor.buf().trim().to_string();
        let api_key = self.api_key_editor.buf().trim().to_string();
        let api_key_env = self.api_key_env_editor.buf().trim().to_string();
        let base_url = self.base_url_editor.buf().trim().to_string();
        let provider_type = self.provider_type_editor.buf().trim().to_string();
        let enabled = matches!(
            self.enabled_editor.buf().trim().to_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
        if name.is_empty() || base_url.is_empty() {
            return None;
        }
        let provider_type = if provider_type.is_empty() {
            atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE.into()
        } else {
            provider_type
        };
        let is_preset = atman_runtime::model_registry::PROVIDER_PRESETS
            .iter()
            .any(|p| p.base_url == base_url);
        if let Some(tx) = control_tx {
            if self.editing_provider.is_some() {
                let _ = tx.send(crate::TuiControl::UpdateConfigProvider {
                    name: name.clone(),
                    provider_type,
                    api_key,
                    api_key_env,
                    base_url,
                    max_tokens: None,
                    enabled,
                });
            } else {
                let _ = tx.send(crate::TuiControl::AddConfigProvider {
                    name: name.clone(),
                    provider_type,
                    api_key,
                    api_key_env,
                    base_url,
                    max_tokens: None,
                    enabled,
                });
            }
        }
        self.show_add = false;
        self.in_form = false;
        self.editing_provider = None;
        self.last_added_name = Some(name.clone());
        self.add_just_completed = true;
        if !is_preset {
            Some(ModalAction::OpenModelManager(name))
        } else {
            None
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
        let Some(option) = self.add_options.get(self.kind_selected).cloned() else {
            return;
        };
        let kind = match option.kind {
            AddProviderKind::OAuth(kind) => kind,
            AddProviderKind::Preset(idx)
                if atman_runtime::model_registry::PROVIDER_PRESETS[idx].provider_type
                    == "codex" =>
            {
                atman_runtime::auth_store::ProviderKind::Codex
            }
            _ => return,
        };
        if let Some(tx) = control_tx {
            let _ = tx.send(crate::TuiControl::AuthLogin {
                kind,
                name: name.clone(),
            });
        }
        self.show_add = false;
        self.open = false;
        self.name_focused = false;
        self.last_added_name = Some(name);
        self.add_just_completed = true;
    }
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
            let tag_str = format!("[{}]", source_tag);
            let tag_w = crate::width::width(&tag_str);
            let max_name_w = (area.width as usize).saturating_sub(3 + tag_w);
            let name =
                crate::width::pad_right(&crate::width::truncate(&p.name, max_name_w), max_name_w);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", status_symbol), s),
                Span::styled(name, s),
                Span::styled(tag_str, Style::default().fg(theme.meta_fg.into())),
            ]))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(mgr.selected));
    crate::wm::shell::render_section_header(f, area, Line::from("Providers"), theme);
    let inner = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2).saturating_sub(1),
    };
    f.render_stateful_widget(List::new(items), inner, &mut state);
}

fn render_model_detail(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &ProviderManager,
    theme: &crate::theme::Theme,
) {
    crate::wm::shell::render_section_header(f, area, Line::from("Details"), theme);
    let inner = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2).saturating_sub(1),
    };

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

        // Show models for this provider (works for all sources)
        for g in &mgr.groups {
            if g.provider_name == p.name {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    format!(" {} models:", g.models.len()),
                    label_style,
                )));
                for m in &g.models {
                    let thinking = if m.thinking { " \u{1F9E0}" } else { "" };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  {}  {}{}",
                            m.slug,
                            atman_runtime::humanize::format_count(m.context_budget),
                            thinking
                        ),
                        val_style,
                    )));
                }
                break;
            }
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            " Select a provider to view details",
            Style::default().fg(theme.meta_fg.into()),
        )));
    }

    let is_config = provider
        .map(|p| matches!(p.source, ProviderSource::Config))
        .unwrap_or(false);
    if is_config {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Press m to manage models",
            Style::default().fg(theme.subtle_fg.into()),
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_add_dialog(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut ProviderManager,
    theme: &crate::theme::Theme,
) {
    crate::wm::shell::render_section_header(f, area, Line::from("Add Provider"), theme);
    let inner = Rect {
        x: area.x,
        y: area.y + 2,
        width: area.width,
        height: area.height.saturating_sub(2).saturating_sub(1),
    };
    let mut lines = vec![];
    let mut cursor_pos: Option<(u16, u16)> = None;
    if mgr.in_form {
        let fields: [(&str, &str); 6] = [
            ("Name", mgr.name_editor.buf()),
            ("Type", mgr.provider_type_editor.buf()),
            ("API Key", mgr.api_key_editor.buf()),
            ("API Key Env", mgr.api_key_env_editor.buf()),
            ("Base URL", mgr.base_url_editor.buf()),
            ("Enabled", mgr.enabled_editor.buf()),
        ];
        let mut y = inner.y;
        for (i, (label, val)) in fields.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let active = i == mgr.form_field;
            let style = if active {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.tinted_fg.into())
            };
            let display_val = if *label == "API Key" && !val.is_empty() && !active {
                "•".repeat(val.len().min(20))
            } else {
                (*val).to_string()
            };
            let toggle_hint = matches!(*label, "Type" | "Thinking" | "Enabled") && active;
            let display = if toggle_hint {
                format!("{display_val}  ← →")
            } else {
                display_val.clone()
            };
            if *label == "API Key" && inner.bottom().saturating_sub(y) >= 3 {
                let label_rect = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(" API Key:", style))),
                    label_rect,
                );
                y = y.saturating_add(1);
                let value_rect = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        format!("  {display_val}"),
                        Style::default().fg(theme.tinted_fg.into()),
                    ))),
                    value_rect,
                );
                if active {
                    let display_w =
                        crate::width::width(&display_val).min(inner.width as usize) as u16;
                    cursor_pos = Some((inner.x + 2 + display_w, y));
                }
                y = y.saturating_add(1);
                let underline = "─".repeat(inner.width as usize);
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        underline,
                        Style::default().fg(if active {
                            theme.accent.into()
                        } else {
                            theme.border.into()
                        }),
                    ))),
                    Rect {
                        x: inner.x,
                        y,
                        width: inner.width,
                        height: 1,
                    },
                );
                y = y.saturating_add(1);
                continue;
            }

            let label_rect = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(format!(" {label}:"), style))),
                label_rect,
            );
            y = y.saturating_add(1);
            if y >= inner.bottom() {
                break;
            }
            let value_rect = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!("  {display}"),
                    Style::default().fg(theme.tinted_fg.into()),
                ))),
                value_rect,
            );
            if active && !toggle_hint {
                cursor_pos = Some((inner.x + 2 + crate::width::width(&display_val) as u16, y));
            }
            y = y.saturating_add(1);
        }
        y = y.saturating_add(1);
        if y < inner.bottom() {
            let test_active = mgr.form_field == 6;
            let test_style = if test_active {
                Style::default()
                    .fg(theme.modal_bg.into())
                    .bg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(theme.tinted_fg.into())
                    .bg(theme.border.into())
            };
            let hint = if test_active {
                "  ← Enter to test"
            } else {
                ""
            };
            let test_btn = Rect {
                x: inner.x + 1,
                y,
                width: 6,
                height: 1,
            };
            mgr.test_btn_rect = Some(test_btn);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(" "),
                    Span::styled(" Test ", test_style),
                    Span::raw(hint),
                ])),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
        }
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Tab/Shift+Tab cycle · Enter:save/test · Esc:cancel",
                Style::default().fg(theme.subtle_fg.into()),
            ))),
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width,
                height: 1,
            },
        );
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
        let items: Vec<ListItem> = mgr
            .add_options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let selected = i == mgr.kind_selected;
                let style = if selected {
                    Style::default()
                        .fg(theme.accent.into())
                        .bg(theme.panel_bg.into())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.tinted_fg.into())
                };
                let prefix = if selected { "›" } else { " " };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {prefix} {}", option.label), style),
                    Span::styled(
                        format!("  {}", option.description),
                        Style::default().fg(theme.meta_fg.into()),
                    ),
                ]))
            })
            .collect();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        let mut state = ListState::default().with_selected(Some(mgr.kind_selected));
        f.render_stateful_widget(
            List::new(items).highlight_style(
                Style::default()
                    .fg(theme.accent.into())
                    .bg(theme.panel_bg.into())
                    .add_modifier(Modifier::BOLD),
            ),
            rows[0],
            &mut state,
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    " Enter ",
                    Style::default()
                        .fg(theme.accent.into())
                        .bg(theme.panel_bg.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" select  ", Style::default().fg(theme.subtle_fg.into())),
                Span::styled(
                    " Esc ",
                    Style::default()
                        .fg(theme.accent.into())
                        .bg(theme.panel_bg.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" cancel", Style::default().fg(theme.subtle_fg.into())),
            ]))
            .alignment(ratatui::layout::Alignment::Right),
            rows[1],
        );
    }
    if !mgr.in_form && (mgr.name_focused || mgr.add_options.is_empty()) {
        f.render_widget(Paragraph::new(lines), inner);
    }
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
    let w = area.width.saturating_sub(4).clamp(40, 60);
    let h = 9u16;
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let dlg = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        dlg,
        Line::from(format!(" {action}? ")),
        "⚠",
        theme.warn.into(),
        true,
        theme,
    );
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

impl crate::wm::modal::ModalOverlay for ProviderManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        if area.height < 4 {
            return;
        }
        if self.show_confirm {
            render_confirm_dialog(f, area, self, t);
            return;
        }
        if self.show_add {
            render_add_dialog(f, area, self, t);
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
            .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
            .split(main);
        let left_col = columns[0];
        let right_col = Rect {
            x: columns[1].x + 1,
            width: columns[1].width.saturating_sub(1),
            ..columns[1]
        };
        render_provider_list(f, left_col, self, t);
        render_model_detail(f, right_col, self, t);
        crate::wm::shell::render_column_divider(
            f,
            columns[0].right(),
            columns[0].y,
            columns[0].height,
            t,
        );
        let help = match self.focus {
            ProviderFocus::ProviderList => {
                "n:add  e:enable/disable  d:delete  r:refresh  t:test  m:manage models  Enter:edit/logout  Esc:close"
            }
        };
        let footer = Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(t.meta_fg.into()),
        )))
        .alignment(ratatui::layout::Alignment::Right);
        f.render_widget(footer, footer_area);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action, tx).or(Some(ModalAction::Consumed))
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
    }

    fn handle_paste(&mut self, text: &str) {
        if !self.in_form {
            return;
        }
        let editor = match self.form_field {
            0 => &mut self.name_editor,
            1 => &mut self.provider_type_editor,
            2 => &mut self.api_key_editor,
            3 => &mut self.api_key_env_editor,
            4 => &mut self.base_url_editor,
            _ => &mut self.enabled_editor,
        };
        editor.insert_str(text);
    }

    fn title(&self) -> Line<'static> {
        Line::from("Provider Manager")
    }

    fn icon(&self) -> &str {
        "⚙"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}
