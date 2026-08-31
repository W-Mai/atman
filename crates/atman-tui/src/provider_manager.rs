use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

fn provider_types() -> Vec<&'static str> {
    atman_runtime::model_registry::config_provider_types()
}

const REASONING_FORMATS: [&str; 2] = ["thinking-toggle", "reasoning-effort"];

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

fn config_provider_status(
    availability: atman_runtime::config_provider::ConfigProviderAvailability,
) -> ProviderStatus {
    match availability {
        atman_runtime::config_provider::ConfigProviderAvailability::Available => {
            ProviderStatus::Active
        }
        atman_runtime::config_provider::ConfigProviderAvailability::Disabled => {
            ProviderStatus::Disabled
        }
        atman_runtime::config_provider::ConfigProviderAvailability::MissingCredential
        | atman_runtime::config_provider::ConfigProviderAvailability::UnsupportedKind => {
            ProviderStatus::Inactive
        }
        _ => ProviderStatus::Inactive,
    }
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
    pub test_btn_rect: Option<Rect>,
    show_add: bool,
    focus: ProviderFocus,
    add_options: Vec<AddProviderOption>,
    kind_selected: usize,
    name_editor: InputEditor,
    name_focused: bool,
    api_key_editor: InputEditor,
    base_url_editor: InputEditor,
    provider_type_editor: InputEditor,
    reasoning_format_editor: InputEditor,
    api_key_env_editor: InputEditor,
    enabled_editor: InputEditor,
    pub in_form: bool,
    form_field: usize,
    editing_provider: Option<String>,
    form_max_tokens: Option<u32>,
    show_confirm: bool,
    confirm_kind: Option<ConfirmKind>,
    confirm_provider_id: Option<String>,
    confirm_provider_name: String,
    next_mutation_request_id: u64,
    pending_mutation: Option<crate::ProviderMutationRequest>,
    feedback: Option<ProviderFeedback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderDispatchOutcome {
    Started,
    Busy,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderFeedback {
    RefreshStarted,
    TestStarted,
    DispatchUnavailable,
    InvalidTestConfiguration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderMutationResolution {
    Ignored,
    Failed,
    ProtocolError,
    Succeeded,
    Installed { name: String },
    ConfigSaved { name: String, created: bool },
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
        if self.pending_mutation.is_some() || !self.in_form {
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

    pub(crate) fn has_pending_mutation(&self) -> bool {
        self.pending_mutation.is_some()
    }

    pub(crate) fn take_feedback(&mut self) -> Option<ProviderFeedback> {
        self.feedback.take()
    }

    fn record_feedback(&mut self, feedback: ProviderFeedback) {
        if self.feedback.is_none() {
            self.feedback = Some(feedback);
        }
    }

    fn pending_help(&self) -> Option<&'static str> {
        let request = self.pending_mutation.as_ref()?;
        Some(match &request.action {
            crate::ProviderMutation::Login { .. } => "waiting for OAuth login…  Esc:hide",
            crate::ProviderMutation::SetEnabled { .. } => "updating provider state…  Esc:hide",
            crate::ProviderMutation::Remove { .. } => "removing provider…  Esc:hide",
            crate::ProviderMutation::Refresh { .. } => "refreshing models…  Esc:hide",
            crate::ProviderMutation::UpsertConfig { .. } => {
                "saving provider configuration…  Esc:hide"
            }
        })
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
            let status = config_provider_status(
                atman_runtime::config_provider::config_provider_availability(&entry),
            );
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
        self.form_max_tokens = None;
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
        self.form_field = 1;
        self.form_max_tokens = entry.max_tokens;
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
        let mut reasoning_ed = InputEditor::default();
        reasoning_ed.insert_str(
            entry
                .reasoning_format
                .unwrap_or_else(|| {
                    atman_runtime::providers::openai::OpenAiReasoningFormat::for_provider_kind(
                        &entry.kind,
                    )
                })
                .as_str(),
        );
        self.reasoning_format_editor = reasoning_ed;
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
        if self.pending_mutation.is_some() {
            if matches!(action, KeyAction::Escape) {
                self.open = false;
            }
            return Some(ModalAction::Consumed);
        }
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
                        let provider = match &p.source {
                            ProviderSource::AuthStore { id } => Some(id.clone()),
                            ProviderSource::Config => Some(p.name.clone()),
                            ProviderSource::Env => None,
                        };
                        if let Some(provider) = provider {
                            return Some(ModalAction::OpenModelManager(provider));
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
                    self.begin_mutation(
                        crate::ProviderMutation::SetEnabled {
                            provider_id: id,
                            enabled: new_enabled,
                        },
                        control_tx,
                    );
                }
                ProviderSource::Config => {
                    let providers = atman_runtime::model_registry::all_provider_entries();
                    if let Some(entry) = providers
                        .iter()
                        .find(|(n, _)| *n == p.name)
                        .map(|(_, e)| e.clone())
                    {
                        let current_enabled = entry.enabled.unwrap_or(true);
                        self.begin_mutation(
                            crate::ProviderMutation::UpsertConfig {
                                name: p.name.clone(),
                                kind: entry.kind.clone(),
                                api_key: entry.api_key.unwrap_or_default(),
                                api_key_env: entry.api_key_env.unwrap_or_default(),
                                base_url: entry.base_url.unwrap_or_default(),
                                max_tokens: entry.max_tokens,
                                reasoning_format: entry
                                    .reasoning_format
                                    .unwrap_or_else(|| {
                                        atman_runtime::providers::openai::OpenAiReasoningFormat::for_provider_kind(
                                            &entry.kind,
                                        )
                                    })
                                    .to_string(),
                                enabled: !current_enabled,
                                create: false,
                            },
                            control_tx,
                        );
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
        let Some(id) = self.confirm_provider_id.clone() else {
            return;
        };
        let outcome = match self.confirm_kind {
            Some(ConfirmKind::Delete) | Some(ConfirmKind::Logout) => self.begin_mutation(
                crate::ProviderMutation::Remove { provider_id: id },
                control_tx,
            ),
            None => ProviderDispatchOutcome::Busy,
        };
        if matches!(outcome, ProviderDispatchOutcome::Started) {
            self.confirm_provider_id = None;
            self.confirm_kind = None;
            self.show_confirm = false;
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
            if matches!(
                self.begin_mutation(
                    crate::ProviderMutation::Refresh {
                        provider_id: id.clone(),
                    },
                    control_tx,
                ),
                ProviderDispatchOutcome::Started
            ) {
                self.record_feedback(ProviderFeedback::RefreshStarted);
            }
        }
    }

    pub(crate) fn begin_mutation(
        &mut self,
        action: crate::ProviderMutation,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> ProviderDispatchOutcome {
        if self.pending_mutation.is_some() {
            return ProviderDispatchOutcome::Busy;
        }
        let Some(tx) = control_tx else {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        };
        self.next_mutation_request_id = self.next_mutation_request_id.wrapping_add(1);
        let request = crate::ProviderMutationRequest {
            request_id: self.next_mutation_request_id,
            action,
        };
        if tx
            .send(crate::TuiControl::MutateProvider(request.clone()))
            .is_err()
        {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        }
        self.pending_mutation = Some(request);
        ProviderDispatchOutcome::Started
    }

    pub(crate) fn resolve_mutation(
        &mut self,
        request: &crate::ProviderMutationRequest,
        result: &Result<crate::ProviderMutationSuccess, String>,
    ) -> ProviderMutationResolution {
        if self.pending_mutation.as_ref() != Some(request) {
            return ProviderMutationResolution::Ignored;
        }
        let request = self
            .pending_mutation
            .take()
            .expect("pending request matched");
        self.refresh_list();
        let succeeded = match result {
            Ok(success) if mutation_success_matches(&request.action, success) => true,
            Ok(_) => return ProviderMutationResolution::ProtocolError,
            Err(_) => false,
        };
        if !succeeded {
            return ProviderMutationResolution::Failed;
        }
        match request.action {
            crate::ProviderMutation::Login { name, .. } => {
                self.close();
                self.name_focused = false;
                ProviderMutationResolution::Installed { name }
            }
            crate::ProviderMutation::UpsertConfig { name, create, .. } => {
                if self.in_form {
                    self.show_add = false;
                    self.in_form = false;
                    self.editing_provider = None;
                    self.form_max_tokens = None;
                }
                ProviderMutationResolution::ConfigSaved {
                    name,
                    created: create,
                }
            }
            _ => ProviderMutationResolution::Succeeded,
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
                if let Some(mut entry) = providers
                    .iter()
                    .find(|(name, _)| *name == p.name)
                    .map(|(_, entry)| entry.clone())
                {
                    entry.name = p.name.clone();
                    entry.enabled = Some(true);
                    self.dispatch_test(p.name, entry, control_tx);
                }
            }
        }
    }

    fn open_custom_form(&mut self) {
        self.in_form = true;
        self.form_field = 0;
        self.form_max_tokens = None;
        self.name_editor = InputEditor::default();
        self.api_key_editor = InputEditor::default();
        self.base_url_editor = InputEditor::default();
        let mut pt_ed = InputEditor::default();
        pt_ed.insert_str(atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE);
        self.provider_type_editor = pt_ed;
        let mut reasoning_ed = InputEditor::default();
        reasoning_ed.insert_str("thinking-toggle");
        self.reasoning_format_editor = reasoning_ed;
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
        if name.is_empty() {
            return;
        }
        let kind = self.provider_type_editor.buf().trim().to_string();
        let kind = if kind.is_empty() {
            atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE.into()
        } else {
            kind
        };
        let reasoning_format = match self.reasoning_format_editor.buf().parse() {
            Ok(reasoning_format) => Some(reasoning_format),
            Err(_) => {
                self.record_feedback(ProviderFeedback::InvalidTestConfiguration);
                return;
            }
        };
        let entry = atman_runtime::model_registry::ProviderEntry {
            name: name.clone(),
            kind,
            api_key: non_empty(self.api_key_editor.buf()),
            api_key_env: non_empty(self.api_key_env_editor.buf()),
            base_url: non_empty(self.base_url_editor.buf()),
            max_tokens: self.form_max_tokens,
            reasoning_format,
            prompt_cache_key: None,
            enabled: Some(true),
        };
        self.dispatch_test(name, entry, control_tx);
    }

    fn dispatch_test(
        &mut self,
        name: String,
        entry: atman_runtime::model_registry::ProviderEntry,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> ProviderDispatchOutcome {
        if self.pending_mutation.is_some() {
            return ProviderDispatchOutcome::Busy;
        }
        let Some(tx) = control_tx else {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        };
        if tx
            .send(crate::TuiControl::TestProvider { name, entry })
            .is_err()
        {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        }
        self.record_feedback(ProviderFeedback::TestStarted);
        ProviderDispatchOutcome::Started
    }

    fn handle_add_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if self.in_form {
            if self.form_field == 7 {
                match action {
                    KeyAction::Escape => {
                        if self.editing_provider.is_some() {
                            self.show_add = false;
                            self.in_form = false;
                            self.editing_provider = None;
                            self.form_max_tokens = None;
                        } else {
                            self.in_form = false;
                        }
                    }
                    KeyAction::Submit => {
                        self.test_form(control_tx);
                    }
                    KeyAction::Tab => {
                        self.form_field = if self.editing_provider.is_some() {
                            1
                        } else {
                            0
                        };
                    }
                    KeyAction::BackTab => {
                        self.form_field = 6;
                    }
                    _ => {}
                }
                return None;
            }
            let name_locked = self.editing_provider.is_some();
            let editor = match self.form_field {
                0 => &mut self.name_editor,
                1 => &mut self.provider_type_editor,
                2 => &mut self.api_key_editor,
                3 => &mut self.api_key_env_editor,
                4 => &mut self.base_url_editor,
                5 => &mut self.reasoning_format_editor,
                _ => &mut self.enabled_editor,
            };
            match action {
                KeyAction::Escape => {
                    if self.editing_provider.is_some() {
                        self.show_add = false;
                        self.in_form = false;
                        self.editing_provider = None;
                        self.form_max_tokens = None;
                    } else {
                        self.in_form = false;
                    }
                }
                KeyAction::Submit => {
                    return self.commit_form(control_tx);
                }
                KeyAction::Tab => {
                    self.form_field = (self.form_field + 1) % 8;
                    if name_locked && self.form_field == 0 {
                        self.form_field = 1;
                    }
                }
                KeyAction::BackTab => {
                    self.form_field = if self.form_field == 0 {
                        7
                    } else {
                        self.form_field - 1
                    };
                    if name_locked && self.form_field == 0 {
                        self.form_field = 7;
                    }
                }
                KeyAction::CursorLeft | KeyAction::CursorRight if self.form_field == 1 => {
                    let direction =
                        crate::directional_selector::SelectorDirection::from_key(action)?;
                    let types = provider_types();
                    let mut selected = types
                        .iter()
                        .position(|kind| *kind == self.provider_type_editor.buf().trim())
                        .unwrap_or(0);
                    crate::directional_selector::move_wrapped(
                        &mut selected,
                        types.len(),
                        direction,
                    );
                    self.provider_type_editor.replace_with(types[selected]);
                    let default =
                        atman_runtime::providers::openai::OpenAiReasoningFormat::for_provider_kind(
                            types[selected],
                        );
                    self.reasoning_format_editor.replace_with(default.as_str());
                    self.kind_selected = selected;
                }
                KeyAction::Backspace if self.form_field == 1 => {}
                KeyAction::Char(_) if self.form_field == 1 => {}
                KeyAction::CursorLeft | KeyAction::CursorRight if self.form_field == 5 => {
                    let current = self.reasoning_format_editor.buf().trim();
                    let next = if current == REASONING_FORMATS[0] {
                        REASONING_FORMATS[1]
                    } else {
                        REASONING_FORMATS[0]
                    };
                    self.reasoning_format_editor.replace_with(next);
                }
                KeyAction::Backspace | KeyAction::Char(_) if self.form_field == 5 => {}
                KeyAction::CursorLeft if self.form_field == 6 => {
                    let current = self.enabled_editor.buf().trim();
                    let new = if current == "true" { "false" } else { "true" };
                    let mut ed = InputEditor::default();
                    ed.insert_str(new);
                    self.enabled_editor = ed;
                }
                KeyAction::CursorRight if self.form_field == 6 => {
                    let current = self.enabled_editor.buf().trim();
                    let new = if current == "true" { "false" } else { "true" };
                    let mut ed = InputEditor::default();
                    ed.insert_str(new);
                    self.enabled_editor = ed;
                }
                KeyAction::Backspace if self.form_field == 6 => {}
                KeyAction::Char(_) if self.form_field == 6 => {}
                KeyAction::DeleteWordBackward
                | KeyAction::Backspace
                | KeyAction::Delete
                | KeyAction::CursorLeft
                | KeyAction::CursorRight
                | KeyAction::CursorHome
                | KeyAction::CursorEnd
                | KeyAction::Char(_)
                | KeyAction::Newline => {
                    if !name_locked || self.form_field != 0 {
                        editor.handle_key(action);
                    }
                }
                _ => {}
            }
        } else if self.name_focused {
            match action {
                KeyAction::Escape => self.name_focused = false,
                KeyAction::Submit => self.commit_add(control_tx),
                KeyAction::DeleteWordBackward
                | KeyAction::Backspace
                | KeyAction::Delete
                | KeyAction::CursorLeft
                | KeyAction::CursorRight
                | KeyAction::CursorHome
                | KeyAction::CursorEnd
                | KeyAction::Char(_)
                | KeyAction::Newline => {
                    self.name_editor.handle_key(action);
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
                                self.begin_mutation(
                                    crate::ProviderMutation::Login {
                                        kind: atman_runtime::auth_store::ProviderKind::Codex,
                                        name: preset.name.to_string(),
                                    },
                                    control_tx,
                                );
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
        let create = self.editing_provider.is_none();
        let name = self
            .editing_provider
            .clone()
            .unwrap_or_else(|| self.name_editor.buf().trim().to_string());
        let api_key = self.api_key_editor.buf().trim().to_string();
        let api_key_env = self.api_key_env_editor.buf().trim().to_string();
        let base_url = self.base_url_editor.buf().trim().to_string();
        let kind = self.provider_type_editor.buf().trim().to_string();
        let reasoning_format = self.reasoning_format_editor.buf().trim().to_string();
        let enabled = matches!(
            self.enabled_editor.buf().trim().to_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        );
        if name.is_empty() || base_url.is_empty() {
            return None;
        }
        let kind = if kind.is_empty() {
            atman_runtime::model_registry::DEFAULT_CONFIG_PROVIDER_TYPE.into()
        } else {
            kind
        };
        self.begin_mutation(
            crate::ProviderMutation::UpsertConfig {
                name,
                kind,
                api_key,
                api_key_env,
                base_url,
                max_tokens: self.form_max_tokens,
                reasoning_format,
                enabled,
                create,
            },
            control_tx,
        );
        None
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
        self.begin_mutation(
            crate::ProviderMutation::Login {
                kind,
                name: name.clone(),
            },
            control_tx,
        );
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn mutation_success_matches(
    action: &crate::ProviderMutation,
    success: &crate::ProviderMutationSuccess,
) -> bool {
    match (action, success) {
        (
            crate::ProviderMutation::Login { kind, name },
            crate::ProviderMutationSuccess::Installed {
                name: installed_name,
                kind: installed_kind,
                ..
            },
        ) => kind == installed_kind && name == installed_name,
        (
            crate::ProviderMutation::SetEnabled {
                provider_id,
                enabled,
            },
            crate::ProviderMutationSuccess::StateChanged {
                provider_id: changed_id,
                enabled: Some(changed_enabled),
                ..
            },
        ) => provider_id == changed_id && enabled == changed_enabled,
        (
            crate::ProviderMutation::Remove { provider_id },
            crate::ProviderMutationSuccess::StateChanged {
                provider_id: changed_id,
                enabled: None,
                ..
            },
        ) => provider_id == changed_id,
        (
            crate::ProviderMutation::Refresh { provider_id },
            crate::ProviderMutationSuccess::Refreshed {
                provider_id: refreshed_id,
                ..
            },
        ) => provider_id == refreshed_id,
        (
            crate::ProviderMutation::UpsertConfig { name, create, .. },
            crate::ProviderMutationSuccess::ConfigSaved {
                name: saved_name,
                created,
            },
        ) => name == saved_name && create == created,
        _ => false,
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

        let provider_key = match &p.source {
            ProviderSource::AuthStore { id } => id.as_str(),
            ProviderSource::Env | ProviderSource::Config => p.name.as_str(),
        };
        for g in &mgr.groups {
            if g.provider_name == provider_key {
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
        let fields: [(&str, &str); 7] = [
            ("Name", mgr.name_editor.buf()),
            ("Type", mgr.provider_type_editor.buf()),
            ("API Key", mgr.api_key_editor.buf()),
            ("API Key Env", mgr.api_key_env_editor.buf()),
            ("Base URL", mgr.base_url_editor.buf()),
            ("Reasoning wire", mgr.reasoning_format_editor.buf()),
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
            let toggle_hint = matches!(*label, "Type" | "Reasoning wire" | "Enabled") && active;
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
                    let cursor_w = mgr.api_key_editor.cursor_display_col();
                    cursor_pos = Some((inner.x + 2 + cursor_w.min(inner.width as usize) as u16, y));
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
                let cursor_w = match i {
                    0 => mgr.name_editor.cursor_display_col(),
                    2 => mgr.api_key_editor.cursor_display_col(),
                    3 => mgr.api_key_env_editor.cursor_display_col(),
                    4 => mgr.base_url_editor.cursor_display_col(),
                    _ => 0,
                } as u16;
                cursor_pos = Some((inner.x + 2 + cursor_w, y));
            }
            y = y.saturating_add(1);
        }
        y = y.saturating_add(1);
        if y < inner.bottom() {
            let test_active = mgr.form_field == 7;
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
                crate::directional_selector::footer_help(
                    "Tab/Shift+Tab cycle · Enter:save/test",
                    "Esc:cancel",
                ),
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
        let x = inner.x + 2 + mgr.name_editor.cursor_display_col() as u16;
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
        self.test_btn_rect = None;
        if area.height < 4 {
            return;
        }
        if self.show_confirm {
            render_confirm_dialog(f, area, self, t);
            return;
        }
        if self.show_add {
            render_add_dialog(f, area, self, t);
            if let Some(help) = self.pending_help() {
                render_pending_help(f, area, help, t);
            }
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
        let help = self.pending_help().unwrap_or(match self.focus {
            ProviderFocus::ProviderList => {
                "n:add  e:enable/disable  d:delete  r:refresh  t:test  m:manage models  Enter:edit/logout  Esc:close"
            }
        });
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
        if self.form_field == 0 && self.editing_provider.is_some() {
            return;
        }
        if self.form_field == 1 {
            let value = text.trim();
            if let Some(index) = provider_types().iter().position(|kind| *kind == value) {
                self.kind_selected = index;
                self.provider_type_editor.replace_with(value);
            }
            return;
        }
        if self.form_field == 5 {
            let value = text.trim();
            if REASONING_FORMATS.contains(&value) {
                self.reasoning_format_editor.replace_with(value);
            }
            return;
        }
        let editor = match self.form_field {
            0 => &mut self.name_editor,
            2 => &mut self.api_key_editor,
            3 => &mut self.api_key_env_editor,
            4 => &mut self.base_url_editor,
            6 => &mut self.enabled_editor,
            _ => return,
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

fn render_pending_help(
    f: &mut ratatui::Frame,
    area: Rect,
    help: &str,
    theme: &crate::theme::Theme,
) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(theme.accent.into()),
        )))
        .alignment(ratatui::layout::Alignment::Right),
        Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populate_config_form(manager: &mut ProviderManager, editing: Option<&str>) {
        manager.open = true;
        manager.show_add = true;
        manager.in_form = true;
        manager.editing_provider = editing.map(str::to_string);
        manager.form_field = if editing.is_some() { 1 } else { 0 };
        manager
            .name_editor
            .replace_with(editing.unwrap_or("gateway"));
        manager.provider_type_editor.replace_with("openai-compat");
        manager.api_key_editor.replace_with("test-key");
        manager.api_key_env_editor.replace_with("GATEWAY_API_KEY");
        manager
            .base_url_editor
            .replace_with("https://gateway.example/v1");
        manager
            .reasoning_format_editor
            .replace_with("thinking-toggle");
        manager.enabled_editor.replace_with("true");
    }

    #[test]
    fn unavailable_config_providers_are_not_presented_as_active() {
        use atman_runtime::config_provider::ConfigProviderAvailability;

        assert_eq!(
            config_provider_status(ConfigProviderAvailability::Available),
            ProviderStatus::Active
        );
        assert_eq!(
            config_provider_status(ConfigProviderAvailability::Disabled),
            ProviderStatus::Disabled
        );
        assert_eq!(
            config_provider_status(ConfigProviderAvailability::MissingCredential),
            ProviderStatus::Inactive
        );
        assert_eq!(
            config_provider_status(ConfigProviderAvailability::UnsupportedKind),
            ProviderStatus::Inactive
        );
    }

    #[test]
    fn paste_accepts_only_known_provider_type() {
        let mut manager = ProviderManager::default();
        manager.open_custom_form();
        manager.form_field = 1;
        let first = provider_types()[0];
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, first);
        assert_eq!(manager.provider_type_editor.buf(), first);
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(
            &mut manager,
            "not-a-provider-type",
        );
        assert_eq!(manager.provider_type_editor.buf(), first);
    }

    #[test]
    fn provider_type_selector_wraps_with_directional_keys() {
        let mut manager = ProviderManager::default();
        manager.open_custom_form();
        manager.show_add = true;
        manager.form_field = 1;
        let types = provider_types();
        if types.len() < 2 {
            return;
        }
        let current = manager.provider_type_editor.buf().trim().to_owned();
        let current_idx = types.iter().position(|kind| *kind == current).unwrap();
        let expected_next = types[(current_idx + 1) % types.len()];
        manager.handle_key(&KeyAction::CursorRight, None);
        assert_eq!(manager.provider_type_editor.buf(), expected_next);
        manager.handle_key(&KeyAction::CursorLeft, None);
        assert_eq!(manager.provider_type_editor.buf(), current);
    }

    #[test]
    fn paste_inserts_text_fields() {
        let mut manager = ProviderManager::default();
        manager.open_custom_form();
        manager.form_field = 0;
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "provider");
        assert_eq!(manager.name_editor.buf(), "provider");
    }

    #[test]
    fn reasoning_wire_selector_accepts_only_known_formats() {
        let mut manager = ProviderManager::default();
        manager.open_custom_form();
        manager.show_add = true;
        manager.form_field = 5;

        manager.handle_key(&KeyAction::CursorRight, None);
        assert_eq!(manager.reasoning_format_editor.buf(), "reasoning-effort");
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "invalid");
        assert_eq!(manager.reasoning_format_editor.buf(), "reasoning-effort");
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(
            &mut manager,
            "thinking-toggle",
        );
        assert_eq!(manager.reasoning_format_editor.buf(), "thinking-toggle");
    }

    #[test]
    fn config_form_waits_for_matching_ack_and_preserves_failed_input() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);

        manager.commit_form(Some(&tx));
        let failed_request = receive_mutation(&mut rx);
        assert!(matches!(
            &failed_request.action,
            crate::ProviderMutation::UpsertConfig {
                name,
                create: true,
                ..
            } if name == "gateway"
        ));
        assert!(manager.in_form);
        assert!(manager.show_add);
        assert_eq!(manager.pending_mutation.as_ref(), Some(&failed_request));

        assert_eq!(
            manager.resolve_mutation(&failed_request, &Err("save failed".into())),
            ProviderMutationResolution::Failed
        );
        assert!(manager.in_form);
        assert!(manager.show_add);
        assert_eq!(manager.api_key_editor.buf(), "test-key");
        assert!(manager.pending_mutation.is_none());

        manager.commit_form(Some(&tx));
        let successful_request = receive_mutation(&mut rx);
        assert_eq!(
            manager.resolve_mutation(
                &successful_request,
                &Ok(crate::ProviderMutationSuccess::ConfigSaved {
                    name: "gateway".into(),
                    created: true,
                }),
            ),
            ProviderMutationResolution::ConfigSaved {
                name: "gateway".into(),
                created: true,
            }
        );
        assert!(!manager.in_form);
        assert!(!manager.show_add);
        assert!(manager.pending_mutation.is_none());
    }

    #[test]
    fn provider_test_dispatch_uses_the_complete_draft_and_truthful_feedback() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.api_key_editor.replace_with("");
        manager.base_url_editor.replace_with("");
        manager.form_max_tokens = Some(16_384);

        manager.test_form(Some(&tx));
        assert_eq!(manager.take_feedback(), Some(ProviderFeedback::TestStarted));
        let crate::TuiControl::TestProvider { name, entry } = rx.try_recv().unwrap() else {
            panic!("expected provider test");
        };
        assert_eq!(name, "gateway");
        assert_eq!(entry.name, "gateway");
        assert_eq!(entry.kind, "openai-compat");
        assert_eq!(entry.api_key, None);
        assert_eq!(entry.api_key_env.as_deref(), Some("GATEWAY_API_KEY"));
        assert_eq!(entry.base_url, None);
        assert_eq!(entry.max_tokens, Some(16_384));
        assert_eq!(
            entry.reasoning_format,
            Some(atman_runtime::providers::openai::OpenAiReasoningFormat::CompatibleThinking)
        );
        assert_eq!(entry.enabled, Some(true));

        manager.reasoning_format_editor.replace_with("invalid");
        manager.test_form(Some(&tx));
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::InvalidTestConfiguration)
        );
        assert!(rx.try_recv().is_err());
        manager
            .reasoning_format_editor
            .replace_with("thinking-toggle");

        let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(closed_rx);
        manager.test_form(Some(&closed_tx));
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );
        assert!(manager.in_form);

        manager.test_form(None);
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );
        assert!(manager.in_form);
    }

    #[test]
    fn config_form_survives_an_unavailable_dispatch() {
        let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(closed_rx);
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);

        manager.commit_form(Some(&closed_tx));

        assert!(manager.pending_mutation.is_none());
        assert!(manager.in_form);
        assert!(manager.show_add);
        assert_eq!(manager.api_key_editor.buf(), "test-key");
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );
    }

    #[test]
    fn selected_provider_test_preserves_env_configuration_and_forces_test_enabled() {
        struct RegistryReset;
        impl Drop for RegistryReset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = RegistryReset;
        let mut config = atman_runtime::model_registry::ProviderConfig::default();
        config.providers.insert(
            "gateway".into(),
            atman_runtime::model_registry::ProviderEntry {
                kind: "openai-compat".into(),
                api_key_env: Some("GATEWAY_API_KEY".into()),
                enabled: Some(false),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_provider_config(config);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager {
            providers: vec![ProviderEntry {
                source: ProviderSource::Config,
                name: "gateway".into(),
                kind: "openai-compat".into(),
                status: ProviderStatus::Disabled,
                detail: String::new(),
            }],
            ..Default::default()
        };

        manager.test_selected(Some(&tx));
        assert_eq!(manager.take_feedback(), Some(ProviderFeedback::TestStarted));
        let crate::TuiControl::TestProvider { name, entry } = rx.try_recv().unwrap() else {
            panic!("expected provider test");
        };
        assert_eq!(name, "gateway");
        assert_eq!(entry.api_key_env.as_deref(), Some("GATEWAY_API_KEY"));
        assert_eq!(entry.enabled, Some(true));
        let saved = atman_runtime::model_registry::all_provider_entries()
            .into_iter()
            .find(|(name, _)| name == "gateway")
            .map(|(_, entry)| entry)
            .unwrap();
        assert_eq!(saved.enabled, Some(false));
    }

    #[test]
    fn pending_mutation_blocks_mouse_provider_tests() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.test_btn_rect = Some(Rect::new(4, 3, 6, 1));
        assert_eq!(
            manager.begin_mutation(
                crate::ProviderMutation::Refresh {
                    provider_id: "auth-id".into(),
                },
                Some(&tx),
            ),
            ProviderDispatchOutcome::Started
        );
        let _ = receive_mutation(&mut rx);

        manager.handle_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Some(&tx),
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(manager.take_feedback(), None);
    }

    #[test]
    fn mouse_provider_test_records_started_only_after_a_successful_send() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.test_btn_rect = Some(Rect::new(4, 3, 6, 1));

        manager.handle_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 4,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Some(&tx),
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(crate::TuiControl::TestProvider { .. })
        ));
        assert_eq!(manager.take_feedback(), Some(ProviderFeedback::TestStarted));
    }

    #[test]
    fn rendering_a_form_clears_an_obsolete_test_hit_region() {
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.test_btn_rect = Some(Rect::new(4, 3, 6, 1));
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 3)).unwrap();

        terminal
            .draw(|frame| {
                <ProviderManager as crate::wm::modal::ModalOverlay>::render_content(
                    &mut manager,
                    frame,
                    frame.area(),
                    &app,
                    &theme,
                );
            })
            .unwrap();

        assert!(manager.test_btn_rect.is_none());
    }

    #[test]
    fn config_edit_keeps_name_and_hidden_max_tokens() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, Some("gateway"));
        manager.form_max_tokens = Some(16_384);

        manager.handle_add_key(&KeyAction::BackTab, Some(&tx));
        assert_eq!(manager.form_field, 7);
        manager.handle_add_key(&KeyAction::Tab, Some(&tx));
        assert_eq!(manager.form_field, 1);

        manager.form_field = 0;
        manager.handle_add_key(&KeyAction::Char('x'), Some(&tx));
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "renamed");
        assert_eq!(manager.name_editor.buf(), "gateway");
        manager.name_editor.replace_with("corrupted");

        manager.commit_form(Some(&tx));
        let request = receive_mutation(&mut rx);
        assert!(matches!(
            request.action,
            crate::ProviderMutation::UpsertConfig {
                ref name,
                max_tokens: Some(16_384),
                create: false,
                ..
            } if name == "gateway"
        ));
    }

    fn receive_mutation(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::TuiControl>,
    ) -> crate::ProviderMutationRequest {
        match rx.try_recv().unwrap() {
            crate::TuiControl::MutateProvider(request) => request,
            _ => panic!("expected provider mutation"),
        }
    }

    fn begin_codex_login(
        manager: &mut ProviderManager,
        tx: &tokio::sync::mpsc::UnboundedSender<crate::TuiControl>,
    ) -> String {
        manager.open_add();
        manager.kind_selected = manager
            .add_options
            .iter()
            .position(|option| {
                matches!(
                    &option.kind,
                    AddProviderKind::Preset(index)
                        if atman_runtime::model_registry::PROVIDER_PRESETS[*index].provider_type
                            == "codex"
                )
            })
            .unwrap();
        let name = manager.add_options[manager.kind_selected].label.to_string();
        manager.handle_add_key(&KeyAction::Submit, Some(tx));
        name
    }

    #[test]
    fn oauth_login_waits_for_the_matching_success_before_closing() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        let name = begin_codex_login(&mut manager, &tx);
        let request = receive_mutation(&mut rx);

        assert!(manager.open);
        assert!(manager.show_add);
        assert_eq!(manager.pending_mutation.as_ref(), Some(&request));

        let result = Ok(crate::ProviderMutationSuccess::Installed {
            provider_id: "provider-id".into(),
            name: name.clone(),
            kind: atman_runtime::auth_store::ProviderKind::Codex,
            delta: Default::default(),
        });
        assert_eq!(
            manager.resolve_mutation(&request, &result),
            ProviderMutationResolution::Installed { name }
        );
        assert!(!manager.open);
        assert!(manager.pending_mutation.is_none());
    }

    #[test]
    fn oauth_login_failure_keeps_the_form_open_for_retry() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        begin_codex_login(&mut manager, &tx);
        let request = receive_mutation(&mut rx);

        assert_eq!(
            manager.resolve_mutation(&request, &Err("login failed".into())),
            ProviderMutationResolution::Failed
        );
        assert!(manager.open);
        assert!(manager.show_add);
        assert!(manager.pending_mutation.is_none());

        manager.handle_add_key(&KeyAction::Submit, Some(&tx));
        let retry = receive_mutation(&mut rx);
        assert_ne!(retry.request_id, request.request_id);
    }

    #[test]
    fn pending_mutation_can_be_hidden_without_losing_its_result() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        let name = begin_codex_login(&mut manager, &tx);
        let request = receive_mutation(&mut rx);

        manager.handle_key(&KeyAction::Escape, Some(&tx));
        assert!(!manager.open);
        assert_eq!(manager.pending_mutation.as_ref(), Some(&request));

        let result = Ok(crate::ProviderMutationSuccess::Installed {
            provider_id: "provider-id".into(),
            name: name.clone(),
            kind: atman_runtime::auth_store::ProviderKind::Codex,
            delta: Default::default(),
        });
        assert_eq!(
            manager.resolve_mutation(&request, &result),
            ProviderMutationResolution::Installed { name }
        );
        assert!(manager.pending_mutation.is_none());
    }

    #[test]
    fn stale_and_mismatched_provider_results_cannot_complete_a_request() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        let name = begin_codex_login(&mut manager, &tx);
        let request = receive_mutation(&mut rx);
        let mut stale = request.clone();
        stale.request_id += 1;

        assert_eq!(
            manager.resolve_mutation(
                &stale,
                &Ok(crate::ProviderMutationSuccess::Installed {
                    provider_id: "provider-id".into(),
                    name: name.clone(),
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    delta: Default::default(),
                }),
            ),
            ProviderMutationResolution::Ignored
        );
        assert_eq!(manager.pending_mutation.as_ref(), Some(&request));

        let same_id_different_action = crate::ProviderMutationRequest {
            request_id: request.request_id,
            action: crate::ProviderMutation::Refresh {
                provider_id: "provider-id".into(),
            },
        };
        assert_eq!(
            manager.resolve_mutation(
                &same_id_different_action,
                &Ok(crate::ProviderMutationSuccess::Refreshed {
                    provider_id: "provider-id".into(),
                    delta: Default::default(),
                }),
            ),
            ProviderMutationResolution::Ignored
        );
        assert_eq!(manager.pending_mutation.as_ref(), Some(&request));

        assert_eq!(
            manager.resolve_mutation(
                &request,
                &Ok(crate::ProviderMutationSuccess::Installed {
                    provider_id: "provider-id".into(),
                    name: "different".into(),
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    delta: Default::default(),
                }),
            ),
            ProviderMutationResolution::ProtocolError
        );
        assert!(manager.open);
        assert!(manager.pending_mutation.is_none());
    }

    #[test]
    fn auth_actions_emit_typed_mutations_without_optimistic_state_changes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager {
            providers: vec![ProviderEntry {
                source: ProviderSource::AuthStore {
                    id: "auth-id".into(),
                },
                name: "Auth".into(),
                kind: "codex".into(),
                status: ProviderStatus::Cached,
                detail: String::new(),
            }],
            ..Default::default()
        };
        manager.toggle_enabled(Some(&tx));
        let toggle = receive_mutation(&mut rx);
        assert_eq!(
            toggle.action,
            crate::ProviderMutation::SetEnabled {
                provider_id: "auth-id".into(),
                enabled: false,
            }
        );
        assert_eq!(manager.providers[0].status, ProviderStatus::Cached);

        let mut remove_manager = ProviderManager {
            confirm_provider_id: Some("auth-id".into()),
            confirm_kind: Some(ConfirmKind::Logout),
            show_confirm: true,
            ..Default::default()
        };
        remove_manager.execute_confirm(Some(&tx));
        assert_eq!(
            receive_mutation(&mut rx).action,
            crate::ProviderMutation::Remove {
                provider_id: "auth-id".into()
            }
        );

        let mut refresh_manager = ProviderManager {
            providers: manager.providers.clone(),
            ..Default::default()
        };
        refresh_manager.refresh_selected(Some(&tx));
        assert_eq!(
            receive_mutation(&mut rx).action,
            crate::ProviderMutation::Refresh {
                provider_id: "auth-id".into()
            }
        );
        assert_eq!(
            refresh_manager.take_feedback(),
            Some(ProviderFeedback::RefreshStarted)
        );
    }

    #[test]
    fn config_toggle_uses_the_acknowledged_mutation_path() {
        struct RegistryReset;
        impl Drop for RegistryReset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = RegistryReset;
        let mut config = atman_runtime::model_registry::ProviderConfig::default();
        config.providers.insert(
            "gateway".into(),
            atman_runtime::model_registry::ProviderEntry {
                kind: "openai-compat".into(),
                api_key_env: Some("GATEWAY_API_KEY".into()),
                base_url: Some("https://gateway.example/v1".into()),
                max_tokens: Some(8_192),
                enabled: Some(true),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_provider_config(config);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager {
            providers: vec![ProviderEntry {
                source: ProviderSource::Config,
                name: "gateway".into(),
                kind: "openai-compat".into(),
                status: ProviderStatus::Active,
                detail: String::new(),
            }],
            ..Default::default()
        };

        manager.toggle_enabled(Some(&tx));
        let request = receive_mutation(&mut rx);
        assert!(matches!(
            &request.action,
            crate::ProviderMutation::UpsertConfig {
                name,
                max_tokens: Some(8_192),
                enabled: false,
                create: false,
                ..
            } if name == "gateway"
        ));
        assert_eq!(manager.providers[0].status, ProviderStatus::Active);
        assert_eq!(manager.pending_mutation.as_ref(), Some(&request));
    }

    #[test]
    fn failed_dispatch_and_pending_request_reject_additional_mutations() {
        let mut manager = ProviderManager::default();
        assert_eq!(
            manager.begin_mutation(
                crate::ProviderMutation::Refresh {
                    provider_id: "auth-id".into(),
                },
                None,
            ),
            ProviderDispatchOutcome::Unavailable
        );
        assert!(manager.pending_mutation.is_none());
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );

        let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
        drop(closed_rx);
        assert_eq!(
            manager.begin_mutation(
                crate::ProviderMutation::Refresh {
                    provider_id: "auth-id".into(),
                },
                Some(&closed_tx),
            ),
            ProviderDispatchOutcome::Unavailable
        );
        assert!(manager.pending_mutation.is_none());
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );

        manager.confirm_provider_id = Some("auth-id".into());
        manager.confirm_provider_name = "Auth".into();
        manager.confirm_kind = Some(ConfirmKind::Logout);
        manager.show_confirm = true;
        manager.execute_confirm(Some(&closed_tx));
        assert_eq!(manager.confirm_provider_id.as_deref(), Some("auth-id"));
        assert_eq!(manager.confirm_kind, Some(ConfirmKind::Logout));
        assert!(manager.show_confirm);
        assert_eq!(
            manager.take_feedback(),
            Some(ProviderFeedback::DispatchUnavailable)
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(
            manager.begin_mutation(
                crate::ProviderMutation::Refresh {
                    provider_id: "first".into(),
                },
                Some(&tx),
            ),
            ProviderDispatchOutcome::Started
        );
        assert_eq!(
            manager.begin_mutation(
                crate::ProviderMutation::Remove {
                    provider_id: "second".into(),
                },
                Some(&tx),
            ),
            ProviderDispatchOutcome::Busy
        );
        assert_eq!(manager.take_feedback(), None);
        assert_eq!(
            receive_mutation(&mut rx).action,
            crate::ProviderMutation::Refresh {
                provider_id: "first".into()
            }
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn mutation_success_payload_must_match_the_requested_action() {
        let changed = atman_runtime::provider_lifecycle::ProviderStateChange {
            auth_changed: true,
            live_changed: true,
            catalog_changed: true,
        };
        let mismatches = [
            (
                crate::ProviderMutation::Login {
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    name: "provider-a".into(),
                },
                crate::ProviderMutationSuccess::Installed {
                    provider_id: "provider-id".into(),
                    name: "provider-a".into(),
                    kind: atman_runtime::auth_store::ProviderKind::AnthropicOauth,
                    delta: Default::default(),
                },
            ),
            (
                crate::ProviderMutation::Login {
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    name: "provider-a".into(),
                },
                crate::ProviderMutationSuccess::Installed {
                    provider_id: "provider-id".into(),
                    name: "provider-b".into(),
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    delta: Default::default(),
                },
            ),
            (
                crate::ProviderMutation::SetEnabled {
                    provider_id: "provider-a".into(),
                    enabled: true,
                },
                crate::ProviderMutationSuccess::StateChanged {
                    provider_id: "provider-b".into(),
                    enabled: Some(true),
                    change: changed,
                    catalog: None,
                },
            ),
            (
                crate::ProviderMutation::SetEnabled {
                    provider_id: "provider-a".into(),
                    enabled: false,
                },
                crate::ProviderMutationSuccess::StateChanged {
                    provider_id: "provider-a".into(),
                    enabled: Some(true),
                    change: changed,
                    catalog: None,
                },
            ),
            (
                crate::ProviderMutation::Remove {
                    provider_id: "provider-a".into(),
                },
                crate::ProviderMutationSuccess::StateChanged {
                    provider_id: "provider-a".into(),
                    enabled: Some(false),
                    change: changed,
                    catalog: None,
                },
            ),
            (
                crate::ProviderMutation::Refresh {
                    provider_id: "provider-a".into(),
                },
                crate::ProviderMutationSuccess::Refreshed {
                    provider_id: "provider-b".into(),
                    delta: Default::default(),
                },
            ),
            (
                crate::ProviderMutation::UpsertConfig {
                    name: "provider-a".into(),
                    kind: "openai-compat".into(),
                    api_key: String::new(),
                    api_key_env: String::new(),
                    base_url: "https://gateway.example/v1".into(),
                    max_tokens: None,
                    reasoning_format: "thinking-toggle".into(),
                    enabled: true,
                    create: true,
                },
                crate::ProviderMutationSuccess::ConfigSaved {
                    name: "provider-b".into(),
                    created: true,
                },
            ),
            (
                crate::ProviderMutation::UpsertConfig {
                    name: "provider-a".into(),
                    kind: "openai-compat".into(),
                    api_key: String::new(),
                    api_key_env: String::new(),
                    base_url: "https://gateway.example/v1".into(),
                    max_tokens: None,
                    reasoning_format: "thinking-toggle".into(),
                    enabled: true,
                    create: false,
                },
                crate::ProviderMutationSuccess::ConfigSaved {
                    name: "provider-a".into(),
                    created: true,
                },
            ),
        ];

        for (action, success) in mismatches {
            assert!(!mutation_success_matches(&action, &success));
        }
    }
}
