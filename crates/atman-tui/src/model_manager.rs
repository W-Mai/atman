use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::model_browser::{BrowserRow, BrowserRowKind, ModelBrowser};

fn reasoning_label(selection: &atman_runtime::provider::ReasoningSelection) -> String {
    selection.to_string()
}

fn parse_reasoning(value: &str) -> Option<atman_runtime::provider::ReasoningSelection> {
    value.parse().ok()
}

#[derive(Default)]
pub struct ModelManager {
    pub open: bool,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    provider_idx: usize,
    model_idx: Vec<usize>,
    browser: ModelBrowser,
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
    form_api: Option<atman_runtime::provider::OpenAiApi>,
    form_hits: Vec<(usize, Rect, usize)>,
    selector_hits: Vec<(usize, Rect, Rect)>,
    hovered_field: Option<usize>,
    test_rect: Option<Rect>,
    pending_test: Option<PendingModelTest>,
    test_result: Option<(String, bool)>,
    next_request_id: u64,
    pending: Option<crate::ModelMutationRequest>,
    confirm_delete: Option<String>,
    list_rect: Option<Rect>,
    save_rect: Option<Rect>,
    confirm_yes_rect: Option<Rect>,
    confirm_no_rect: Option<Rect>,
}

struct PendingModelTest {
    request_id: u64,
    _cancel_on_drop: tokio_util::sync::DropGuard,
}

