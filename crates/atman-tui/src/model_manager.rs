use crate::wm::modal::ModalAction;

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::model_browser::{BrowserRow, BrowserRowKind, ModelBrowser};

mod view;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
    #[default]
    List,
    Details,
    Actions,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Add,
    Edit,
    Alias,
    Delete,
    All,
    More,
    Back,
    Save,
    Cancel,
    Test,
    Stop,
    Details,
    Confirm,
    Discard,
    Keep,
    Aliases,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Focus {
    Search,
    #[default]
    List,
    Details,
    Action(Action),
}

fn reasoning_label(selection: &atman_runtime::provider::ReasoningSelection) -> String {
    selection.to_string()
}

fn parse_reasoning(value: &str) -> Option<atman_runtime::provider::ReasoningSelection> {
    value.parse().ok()
}

#[derive(Default)]
pub struct ModelManager {
    pub open: bool,
    resume_on_open: bool,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    provider_idx: usize,
    model_idx: Vec<usize>,
    browser: ModelBrowser,
    list_page: usize,
    list_height: u16,
    configured: std::collections::HashSet<(String, String)>,
    search: InputEditor,
    search_hit: Option<crate::settings_form::FieldHit>,
    page: Page,
    return_page: Page,
    focus: Focus,
    narrow: bool,
    record_hits: Vec<(String, Rect)>,
    action_hits: Vec<(Action, Rect)>,
    hovered_record: Option<String>,
    hovered_action: Option<Action>,
    detail_rect: Option<Rect>,
    detail_scroll: usize,
    detail_max_scroll: usize,
    actions_offset: usize,
    draft_snapshot: Vec<String>,
    form_enabled: bool,
    confirm_discard: bool,
    confirm_focus: usize,
    delete_aliases: Vec<String>,
    status: Option<(String, bool)>,
    open_alias_manager: bool,
    pub(crate) show_form: bool,
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
    form_hits: Vec<(usize, crate::settings_form::FieldHit)>,
    form_info_rect: Option<Rect>,
    form_info_scroll: usize,
    form_info_max_scroll: usize,
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
    identity: Option<String>,
    draft: Vec<String>,
    cancel: tokio_util::sync::CancellationToken,
    _cancel_on_drop: tokio_util::sync::DropGuard,
}

impl ModelManager {
    fn clear_test_result(&mut self) {
        self.test_result = None;
        self.form_info_scroll = 0;
    }

    fn inputs_locked(&self) -> bool {
        self.pending.is_some() || self.pending_test.is_some()
    }

    pub fn open(&mut self) {
        self.open = true;
        if std::mem::take(&mut self.resume_on_open) {
            return;
        }
        if !self.show_form && !self.inputs_locked() {
            self.locked_provider = None;
            self.refresh();
        }
    }

    pub fn open_with_provider(&mut self, provider: &str) {
        self.open = true;
        if std::mem::take(&mut self.resume_on_open) {
            return;
        }
        if !self.show_form && !self.inputs_locked() {
            self.locked_provider = Some(provider.to_owned());
            self.page = Page::List;
            self.refresh();
        }
    }

    pub fn close(&mut self) {
        self.resume_on_open = self.inputs_locked() || self.show_form;
        self.open = false;
        self.clear_geometry();
    }

    fn clear_geometry(&mut self) {
        self.record_hits.clear();
        self.action_hits.clear();
        self.form_hits.clear();
        self.search_hit = None;
        self.list_rect = None;
        self.detail_rect = None;
        self.form_info_rect = None;
        self.save_rect = None;
        self.test_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        self.hovered_record = None;
        self.hovered_action = None;
        self.hovered_field = None;
    }

    pub fn has_text_focus(&self) -> bool {
        self.page != Page::Status
            && !self.inputs_locked()
            && !self.confirm_discard
            && self.confirm_delete.is_none()
            && ((self.show_form && matches!(self.form_field, 0 | 1 | 3 | 4 | 5))
                || (!self.show_form && self.focus == Focus::Search))
    }

