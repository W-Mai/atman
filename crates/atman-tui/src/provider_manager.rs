use crate::wm::modal::ModalAction;

use ratatui::layout::Rect;
use ratatui::text::Line;

mod view;

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
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ProviderFocus {
    #[default]
    ProviderList,
    Search,
    Details,
    Action(ProviderAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderKey {
    Config(String),
    Auth(String),
    Env(String),
}

impl ProviderEntry {
    fn key(&self) -> ProviderKey {
        match &self.source {
            ProviderSource::Config => ProviderKey::Config(self.name.clone()),
            ProviderSource::AuthStore { id } => ProviderKey::Auth(id.clone()),
            ProviderSource::Env => ProviderKey::Env(self.detail.clone()),
        }
    }

    fn source_label(&self) -> &'static str {
        match self.source {
            ProviderSource::Config => "Config",
            ProviderSource::AuthStore { .. } => "OAuth",
            ProviderSource::Env => "Environment",
        }
    }

    fn status_label(&self) -> &'static str {
        match self.status {
            ProviderStatus::Active => "Ready",
            ProviderStatus::Disabled => "Disabled",
            ProviderStatus::Inactive => "Unavailable",
            ProviderStatus::EnvKey => "Environment key",
            ProviderStatus::Cached => "Cached",
            ProviderStatus::Refreshing => "Refreshing",
            ProviderStatus::Error => "Error",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderAction {
    Add,
    Edit,
    Models,
    Test,
    Toggle,
    Refresh,
    Remove,
    More,
    Back,
    Previous,
    Next,
    Choose,
    Save,
    Cancel,
    ConfirmRemove,
    Discard,
    KeepEditing,
    Login,
    Result,
    Hide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderDraft {
    name: String,
    kind: String,
    api_key: String,
    api_key_env: String,
    base_url: String,
    reasoning_format: String,
    enabled: String,
    api: Option<atman_runtime::provider::OpenAiApi>,
    max_tokens: Option<u32>,
    max_tools: Option<usize>,
}

struct ProviderTestBinding {
    request_id: u64,
    key: ProviderKey,
    name: String,
    draft: Option<ProviderDraft>,
}

struct ProviderResult {
    key: ProviderKey,
    name: String,
    operation: &'static str,
    result: Result<String, String>,
    draft: Option<ProviderDraft>,
}

#[derive(Debug, Clone)]
enum AddProviderKind {
    Preset(usize),
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
    model_counts: Vec<(ProviderKey, usize)>,
    pub test_btn_rect: Option<Rect>,
    list_rect: Option<Rect>,
    list_offset: usize,
    save_rect: Option<Rect>,
    confirm_yes_rect: Option<Rect>,
    confirm_no_rect: Option<Rect>,
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
    form_api: Option<atman_runtime::provider::OpenAiApi>,
    form_max_tools: Option<usize>,
    form_hits: Vec<(usize, crate::settings_form::FieldHit)>,
    selector_hits: Vec<(usize, Rect, Rect)>,
    hovered_field: Option<usize>,
    show_confirm: bool,
    confirm_kind: Option<ConfirmKind>,
    confirm_provider_id: Option<String>,
    confirm_provider_name: String,
    confirm_config_provider: bool,
    next_mutation_request_id: u64,
    pending_mutation: Option<crate::ProviderMutationRequest>,
    feedback: Option<ProviderFeedback>,
    search: InputEditor,
    search_hit: Option<crate::settings_form::FieldHit>,
    record_hits: Vec<(ProviderKey, Rect)>,
    option_hits: Vec<(usize, Rect)>,
    action_hits: Vec<(ProviderAction, Rect)>,
    hovered_record: Option<ProviderKey>,
    hovered_option: Option<usize>,
    hovered_action: Option<ProviderAction>,
    details: bool,
    more: bool,
    show_result: bool,
    details_rect: Option<Rect>,
    details_scroll: usize,
    details_max_scroll: usize,
    visible_records: usize,
    option_offset: usize,
    action_offset: usize,
    action_selected: usize,
    compact: bool,
    initial_draft: Option<ProviderDraft>,
    initial_oauth_name: String,
    confirm_discard: bool,
    remove_blockers: Vec<String>,
    pending_test: Option<ProviderTestBinding>,
    next_test_request_id: u64,
    result: Option<ProviderResult>,
    form_prompt_cache_key: Option<bool>,
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
    pub fn has_text_focus(&self) -> bool {
        if !self.open
            || self.has_pending_mutation()
            || self.confirm_discard
            || self.show_confirm
            || self.show_result
        {
            return false;
        }
        if !self.show_add {
            return self.focus == ProviderFocus::Search;
        }
        if !self.in_form {
            return self.name_focused
                && self.focus != ProviderFocus::Action(ProviderAction::Login)
                && self.focus != ProviderFocus::Action(ProviderAction::Cancel);
        }
        matches!(self.form_field, 0 | 2 | 3 | 4)
            && !(self.form_field == 0 && self.editing_provider.is_some())
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.handle_key(&KeyAction::Escape, None);
        } else {
            self.open();
        }
    }

    fn clear_geometry(&mut self) {
        self.last_input_rect = None;
        self.test_btn_rect = None;
        self.save_rect = None;
        self.list_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        self.search_hit = None;
        self.details_rect = None;
        self.form_hits.clear();
        self.selector_hits.clear();
        self.record_hits.clear();
        self.option_hits.clear();
        self.action_hits.clear();
    }

    pub fn handle_mouse(
        &mut self,
        me: &crossterm::event::MouseEvent,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (me.column, me.row);
        if matches!(me.kind, MouseEventKind::Moved) {
            self.hovered_action = self
                .action_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(action, _)| *action);
            self.hovered_record = self
                .record_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(key, _)| key.clone());
            self.hovered_option = self
                .option_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(index, _)| *index);
            self.hovered_field = if hit(self.save_rect, point) {
                Some(usize::MAX)
            } else if hit(self.test_btn_rect, point) {
                Some(7)
            } else {
                self.form_hits
                    .iter()
                    .find(|(_, field)| hit(Some(field.record), point))
                    .map(|(field, _)| *field)
            };
            return None;
        }
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(action) = self
                .action_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(action, _)| *action)
            {
                return self.dispatch(action, control_tx);
            }
            if self.has_pending_mutation() {
                return None;
            }
            // Legacy public rectangles also route through the same action dispatcher.
            if hit(self.test_btn_rect, point) {
                return self.dispatch(ProviderAction::Test, control_tx);
            }
            if self.show_confirm || self.confirm_discard || self.show_result {
                return None;
            }
            if !self.show_add
                && !self.more
                && let Some(field) = self.search_hit
                && hit(Some(field.record), point)
            {
                self.focus = ProviderFocus::Search;
                self.search.set_cursor_by_display(
                    0,
                    me.column
                        .saturating_sub(field.value.x)
                        .saturating_add(field.offset as u16),
                );
                return None;
            }
            if self.show_add && self.name_focused && !self.in_form {
                if let Some((_, field)) = self.form_hits.first().copied()
                    && hit(Some(field.record), point)
                {
                    self.focus = ProviderFocus::ProviderList;
                    self.name_editor.set_cursor_by_display(
                        0,
                        me.column
                            .saturating_sub(field.value.x)
                            .saturating_add(field.offset as u16),
                    );
                }
                return None;
            }
            if self.in_form {
                if let Some((field, field_hit)) = self
                    .form_hits
                    .iter()
                    .find(|(_, field)| hit(Some(field.record), point))
                    .copied()
                {
                    self.form_field = field;
                    if let Some((left, right)) = field_hit.arrows {
                        if hit(Some(left), point) || hit(Some(right), point) {
                            let action = if hit(Some(left), point) {
                                KeyAction::CursorLeft
                            } else {
                                KeyAction::CursorRight
                            };
                            self.handle_add_key(&action, control_tx);
                            return None;
                        }
                    }
                    if hit(Some(field_hit.value), point) {
                        if matches!(field, 1 | 5 | 6 | 8) {
                            self.handle_add_key(&KeyAction::CursorRight, control_tx);
                        } else if let Some(editor) = self.form_editor_mut() {
                            editor.set_cursor_by_display(
                                0,
                                me.column
                                    .saturating_sub(field_hit.value.x)
                                    .saturating_add(field_hit.offset as u16),
                            );
                        }
                    }
                }
                return None;
            }
            if let Some(index) = self
                .option_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(index, _)| *index)
            {
                self.kind_selected = index;
                return self.dispatch(ProviderAction::Choose, control_tx);
            }
            if let Some(key) = self
                .record_hits
                .iter()
                .find(|(_, rect)| hit(Some(*rect), point))
                .map(|(key, _)| key.clone())
            {
                if let Some(index) = self
                    .providers
                    .iter()
                    .position(|provider| provider.key() == key)
                {
                    self.select(index);
                    self.details = true;
                    self.focus = ProviderFocus::Details;
                    self.clear_geometry();
                }
            } else if hit(self.details_rect, point) {
                self.focus = ProviderFocus::Details;
            }
        } else if matches!(
            me.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            if self.has_pending_mutation() {
                return None;
            }
            let down = matches!(me.kind, MouseEventKind::ScrollDown);
            if hit(self.details_rect, point) {
                self.scroll_details(if down { 3 } else { -3 });
            } else if self.in_form
                && self
                    .form_hits
                    .iter()
                    .any(|(_, field)| hit(Some(field.record), point))
            {
                self.handle_add_key(
                    if down {
                        &KeyAction::HistoryDown
                    } else {
                        &KeyAction::HistoryUp
                    },
                    control_tx,
                );
            } else if hit(self.list_rect, point) {
                self.move_selection(if down { 1 } else { -1 });
            }
        }
        None
    }

    pub fn open(&mut self) {
        self.open = true;
        self.refresh_list();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.clear_geometry();
    }

    pub(crate) fn has_pending_mutation(&self) -> bool {
        self.pending_mutation.is_some() || self.pending_test.is_some()
    }

    pub(crate) fn take_feedback(&mut self) -> Option<ProviderFeedback> {
        self.feedback.take()
    }

    fn record_feedback(&mut self, feedback: ProviderFeedback) {
        let message = match feedback {
            ProviderFeedback::DispatchUnavailable => Some(
                "The operation could not be started because the host connection is unavailable.",
            ),
            ProviderFeedback::InvalidTestConfiguration => {
                Some("The test configuration is invalid. Check the provider fields and try again.")
            }
            _ => None,
        };
        if let Some(message) = message {
            self.local_error(message);
        }
        if self.feedback.is_none() {
            self.feedback = Some(feedback);
        }
    }

    fn local_error(&mut self, message: &str) {
        let name = if self.show_add {
            self.name_editor.buf().trim().to_owned()
        } else {
            self.providers
                .get(self.selected)
                .map_or_else(String::new, |provider| provider.name.clone())
        };
        let key = if self.show_add {
            ProviderKey::Config(name.clone())
        } else {
            self.selected_key()
                .unwrap_or_else(|| ProviderKey::Config(name.clone()))
        };
        self.result = Some(ProviderResult {
            key,
            name,
            operation: "Operation unavailable",
            result: Err(message.into()),
            draft: self.in_form.then(|| self.draft()),
        });
    }

    fn pending_help(&self) -> Option<&'static str> {
        if self.pending_test.is_some() {
            return Some("Testing connection · Hide Esc");
        }
        let request = self.pending_mutation.as_ref()?;
        Some(match &request.action {
            crate::ProviderMutation::Login { .. } => "waiting for OAuth login…",
            crate::ProviderMutation::SetEnabled { .. } => "updating provider state…",
            crate::ProviderMutation::Remove { .. }
            | crate::ProviderMutation::RemoveConfig { .. } => "removing provider…",
            crate::ProviderMutation::Refresh { .. } => "refreshing models…",
            crate::ProviderMutation::UpsertConfig { .. } => "saving provider configuration…",
        })
    }

    pub fn refresh_list(&mut self) {
        let selected_key = self.selected_key();
        self.providers.clear();
        self.model_counts.clear();
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
                self.model_counts.push((
                    ProviderKey::Auth(p.id.clone()),
                    p.model_cache.as_ref().map_or(0, |cache| cache.models.len()),
                ));
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
        let model_entries = atman_runtime::model_registry::all_model_entries();
        for (name, entry) in atman_runtime::model_registry::all_provider_entries() {
            let count = model_entries
                .iter()
                .filter(|(_, model)| model.provider.as_deref() == Some(&name))
                .count();
            self.model_counts
                .push((ProviderKey::Config(name.clone()), count));
            let detail = entry.base_url.as_deref().unwrap_or("").to_owned();
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
        if let Some(key) = selected_key {
            if let Some(index) = self
                .providers
                .iter()
                .position(|provider| provider.key() == key)
            {
                self.selected = index;
            } else {
                self.selected = self.selected.min(self.providers.len().saturating_sub(1));
                self.details_scroll = 0;
                if self.result.as_ref().is_some_and(|result| result.key == key) {
                    self.result = None;
                }
            }
        }
        if self.show_confirm
            && let Some(id) = self.confirm_provider_id.clone()
        {
            let target = if self.confirm_config_provider {
                ProviderKey::Config(id.clone())
            } else {
                ProviderKey::Auth(id.clone())
            };
            if self
                .providers
                .iter()
                .any(|provider| provider.key() == target)
            {
                self.remove_blockers = self.provider_dependencies(&id);
            } else {
                self.show_confirm = false;
                self.confirm_provider_id = None;
                self.remove_blockers.clear();
                self.focus = ProviderFocus::ProviderList;
            }
        }
        self.clear_geometry();
    }

    pub fn open_add(&mut self) {
        self.open = true;
        if self.has_pending_mutation() {
            return;
        }
        self.show_add = true;
        self.editing_provider = None;
        self.in_form = false;
        self.name_focused = false;
        self.form_max_tokens = None;
        self.more = false;
        self.result = None;
        self.confirm_discard = false;
        self.focus = ProviderFocus::ProviderList;
        self.option_offset = 0;
        self.add_options = atman_runtime::model_registry::PROVIDER_PRESETS
            .iter()
            .enumerate()
            .map(|(i, preset)| AddProviderOption {
                label: preset.name,
                description: preset.description,
                kind: AddProviderKind::Preset(i),
            })
            .chain([AddProviderOption {
                label: "Custom",
                description: "API Key / OpenAI-compatible",
                kind: AddProviderKind::Custom,
            }])
            .collect();
        self.kind_selected = 0;
        self.name_editor = InputEditor::default();
        self.clear_geometry();
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
        self.form_api = entry.api;
        self.form_max_tools = entry.max_tools;
        self.form_prompt_cache_key = entry.prompt_cache_key;
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
        self.initial_draft = Some(self.draft());
        self.result = None;
        self.more = false;
        self.clear_geometry();
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if self.has_pending_mutation() {
            if matches!(action, KeyAction::Escape) {
                self.close();
            }
            return Some(ModalAction::Consumed);
        }
        if self.show_result {
            if matches!(action, KeyAction::Escape | KeyAction::Submit) {
                return self.dispatch(ProviderAction::Back, control_tx);
            }
            if matches!(action, KeyAction::Tab | KeyAction::BackTab) {
                self.focus = ProviderFocus::Action(ProviderAction::Back);
            }
            self.navigate_details(action);
            return Some(ModalAction::Consumed);
        }
        if self.show_confirm || self.confirm_discard {
            let actions = if self.confirm_discard {
                vec![
                    ProviderAction::Save,
                    ProviderAction::Discard,
                    ProviderAction::KeepEditing,
                ]
            } else if self.remove_blockers.is_empty() {
                vec![ProviderAction::ConfirmRemove, ProviderAction::Cancel]
            } else {
                vec![ProviderAction::Models, ProviderAction::Cancel]
            };
            match action {
                KeyAction::Save if self.confirm_discard => {
                    return self.dispatch(ProviderAction::Save, control_tx);
                }
                KeyAction::Escape => {
                    return self.dispatch(
                        if self.confirm_discard {
                            ProviderAction::KeepEditing
                        } else {
                            ProviderAction::Cancel
                        },
                        control_tx,
                    );
                }
                KeyAction::Tab
                | KeyAction::BackTab
                | KeyAction::CursorLeft
                | KeyAction::CursorRight => self.cycle_actions(
                    &actions,
                    matches!(action, KeyAction::BackTab | KeyAction::CursorLeft),
                ),
                KeyAction::Submit => {
                    if let ProviderFocus::Action(action) = self.focus {
                        return self.dispatch(action, control_tx);
                    }
                }
                _ => self.navigate_details(action),
            }
            return Some(ModalAction::Consumed);
        }
        if self.show_add {
            return self
                .handle_add_key(action, control_tx)
                .or(Some(ModalAction::Consumed));
        }
        if self.focus == ProviderFocus::Search {
            match action {
                KeyAction::Escape | KeyAction::Submit => self.focus = ProviderFocus::ProviderList,
                KeyAction::Tab | KeyAction::BackTab => {
                    self.cycle_focus(matches!(action, KeyAction::BackTab))
                }
                _ => {
                    let old = self.search.buf().to_owned();
                    self.search.handle_key(action);
                    if self.search.buf() != old {
                        self.filter_changed();
                    }
                }
            }
            return Some(ModalAction::Consumed);
        }
        match action {
            KeyAction::Escape => return self.dispatch(ProviderAction::Back, control_tx),
            KeyAction::Char('/') if !self.more => {
                self.focus = ProviderFocus::Search;
            }
            KeyAction::Tab | KeyAction::BackTab => {
                self.cycle_focus(matches!(action, KeyAction::BackTab))
            }
            KeyAction::Submit => {
                if let ProviderFocus::Action(action) = self.focus {
                    return self.dispatch(action, control_tx);
                }
                if self.more {
                    if let Some(action) =
                        self.available_actions().get(self.action_selected).copied()
                    {
                        return self.dispatch(action, control_tx);
                    }
                } else {
                    self.details = true;
                    self.focus = ProviderFocus::Details;
                    self.details_scroll = 0;
                    self.clear_geometry();
                }
            }
            KeyAction::Char('n') => return self.dispatch(ProviderAction::Add, control_tx),
            KeyAction::Char('e') => return self.dispatch(ProviderAction::Edit, control_tx),
            KeyAction::Char('m') => return self.dispatch(ProviderAction::Models, control_tx),
            KeyAction::Char('d') => return self.dispatch(ProviderAction::Toggle, control_tx),
            KeyAction::Char('r') => return self.dispatch(ProviderAction::Refresh, control_tx),
            KeyAction::Char('t') => return self.dispatch(ProviderAction::Test, control_tx),
            KeyAction::Delete => return self.dispatch(ProviderAction::Remove, control_tx),
            _ if self.focus == ProviderFocus::Details => self.navigate_details(action),
            KeyAction::HistoryUp | KeyAction::Char('k') => self.move_selection(-1),
            KeyAction::HistoryDown | KeyAction::Char('j') => self.move_selection(1),
            KeyAction::PageUp => self.move_selection(-(self.visible_records.max(1) as isize)),
            KeyAction::PageDown => self.move_selection(self.visible_records.max(1) as isize),
            KeyAction::Home | KeyAction::CursorHome => self.move_selection(isize::MIN),
            KeyAction::End | KeyAction::CursorEnd => self.move_selection(isize::MAX),
            _ => {}
        }
        Some(ModalAction::Consumed)
    }

    fn selected_key(&self) -> Option<ProviderKey> {
        self.providers.get(self.selected).map(ProviderEntry::key)
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let query = self.search.buf().trim().to_lowercase();
        self.providers
            .iter()
            .enumerate()
            .filter(|(_, provider)| {
                query.is_empty()
                    || format!(
                        "{} {} {} {}",
                        provider.name,
                        provider.kind,
                        provider.source_label(),
                        provider.detail
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn list_page(&self) -> (usize, usize) {
        let visible = self.filtered_indices();
        let per_page = self.visible_records.max(1);
        let page = visible
            .iter()
            .position(|index| *index == self.selected)
            .map_or(0, |index| index / per_page + 1);
        (page, visible.len().div_ceil(per_page))
    }

    fn filter_changed(&mut self) {
        let visible = self.filtered_indices();
        if !visible.contains(&self.selected)
            && let Some(index) = visible.first()
        {
            self.select(*index);
        }
        self.list_offset = 0;
        self.clear_geometry();
    }

    fn select(&mut self, index: usize) {
        if self.selected != index {
            self.selected = index;
            self.details_scroll = 0;
            self.clear_geometry();
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.more {
            self.focus = ProviderFocus::ProviderList;
            let count = self.available_actions().len();
            self.action_selected = self
                .action_selected
                .saturating_add_signed(delta)
                .min(count.saturating_sub(1));
        } else if self.show_add {
            self.kind_selected = self
                .kind_selected
                .saturating_add_signed(delta)
                .min(self.add_options.len().saturating_sub(1));
        } else {
            let visible = self.filtered_indices();
            let current = visible
                .iter()
                .position(|index| *index == self.selected)
                .unwrap_or(0);
            if let Some(index) = visible.get(
                current
                    .saturating_add_signed(delta)
                    .min(visible.len().saturating_sub(1)),
            ) {
                self.select(*index);
            }
        }
        self.clear_geometry();
    }

    fn scroll_details(&mut self, delta: isize) {
        self.details_scroll = self
            .details_scroll
            .saturating_add_signed(delta)
            .min(self.details_max_scroll);
    }

    fn navigate_details(&mut self, action: &KeyAction) {
        let page = self.details_rect.map_or(1, |rect| rect.height.max(1)) as isize;
        match action {
            KeyAction::HistoryUp | KeyAction::Char('k') => self.scroll_details(-1),
            KeyAction::HistoryDown | KeyAction::Char('j') => self.scroll_details(1),
            KeyAction::PageUp => self.scroll_details(-page),
            KeyAction::PageDown => self.scroll_details(page),
            KeyAction::Home | KeyAction::CursorHome => self.details_scroll = 0,
            KeyAction::End | KeyAction::CursorEnd => self.details_scroll = self.details_max_scroll,
            _ => {}
        }
    }

    fn cycle_actions(&mut self, actions: &[ProviderAction], backward: bool) {
        let index = actions
            .iter()
            .position(|action| self.focus == ProviderFocus::Action(*action))
            .unwrap_or(0);
        let next = if backward {
            (index + actions.len() - 1) % actions.len()
        } else {
            (index + 1) % actions.len()
        };
        self.focus = ProviderFocus::Action(actions[next]);
    }

    fn cycle_focus(&mut self, backward: bool) {
        let mut focuses = if self.more {
            vec![ProviderFocus::ProviderList]
        } else {
            vec![ProviderFocus::Search, ProviderFocus::ProviderList]
        };
        if !self.more && (self.details || !self.compact) {
            focuses.push(ProviderFocus::Details);
        }
        focuses.extend(
            self.action_hits
                .iter()
                .map(|(action, _)| ProviderFocus::Action(*action)),
        );
        let current = focuses
            .iter()
            .position(|focus| *focus == self.focus)
            .unwrap_or(0);
        let next = if backward {
            (current + focuses.len() - 1) % focuses.len()
        } else {
            (current + 1) % focuses.len()
        };
        self.focus = focuses[next];
    }

    fn available_actions(&self) -> Vec<ProviderAction> {
        let mut actions = vec![ProviderAction::Add];
        if let Some(provider) = self
            .providers
            .get(self.selected)
            .filter(|_| !self.filtered_indices().is_empty())
        {
            match provider.source {
                ProviderSource::Config => actions.extend([
                    ProviderAction::Edit,
                    ProviderAction::Models,
                    ProviderAction::Test,
                    ProviderAction::Toggle,
                    ProviderAction::Remove,
                ]),
                ProviderSource::AuthStore { .. } => actions.extend([
                    ProviderAction::Models,
                    ProviderAction::Toggle,
                    ProviderAction::Refresh,
                    ProviderAction::Remove,
                ]),
                ProviderSource::Env => {}
            }
        }
        if self.current_result().is_some() {
            actions.push(ProviderAction::Result);
        }
        if self.filtered_indices().len() > self.visible_records.max(1) {
            actions.extend([ProviderAction::Previous, ProviderAction::Next]);
        }
        actions
    }

    fn dispatch(
        &mut self,
        action: ProviderAction,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if self.has_pending_mutation() {
            if matches!(action, ProviderAction::Hide | ProviderAction::Back) {
                self.close();
            }
            return Some(ModalAction::Consumed);
        }
        if !self.in_form
            && !self.show_confirm
            && matches!(
                action,
                ProviderAction::Edit
                    | ProviderAction::Models
                    | ProviderAction::Test
                    | ProviderAction::Toggle
                    | ProviderAction::Refresh
                    | ProviderAction::Remove
            )
            && !self.filtered_indices().contains(&self.selected)
        {
            return Some(ModalAction::Consumed);
        }
        if matches!(
            action,
            ProviderAction::Edit
                | ProviderAction::Test
                | ProviderAction::Toggle
                | ProviderAction::Refresh
                | ProviderAction::Remove
        ) {
            self.more = false;
        }
        match action {
            ProviderAction::Add => self.open_add(),
            ProviderAction::Edit => {
                if let Some(provider) = self.providers.get(self.selected).cloned()
                    && matches!(provider.source, ProviderSource::Config)
                {
                    self.open_edit(&provider.name);
                }
            }
            ProviderAction::Models => {
                let key = if self.show_confirm {
                    self.confirm_provider_id.clone()
                } else {
                    self.providers
                        .get(self.selected)
                        .and_then(|provider| match &provider.source {
                            ProviderSource::Config => Some(provider.name.clone()),
                            ProviderSource::AuthStore { id } => Some(id.clone()),
                            ProviderSource::Env => None,
                        })
                };
                if let Some(key) = key {
                    self.more = false;
                    self.clear_geometry();
                    return Some(ModalAction::OpenModelManager(key));
                }
            }
            ProviderAction::Test => {
                if self.in_form {
                    self.test_form(tx);
                } else {
                    self.test_selected(tx);
                }
            }
            ProviderAction::Toggle => self.toggle_enabled(tx),
            ProviderAction::Refresh => self.refresh_selected(tx),
            ProviderAction::Remove => self.request_confirm(ConfirmKind::Remove),
            ProviderAction::More => {
                self.more = true;
                self.action_selected = 0;
                self.focus = ProviderFocus::ProviderList;
            }
            ProviderAction::Previous | ProviderAction::Next => {
                let delta = self.visible_records.max(1) as isize
                    * if action == ProviderAction::Previous {
                        -1
                    } else {
                        1
                    };
                self.more = false;
                self.details = false;
                self.focus = ProviderFocus::ProviderList;
                self.move_selection(delta);
            }
            ProviderAction::Choose => self.choose_add_option(),
            ProviderAction::Save => {
                self.confirm_discard = false;
                if self.name_focused && !self.in_form {
                    self.commit_add(tx);
                } else {
                    self.commit_form(tx);
                }
            }
            ProviderAction::Cancel => {
                if self.show_confirm {
                    self.show_confirm = false;
                    self.focus = if self.details {
                        ProviderFocus::Details
                    } else {
                        ProviderFocus::ProviderList
                    };
                } else {
                    self.cancel_form();
                }
            }
            ProviderAction::ConfirmRemove => {
                if self.remove_blockers.is_empty() {
                    self.execute_confirm(tx);
                }
            }
            ProviderAction::KeepEditing => {
                self.confirm_discard = false;
                self.focus = ProviderFocus::ProviderList;
            }
            ProviderAction::Discard => {
                self.confirm_discard = false;
                self.finish_edit();
            }
            ProviderAction::Login => self.commit_add(tx),
            ProviderAction::Result => {
                self.show_result = true;
                self.details_scroll = 0;
                self.focus = ProviderFocus::Details;
            }
            ProviderAction::Back => {
                if self.show_result {
                    self.show_result = false;
                    self.focus = if self.details {
                        ProviderFocus::Details
                    } else {
                        ProviderFocus::ProviderList
                    };
                } else if self.more {
                    self.more = false;
                    self.focus = if self.details {
                        ProviderFocus::Details
                    } else {
                        ProviderFocus::ProviderList
                    };
                } else if self.show_add {
                    self.cancel_form();
                } else if self.details {
                    self.details = false;
                    self.focus = ProviderFocus::ProviderList;
                } else {
                    self.close();
                }
            }
            ProviderAction::Hide => self.close(),
        }
        self.clear_geometry();
        Some(ModalAction::Consumed)
    }

    fn draft(&self) -> ProviderDraft {
        ProviderDraft {
            name: self.name_editor.buf().into(),
            kind: self.provider_type_editor.buf().into(),
            api_key: self.api_key_editor.buf().into(),
            api_key_env: self.api_key_env_editor.buf().into(),
            base_url: self.base_url_editor.buf().into(),
            reasoning_format: self.reasoning_format_editor.buf().into(),
            enabled: self.enabled_editor.buf().into(),
            api: self.form_api,
            max_tokens: self.form_max_tokens,
            max_tools: self.form_max_tools,
        }
    }

    fn cancel_form(&mut self) {
        let dirty = if self.in_form {
            self.initial_draft
                .as_ref()
                .is_some_and(|initial| initial != &self.draft())
        } else {
            self.name_focused && self.name_editor.buf() != self.initial_oauth_name
        };
        if dirty {
            self.confirm_discard = true;
            self.focus = ProviderFocus::Action(ProviderAction::KeepEditing);
            self.details_scroll = 0;
        } else {
            self.finish_edit();
        }
    }

    fn finish_edit(&mut self) {
        if self.in_form || self.name_focused {
            if self.editing_provider.is_some() {
                self.show_add = false;
                self.details = true;
            }
            self.in_form = false;
            self.name_focused = false;
            self.editing_provider = None;
        } else {
            self.show_add = false;
        }
        self.focus = if self.details && !self.show_add {
            ProviderFocus::Details
        } else {
            ProviderFocus::ProviderList
        };
        self.result = None;
    }

    fn choose_add_option(&mut self) {
        let Some(option) = self.add_options.get(self.kind_selected).cloned() else {
            return;
        };
        match option.kind {
            AddProviderKind::Preset(index)
                if atman_runtime::model_registry::PROVIDER_PRESETS[index].provider_type
                    != "codex" =>
            {
                self.open_preset_form(index)
            }
            AddProviderKind::Preset(_) => {
                self.name_editor.replace_with(option.label);
                self.initial_oauth_name = option.label.into();
                self.name_focused = true;
                self.focus = ProviderFocus::ProviderList;
            }
            AddProviderKind::Custom => self.open_custom_form(),
        }
    }

    fn current_result(&self) -> Option<&ProviderResult> {
        let result = self.result.as_ref()?;
        let key = if self.in_form || self.name_focused {
            ProviderKey::Config(self.name_editor.buf().trim().into())
        } else {
            if !self.filtered_indices().contains(&self.selected) {
                return None;
            }
            self.selected_key()?
        };
        (result.key == key
            && result
                .draft
                .as_ref()
                .is_none_or(|draft| self.in_form && draft == &self.draft()))
        .then_some(result)
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
                                api: entry.api,
                                max_tools: entry.max_tools,
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
        let Some(provider) = self.providers.get(self.selected) else {
            return;
        };
        let (id, config) = match &provider.source {
            ProviderSource::AuthStore { id } => (id.clone(), false),
            ProviderSource::Config => (provider.name.clone(), true),
            ProviderSource::Env => {
                atman_runtime::notify!(warn, "Environment providers cannot be deleted");
                return;
            }
        };
        self.show_confirm = true;
        self.confirm_kind = Some(kind);
        self.confirm_provider_id = Some(id.clone());
        self.confirm_provider_name = provider.name.clone();
        self.confirm_config_provider = config;
        self.remove_blockers = self.provider_dependencies(&id);
        self.focus = ProviderFocus::Action(ProviderAction::Cancel);
        self.details_scroll = 0;
        self.more = false;
        self.clear_geometry();
    }

    fn provider_dependencies(&self, key: &str) -> Vec<String> {
        atman_runtime::model_registry::all_model_entries()
            .into_iter()
            .filter(|(_, model)| !model.discovered && model.provider.as_deref() == Some(key))
            .map(|(name, _)| name)
            .collect()
    }

    fn execute_confirm(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let Some(id) = self.confirm_provider_id.clone() else {
            return;
        };
        self.remove_blockers = self.provider_dependencies(&id);
        if !self.remove_blockers.is_empty() {
            return;
        }
        let action = if self.confirm_config_provider {
            crate::ProviderMutation::RemoveConfig { name: id }
        } else {
            crate::ProviderMutation::Remove { provider_id: id }
        };
        let outcome = match self.confirm_kind {
            Some(ConfirmKind::Remove) => self.begin_mutation(action, control_tx),
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
        if self.has_pending_mutation() {
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
        self.clear_geometry();
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
        let (key, name, operation) = match &request.action {
            crate::ProviderMutation::UpsertConfig { name, .. } => (
                ProviderKey::Config(name.clone()),
                name.clone(),
                "Save configuration",
            ),
            crate::ProviderMutation::RemoveConfig { name } => (
                ProviderKey::Config(name.clone()),
                name.clone(),
                "Remove provider",
            ),
            crate::ProviderMutation::Login { name, .. } => {
                (ProviderKey::Config(name.clone()), name.clone(), "Login")
            }
            crate::ProviderMutation::Remove { provider_id } => (
                ProviderKey::Auth(provider_id.clone()),
                self.confirm_provider_name.clone(),
                "Remove provider",
            ),
            crate::ProviderMutation::Refresh { provider_id } => (
                ProviderKey::Auth(provider_id.clone()),
                self.providers
                    .iter()
                    .find(|provider| provider.key() == ProviderKey::Auth(provider_id.clone()))
                    .map_or_else(|| provider_id.clone(), |provider| provider.name.clone()),
                "Refresh catalog",
            ),
            crate::ProviderMutation::SetEnabled { provider_id, .. } => (
                ProviderKey::Auth(provider_id.clone()),
                self.providers
                    .iter()
                    .find(|provider| provider.key() == ProviderKey::Auth(provider_id.clone()))
                    .map_or_else(|| provider_id.clone(), |provider| provider.name.clone()),
                "Update state",
            ),
        };
        self.refresh_list();
        let succeeded = match result {
            Ok(success) if mutation_success_matches(&request.action, success) => true,
            Ok(_) => {
                self.result = Some(ProviderResult {
                    key,
                    name,
                    operation,
                    result: Err("The result did not match the requested operation.".into()),
                    draft: self.in_form.then(|| self.draft()),
                });
                return ProviderMutationResolution::ProtocolError;
            }
            Err(_) => false,
        };
        self.result = Some(ProviderResult {
            key,
            name,
            operation,
            result: result
                .as_ref()
                .map(|_| "Completed".to_owned())
                .map_err(Clone::clone),
            draft: self.in_form.then(|| self.draft()),
        });
        if !succeeded {
            if !self.show_add {
                self.details = true;
                self.focus = ProviderFocus::Details;
            }
            return ProviderMutationResolution::Failed;
        }
        match request.action {
            crate::ProviderMutation::Login { name, .. } => {
                self.show_add = false;
                self.name_focused = false;
                self.details = true;
                self.focus = ProviderFocus::Details;
                if let Ok(crate::ProviderMutationSuccess::Installed { provider_id, .. }) = result {
                    if let Some(index) = self.providers.iter().position(|provider| {
                        provider.key() == ProviderKey::Auth(provider_id.clone())
                    }) {
                        self.selected = index;
                    }
                    if let Some(result) = &mut self.result {
                        result.key = ProviderKey::Auth(provider_id.clone());
                        result.draft = None;
                    }
                }
                ProviderMutationResolution::Installed { name }
            }
            crate::ProviderMutation::UpsertConfig { name, create, .. } => {
                if self.in_form {
                    self.show_add = false;
                    self.in_form = false;
                    self.editing_provider = None;
                    self.form_max_tokens = None;
                }
                if let Some(index) = self
                    .providers
                    .iter()
                    .position(|provider| provider.key() == ProviderKey::Config(name.clone()))
                {
                    self.selected = index;
                }
                self.details = true;
                self.focus = ProviderFocus::Details;
                if let Some(result) = &mut self.result {
                    result.draft = None;
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

    fn api_visible(&self) -> bool {
        matches!(
            self.provider_type_editor.buf().trim(),
            "openai" | "openai-compat"
        )
    }

    fn selected_api(&self) -> Option<atman_runtime::provider::OpenAiApi> {
        self.api_visible()
            .then(|| self.form_api.unwrap_or_default())
    }

    // Rendering, keyboard navigation and mouse hit regions share this order.
    fn form_fields(&self) -> Vec<usize> {
        let mut fields = Vec::new();
        if self.editing_provider.is_none() {
            fields.push(0);
        }
        fields.push(1);
        if self.api_visible() {
            fields.push(8);
        }
        fields.extend([2, 3, 4]);
        if self.selected_api() == Some(atman_runtime::provider::OpenAiApi::ChatCompletions) {
            fields.push(5);
        }
        fields.extend([6, 7]);
        fields
    }

    fn form_editor_mut(&mut self) -> Option<&mut InputEditor> {
        match self.form_field {
            0 if self.editing_provider.is_none() => Some(&mut self.name_editor),
            2 => Some(&mut self.api_key_editor),
            3 => Some(&mut self.api_key_env_editor),
            4 => Some(&mut self.base_url_editor),
            _ => None,
        }
    }

    fn form_endpoint(&self) -> String {
        let entry = atman_runtime::model_registry::ProviderEntry {
            kind: self.provider_type_editor.buf().trim().into(),
            base_url: non_empty(self.base_url_editor.buf()),
            api: self.selected_api(),
            ..Default::default()
        };
        entry
            .api_endpoint(None)
            .unwrap_or_else(|| "Protocol-defined endpoint".into())
    }

    fn open_custom_form(&mut self) {
        self.show_add = true;
        self.in_form = true;
        self.form_field = 0;
        self.form_max_tokens = None;
        self.form_api = None;
        self.form_max_tools = None;
        self.form_prompt_cache_key = None;
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
        self.initial_draft = Some(self.draft());
        self.clear_geometry();
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
        self.form_api = Some(if preset.provider_type == "openai" {
            atman_runtime::provider::OpenAiApi::Responses
        } else {
            atman_runtime::provider::OpenAiApi::ChatCompletions
        });
        let mut url_ed = InputEditor::default();
        url_ed.insert_str(preset.base_url);
        self.base_url_editor = url_ed;
        if !preset.needs_api_key {
            self.form_field = 2;
        }
        self.initial_draft = Some(self.draft());
    }

    fn test_form(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let name = self.name_editor.buf().trim().to_string();
        if name.is_empty() {
            self.local_error("Enter a provider name before testing the connection.");
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
            api: self.selected_api(),
            max_tools: self.form_max_tools,
            reasoning_format,
            prompt_cache_key: self.form_prompt_cache_key,
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
        if self.has_pending_mutation() {
            return ProviderDispatchOutcome::Busy;
        }
        let Some(tx) = control_tx else {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        };
        self.next_test_request_id = self.next_test_request_id.wrapping_add(1);
        let request_id = self.next_test_request_id;
        if tx
            .send(crate::TuiControl::TestProviderConnection(
                crate::ProviderConnectionTestRequest {
                    request_id,
                    name: name.clone(),
                    entry,
                },
            ))
            .is_err()
        {
            self.record_feedback(ProviderFeedback::DispatchUnavailable);
            return ProviderDispatchOutcome::Unavailable;
        }
        self.pending_test = Some(ProviderTestBinding {
            request_id,
            key: ProviderKey::Config(name.clone()),
            name,
            draft: self.in_form.then(|| self.draft()),
        });
        self.result = None;
        self.clear_geometry();
        self.record_feedback(ProviderFeedback::TestStarted);
        ProviderDispatchOutcome::Started
    }

    pub(crate) fn resolve_test(
        &mut self,
        request_id: u64,
        result: &Result<String, String>,
    ) -> bool {
        if self
            .pending_test
            .as_ref()
            .is_none_or(|pending| pending.request_id != request_id)
        {
            return false;
        }
        let pending = self.pending_test.take().expect("matching test request");
        if pending
            .draft
            .as_ref()
            .is_some_and(|draft| !self.in_form || draft != &self.draft())
        {
            return false;
        }
        self.result = Some(ProviderResult {
            key: pending.key,
            name: pending.name,
            operation: "Test connection",
            result: result.clone(),
            draft: pending.draft,
        });
        true
    }

    fn handle_add_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if self.in_form {
            let mut fields: Vec<_> = self
                .form_fields()
                .into_iter()
                .filter(|field| *field != 7)
                .collect();
            if self.current_result().is_some() {
                fields.push(11);
            }
            fields.extend([10, 7, 9]);
            let index = fields
                .iter()
                .position(|field| *field == self.form_field)
                .unwrap_or(0);
            match action {
                KeyAction::Escape => return self.dispatch(ProviderAction::Cancel, control_tx),
                KeyAction::Save => return self.dispatch(ProviderAction::Save, control_tx),
                KeyAction::Submit => match self.form_field {
                    7 => return self.dispatch(ProviderAction::Test, control_tx),
                    9 => return self.dispatch(ProviderAction::Save, control_tx),
                    10 => return self.dispatch(ProviderAction::Cancel, control_tx),
                    11 => return self.dispatch(ProviderAction::Result, control_tx),
                    _ => {}
                },
                KeyAction::Tab | KeyAction::HistoryDown => {
                    self.form_field = fields[(index + 1) % fields.len()]
                }
                KeyAction::BackTab | KeyAction::HistoryUp => {
                    self.form_field = fields[(index + fields.len() - 1) % fields.len()]
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
                }
                KeyAction::CursorLeft | KeyAction::CursorRight if self.form_field == 8 => {
                    self.form_api = Some(match self.form_api.unwrap_or_default() {
                        atman_runtime::provider::OpenAiApi::ChatCompletions => {
                            atman_runtime::provider::OpenAiApi::Responses
                        }
                        atman_runtime::provider::OpenAiApi::Responses => {
                            atman_runtime::provider::OpenAiApi::ChatCompletions
                        }
                    });
                }
                KeyAction::CursorLeft | KeyAction::CursorRight if self.form_field == 5 => {
                    let next = if self.reasoning_format_editor.buf().trim() == REASONING_FORMATS[0]
                    {
                        REASONING_FORMATS[1]
                    } else {
                        REASONING_FORMATS[0]
                    };
                    self.reasoning_format_editor.replace_with(next);
                }
                KeyAction::CursorLeft | KeyAction::CursorRight if self.form_field == 6 => {
                    self.enabled_editor.replace_with(
                        if self.enabled_editor.buf().trim() == "true" {
                            "false"
                        } else {
                            "true"
                        },
                    );
                }
                _ => {
                    if let Some(editor) = self.form_editor_mut() {
                        editor.handle_key(action);
                    }
                }
            }
        } else if self.name_focused {
            match action {
                KeyAction::Escape => return self.dispatch(ProviderAction::Cancel, control_tx),
                KeyAction::Save => return self.dispatch(ProviderAction::Login, control_tx),
                KeyAction::Tab | KeyAction::BackTab => {
                    let mut focuses = vec![
                        ProviderFocus::ProviderList,
                        ProviderFocus::Action(ProviderAction::Login),
                        ProviderFocus::Action(ProviderAction::Cancel),
                    ];
                    if self.current_result().is_some() {
                        focuses.push(ProviderFocus::Action(ProviderAction::Result));
                    }
                    let index = focuses
                        .iter()
                        .position(|focus| *focus == self.focus)
                        .unwrap_or(0);
                    self.focus = focuses[(index
                        + if *action == KeyAction::BackTab {
                            focuses.len() - 1
                        } else {
                            1
                        })
                        % focuses.len()];
                }
                KeyAction::Submit => {
                    if let ProviderFocus::Action(action) = self.focus {
                        return self.dispatch(action, control_tx);
                    }
                }
                _ if self.focus == ProviderFocus::ProviderList => {
                    self.name_editor.handle_key(action);
                }
                _ => {}
            }
        } else {
            match action {
                KeyAction::Escape => return self.dispatch(ProviderAction::Back, control_tx),
                KeyAction::Submit => {
                    return self.dispatch(
                        if let ProviderFocus::Action(action) = self.focus {
                            action
                        } else {
                            ProviderAction::Choose
                        },
                        control_tx,
                    );
                }
                KeyAction::Tab | KeyAction::BackTab => {
                    let focuses = [
                        ProviderFocus::ProviderList,
                        ProviderFocus::Action(ProviderAction::Choose),
                        ProviderFocus::Action(ProviderAction::Back),
                    ];
                    let index = focuses
                        .iter()
                        .position(|focus| *focus == self.focus)
                        .unwrap_or(0);
                    self.focus =
                        focuses[(index + if *action == KeyAction::BackTab { 2 } else { 1 }) % 3];
                }
                KeyAction::HistoryUp | KeyAction::Char('k') => self.move_selection(-1),
                KeyAction::HistoryDown | KeyAction::Char('j') => self.move_selection(1),
                KeyAction::PageUp => self.move_selection(-(self.visible_records.max(1) as isize)),
                KeyAction::PageDown => self.move_selection(self.visible_records.max(1) as isize),
                KeyAction::Home | KeyAction::CursorHome => self.move_selection(isize::MIN),
                KeyAction::End | KeyAction::CursorEnd => self.move_selection(isize::MAX),
                _ => {}
            }
        }
        self.clear_geometry();
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
        if name.is_empty() {
            self.local_error("Enter a provider name before saving.");
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
                api: self.form_api,
                max_tools: self.form_max_tools,
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
            self.local_error("Enter a connection name before starting login.");
            return;
        }
        let Some(option) = self.add_options.get(self.kind_selected).cloned() else {
            return;
        };
        let kind = match option.kind {
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

fn hit(rect: Option<Rect>, point: (u16, u16)) -> bool {
    rect.is_some_and(|rect| {
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    })
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
            crate::ProviderMutation::RemoveConfig { name },
            crate::ProviderMutationSuccess::ConfigRemoved { name: removed_name },
        ) => name == removed_name,
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

impl crate::wm::modal::ModalOverlay for ProviderManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.clear_geometry();
        view::render(f, area, self, t);
    }

    fn handle_key(
        &mut self,
        action: &KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action, tx).or(Some(ModalAction::Consumed))
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
    }

    fn handle_paste(&mut self, text: &str) {
        if self.has_pending_mutation()
            || self.confirm_discard
            || self.show_confirm
            || self.show_result
        {
            return;
        }
        if !self.show_add && self.focus == ProviderFocus::Search {
            self.search.insert_str(text);
            self.filter_changed();
            return;
        }
        if self.name_focused && !self.in_form {
            self.name_editor.insert_str(text);
            return;
        }
        if !self.in_form {
            return;
        }
        if self.form_field == 1 {
            let value = text.trim();
            if let Some(index) = provider_types().iter().position(|kind| *kind == value) {
                self.kind_selected = index;
                self.provider_type_editor.replace_with(value);
            }
        } else if self.form_field == 5 {
            if REASONING_FORMATS.contains(&text.trim()) {
                self.reasoning_format_editor.replace_with(text.trim());
            }
        } else if let Some(editor) = self.form_editor_mut() {
            editor.insert_str(text);
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn empty_search_cannot_dispatch_actions_for_a_hidden_provider() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager {
            open: true,
            providers: vec![ProviderEntry {
                source: ProviderSource::AuthStore {
                    id: "auth-id".into(),
                },
                name: "Account".into(),
                kind: "codex".into(),
                status: ProviderStatus::Active,
                detail: String::new(),
            }],
            ..Default::default()
        };
        manager.search.replace_with("no matching provider");
        manager.filter_changed();
        for action in [
            KeyAction::Char('e'),
            KeyAction::Char('d'),
            KeyAction::Char('t'),
            KeyAction::Char('m'),
            KeyAction::Char('r'),
            KeyAction::Delete,
        ] {
            assert!(matches!(
                manager.handle_key(&action, Some(&tx)),
                Some(ModalAction::Consumed)
            ));
            assert!(!manager.show_confirm);
            assert!(!manager.show_add);
            assert!(!manager.has_pending_mutation());
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn final_page_count_uses_filtered_selection_not_scroll_offset() {
        let mut manager = ProviderManager {
            providers: (0..10)
                .map(|index| ProviderEntry {
                    source: ProviderSource::Config,
                    name: format!("gateway-{index:02}"),
                    kind: "openai".into(),
                    status: ProviderStatus::Active,
                    detail: String::new(),
                })
                .collect(),
            selected: 9,
            visible_records: 3,
            list_offset: 7,
            ..Default::default()
        };
        assert_eq!(manager.list_page(), (4, 4));
        manager.search.replace_with("gateway-09");
        assert_eq!(manager.list_page(), (1, 1));
    }

    #[test]
    fn returning_to_details_keeps_navigation_in_the_visible_region() {
        let mut manager = ProviderManager {
            details: true,
            details_max_scroll: 10,
            ..Default::default()
        };
        manager.dispatch(ProviderAction::More, None);
        manager.dispatch(ProviderAction::Back, None);
        manager.handle_key(&KeyAction::HistoryDown, None);
        assert_eq!(manager.details_scroll, 1);
        assert_eq!(manager.focus, ProviderFocus::Details);
        manager.show_add = true;
        manager.in_form = true;
        manager.editing_provider = Some("gateway".into());
        manager.finish_edit();
        assert_eq!(manager.focus, ProviderFocus::Details);
    }

    #[test]
    fn removal_refresh_rechecks_the_bound_target_and_dependencies() {
        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }
        let _reset = Reset;
        let mut config = atman_runtime::model_registry::ProviderConfig::default();
        config
            .providers
            .insert("gateway".into(), Default::default());
        config.providers.insert("other".into(), Default::default());
        config.models.insert(
            "analysis".into(),
            atman_runtime::model_registry::ModelEntry {
                model: "vendor/reasoning-model".into(),
                provider: Some("gateway".into()),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_provider_config(config.clone());
        let mut manager = ProviderManager::default();
        manager.refresh_list();
        manager.selected = manager
            .providers
            .iter()
            .position(|provider| provider.key() == ProviderKey::Config("gateway".into()))
            .unwrap();
        manager.request_confirm(ConfirmKind::Remove);
        assert_eq!(manager.remove_blockers, vec!["analysis"]);
        assert!(
            matches!(manager.dispatch(ProviderAction::Models, None), Some(ModalAction::OpenModelManager(name)) if name == "gateway")
        );
        assert!(manager.show_confirm);
        config.models.clear();
        atman_runtime::model_registry::set_provider_config(config.clone());
        manager.selected = manager
            .providers
            .iter()
            .position(|provider| provider.key() == ProviderKey::Config("other".into()))
            .unwrap();
        manager.refresh_list();
        assert!(manager.show_confirm);
        assert!(manager.remove_blockers.is_empty());
        assert_eq!(manager.confirm_provider_id.as_deref(), Some("gateway"));
        config.providers.remove("gateway");
        atman_runtime::model_registry::set_provider_config(config);
        manager.refresh_list();
        assert!(!manager.show_confirm);
        assert!(manager.confirm_provider_id.is_none());
    }

    #[test]
    fn provider_sources_control_actions_and_enter_only_opens_details() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        for source in [
            ProviderSource::Config,
            ProviderSource::AuthStore {
                id: "auth-id".into(),
            },
            ProviderSource::Env,
        ] {
            let mut manager = ProviderManager {
                open: true,
                providers: vec![ProviderEntry {
                    source,
                    name: "connection".into(),
                    kind: "openai".into(),
                    status: ProviderStatus::Disabled,
                    detail: String::new(),
                }],
                ..Default::default()
            };
            let actions = manager.available_actions();
            let config = matches!(manager.providers[0].source, ProviderSource::Config);
            let env = matches!(manager.providers[0].source, ProviderSource::Env);
            assert_eq!(actions.contains(&ProviderAction::Edit), config);
            assert_eq!(actions.contains(&ProviderAction::Test), config);
            assert_eq!(actions.contains(&ProviderAction::Remove), !env);
            manager.handle_key(&KeyAction::Submit, Some(&tx));
            assert!(manager.details);
            assert!(!manager.show_confirm);
            assert!(!manager.in_form);
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn discard_and_remove_default_to_preserving_data() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, Some("gateway"));
        manager.api_key_editor.insert_str("-changed");
        manager.handle_key(&KeyAction::Escape, Some(&tx));
        assert!(manager.confirm_discard);
        assert_eq!(
            manager.focus,
            ProviderFocus::Action(ProviderAction::KeepEditing)
        );
        manager.handle_key(&KeyAction::Submit, Some(&tx));
        assert!(manager.in_form);
        assert_eq!(manager.api_key_editor.buf(), "test-key-changed");
        assert!(rx.try_recv().is_err());

        manager.show_add = false;
        manager.in_form = false;
        manager.providers = vec![ProviderEntry {
            source: ProviderSource::AuthStore {
                id: "auth-id".into(),
            },
            name: "OAuth".into(),
            kind: "codex".into(),
            status: ProviderStatus::Active,
            detail: String::new(),
        }];
        manager.request_confirm(ConfirmKind::Remove);
        assert_eq!(manager.focus, ProviderFocus::Action(ProviderAction::Cancel));
        manager.handle_key(&KeyAction::Submit, Some(&tx));
        assert!(!manager.show_confirm);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn tests_bind_to_request_and_draft_and_survive_hide() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.test_form(Some(&tx));
        let crate::TuiControl::TestProviderConnection(request) = rx.try_recv().unwrap() else {
            panic!("expected test request");
        };
        assert!(!manager.resolve_test(request.request_id + 1, &Ok("unrelated".into())));
        manager.handle_key(&KeyAction::Escape, Some(&tx));
        assert!(!manager.open);
        assert!(manager.in_form);
        assert!(manager.resolve_test(request.request_id, &Ok("Connected".into())));
        assert!(manager.current_result().is_some());
        manager.base_url_editor.insert_str("/changed");
        assert!(manager.current_result().is_none());
    }

    #[test]
    fn provider_refresh_preserves_source_identity_and_form_hidden_values() {
        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }
        let _reset = Reset;
        let mut config = atman_runtime::model_registry::ProviderConfig::default();
        config.providers.insert(
            "gateway".into(),
            atman_runtime::model_registry::ProviderEntry {
                kind: "openai-compat".into(),
                api: Some(atman_runtime::provider::OpenAiApi::Responses),
                max_tokens: Some(12_345),
                max_tools: Some(81),
                prompt_cache_key: Some(false),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_provider_config(config.clone());
        let mut manager = ProviderManager::default();
        manager.refresh_list();
        manager.selected = manager
            .providers
            .iter()
            .position(|provider| provider.key() == ProviderKey::Config("gateway".into()))
            .unwrap();
        config.providers.insert("aaa".into(), Default::default());
        atman_runtime::model_registry::set_provider_config(config);
        manager.refresh_list();
        assert_eq!(
            manager.selected_key(),
            Some(ProviderKey::Config("gateway".into()))
        );
        manager.open_edit("gateway");
        manager.provider_type_editor.replace_with("anthropic");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.commit_form(Some(&tx));
        assert!(matches!(
            receive_mutation(&mut rx).action,
            crate::ProviderMutation::UpsertConfig {
                api: Some(atman_runtime::provider::OpenAiApi::Responses),
                max_tokens: Some(12_345),
                max_tools: Some(81),
                ..
            }
        ));
        assert_eq!(manager.form_prompt_cache_key, Some(false));
    }

    #[test]
    fn cursor_visibility_tracks_provider_form_focus() {
        let mut manager = ProviderManager {
            open: true,
            show_add: true,
            in_form: true,
            ..Default::default()
        };
        for field in [0, 2, 3, 4] {
            manager.form_field = field;
            assert!(manager.has_text_focus());
        }
        for field in [1, 5, 6, 7] {
            manager.form_field = field;
            assert!(!manager.has_text_focus());
        }
        manager.in_form = false;
        manager.name_focused = true;
        assert!(manager.has_text_focus());
    }

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
        manager.initial_draft = Some(manager.draft());
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
    fn scrolled_provider_hitbox_targets_the_rendered_record() {
        use crate::wm::modal::ModalOverlay;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut manager = ProviderManager {
            open: true,
            providers: (0..16)
                .map(|index| ProviderEntry {
                    source: ProviderSource::Config,
                    name: format!("gateway-{index}"),
                    kind: "openai-compat".into(),
                    status: ProviderStatus::Active,
                    detail: String::new(),
                })
                .collect(),
            selected: 15,
            ..Default::default()
        };
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 18)).unwrap();
        terminal
            .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
            .unwrap();
        assert!(manager.list_offset > 0);
        let (key, rect) = manager.record_hits[0].clone();
        assert_eq!(rect.height, 4);
        assert!(manager.record_hits.len() >= 2);
        let expected = manager
            .providers
            .iter()
            .position(|provider| provider.key() == key)
            .unwrap();
        let click = |button| MouseEvent {
            kind: MouseEventKind::Down(button),
            column: rect.x + 1,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        manager.handle_mouse(&click(MouseButton::Right), None);
        assert!(!manager.show_confirm);
        manager.handle_mouse(&click(MouseButton::Left), None);
        assert_eq!(manager.selected, expected);
        assert!(manager.details);
        assert!(manager.record_hits.is_empty());
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
    fn config_form_env_horizontal_keys_edit_and_test_row_vertical_keys_change_fields() {
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.form_field = 3;
        let original_cursor = manager.api_key_env_editor.cursor();

        manager.handle_key(&KeyAction::CursorLeft, None);
        assert_eq!(manager.form_field, 3);
        assert!(manager.api_key_env_editor.cursor() < original_cursor);

        manager.form_field = 7;
        manager.handle_key(&KeyAction::HistoryUp, None);
        assert_eq!(manager.form_field, 10);
        manager.form_field = 7;
        manager.handle_key(&KeyAction::HistoryDown, None);
        assert_eq!(manager.form_field, 9);
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
        manager.base_url_editor.replace_with("");

        manager.commit_form(Some(&tx));
        let failed_request = receive_mutation(&mut rx);
        assert!(matches!(
            &failed_request.action,
            crate::ProviderMutation::UpsertConfig {
                name,
                base_url,
                create: true,
                ..
            } if name == "gateway" && base_url.is_empty()
        ));
        assert!(manager.in_form);
        assert!(manager.show_add);
        assert_eq!(manager.pending_mutation.as_ref(), Some(&failed_request));
        manager.form_field = 2;
        assert!(!manager.has_text_focus());
        <ProviderManager as crate::wm::modal::ModalOverlay>::handle_paste(
            &mut manager,
            "lost text",
        );
        manager.handle_key(&KeyAction::Char('x'), Some(&tx));
        assert_eq!(manager.api_key_editor.buf(), "test-key");

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
        let crate::TuiControl::TestProviderConnection(crate::ProviderConnectionTestRequest {
            name,
            entry,
            request_id,
        }) = rx.try_recv().unwrap()
        else {
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

        assert!(manager.resolve_test(request_id, &Ok("Connected".into())));
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
        let crate::TuiControl::TestProviderConnection(crate::ProviderConnectionTestRequest {
            name,
            entry,
            request_id,
        }) = rx.try_recv().unwrap()
        else {
            panic!("expected provider test");
        };
        assert!(manager.resolve_test(request_id, &Ok("Connected".into())));
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
            Ok(crate::TuiControl::TestProviderConnection(_))
        ));
        assert_eq!(manager.take_feedback(), Some(ProviderFeedback::TestStarted));
    }

    #[test]
    fn rendering_a_form_clears_an_obsolete_test_hit_region() {
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.test_btn_rect = Some(Rect::new(4, 3, 6, 1));
        let app = crate::app::AppState::new("session".into(), None);
        manager.list_rect = Some(Rect::new(0, 0, 40, 20));
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
        assert!(manager.list_rect.is_none());
    }

    #[test]
    fn api_controls_share_visible_focus_and_mouse_order_without_losing_chat_format() {
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, None);
        manager.form_field = 8;
        manager.handle_add_key(&KeyAction::CursorRight, None);
        assert_eq!(
            manager.selected_api(),
            Some(atman_runtime::provider::OpenAiApi::Responses)
        );
        assert!(!manager.form_fields().contains(&5));
        assert_eq!(manager.reasoning_format_editor.buf(), "thinking-toggle");

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 30)).unwrap();
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
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
        assert!(!manager.form_hits.iter().any(|(field, _)| *field == 5));
        let (_, field_hit) = manager
            .form_hits
            .iter()
            .find(|(field, _)| *field == 8)
            .copied()
            .unwrap();
        let (_, right) = field_hit.arrows.unwrap();
        manager.handle_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: right.x,
                row: right.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            None,
        );
        assert_eq!(
            manager.selected_api(),
            Some(atman_runtime::provider::OpenAiApi::ChatCompletions)
        );
        assert!(manager.form_fields().contains(&5));
        assert_eq!(manager.reasoning_format_editor.buf(), "thinking-toggle");
        manager.provider_type_editor.replace_with("anthropic");
        assert!(!manager.form_fields().contains(&8));
        assert!(!manager.form_fields().contains(&5));
    }

    #[test]
    fn provider_form_uses_rendered_field_geometry_and_separate_action_hits() {
        use crate::wm::modal::ModalOverlay;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        for (width, row_height) in [(52, 4), (100, 3)] {
            let mut manager = ProviderManager::default();
            populate_config_form(&mut manager, None);
            manager.form_field = 2;
            manager.api_key_editor.replace_with(&"key-".repeat(30));
            let app = crate::app::AppState::new("session".into(), None);
            let theme = crate::theme::theme();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 36)).unwrap();
            terminal
                .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
                .unwrap();

            let (_, field) = manager
                .form_hits
                .iter()
                .find(|(field, _)| *field == 2)
                .copied()
                .unwrap();
            assert_eq!(field.record.height, row_height);
            assert!(field.offset > 0);
            manager.handle_mouse(
                &MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: field.record.x + 1,
                    row: field.record.y,
                    modifiers: KeyModifiers::NONE,
                },
                None,
            );
            assert_eq!(manager.hovered_field, Some(2));
            manager.handle_mouse(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: field.value.x + 2,
                    row: field.value.y,
                    modifiers: KeyModifiers::NONE,
                },
                None,
            );
            assert_eq!(
                manager.api_key_editor.cursor_display_col(),
                field.offset + 2
            );

            let test = manager.test_btn_rect.unwrap();
            let save = manager.save_rect.unwrap();
            assert_eq!(test.height, 3);
            assert_eq!(save.height, 3);
            assert!(test.right() < save.x);
            assert!(
                manager
                    .form_hits
                    .iter()
                    .all(|(index, field)| { *index != 7 && field.record.bottom() <= test.y })
            );
            manager.handle_mouse(
                &MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: save.x + 1,
                    row: save.y,
                    modifiers: KeyModifiers::NONE,
                },
                None,
            );
            assert_eq!(manager.hovered_field, Some(usize::MAX));
            manager.form_field = 7;
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            manager.handle_mouse(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: save.x + 1,
                    row: save.y + 2,
                    modifiers: KeyModifiers::NONE,
                },
                Some(&tx),
            );
            assert!(matches!(
                receive_mutation(&mut rx).action,
                crate::ProviderMutation::UpsertConfig { .. }
            ));
        }
    }

    #[test]
    fn config_edit_requires_explicit_save_and_failure_keeps_form() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, Some("gateway"));

        manager.handle_add_key(&KeyAction::Submit, Some(&tx));
        assert!(rx.try_recv().is_err());
        manager.handle_add_key(&KeyAction::Save, Some(&tx));

        let request = receive_mutation(&mut rx);
        assert!(matches!(
            request.action,
            crate::ProviderMutation::UpsertConfig {
                ref name,
                create: false,
                ..
            } if name == "gateway"
        ));
        assert!(manager.in_form);
        assert_eq!(
            manager.resolve_mutation(&request, &Err("write failed".into())),
            ProviderMutationResolution::Failed
        );
        assert!(manager.in_form);
        assert_eq!(manager.editing_provider.as_deref(), Some("gateway"));
    }

    #[test]
    fn config_edit_keeps_name_and_hidden_max_tokens() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut manager = ProviderManager::default();
        populate_config_form(&mut manager, Some("gateway"));
        manager.form_max_tokens = Some(16_384);

        manager.handle_add_key(&KeyAction::BackTab, Some(&tx));
        assert_eq!(manager.form_field, 9);
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
        assert!(manager.pending_mutation.is_none());
        manager.handle_add_key(&KeyAction::Save, Some(tx));
        name
    }

    #[test]
    fn oauth_login_waits_for_the_matching_success_before_showing_details() {
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
        assert!(manager.open);
        assert!(!manager.show_add);
        assert!(manager.details);
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

        manager.handle_add_key(&KeyAction::Save, Some(&tx));
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
            confirm_kind: Some(ConfirmKind::Remove),
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
        manager.confirm_kind = Some(ConfirmKind::Remove);
        manager.show_confirm = true;
        manager.execute_confirm(Some(&closed_tx));
        assert_eq!(manager.confirm_provider_id.as_deref(), Some("auth-id"));
        assert_eq!(manager.confirm_kind, Some(ConfirmKind::Remove));
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
                    api: None,
                    max_tools: None,
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
                    api: None,
                    max_tools: None,
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