impl ModelManager {
    fn inputs_locked(&self) -> bool {
        self.pending.is_some() || self.pending_test.is_some()
    }

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
        self.confirm_delete = None;
        self.pending_test = None;
        self.test_result = None;
    }

    pub fn has_text_focus(&self) -> bool {
        self.show_form && !self.inputs_locked() && matches!(self.form_field, 0 | 1 | 3 | 4 | 5)
    }

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if self.inputs_locked() {
            return;
        }
        let point = (event.column, event.row);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if rect_hit(self.confirm_yes_rect, point) => {
                self.handle_key(&KeyAction::Submit, control_tx);
            }
            MouseEventKind::Down(MouseButton::Left) if rect_hit(self.confirm_no_rect, point) => {
                self.handle_key(&KeyAction::Escape, control_tx);
            }
            MouseEventKind::Down(MouseButton::Left) if rect_hit(self.test_rect, point) => {
                self.form_field = 7;
                self.test_model(control_tx);
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.selector_hits.iter().any(|(_, left, right)| {
                    rect_hit(Some(*left), point) || rect_hit(Some(*right), point)
                }) =>
            {
                let (field, left, _) = self
                    .selector_hits
                    .iter()
                    .find(|(_, left, right)| {
                        rect_hit(Some(*left), point) || rect_hit(Some(*right), point)
                    })
                    .copied()
                    .unwrap();
                self.form_field = field;
                let action = if rect_hit(Some(left), point) {
                    KeyAction::CursorLeft
                } else {
                    KeyAction::CursorRight
                };
                self.handle_form_key(&action, control_tx);
            }
            MouseEventKind::Moved => {
                self.hovered_field = self
                    .form_hits
                    .iter()
                    .find(|(_, rect, _)| rect_hit(Some(*rect), point))
                    .map(|(field, _, _)| *field);
            }
            MouseEventKind::Down(MouseButton::Left)
                if self.show_form
                    && self
                        .form_hits
                        .iter()
                        .any(|(_, rect, _)| rect_hit(Some(*rect), point)) =>
            {
                let (field, rect, offset) = self
                    .form_hits
                    .iter()
                    .find(|(_, rect, _)| rect_hit(Some(*rect), point))
                    .copied()
                    .unwrap();
                self.form_field = field;
                if matches!(field, 2 | 6) {
                    let action = if event.column < rect.x + rect.width / 2 {
                        KeyAction::CursorLeft
                    } else {
                        KeyAction::CursorRight
                    };
                    self.handle_form_key(&action, control_tx);
                } else if let Some(editor) = self.form_editor_mut() {
                    editor.set_cursor_by_display(
                        0,
                        event
                            .column
                            .saturating_sub(rect.x + 18)
                            .saturating_add(offset as u16),
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Left) if rect_hit(self.save_rect, point) => {
                self.commit_form(control_tx);
            }
            MouseEventKind::Down(button) if rect_hit(self.list_rect, point) => {
                let rect = self.list_rect.expect("matched list rect");
                if self
                    .browser
                    .select_visible_row(usize::from(event.row.saturating_sub(rect.y)))
                {
                    self.apply_browser_selection();
                    if button == MouseButton::Right {
                        self.handle_key(&KeyAction::Char('d'), control_tx);
                    }
                }
            }
            _ => {}
        }
    }

    fn is_config_model(name: &str) -> bool {
        atman_runtime::config_hub::ConfigHub::global()
            .and_then(|hub| hub.model_config())
            .ok()
            .flatten()
            .is_some_and(|config| config.models.contains_key(name))
    }

    fn begin_mutation(
        &mut self,
        action: crate::ModelMutation,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> bool {
        if self.pending.is_some() {
            return false;
        }
        let Some(tx) = control_tx else {
            atman_runtime::notify!(error, "Model operation is unavailable");
            return false;
        };
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let request = crate::ModelMutationRequest {
            request_id: self.next_request_id,
            action,
        };
        if tx
            .send(crate::TuiControl::MutateModel(request.clone()))
            .is_err()
        {
            atman_runtime::notify!(error, "Model operation is unavailable");
            return false;
        }
        self.pending = Some(request);
        true
    }

    pub(crate) fn resolve_mutation(
        &mut self,
        request: &crate::ModelMutationRequest,
        result: &Result<crate::ModelMutationSuccess, String>,
    ) -> bool {
        if self.pending.as_ref() != Some(request) {
            return false;
        }
        self.pending = None;
        let matches = matches!(
            (&request.action, result),
            (
                crate::ModelMutation::Upsert { name, .. },
                Ok(crate::ModelMutationSuccess::Saved { name: saved })
            ) if name == saved
        ) || matches!(
            (&request.action, result),
            (
                crate::ModelMutation::Remove { name },
                Ok(crate::ModelMutationSuccess::Removed { name: removed })
            ) if name == removed
        );
        if !matches {
            return true;
        }
        match &request.action {
            crate::ModelMutation::Upsert { .. } => {
                self.show_form = false;
                self.editing = None;
            }
            crate::ModelMutation::Remove { .. } => {
                self.confirm_delete = None;
            }
        }
        self.refresh();
        true
    }

    pub fn refresh(&mut self) {
        let selected_provider = self.current_provider().to_string();
        let selected_model = self.current_model().map(|model| model.slug.clone());
        let enabled_providers = atman_runtime::model_registry::enabled_provider_names();
        self.groups = atman_runtime::model_registry::all_provider_groups_with_empty()
            .into_iter()
            .filter(|g| enabled_providers.contains(&g.provider_name))
            .collect();
        self.provider_idx = self
            .groups
            .iter()
            .position(|group| group.provider_name == selected_provider)
            .unwrap_or(0);
        self.model_idx = vec![0; self.groups.len()];
        if let (Some(group), Some(selected_model)) =
            (self.groups.get(self.provider_idx), selected_model)
        {
            self.model_idx[self.provider_idx] = group
                .models
                .iter()
                .position(|model| model.slug == selected_model)
                .unwrap_or(0);
        }
        self.sync_browser();
    }

    fn sync_browser(&mut self) {
        let mut rows = Vec::new();
        for group in &self.groups {
            rows.push(BrowserRow {
                kind: BrowserRowKind::Provider,
                label: atman_runtime::model_registry::provider_display_name(&group.provider_name),
                value: group.provider_name.clone(),
                selectable: false,
            });
            rows.extend(group.models.iter().map(|model| BrowserRow {
                kind: BrowserRowKind::Model,
                label: model.slug.clone(),
                value: format!("{}\0{}", group.provider_name, model.slug),
                selectable: true,
            }));
        }
        let selected = self
            .groups
            .get(self.provider_idx)
            .and_then(|group| {
                group
                    .models
                    .get(self.model_idx.get(self.provider_idx).copied().unwrap_or(0))
            })
            .map(|model| format!("{}\0{}", self.current_provider(), model.slug));
        self.browser.replace_rows(rows, selected.as_deref());
    }

    fn apply_browser_selection(&mut self) {
        let Some(row) = self.browser.selected() else {
            return;
        };
        let Some((provider, slug)) = row.value.split_once('\0') else {
            return;
        };
        if let Some(pi) = self
            .groups
            .iter()
            .position(|group| group.provider_name == provider)
        {
            if let Some(mi) = self.groups[pi]
                .models
                .iter()
                .position(|model| model.slug == slug)
            {
                self.provider_idx = pi;
                self.model_idx[pi] = mi;
            }
        }
    }

    fn select_provider(&mut self, name: &str) {
        if let Some(idx) = self.groups.iter().position(|g| g.provider_name == name) {
            self.provider_idx = idx;
            self.sync_browser();
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

    fn provider_entry(&self) -> Option<atman_runtime::model_registry::ProviderEntry> {
        atman_runtime::model_registry::all_provider_entries()
            .into_iter()
            .find(|(name, _)| name == &self.selected_provider)
            .map(|(_, entry)| entry)
    }

    fn api_visible(&self) -> bool {
        self.provider_entry()
            .is_some_and(|entry| entry.supports_api_selection())
    }

    fn api_label(&self) -> String {
        if let Some(api) = self.form_api {
            return api.to_string();
        }
        format!(
            "Inherit ({})",
            self.provider_entry()
                .and_then(|entry| entry.effective_api(None))
                .unwrap_or_default()
        )
    }

    fn reasoning_choices(&self) -> Vec<String> {
        atman_runtime::model_registry::reasoning_selections_for_model_draft(
            self.editing.as_deref(),
            &self.selected_provider,
            if self.model_editor.buf().trim().is_empty() {
                self.name_editor.buf().trim()
            } else {
                self.model_editor.buf().trim()
            },
            self.form_api,
            self.max_tokens_editor.buf().trim().parse().ok(),
        )
        .into_iter()
        .map(|selection| selection.to_string())
        .collect()
    }

    fn form_fields(&self) -> Vec<usize> {
        let mut fields = vec![0, 1];
        if self.locked_provider.is_none() {
            fields.push(2);
        }
        if self.api_visible() {
            fields.push(6);
        }
        fields.extend([3, 4, 5, 7]);
        fields
    }

    fn form_editor_mut(&mut self) -> Option<&mut InputEditor> {
        match self.form_field {
            0 => Some(&mut self.name_editor),
            1 => Some(&mut self.model_editor),
            3 => Some(&mut self.context_budget_editor),
            4 => Some(&mut self.thinking_editor),
            5 => Some(&mut self.max_tokens_editor),
            _ => None,
        }
    }

    fn test_model(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.inputs_locked() {
            return;
        }
        let model = self.model_editor.buf().trim();
        let model = if model.is_empty() {
            self.name_editor.buf().trim()
        } else {
            model
        };
        if model.is_empty() || self.selected_provider.is_empty() {
            atman_runtime::notify!(error, "Select a provider and model before testing");
            return;
        }
        let Some(reasoning) = parse_reasoning(self.thinking_editor.buf()) else {
            atman_runtime::notify!(error, "Reasoning value is invalid");
            return;
        };
        let max_tokens = if self.max_tokens_editor.buf().trim().is_empty() {
            None
        } else {
            match self.max_tokens_editor.buf().trim().parse::<u32>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    atman_runtime::notify!(error, "Max Tokens must be a positive integer");
                    return;
                }
            }
        };
        let Some(tx) = control_tx else {
            atman_runtime::notify!(error, "Model test is unavailable");
            return;
        };
        self.next_request_id = self.next_request_id.wrapping_add(1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let request = crate::ModelTestRequest {
            request_id: self.next_request_id,
            identity: self.editing.clone(),
            provider: self.selected_provider.clone(),
            model: model.into(),
            api: self.form_api,
            reasoning,
            max_tokens,
            cancel: cancel.clone(),
        };
        if tx.send(crate::TuiControl::TestModel(request)).is_err() {
            atman_runtime::notify!(error, "Model test is unavailable");
            return;
        }
        self.pending_test = Some(PendingModelTest {
            request_id: self.next_request_id,
            _cancel_on_drop: cancel.drop_guard(),
        });
        self.test_result = None;
    }

    pub(crate) fn resolve_test(
        &mut self,
        request_id: u64,
        result: &Result<String, String>,
    ) -> bool {
        if self.pending_test.as_ref().map(|test| test.request_id) != Some(request_id) {
            return false;
        }
        self.pending_test = None;
        self.test_result = Some(match result {
            Ok(message) => (message.clone(), true),
            Err(message) => (message.clone(), false),
        });
        true
    }

    fn open_form(&mut self) {
        self.show_form = true;
        self.form_field = 0;
        self.editing = None;
        self.form_api = None;
        self.test_result = None;
        self.selected_provider = self.current_provider().to_string();
        self.name_editor = InputEditor::default();
        self.model_editor = InputEditor::default();
        self.context_budget_editor = InputEditor::default();
        self.thinking_editor = InputEditor::default();
        self.thinking_editor.insert_str("default");
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
        self.form_api = entry.api;
        self.test_result = None;
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
            .insert_str(&reasoning_label(&model.reasoning));
        self.max_tokens_editor = InputEditor::default();
        if let Some(mt) = entry.max_tokens {
            self.max_tokens_editor.insert_str(&mt.to_string());
        }
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.pending_test.is_some() {
            if matches!(action, KeyAction::Escape) {
                self.close();
            }
            return;
        }
        if self.pending.is_some() {
            return;
        }
        if let Some(name) = self.confirm_delete.clone() {
            match action {
                KeyAction::Submit | KeyAction::Char('y') | KeyAction::Char('Y') => {
                    self.begin_mutation(crate::ModelMutation::Remove { name }, control_tx);
                }
                KeyAction::Escape | KeyAction::Char('n') | KeyAction::Char('N') => {
                    self.confirm_delete = None;
                }
                _ => {}
            }
            return;
        }
        if self.show_form {
            self.handle_form_key(action, control_tx);
            return;
        }
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp
            | KeyAction::HistoryDown
            | KeyAction::Char('j')
            | KeyAction::Char('k')
            | KeyAction::PageUp
            | KeyAction::PageDown
            | KeyAction::Home
            | KeyAction::End => {
                self.browser.handle_key(action, 0);
                self.apply_browser_selection();
            }
            KeyAction::Char('n') => self.open_form(),
            KeyAction::Char('d') => {
                if let Some(name) = self.current_model().map(|model| model.slug.clone()) {
                    if Self::is_config_model(&name) {
                        self.confirm_delete = Some(name);
                    } else {
                        atman_runtime::notify!(
                            warn,
                            "Discovered and preset models cannot be deleted"
                        );
                    }
                }
            }
            KeyAction::Char('a') => {
                if let Some(m) = self.current_model() {
                    self.open_alias_model = Some(m.slug.clone());
                }
            }
            KeyAction::Submit => {
                if let Some(name) = self.current_model().map(|model| model.slug.clone()) {
                    if Self::is_config_model(&name) {
                        self.open_edit();
                    } else {
                        atman_runtime::notify!(
                            warn,
                            "Discovered and preset models cannot be edited"
                        );
                    }
                }
            }
            _ => {}
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.inputs_locked() || !self.show_form || matches!(self.form_field, 2 | 6 | 7) {
            return;
        }
        let editor = match self.form_field {
            0 => &mut self.name_editor,
            1 => &mut self.model_editor,
            3 => &mut self.context_budget_editor,
            4 => &mut self.thinking_editor,
            5 => &mut self.max_tokens_editor,
            _ => return,
        };
        editor.insert_str(text);
        if !text.is_empty() {
            self.test_result = None;
        }
    }

    fn handle_form_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.inputs_locked() {
            return;
        }
        let fields = self.form_fields();
        let index = fields
            .iter()
            .position(|field| *field == self.form_field)
            .unwrap_or(0);
        match action {
            KeyAction::Submit if self.form_field == 7 => self.test_model(control_tx),
            KeyAction::Escape | KeyAction::Submit => self.commit_form(control_tx),
            KeyAction::Tab | KeyAction::HistoryDown => {
                self.form_field = fields[(index + 1) % fields.len()]
            }
            KeyAction::BackTab | KeyAction::HistoryUp => {
                self.form_field = fields[(index + fields.len() - 1) % fields.len()]
            }
            KeyAction::CursorLeft | KeyAction::CursorRight
                if matches!(self.form_field, 2 | 4 | 6) =>
            {
                let Some(direction) =
                    crate::directional_selector::SelectorDirection::from_key(action)
                else {
                    return;
                };
                match self.form_field {
                    2 => {
                        if self.locked_provider.is_some() {
                            return;
                        }
                        let providers: Vec<_> = self
                            .groups
                            .iter()
                            .map(|group| group.provider_name.clone())
                            .collect();
                        let mut selected = providers
                            .iter()
                            .position(|provider| *provider == self.selected_provider)
                            .unwrap_or(0);
                        if crate::directional_selector::move_wrapped(
                            &mut selected,
                            providers.len(),
                            direction,
                        ) {
                            self.selected_provider = providers[selected].clone();
                            self.test_result = None;
                            if !self.api_visible() {
                                self.form_api = None;
                            }
                        }
                    }
                    6 => {
                        let choices = [
                            None,
                            Some(atman_runtime::provider::OpenAiApi::ChatCompletions),
                            Some(atman_runtime::provider::OpenAiApi::Responses),
                        ];
                        let mut selected = choices
                            .iter()
                            .position(|value| *value == self.form_api)
                            .unwrap_or(0);
                        crate::directional_selector::move_wrapped(
                            &mut selected,
                            choices.len(),
                            direction,
                        );
                        self.form_api = choices[selected];
                        self.test_result = None;
                    }
                    _ => {
                        let choices = self.reasoning_choices();
                        let mut selected = choices
                            .iter()
                            .position(|choice| choice == self.thinking_editor.buf().trim())
                            .unwrap_or(0);
                        if crate::directional_selector::move_wrapped(
                            &mut selected,
                            choices.len(),
                            direction,
                        ) {
                            self.thinking_editor.replace_with(&choices[selected]);
                            self.test_result = None;
                        }
                    }
                }
            }
            _ => {
                if let Some(editor) = self.form_editor_mut() {
                    let before = editor.buf().to_owned();
                    editor.handle_key(action);
                    if editor.buf() != before {
                        self.test_result = None;
                    }
                }
            }
        }
    }

    fn commit_form(
        &mut self,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let name = self.name_editor.buf().trim().to_string();
        if name.is_empty() {
            return;
        }
        let model = self.model_editor.buf().trim().to_string();
        let provider = self.selected_provider.clone();
        let context_budget = if self.context_budget_editor.buf().trim().is_empty() {
            32768
        } else {
            let Ok(value) = self.context_budget_editor.buf().trim().parse() else {
                atman_runtime::notify!(error, "Context Budget must be a positive integer");
                return;
            };
            value
        };
        let Some(reasoning) = parse_reasoning(self.thinking_editor.buf()) else {
            atman_runtime::notify!(error, "Reasoning value is invalid");
            return;
        };
        let max_tokens = if self.max_tokens_editor.buf().trim().is_empty() {
            None
        } else {
            let Ok(value) = self.max_tokens_editor.buf().trim().parse() else {
                atman_runtime::notify!(error, "Max Tokens must be a positive integer");
                return;
            };
            Some(value)
        };
        self.begin_mutation(
            crate::ModelMutation::Upsert {
                old_name: self.editing.clone(),
                name: name.clone(),
                model: if model.is_empty() { name } else { model },
                provider: if provider.is_empty() {
                    None
                } else {
                    Some(provider)
                },
                context_budget,
                reasoning,
                max_tokens,
                api: self.form_api,
                enabled: true,
            },
            control_tx,
        );
    }
}

fn rect_hit(rect: Option<Rect>, point: (u16, u16)) -> bool {
    rect.is_some_and(|rect| {
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    })
}

impl crate::wm::modal::ModalOverlay for ModelManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.list_rect = None;
        self.test_rect = None;
        self.form_hits.clear();
        self.selector_hits.clear();
        self.save_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        if let Some(name) = self.confirm_delete.as_deref() {
            let button_y = area.y + area.height / 2 + 2;
            self.confirm_yes_rect = Some(Rect {
                x: area.x,
                y: button_y,
                width: area.width / 2,
                height: 1,
            });
            self.confirm_no_rect = Some(Rect {
                x: area.x + area.width / 2,
                y: button_y,
                width: area.width - area.width / 2,
                height: 1,
            });
            render_delete_confirm(f, area, "model", name, t);
            return;
        }
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
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
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

        self.browser.set_visible_rows(inner_left.height as usize);
        let mut lines: Vec<Line> = Vec::new();
        for index in self.browser.visible_rows(inner_left.height as usize) {
            let row = &self.browser.rows()[index];
            let selected = index == self.browser.selected_index();
            let style = if selected {
                Style::default()
                    .fg(t.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else if row.kind == BrowserRowKind::Provider {
                Style::default()
                    .fg(t.heading.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(t.tinted_fg.into())
            };
            if row.kind == BrowserRowKind::Provider {
                let count = self
                    .groups
                    .iter()
                    .find(|group| group.provider_name == row.value)
                    .map(|group| group.models.len())
                    .unwrap_or(0);
                lines.push(Line::from(Span::styled(
                    format!(" {} [{} models]", row.label, count),
                    style,
                )));
            } else if let Some((provider, slug)) = row.value.split_once('\0') {
                let suffix = self
                    .groups
                    .iter()
                    .find(|group| group.provider_name == provider)
                    .and_then(|group| group.models.iter().find(|model| model.slug == slug))
                    .map(|model| {
                        let thinking = if model.thinking { " 🧠" } else { "" };
                        format!(
                            "  {}{}",
                            atman_runtime::humanize::format_count(model.context_budget),
                            thinking
                        )
                    })
                    .unwrap_or_default();
                lines.push(Line::from(Span::styled(
                    format!("   {slug}{suffix}"),
                    style,
                )));
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                " No models. Press n to add one.",
                Style::default().fg(t.meta_fg.into()),
            )));
        }
        f.render_widget(Paragraph::new(lines), inner_left);

        crate::wm::shell::render_section_header(f, right_col, Line::from("Details"), t);
        let inner_right = Rect {
            x: right_col.x,
            y: right_col.y + 2,
            width: right_col.width,
            height: right_col.height.saturating_sub(2).saturating_sub(1),
        };
        let mut detail_lines = vec![];
        if let Some(m) = self.current_model() {
            let label = Style::default().fg(t.meta_fg.into());
            let val = Style::default().fg(t.accent.into());
            detail_lines.push(Line::from(vec![
                Span::styled(" Name:     ", label),
                Span::styled(&m.slug, val),
            ]));
            detail_lines.push(Line::from(vec![
                Span::styled(" Provider: ", label),
                Span::styled(self.current_provider(), val),
            ]));
            detail_lines.push(Line::from(vec![
                Span::styled(" Budget:   ", label),
                Span::styled(atman_runtime::humanize::format_count(m.context_budget), val),
            ]));
            detail_lines.push(Line::from(vec![
                Span::styled(" Reasoning:", label),
                Span::styled(format!(" {}", reasoning_label(&m.reasoning)), val),
            ]));
            let slug = m.slug.clone();
            let entries = atman_runtime::model_registry::all_model_entries();
            if let Some((_, provider)) = atman_runtime::model_registry::all_provider_entries()
                .into_iter()
                .find(|(name, _)| name == self.current_provider())
            {
                let api = entries
                    .iter()
                    .find(|(name, _)| *name == slug)
                    .and_then(|(_, entry)| entry.api);
                if let Some(effective) = provider.effective_api(api) {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" API:      ", label),
                        Span::styled(
                            if api.is_some() {
                                effective.to_string()
                            } else {
                                format!("Inherit ({effective})")
                            },
                            val,
                        ),
                    ]));
                }
                if let Some(endpoint) = provider.api_endpoint(api) {
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Endpoint: ", label),
                        Span::styled(endpoint, val),
                    ]));
                }
            }
            if let Some((_, e)) = entries.iter().find(|(n, _)| *n == slug) {
                if e.model != slug && !e.model.is_empty() {
                    let model_id = e.model.clone();
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Model ID: ", label),
                        Span::styled(model_id, val),
                    ]));
                }
                if let Some(mt) = e.max_tokens {
                    let mt_str = mt.to_string();
                    detail_lines.push(Line::from(vec![
                        Span::styled(" Max Out:  ", label),
                        Span::styled(mt_str, val),
                    ]));
                }
            }
        } else {
            detail_lines.push(Line::from(Span::styled(
                " Select a model to view details",
                Style::default().fg(t.meta_fg.into()),
            )));
        }
        f.render_widget(Paragraph::new(detail_lines), inner_right);

        crate::wm::shell::render_column_divider(
            f,
            columns[0].right(),
            columns[0].y,
            columns[0].height,
            t,
        );

        let footer = Paragraph::new(Line::from(Span::styled(
            crate::directional_selector::footer_help("n:add  a:alias  Enter:edit", "Esc:close"),
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
        self.handle_key(action, tx);
        if let Some(model) = self.open_alias_model.take() {
            return Some(ModalAction::OpenAliasForModel(model));
        }
        Some(ModalAction::Consumed)
    }

    fn handle_paste(&mut self, text: &str) {
        self.paste(text);
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
        let fields = self.form_fields();
        if !fields.contains(&self.form_field) {
            self.form_field = fields[0];
        }
        let selected = fields
            .iter()
            .position(|field| *field == self.form_field)
            .unwrap_or(0);
        let available = inner.height.saturating_sub(5) as usize;
        let start = selected.saturating_sub(available.saturating_sub(1));
        let mut cursor_pos = None;
        for (row, field) in fields
            .iter()
            .skip(start)
            .take(available)
            .copied()
            .enumerate()
        {
            let active = field == self.form_field;
            let (label, value, cursor) = match field {
                0 => (
                    "Name",
                    self.name_editor.buf().to_owned(),
                    Some(self.name_editor.cursor_display_col()),
                ),
                1 => (
                    "Model ID",
                    self.model_editor.buf().to_owned(),
                    Some(self.model_editor.cursor_display_col()),
                ),
                2 => ("Provider", self.selected_provider.clone(), None),
                3 => (
                    "Context Budget",
                    self.context_budget_editor.buf().to_owned(),
                    Some(self.context_budget_editor.cursor_display_col()),
                ),
                4 => (
                    "Reasoning",
                    self.thinking_editor.buf().to_owned(),
                    Some(self.thinking_editor.cursor_display_col()),
                ),
                5 => (
                    "Max Tokens",
                    self.max_tokens_editor.buf().to_owned(),
                    Some(self.max_tokens_editor.cursor_display_col()),
                ),
                6 => ("API", self.api_label(), None),
                7 => (
                    "Test model",
                    if self.pending_test.is_some() {
                        "Testing…".into()
                    } else {
                        "Enter / click · sends a model request".into()
                    },
                    None,
                ),
                _ => continue,
            };
            let rect = Rect {
                y: inner.y + row as u16,
                height: 1,
                ..inner
            };
            let has_arrows = active && matches!(field, 2 | 4 | 6) && rect.width >= 25;
            let width = rect.width.saturating_sub(if has_arrows { 25 } else { 19 }) as usize;
            let offset = if active {
                cursor
                    .unwrap_or_default()
                    .saturating_sub(width.saturating_sub(1))
            } else {
                0
            };
            self.form_hits.push((field, rect, offset));
            if field == 7 {
                self.test_rect = Some(rect);
            }
            let bg = if self.hovered_field == Some(field) {
                t.work_hover_bg.into()
            } else {
                t.modal_bg.into()
            };
            let style = if matches!(field, 2 | 4 | 6) {
                crate::directional_selector::value_style(t, active).bg(bg)
            } else {
                Style::default().bg(bg).fg(if active {
                    t.accent.into()
                } else {
                    t.tinted_fg.into()
                })
            };
            let shown = crate::width::trim_display_offset(&value, offset, width);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        format!(" {label:<17}"),
                        Style::default().fg(t.meta_fg.into()),
                    ),
                    Span::styled(shown, style),
                ]))
                .style(Style::default().bg(bg)),
                rect,
            );
            if has_arrows {
                let left = Rect::new(rect.right() - 5, rect.y, 2, 1);
                let right = Rect::new(rect.right() - 2, rect.y, 2, 1);
                f.render_widget(Paragraph::new("←").style(style), left);
                f.render_widget(Paragraph::new("→").style(style), right);
                self.selector_hits.push((field, left, right));
            }
            if active
                && !self.inputs_locked()
                && let Some(cursor) = cursor
            {
                cursor_pos = Some((
                    (rect.x
                        + 18
                        + cursor.saturating_sub(offset).min(width.saturating_sub(1)) as u16)
                        .min(rect.right().saturating_sub(1)),
                    rect.y,
                ));
            }
        }
        if inner.height >= 5 {
            let endpoint = self
                .provider_entry()
                .and_then(|entry| entry.api_endpoint(self.form_api))
                .unwrap_or_else(|| "Provider-managed endpoint".into());
            let result = self
                .test_result
                .as_ref()
                .map(|(message, _)| message.as_str())
                .unwrap_or("Test validates API, reasoning and tool schema; no tools are executed.");
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!(" {} · {endpoint}", self.selected_provider)),
                    Line::from(format!(" {result}")),
                ])
                .style(Style::default().fg(t.meta_fg.into())),
                Rect {
                    y: inner.bottom() - 4,
                    height: 2,
                    ..inner
                },
            );
            self.save_rect = Some(Rect {
                y: inner.bottom() - 1,
                height: 1,
                ..inner
            });
            f.render_widget(
                Paragraph::new(if self.pending_test.is_some() {
                    " Testing model… inputs locked · Esc: close without saving"
                } else {
                    " Tab / Shift+Tab: field · ← →: select · Enter: save / test · Esc: save"
                })
                .style(Style::default().fg(t.meta_fg.into())),
                self.save_rect.unwrap(),
            );
        }
        if let Some((x, y)) = cursor_pos {
            f.set_cursor_position((x, y));
        }
    }
}