    fn take_modal_action(&mut self) -> Option<ModalAction> {
        if let Some(model) = self.open_alias_model.take() {
            Some(ModalAction::OpenAliasForModel(model))
        } else if std::mem::take(&mut self.open_alias_manager) {
            Some(ModalAction::OpenAliasManager)
        } else {
            Some(ModalAction::Consumed)
        }
    }

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (event.column, event.row);
        let action = self
            .action_hits
            .iter()
            .find(|(_, rect)| rect_hit(Some(*rect), point))
            .map(|(action, _)| *action);
        if event.kind == MouseEventKind::Moved {
            self.hovered_action = action;
            self.hovered_record = self
                .record_hits
                .iter()
                .find(|(_, rect)| rect_hit(Some(*rect), point))
                .map(|(key, _)| key.clone());
            self.hovered_field = self
                .form_hits
                .iter()
                .find(|(_, hit)| rect_hit(Some(hit.record), point))
                .map(|(field, _)| *field);
            return Some(ModalAction::Consumed);
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            if let Some(action) = action {
                self.dispatch(action, tx);
                return self.take_modal_action();
            }
            if !self.inputs_locked() && !self.confirm_discard && self.confirm_delete.is_none() {
                if self.show_form {
                    if let Some((field, hit)) = self
                        .form_hits
                        .iter()
                        .find(|(_, hit)| rect_hit(Some(hit.record), point))
                        .copied()
                    {
                        self.form_field = field;
                        if let Some((left, right)) = hit.arrows
                            && (rect_hit(Some(left), point) || rect_hit(Some(right), point))
                        {
                            self.handle_form_key(
                                &if rect_hit(Some(left), point) {
                                    KeyAction::CursorLeft
                                } else {
                                    KeyAction::CursorRight
                                },
                                tx,
                            );
                        } else if rect_hit(Some(hit.value), point) {
                            if matches!(field, 2 | 6) {
                                self.handle_form_key(&KeyAction::CursorRight, tx);
                            } else if let Some(editor) = self.form_editor_mut() {
                                editor.set_cursor_by_display(
                                    0,
                                    event
                                        .column
                                        .saturating_sub(hit.value.x)
                                        .saturating_add(hit.offset.min(u16::MAX as usize) as u16),
                                );
                            }
                        }
                    }
                } else if let Some(hit) = self
                    .search_hit
                    .filter(|hit| rect_hit(Some(hit.record), point))
                {
                    self.focus = Focus::Search;
                    self.search.set_cursor_by_display(
                        0,
                        event
                            .column
                            .saturating_sub(hit.value.x)
                            .saturating_add(hit.offset.min(u16::MAX as usize) as u16),
                    );
                } else if let Some(key) = self
                    .record_hits
                    .iter()
                    .find(|(_, rect)| rect_hit(Some(*rect), point))
                    .map(|(key, _)| key.clone())
                {
                    self.select_key(&key);
                    self.dispatch(Action::Details, tx);
                } else if rect_hit(self.detail_rect, point) {
                    self.focus = Focus::Details;
                }
            }
        } else if matches!(
            event.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let up = event.kind == MouseEventKind::ScrollUp;
            if rect_hit(self.form_info_rect, point) {
                self.form_info_scroll = if up {
                    self.form_info_scroll.saturating_sub(1)
                } else {
                    (self.form_info_scroll + 1).min(self.form_info_max_scroll)
                };
            } else if rect_hit(self.detail_rect, point) {
                self.detail_scroll = if up {
                    self.detail_scroll.saturating_sub(1)
                } else {
                    (self.detail_scroll + 1).min(self.detail_max_scroll)
                };
            } else if rect_hit(self.list_rect, point) && !self.inputs_locked() {
                self.navigate(&if up {
                    KeyAction::HistoryUp
                } else {
                    KeyAction::HistoryDown
                });
            } else if self.page == Page::Actions {
                self.handle_key(
                    &if up {
                        KeyAction::HistoryUp
                    } else {
                        KeyAction::HistoryDown
                    },
                    tx,
                );
            } else if self.show_form
                && self
                    .form_hits
                    .iter()
                    .any(|(_, hit)| rect_hit(Some(hit.record), point))
            {
                self.handle_form_key(
                    &if up {
                        KeyAction::BackTab
                    } else {
                        KeyAction::Tab
                    },
                    tx,
                );
            }
        }
        self.take_modal_action()
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
            self.test_result = Some(("Model operation is unavailable".into(), false));
            self.status = self.test_result.clone();
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
            self.test_result = Some(("Model operation is unavailable".into(), false));
            self.status = self.test_result.clone();
            atman_runtime::notify!(error, "Model operation is unavailable");
            return false;
        }
        self.pending = Some(request);
        self.confirm_discard = false;
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
            let message = result
                .as_ref()
                .err()
                .cloned()
                .unwrap_or_else(|| "Unexpected model operation response".into());
            self.status = Some((message.clone(), false));
            if self.show_form {
                self.test_result = Some((message, false));
            }
            return true;
        }
        match &request.action {
            crate::ModelMutation::Upsert { name, .. } => {
                self.show_form = false;
                self.editing = None;
                self.page = Page::Details;
                self.status = Some((format!("Saved model configuration: {name}"), true));
            }
            crate::ModelMutation::Remove { name } => {
                self.confirm_delete = None;
                self.status = Some((format!("Deleted model configuration: {name}"), true));
            }
        }
        self.refresh();
        if let crate::ModelMutation::Upsert { name, provider, .. } = &request.action {
            let key = self.row_key(provider.as_deref().unwrap_or("unknown"), name);
            if !self.browser.rows().iter().any(|row| row.value == key) {
                self.search = InputEditor::default();
                self.sync_browser();
            }
            self.select_key(&key);
            self.status = Some((format!("Saved model configuration: {name}"), true));
        }
        true
    }

    pub fn refresh(&mut self) {
        self.groups = atman_runtime::model_registry::all_provider_groups_for_management();
        self.groups
            .sort_by(|a, b| a.provider_name.cmp(&b.provider_name));
        self.configured = atman_runtime::config_hub::ConfigHub::global()
            .and_then(|hub| hub.model_config())
            .ok()
            .flatten()
            .map(|config| {
                config
                    .models
                    .into_iter()
                    .map(|(name, entry)| (entry.provider.unwrap_or_else(|| "unknown".into()), name))
                    .collect()
            })
            .unwrap_or_default();
        self.model_idx = vec![0; self.groups.len()];
        self.sync_browser();
        if self
            .confirm_delete
            .as_ref()
            .is_some_and(|name| !self.configured.iter().any(|(_, key)| key == name))
        {
            self.confirm_delete = None;
        }
        self.refresh_delete_references();
    }

    pub(crate) fn refresh_delete_references(&mut self) {
        if let Some(name) = &self.confirm_delete {
            self.delete_aliases = Self::aliases_for(name);
            self.confirm_focus = 0;
            self.clear_geometry();
        }
    }

    fn row_key(&self, provider: &str, name: &str) -> String {
        format!(
            "{}\0{}\0{}",
            if self.is_config_model(provider, name) {
                "config"
            } else {
                "catalog"
            },
            provider,
            name
        )
    }

    fn is_config_model(&self, provider: &str, name: &str) -> bool {
        self.configured
            .contains(&(provider.to_owned(), name.to_owned()))
    }

    fn current_editable(&self) -> bool {
        self.current_model()
            .is_some_and(|model| self.is_config_model(self.current_provider(), &model.slug))
    }

    fn sync_browser(&mut self) {
        let previous_header = self
            .browser
            .rows()
            .get(self.browser.selected_index())
            .filter(|row| row.kind == BrowserRowKind::Provider)
            .map(|row| row.value.clone());
        let selected = self
            .browser
            .rows()
            .get(self.browser.selected_index())
            .map(|row| row.value.clone());
        let old_index = self.browser.selected_index();
        let query = self.search.buf().to_lowercase();
        let entries: std::collections::HashMap<_, _> =
            atman_runtime::model_registry::all_model_entries()
                .into_iter()
                .collect();
        let mut rows = Vec::new();
        for group in &self.groups {
            if self
                .locked_provider
                .as_ref()
                .is_some_and(|scope| scope != &group.provider_name)
            {
                continue;
            }
            let display_name =
                atman_runtime::model_registry::provider_display_name(&group.provider_name);
            let group_start = rows.len();
            rows.push(BrowserRow {
                kind: BrowserRowKind::Provider,
                label: display_name.clone(),
                value: group.provider_name.clone(),
                selectable: false,
            });
            for model in &group.models {
                let source = if self.is_config_model(&group.provider_name, &model.slug) {
                    "Configured"
                } else {
                    "Catalog"
                };
                let api_model = entries
                    .get(&model.slug)
                    .filter(|entry| {
                        entry.provider.as_deref().unwrap_or("unknown") == group.provider_name
                    })
                    .map(|entry| entry.model.as_str())
                    .unwrap_or_default();
                if !format!(
                    "{} {} {display_name} {source} {api_model}",
                    model.slug, group.provider_name
                )
                .to_lowercase()
                .contains(&query)
                {
                    continue;
                }
                rows.push(BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: model.slug.clone(),
                    value: self.row_key(&group.provider_name, &model.slug),
                    selectable: true,
                });
            }
            if rows.len() == group_start + 1
                && !query.is_empty()
                && !format!("{} {display_name}", group.provider_name)
                    .to_lowercase()
                    .contains(&query)
            {
                rows.pop();
            }
        }
        let fallback = rows
            .get(old_index.min(rows.len().saturating_sub(1)))
            .map(|row| row.value.clone());
        let selected = selected
            .filter(|key| rows.iter().any(|row| &row.value == key))
            .or(fallback);
        self.browser.replace_rows(rows, selected.as_deref());
        if let Some(index) = self.browser.rows().iter().position(|row| {
            row.kind == BrowserRowKind::Provider
                && Some(row.value.as_str()) == previous_header.as_deref()
        }) {
            self.browser.focus_page(&[index]);
        }
        self.apply_browser_selection();
        self.align_list_page();
        self.clear_geometry();
    }

    fn select_key(&mut self, key: &str) {
        if self.browser.selected().is_none_or(|row| row.value != key) {
            self.status = None;
        }
        self.browser
            .replace_rows(self.browser.rows().to_vec(), Some(key));
        self.apply_browser_selection();
        self.align_list_page();
        self.detail_scroll = 0;
        self.clear_geometry();
    }

    fn apply_browser_selection(&mut self) {
        if let Some(row) = self.browser.rows().get(self.browser.selected_index())
            && row.kind == BrowserRowKind::Provider
        {
            if let Some(index) = self
                .groups
                .iter()
                .position(|group| group.provider_name == row.value)
            {
                self.provider_idx = index;
            }
            return;
        }
        let Some(row) = self.browser.selected() else {
            return;
        };
        let mut parts = row.value.splitn(3, '\0');
        let _ = parts.next();
        let (Some(provider), Some(slug)) = (parts.next(), parts.next()) else {
            return;
        };
        if let Some(pi) = self
            .groups
            .iter()
            .position(|group| group.provider_name == provider)
            && let Some(mi) = self.groups[pi]
                .models
                .iter()
                .position(|model| model.slug == slug)
        {
            self.provider_idx = pi;
            self.model_idx.resize(self.groups.len(), 0);
            self.model_idx[pi] = mi;
        }
    }

    fn current_model(&self) -> Option<&atman_runtime::model_registry::ModelRow> {
        self.browser.selected()?;
        self.groups.get(self.provider_idx).and_then(|group| {
            group
                .models
                .get(self.model_idx.get(self.provider_idx).copied().unwrap_or(0))
        })
    }

    fn current_provider(&self) -> &str {
        self.locked_provider
            .as_deref()
            .or_else(|| {
                self.groups
                    .get(self.provider_idx)
                    .map(|group| group.provider_name.as_str())
            })
            .unwrap_or("")
    }

    fn navigate(&mut self, action: &KeyAction) {
        let before = self.browser.selected_index();
        let previous_page = self.list_page;
        let step = match action {
            KeyAction::HistoryUp | KeyAction::Char('k') => Some(-1),
            KeyAction::HistoryDown | KeyAction::Char('j') => Some(1),
            _ => None,
        };
        if matches!(action, KeyAction::PageUp | KeyAction::PageDown) {
            self.page_by(if matches!(action, KeyAction::PageUp) {
                -1
            } else {
                1
            });
        } else if let Some(step) = step
            && self.list_pages().get(self.list_page).is_some_and(|page| {
                !page
                    .iter()
                    .any(|&index| self.browser.rows()[index].selectable)
            })
        {
            self.page_by(step);
        } else {
            self.browser.handle_key(action, 0);
            self.align_list_page();
            if let Some(step) = step
                && (before == self.browser.selected_index()
                    || self.list_page.abs_diff(previous_page) > 1)
            {
                self.list_page = previous_page;
                self.page_by(step);
            }
        }
        self.apply_browser_selection();
        if before != self.browser.selected_index() {
            self.detail_scroll = 0;
            self.status = None;
        }
        self.clear_geometry();
    }

    fn page_by(&mut self, step: isize) {
        let pages = self.list_pages();
        let next = self
            .list_page
            .saturating_add_signed(step)
            .min(pages.len().saturating_sub(1));
        if self.list_page == next {
            return;
        }
        self.list_page = next;
        if let Some(page) = pages.get(self.list_page) {
            self.browser.focus_page(page);
        }
    }

    fn list_pages(&self) -> Vec<Vec<usize>> {
        self.browser.grouped_pages(self.list_height.max(7), 3, 4)
    }

    fn align_list_page(&mut self) {
        let pages = self.list_pages();
        self.list_page = pages
            .iter()
            .position(|page| page.contains(&self.browser.selected_index()))
            .unwrap_or(0);
    }

    fn filtered_model_count(&self) -> usize {
        self.browser
            .rows()
            .iter()
            .filter(|row| row.kind == BrowserRowKind::Model)
            .count()
    }

    fn draft(&self) -> Vec<String> {
        vec![
            self.name_editor.buf().into(),
            self.model_editor.buf().into(),
            self.selected_provider.clone(),
            self.context_budget_editor.buf().into(),
            self.thinking_editor.buf().into(),
            self.max_tokens_editor.buf().into(),
            format!("{:?}", self.form_api),
            self.form_enabled.to_string(),
        ]
    }

    fn aliases_for(name: &str) -> Vec<String> {
        atman_runtime::model_registry::all_aliases()
            .into_iter()
            .filter(|(_, model)| {
                model == name || atman_runtime::model_registry::resolve_alias(model) == name
            })
            .map(|(alias, _)| alias)
            .collect()
    }

    fn actions(&self) -> Vec<Action> {
        let mut actions = vec![Action::Add, Action::Edit, Action::Alias, Action::Delete];
        if self.locked_provider.is_some() {
            actions.push(Action::All);
        }
        actions
    }

    fn dispatch(
        &mut self,
        action: Action,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.inputs_locked()
            && !matches!(
                action,
                Action::Cancel | Action::Back | Action::Stop | Action::Details
            )
        {
            return;
        }
        match action {
            Action::Add => self.open_form(),
            Action::Edit if self.current_editable() => self.open_edit(),
            Action::Delete if self.current_editable() => {
                self.status = None;
                if let Some(name) = self.current_model().map(|model| model.slug.clone()) {
                    self.delete_aliases = Self::aliases_for(&name);
                    self.confirm_delete = Some(name);
                    self.confirm_focus = 0;
                    self.detail_scroll = 0;
                }
            }
            Action::Edit | Action::Delete => {
                self.status = Some((
                    "Catalog model · read-only. Add creates a separate configuration.".into(),
                    false,
                ));
                self.page = Page::Details;
                self.focus = Focus::Details;
            }
            Action::Alias => {
                self.open_alias_model = self.current_model().map(|model| model.slug.clone())
            }
            Action::Aliases => self.open_alias_manager = true,
            Action::All => {
                self.locked_provider = None;
                self.page = Page::List;
                self.sync_browser();
            }
            Action::More => {
                self.return_page = self.page;
                self.page = Page::Actions;
                self.focus = Focus::Action(Action::Alias);
                self.actions_offset = 0;
            }
            Action::Details if self.show_form => {
                self.return_page = self.page;
                self.page = Page::Status;
                self.detail_scroll = 0;
            }
            Action::Details => {
                self.page = Page::Details;
                self.focus = Focus::Details;
            }
            Action::Save => self.commit_form(tx),
            Action::Test => self.test_model(tx),
            Action::Stop => {
                if let Some(test) = &self.pending_test {
                    test.cancel.cancel();
                }
            }
            Action::Confirm => {
                if let Some(name) = self.confirm_delete.clone() {
                    self.delete_aliases = Self::aliases_for(&name);
                    if self.delete_aliases.is_empty() {
                        self.begin_mutation(crate::ModelMutation::Remove { name }, tx);
                    }
                }
            }
            Action::Discard => {
                self.show_form = false;
                self.confirm_discard = false;
                self.editing = None;
                self.page = Page::Details;
            }
            Action::Keep => self.confirm_discard = false,
            Action::Cancel | Action::Back => {
                if self.inputs_locked() {
                    self.close();
                } else if self.confirm_discard {
                    self.confirm_discard = false;
                } else if self.confirm_delete.take().is_some() {
                    self.detail_scroll = 0;
                } else if self.page == Page::Status || self.page == Page::Actions {
                    self.page = self.return_page;
                    self.focus = Focus::List;
                } else if self.show_form {
                    if self.draft() != self.draft_snapshot {
                        self.confirm_discard = true;
                        self.confirm_focus = 2;
                    } else {
                        self.show_form = false;
                        self.page = Page::Details;
                    }
                } else if self.page == Page::Details {
                    self.page = Page::List;
                    self.focus = Focus::List;
                } else {
                    self.close();
                }
            }
        }
        self.clear_geometry();
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
        fields.extend([3, 4, 5, 7, 8, 9, 10]);
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
            self.test_result = Some(("Select a provider and model before testing".into(), false));
            atman_runtime::notify!(error, "Select a provider and model before testing");
            return;
        }
        let Some(reasoning) = parse_reasoning(self.thinking_editor.buf()) else {
            self.test_result = Some(("Reasoning value is invalid".into(), false));
            atman_runtime::notify!(error, "Reasoning value is invalid");
            return;
        };
        let max_tokens = if self.max_tokens_editor.buf().trim().is_empty() {
            None
        } else {
            match self.max_tokens_editor.buf().trim().parse::<u32>() {
                Ok(value) if value > 0 => Some(value),
                _ => {
                    self.test_result =
                        Some(("Max Tokens must be a positive integer".into(), false));
                    atman_runtime::notify!(error, "Max Tokens must be a positive integer");
                    return;
                }
            }
        };
        let Some(tx) = control_tx else {
            self.test_result = Some(("Model test is unavailable".into(), false));
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
            self.test_result = Some(("Model test is unavailable".into(), false));
            atman_runtime::notify!(error, "Model test is unavailable");
            return;
        }
        self.pending_test = Some(PendingModelTest {
            request_id: self.next_request_id,
            identity: self.editing.clone(),
            draft: self.draft(),
            cancel: cancel.clone(),
            _cancel_on_drop: cancel.drop_guard(),
        });
        self.clear_test_result();
    }

    pub(crate) fn resolve_test(
        &mut self,
        request_id: u64,
        result: &Result<String, String>,
    ) -> bool {
        if !self.pending_test.as_ref().is_some_and(|test| {
            test.request_id == request_id
                && test.identity == self.editing
                && test.draft == self.draft()
        }) {
            return false;
        }
        self.pending_test = None;
        self.form_info_scroll = 0;
        self.test_result = Some(match result {
            Ok(message) => (message.clone(), true),
            Err(message) => (message.clone(), false),
        });
        true
    }

    fn open_form(&mut self) {
        self.show_form = true;
        self.page = Page::List;
        self.confirm_discard = false;
        self.form_enabled = true;
        self.form_field = 0;
        self.editing = None;
        self.form_api = None;
        self.clear_test_result();
        self.selected_provider = self.current_provider().to_string();
        self.name_editor = InputEditor::default();
        self.model_editor = InputEditor::default();
        self.context_budget_editor = InputEditor::default();
        self.thinking_editor = InputEditor::default();
        self.thinking_editor.insert_str("default");
        self.max_tokens_editor = InputEditor::default();
        self.draft_snapshot = self.draft();
        self.clear_geometry();
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
        self.load_edit_entry(&model, provider, &entry);
    }

    fn load_edit_entry(
        &mut self,
        model: &atman_runtime::model_registry::ModelRow,
        provider: String,
        entry: &atman_runtime::model_registry::ModelEntry,
    ) {
        self.show_form = true;
        self.page = Page::List;
        self.confirm_discard = false;
        self.form_enabled = entry.enabled.unwrap_or(true);
        self.form_field = 0;
        self.editing = Some(model.slug.clone());
        self.form_api = entry.api;
        self.clear_test_result();
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
        if let Some(budget) = entry.context_budget {
            self.context_budget_editor.insert_str(&budget.to_string());
        }
        self.thinking_editor = InputEditor::default();
        self.thinking_editor
            .insert_str(&reasoning_label(&model.reasoning));
        self.max_tokens_editor = InputEditor::default();
        if let Some(mt) = entry.max_tokens {
            self.max_tokens_editor.insert_str(&mt.to_string());
        }
        self.draft_snapshot = self.draft();
        self.clear_geometry();
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if self.inputs_locked() {
            match action {
                KeyAction::Escape => self.close(),
                KeyAction::Tab | KeyAction::BackTab if self.pending_test.is_some() => {
                    self.form_field = if self.form_field == 7 { 9 } else { 7 }
                }
                KeyAction::Submit if self.form_field == 7 && self.pending_test.is_some() => {
                    self.dispatch(Action::Stop, tx)
                }
                KeyAction::Submit if self.form_field == 9 => self.close(),
                KeyAction::PageUp | KeyAction::PageDown if self.show_form => {
                    self.scroll_form_info(action)
                }
                _ => {}
            }
            return;
        }
        if self.confirm_discard || self.confirm_delete.is_some() {
            let actions = if self.confirm_discard {
                vec![Action::Save, Action::Discard, Action::Keep]
            } else if self.delete_aliases.is_empty() {
                vec![Action::Cancel, Action::Confirm]
            } else {
                vec![Action::Cancel, Action::Aliases]
            };
            match action {
                KeyAction::Escape => self.dispatch(Action::Cancel, tx),
                KeyAction::Tab | KeyAction::CursorRight => {
                    self.confirm_focus = (self.confirm_focus + 1) % actions.len()
                }
                KeyAction::BackTab | KeyAction::CursorLeft => {
                    self.confirm_focus = (self.confirm_focus + actions.len() - 1) % actions.len()
                }
                KeyAction::Submit => {
                    self.dispatch(actions[self.confirm_focus.min(actions.len() - 1)], tx)
                }
                KeyAction::PageUp
                | KeyAction::PageDown
                | KeyAction::HistoryUp
                | KeyAction::HistoryDown => self.scroll_details(action),
                _ => {}
            }
            return;
        }
        if self.page == Page::Status {
            match action {
                KeyAction::Escape | KeyAction::Submit => self.dispatch(Action::Back, tx),
                _ => self.scroll_details(action),
            }
            return;
        }
        if self.show_form {
            self.handle_form_key(action, tx);
            return;
        }
        if self.focus == Focus::Search {
            match action {
                KeyAction::Escape | KeyAction::Submit | KeyAction::Tab => self.focus = Focus::List,
                KeyAction::BackTab => self.focus = Focus::Action(Action::More),
                _ => {
                    let before = self.search.buf().to_owned();
                    self.search.handle_key(action);
                    if before != self.search.buf() {
                        self.sync_browser();
                        self.detail_scroll = 0;
                    }
                }
            }
            return;
        }
        if self.page == Page::Actions {
            let actions = self.actions();
            let i = actions
                .iter()
                .position(|a| self.focus == Focus::Action(*a))
                .unwrap_or(0);
            match action {
                KeyAction::Escape => self.dispatch(Action::Back, tx),
                KeyAction::HistoryUp | KeyAction::Char('k') | KeyAction::BackTab => {
                    self.focus = Focus::Action(actions[i.saturating_sub(1)])
                }
                KeyAction::HistoryDown | KeyAction::Char('j') | KeyAction::Tab => {
                    self.focus = Focus::Action(actions[(i + 1).min(actions.len() - 1)])
                }
                KeyAction::Home | KeyAction::CursorHome => self.focus = Focus::Action(actions[0]),
                KeyAction::End | KeyAction::CursorEnd => {
                    self.focus = Focus::Action(*actions.last().unwrap())
                }
                KeyAction::Submit => {
                    self.page = self.return_page;
                    self.dispatch(actions[i], tx);
                }
                _ => self.dispatch_shortcut(action, tx),
            }
            return;
        }
        match action {
            KeyAction::Escape => self.dispatch(Action::Back, tx),
            KeyAction::Tab | KeyAction::BackTab => {
                let mut focus = vec![Focus::Search, Focus::List];
                if !self.narrow || self.page == Page::Details {
                    focus.push(Focus::Details);
                }
                focus.extend(self.action_hits.iter().map(|(a, _)| Focus::Action(*a)));
                let i = focus.iter().position(|f| f == &self.focus).unwrap_or(0);
                self.focus = focus[if matches!(action, KeyAction::Tab) {
                    (i + 1) % focus.len()
                } else {
                    (i + focus.len() - 1) % focus.len()
                }];
            }
            KeyAction::Submit => self.dispatch(
                if let Focus::Action(action) = self.focus {
                    action
                } else {
                    Action::Details
                },
                tx,
            ),
            KeyAction::HistoryUp
            | KeyAction::HistoryDown
            | KeyAction::Char('j' | 'k')
            | KeyAction::PageUp
            | KeyAction::PageDown
            | KeyAction::Home
            | KeyAction::End
            | KeyAction::CursorHome
            | KeyAction::CursorEnd => {
                if self.focus == Focus::Details || (self.narrow && self.page == Page::Details) {
                    self.scroll_details(action);
                } else {
                    self.focus = Focus::List;
                    self.navigate(action);
                }
            }
            _ => self.dispatch_shortcut(action, tx),
        }
    }

    fn dispatch_shortcut(
        &mut self,
        key: &KeyAction,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        match key {
            KeyAction::Char('/') => self.focus = Focus::Search,
            KeyAction::Char('n') => self.dispatch(Action::Add, tx),
            KeyAction::Char('e') => self.dispatch(Action::Edit, tx),
            KeyAction::Char('a') => self.dispatch(Action::Alias, tx),
            KeyAction::Delete => self.dispatch(Action::Delete, tx),
            _ => {}
        }
    }

    fn scroll_details(&mut self, action: &KeyAction) {
        let page = self.detail_rect.map_or(1, |r| usize::from(r.height).max(1));
        self.detail_scroll = match action {
            KeyAction::HistoryUp | KeyAction::Char('k') => self.detail_scroll.saturating_sub(1),
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                (self.detail_scroll + 1).min(self.detail_max_scroll)
            }
            KeyAction::PageUp => self.detail_scroll.saturating_sub(page),
            KeyAction::PageDown => (self.detail_scroll + page).min(self.detail_max_scroll),
            KeyAction::Home | KeyAction::CursorHome => 0,
            KeyAction::End | KeyAction::CursorEnd => self.detail_max_scroll,
            _ => self.detail_scroll,
        };
    }

    fn scroll_form_info(&mut self, action: &KeyAction) {
        let page = self
            .form_info_rect
            .map_or(1, |rect| usize::from(rect.height).max(1));
        self.form_info_scroll = if matches!(action, KeyAction::PageUp) {
            self.form_info_scroll.saturating_sub(page)
        } else {
            (self.form_info_scroll + page).min(self.form_info_max_scroll)
        };
    }
    pub fn paste(&mut self, text: &str) {
        if !self.show_form && self.focus == Focus::Search {
            self.search.insert_str(text);
            self.sync_browser();
            return;
        }
        if self.inputs_locked()
            || self.confirm_discard
            || !self.show_form
            || matches!(self.form_field, 2 | 6 | 7 | 8 | 9 | 10)
        {
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
            self.clear_test_result();
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
            KeyAction::PageUp | KeyAction::PageDown if matches!(self.form_field, 7 | 10) => {
                let page = self
                    .form_info_rect
                    .map_or(1, |rect| usize::from(rect.height).max(1));
                self.form_info_scroll = if matches!(action, KeyAction::PageUp) {
                    self.form_info_scroll.saturating_sub(page)
                } else {
                    self.form_info_scroll
                        .saturating_add(page)
                        .min(self.form_info_max_scroll)
                };
            }
            KeyAction::Save => self.commit_form(control_tx),
            KeyAction::Escape => self.dispatch(Action::Cancel, control_tx),
            KeyAction::Submit if self.form_field == 8 => self.commit_form(control_tx),
            KeyAction::Submit if self.form_field == 9 => self.dispatch(Action::Cancel, control_tx),
            KeyAction::Submit if self.form_field == 10 => {
                self.dispatch(Action::Details, control_tx)
            }
            KeyAction::Submit => {}
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
                            self.clear_test_result();
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
                        self.clear_test_result();
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
                            self.clear_test_result();
                        }
                    }
                }
            }
            _ => {
                if let Some(editor) = self.form_editor_mut() {
                    let before = editor.buf().to_owned();
                    editor.handle_key(action);
                    if editor.buf() != before {
                        self.clear_test_result();
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
            self.test_result = Some(("Model name is required".into(), false));
            return;
        }
        let model = self.model_editor.buf().trim().to_string();
        let provider = self.selected_provider.clone();
        let context_budget = if self.context_budget_editor.buf().trim().is_empty() {
            32768
        } else {
            let Ok(value) = self.context_budget_editor.buf().trim().parse::<u64>() else {
                self.test_result =
                    Some(("Context Budget must be a positive integer".into(), false));
                atman_runtime::notify!(error, "Context Budget must be a positive integer");
                return;
            };
            if value == 0 {
                self.test_result =
                    Some(("Context Budget must be a positive integer".into(), false));
                return;
            }
            value
        };
        let Some(reasoning) = parse_reasoning(self.thinking_editor.buf()) else {
            self.test_result = Some(("Reasoning value is invalid".into(), false));
            atman_runtime::notify!(error, "Reasoning value is invalid");
            return;
        };
        let max_tokens = if self.max_tokens_editor.buf().trim().is_empty() {
            None
        } else {
            let Ok(value) = self.max_tokens_editor.buf().trim().parse::<u32>() else {
                self.test_result = Some(("Max Tokens must be a positive integer".into(), false));
                atman_runtime::notify!(error, "Max Tokens must be a positive integer");
                return;
            };
            if value == 0 {
                self.test_result = Some(("Max Tokens must be a positive integer".into(), false));
                return;
            }
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
                enabled: self.form_enabled,
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
        self.render_manager(f, area, t);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action, tx);
        self.take_modal_action()
    }

    fn handle_paste(&mut self, text: &str) {
        self.paste(text);
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
    }

    fn title(&self) -> Line<'static> {
        Line::from(self.locked_provider.as_ref().map_or_else(
            || "Model Manager".to_owned(),
            |provider| format!("Models · {provider}"),
        ))
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
        use crate::settings_form::{
            Action as Button, Field, FormLayout, render_action, render_field,
        };
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
            height: area.height.saturating_sub(3),
        };
        let navigation = self.form_fields();
        if !navigation.contains(&self.form_field) {
            self.form_field = navigation[0];
        }
        let fields: Vec<_> = navigation.into_iter().filter(|field| *field < 7).collect();
        let selected = fields
            .iter()
            .position(|field| *field == self.form_field)
            .unwrap_or_else(|| fields.len().saturating_sub(1));
        let layout = FormLayout::with_actions(inner, fields.len(), selected, 3);
        let locked = self.inputs_locked();
        for &(index, rect) in &layout.fields {
            let field = fields[index];
            let (label, value, placeholder, cursor) = match field {
                0 => (
                    "Name",
                    self.name_editor.buf().to_owned(),
                    "local model name",
                    Some(self.name_editor.cursor_display_col()),
                ),
                1 => (
                    "Model ID",
                    self.model_editor.buf().to_owned(),
                    "same as name",
                    Some(self.model_editor.cursor_display_col()),
                ),
                2 => (
                    "Provider",
                    self.selected_provider.clone(),
                    "select provider",
                    None,
                ),
                3 => (
                    "Context Budget",
                    self.context_budget_editor.buf().to_owned(),
                    "32768",
                    Some(self.context_budget_editor.cursor_display_col()),
                ),
                4 => (
                    "Reasoning",
                    self.thinking_editor.buf().to_owned(),
                    "default",
                    Some(self.thinking_editor.cursor_display_col()),
                ),
                5 => (
                    "Max Tokens",
                    self.max_tokens_editor.buf().to_owned(),
                    "inherit",
                    Some(self.max_tokens_editor.cursor_display_col()),
                ),
                6 => ("API", self.api_label(), "", None),
                _ => continue,
            };
            let hit = render_field(
                f,
                rect,
                &Field {
                    label,
                    value: &value,
                    placeholder,
                    focused: field == self.form_field,
                    hovered: self.hovered_field == Some(field),
                    cursor,
                    selector: matches!(field, 2 | 4 | 6),
                    locked,
                },
                t,
            );
            self.form_hits.push((field, hit));
        }
        if let (Some((first, _)), Some((last, _))) = (layout.fields.first(), layout.fields.last())
            && layout.fields.len() < fields.len()
        {
            f.render_widget(
                Paragraph::new(format!(
                    " Fields {}–{} / {} · Tab or scroll",
                    first + 1,
                    last + 1,
                    fields.len()
                ))
                .style(
                    Style::default()
                        .fg(t.meta_fg.into())
                        .bg(crate::wm::shell::surface_bg(t)),
                ),
                layout.scroll,
            );
        }

        let info = self.form_status();
        let lines = crate::width::word_wrap(&info, usize::from(layout.info.width).max(1));
        let info_area = Rect {
            height: layout.info.height.saturating_sub(1),
            ..layout.info
        };
        self.form_info_max_scroll = lines.len().saturating_sub(usize::from(info_area.height));
        self.form_info_scroll = self.form_info_scroll.min(self.form_info_max_scroll);
        self.form_info_rect = (info_area.height > 0).then_some(info_area);
        f.render_widget(
            Paragraph::new(
                lines
                    .iter()
                    .skip(self.form_info_scroll)
                    .take(usize::from(info_area.height))
                    .cloned()
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            )
            .style(
                Style::default()
                    .fg(if self.test_result.as_ref().is_some_and(|(_, ok)| !ok) {
                        t.warn.into()
                    } else {
                        t.meta_fg.into()
                    })
                    .bg(crate::wm::shell::surface_bg(t)),
            ),
            info_area,
        );
        if layout.info.height > 0 {
            let link = Rect::new(
                layout.info.x,
                layout.info.bottom() - 1,
                layout.info.width,
                1,
            );
            self.action_hits.push((Action::Details, link));
            f.render_widget(
                Paragraph::new(if self.form_field == 10 {
                    "Details · Enter"
                } else {
                    "Details · click or Tab"
                })
                .style(
                    Style::default()
                        .fg(t.subtle_fg.into())
                        .bg(crate::wm::shell::surface_bg(t)),
                ),
                link,
            );
        }
        let help = if self.pending_test.is_some() {
            "Stop cancels · Esc hides"
        } else if self.pending.is_some() {
            "Saving model · Esc hides"
        } else if self.form_field == 10 {
            "Enter details · PgUp/PgDn scroll"
        } else {
            "Tab · Ctrl+S save · Esc back"
        };
        f.render_widget(
            Paragraph::new(crate::width::truncate(help, layout.help.width as usize)).style(
                Style::default()
                    .fg(t.meta_fg.into())
                    .bg(crate::wm::shell::surface_bg(t)),
            ),
            layout.help,
        );
        let actions = [
            (
                Action::Cancel,
                9,
                if locked { "Hide" } else { "Cancel" },
                "Esc",
            ),
            (
                if self.pending_test.is_some() {
                    Action::Stop
                } else {
                    Action::Test
                },
                7,
                if self.pending_test.is_some() {
                    "Stop test"
                } else {
                    "Test"
                },
                "",
            ),
            (
                Action::Save,
                8,
                if self.pending.is_some() {
                    "Saving…"
                } else {
                    "Save"
                },
                "^S",
            ),
        ];
        for ((action, field, label, shortcut), rect) in
            actions.into_iter().zip(layout.actions.iter().copied())
        {
            let label = match action {
                Action::Stop if rect.width < 13 => "Stop",
                _ => label,
            };
            let shortcut = if action == Action::Cancel && rect.width < 13 {
                ""
            } else {
                shortcut
            };
            render_action(
                f,
                rect,
                &Button {
                    label,
                    shortcut,
                    focused: self.form_field == field,
                    hovered: self.hovered_action == Some(action),
                    disabled: locked && !matches!(action, Action::Cancel | Action::Stop),
                },
                t,
            );
            self.action_hits.push((action, rect));
            if field == 7 {
                self.test_rect = Some(rect);
            }
            if field == 8 {
                self.save_rect = Some(rect);
            }
        }
    }
}

#[cfg(test)]
mod tests;