fn render_delete_confirm(
    f: &mut ratatui::Frame,
    area: Rect,
    kind: &str,
    name: &str,
    theme: &crate::theme::Theme,
) {
    let w = area.width.saturating_sub(4).clamp(40, 60);
    let h = 9u16;
    let dlg = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        dlg,
        Line::from(" Delete? "),
        "⚠",
        theme.warn.into(),
        true,
        theme,
    );
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Delete {kind} \"{name}\"?"),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "This change cannot be undone.",
                Style::default().fg(theme.meta_fg.into()),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "  y / Enter: confirm    n / Esc: cancel",
                Style::default().fg(theme.meta_fg.into()),
            )),
        ])
        .alignment(ratatui::layout::Alignment::Center),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_visibility_tracks_only_editable_model_fields() {
        let mut manager = ModelManager {
            show_form: true,
            ..Default::default()
        };
        for field in [0, 1, 3, 4, 5] {
            manager.form_field = field;
            assert!(manager.has_text_focus());
        }
        for field in [2, 6, 7] {
            manager.form_field = field;
            assert!(!manager.has_text_focus());
        }
    }

    #[test]
    fn model_test_sends_a_draft_and_accepts_only_its_matching_result() {
        let mut manager = ModelManager::default();
        manager.open_form();
        manager.name_editor.replace_with("demo");
        manager.model_editor.replace_with("vendor/model");
        manager.selected_provider = "gateway".into();
        manager.form_api = Some(atman_runtime::provider::OpenAiApi::Responses);
        manager.thinking_editor.replace_with("high");
        manager.form_field = 7;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.handle_form_key(&KeyAction::Submit, Some(&tx));
        let crate::TuiControl::TestModel(request) = rx.try_recv().unwrap() else {
            panic!("expected model test");
        };
        assert_eq!(request.model, "vendor/model");
        assert_eq!(request.api, manager.form_api);
        assert!(manager.pending.is_none());
        manager.form_field = 1;
        manager.handle_key(&KeyAction::Char('x'), Some(&tx));
        assert_eq!(manager.model_editor.buf(), "vendor/model");
        assert!(!manager.resolve_test(request.request_id + 1, &Ok("wrong".into())));
        assert!(manager.resolve_test(request.request_id, &Err("rejected".into())));
        assert!(manager.show_form);
        assert_eq!(manager.thinking_editor.buf(), "high");
        assert!(manager.test_result.is_some());
        manager.form_field = 1;
        manager.handle_key(&KeyAction::CursorLeft, Some(&tx));
        assert!(manager.test_result.is_some());
        manager.handle_key(&KeyAction::Char('x'), Some(&tx));
        assert!(manager.test_result.is_none());
        manager.form_field = 7;
        manager.save_rect = Some(Rect::new(0, 10, 80, 1));
        manager.handle_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 1,
                row: 10,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Some(&tx),
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::TuiControl::MutateModel(_))
        ));
    }

    #[test]
    fn closing_or_dropping_model_form_cancels_its_test_and_ignores_late_results() {
        let mut manager = ModelManager::default();
        manager.open_form();
        manager.name_editor.replace_with("demo");
        manager.selected_provider = "gateway".into();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.test_model(Some(&tx));
        let crate::TuiControl::TestModel(first) = rx.try_recv().unwrap() else {
            panic!("expected model test");
        };
        assert!(!first.cancel.is_cancelled());
        manager.handle_key(&KeyAction::Escape, Some(&tx));
        assert!(first.cancel.is_cancelled());
        manager.open_form();
        manager.name_editor.replace_with("demo");
        manager.selected_provider = "gateway".into();
        manager.test_model(Some(&tx));
        let crate::TuiControl::TestModel(second) = rx.try_recv().unwrap() else {
            panic!("expected model test");
        };
        assert!(!manager.resolve_test(first.request_id, &Ok("late result".into())));
        assert!(manager.test_result.is_none());
        assert!(!second.cancel.is_cancelled());
        drop(manager);
        assert!(second.cancel.is_cancelled());
    }

    #[test]
    fn every_model_edit_invalidates_the_previous_test_result() {
        let mut manager = ModelManager {
            groups: vec![provider_group("alpha"), provider_group("beta")],
            ..Default::default()
        };
        manager.open_form();
        for field in [0, 1, 3, 4, 5] {
            manager.form_field = field;
            manager.test_result = Some(("accepted".into(), true));
            manager.paste("x");
            assert!(manager.test_result.is_none());
        }
        for field in [2, 4, 6] {
            manager.form_field = field;
            manager.test_result = Some(("accepted".into(), true));
            manager.handle_form_key(&KeyAction::CursorRight, None);
            assert!(manager.test_result.is_none());
        }
    }

    #[test]
    fn rendered_reasoning_arrows_select_and_text_region_places_the_cursor() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        let mut manager = ModelManager::default();
        manager.open_form();
        manager.selected_provider = "unknown".into();
        manager.form_field = 4;
        manager.thinking_editor.replace_with("default");
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| {
                <ModelManager as crate::wm::modal::ModalOverlay>::render_content(
                    &mut manager,
                    frame,
                    frame.area(),
                    &app,
                    &theme,
                )
            })
            .unwrap();
        let (_, left, right) = manager
            .selector_hits
            .iter()
            .find(|(field, _, _)| *field == 4)
            .copied()
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(right.x, right.y)].symbol(),
            "→"
        );
        assert_eq!(terminal.backend().buffer()[(left.x, left.y)].symbol(), "←");
        let click = |rect: Rect| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        manager.handle_mouse(&click(right), None);
        assert_eq!(manager.thinking_editor.buf(), "off");
        manager.handle_mouse(&click(left), None);
        assert_eq!(manager.thinking_editor.buf(), "default");
        let (_, field, _) = manager
            .form_hits
            .iter()
            .find(|(field, _, _)| *field == 4)
            .copied()
            .unwrap();
        manager.handle_mouse(&click(Rect::new(field.x + 19, field.y, 1, 1)), None);
        assert_eq!(manager.thinking_editor.cursor(), 1);
        assert_eq!(manager.thinking_editor.buf(), "default");
    }

    #[test]
    fn reasoning_retains_custom_text_entry_and_paste() {
        let mut manager = ModelManager::default();
        manager.open_form();
        manager.form_field = 4;
        manager.thinking_editor.replace_with("budget:");
        manager.paste("8192");
        manager.handle_form_key(&KeyAction::Backspace, None);
        manager.handle_form_key(&KeyAction::Char('4'), None);
        assert_eq!(manager.thinking_editor.buf(), "budget:8194");
        assert!(manager.has_text_focus());
    }

    #[test]
    fn paste_inserts_into_current_text_field() {
        let mut manager = ModelManager::default();
        manager.open_form();

        manager.paste("model-name");
        assert_eq!(manager.name_editor.buf(), "model-name");

        manager.form_field = 3;
        manager.paste("128000");
        assert_eq!(manager.context_budget_editor.buf(), "128000");
    }

    #[test]
    fn empty_catalog_navigation_is_safe() {
        let mut manager = ModelManager::default();

        manager.handle_key(&KeyAction::HistoryUp, None);
        manager.handle_key(&KeyAction::HistoryDown, None);
        manager.handle_key(&KeyAction::Submit, None);

        assert!(manager.current_model().is_none());
    }

    #[test]
    fn modal_overlay_paste_routes_to_model_editor() {
        let mut manager = ModelManager::default();
        manager.open_form();
        <ModelManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "pasted");
        assert_eq!(manager.name_editor.buf(), "pasted");
    }

    #[test]
    fn form_directional_selectors_switch_provider_and_reasoning() {
        let mut manager = ModelManager {
            groups: vec![provider_group("alpha"), provider_group("beta")],
            show_form: true,
            selected_provider: "alpha".to_string(),
            ..Default::default()
        };

        manager.form_field = 2;
        manager.handle_key(&KeyAction::CursorLeft, None);
        assert_eq!(manager.selected_provider, "beta");

        manager.form_field = 4;
        manager.thinking_editor.replace_with("default");
        manager.handle_key(&KeyAction::CursorRight, None);
        assert_eq!(manager.thinking_editor.buf(), "off");
    }

    #[test]
    fn add_form_escape_submits_model() {
        let mut manager = ModelManager::default();
        manager.open_form();
        manager.name_editor.insert_str("vendor/model");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        manager.handle_key(&KeyAction::Escape, Some(&tx));

        assert!(manager.show_form);
        let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
            panic!("unexpected control message");
        };
        assert!(!manager.has_text_focus());
        manager.paste("lost text");
        manager.handle_key(&KeyAction::Char('x'), Some(&tx));
        assert_eq!(manager.name_editor.buf(), "vendor/model");
        assert!(matches!(
            &request.action,
            crate::ModelMutation::Upsert {
                old_name: None,
                name,
                model,
                ..
            } if name == "vendor/model" && model == "vendor/model"
        ));
        assert!(manager.resolve_mutation(
            &request,
            &Ok(crate::ModelMutationSuccess::Saved {
                name: "vendor/model".into()
            })
        ));
        assert!(!manager.show_form);
    }

    #[test]
    fn edit_form_escape_submits_and_failure_keeps_form() {
        let mut manager = ModelManager {
            show_form: true,
            editing: Some("vendor/model".into()),
            selected_provider: "vendor".into(),
            ..Default::default()
        };
        manager.name_editor.insert_str("vendor/model");
        manager.thinking_editor.insert_str("default");
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        manager.handle_key(&KeyAction::Escape, Some(&tx));

        let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
            panic!("unexpected control message");
        };
        assert!(manager.show_form);
        assert!(manager.resolve_mutation(&request, &Err("write failed".into())));
        assert!(manager.show_form);
        assert_eq!(manager.editing.as_deref(), Some("vendor/model"));
    }

    #[test]
    fn delete_failure_keeps_confirmation_for_retry() {
        let mut manager = ModelManager {
            confirm_delete: Some("vendor/model".into()),
            ..Default::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        manager.handle_key(&KeyAction::Submit, Some(&tx));

        let crate::TuiControl::MutateModel(request) = rx.try_recv().unwrap() else {
            panic!("unexpected control message");
        };
        assert!(matches!(
            request.action,
            crate::ModelMutation::Remove { ref name } if name == "vendor/model"
        ));
        assert!(manager.resolve_mutation(&request, &Err("write failed".into())));
        assert_eq!(manager.confirm_delete.as_deref(), Some("vendor/model"));
        assert!(manager.pending.is_none());
    }

    #[test]
    fn delete_confirmation_renders_and_mouse_cancel_closes_it() {
        let mut manager = ModelManager {
            confirm_delete: Some("vendor/model".into()),
            ..Default::default()
        };
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| {
                <ModelManager as crate::wm::modal::ModalOverlay>::render_content(
                    &mut manager,
                    frame,
                    frame.area(),
                    &app,
                    &theme,
                );
            })
            .unwrap();

        let content: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("Delete model \"vendor/model\"?"));
        assert!(manager.list_rect.is_none());
        assert!(manager.save_rect.is_none());
        let cancel = manager.confirm_no_rect.unwrap();
        manager.handle_mouse(
            &crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: cancel.x,
                row: cancel.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            None,
        );
        assert!(manager.confirm_delete.is_none());
    }

    #[test]
    fn reasoning_field_parses_effort_mode_and_budget() {
        assert_eq!(
            parse_reasoning("high@pro"),
            Some(atman_runtime::provider::ReasoningSelection::Effort {
                effort: atman_runtime::provider::ReasoningEffort::High,
                execution_mode: Some(atman_runtime::provider::ReasoningExecutionMode::Pro),
            })
        );
        assert_eq!(
            parse_reasoning("budget:4096"),
            Some(atman_runtime::provider::ReasoningSelection::BudgetTokens { tokens: 4096 })
        );
    }

    fn provider_group(name: &str) -> atman_runtime::model_registry::ProviderGroup {
        atman_runtime::model_registry::ProviderGroup {
            provider_name: name.to_string(),
            models: Vec::new(),
        }
    }
}
