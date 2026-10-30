use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::model_browser::{BrowserRow, BrowserRowKind, ModelBrowser};
use crate::settings_form::{Action, Field, FieldHit, render_action, render_field, render_record};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    NameInput,
    Tree,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Name,
    Search,
    ClearSearch,
    PreviousGroup,
    NextGroup,
    Row(usize),
    New,
    Edit,
    Delete,
    Save,
    Confirm,
    Cancel,
}

#[derive(Default)]
pub struct AliasManager {
    pub(crate) settings: crate::settings::SettingsSource,
    pub open: bool,
    pending: Option<crate::settings::SettingsMutationRequest>,
    error: Option<String>,
    required_models: bool,
    pub aliases: Vec<(String, String)>,
    pub selected: usize,
    pub show_form: bool,
    pub last_input_rect: Option<Rect>,
    is_edit: bool,
    editor: InputEditor,
    search: InputEditor,
    search_hit: Option<FieldHit>,
    group_scope: Option<String>,
    tree_pages: Vec<Vec<usize>>,
    edit_original: Option<String>,
    target_model: Option<String>,
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    focus: Focus,
    provider_idx: usize,
    model_idx: Vec<usize>,
    browser: ModelBrowser,
    hovered: Option<Hit>,
    name_hit: Option<FieldHit>,
    row_hits: Vec<(usize, Rect)>,
    action_hits: Vec<(Hit, Rect)>,
    list_offset: usize,
    confirm_delete: Option<String>,
    list_rect: Option<Rect>,
    save_rect: Option<Rect>,
    confirm_yes_rect: Option<Rect>,
    confirm_no_rect: Option<Rect>,
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
        self.clear_geometry();
        self.open = true;
        self.refresh_list();
        self.refresh_groups();
    }

    pub(crate) fn require_execution_models(&mut self) -> bool {
        // Archived views have no execution configuration; continuation chooses its own scope.
        if self.settings.config().is_err() {
            return false;
        }
        let Some(alias) = ["smart", "cheap"].into_iter().find(|alias| {
            self.settings
                .model_entry(alias)
                .is_none_or(|entry| entry.provider.as_deref().is_none_or(str::is_empty))
        }) else {
            return false;
        };
        self.open();
        self.required_models = true;
        let target = self
            .aliases
            .iter()
            .find(|(name, _)| name == alias)
            .map(|(_, model)| model.clone());
        self.open_form(target.is_some(), Some(alias), target.as_deref());
        self.focus = Focus::Tree;
        true
    }

    pub fn close(&mut self) {
        self.pending = None;
        self.error = None;
        self.required_models = false;
        self.open = false;
        self.show_form = false;
        self.confirm_delete = None;
        self.clear_geometry();
        self.hovered = None;
    }

    fn clear_geometry(&mut self) {
        self.last_input_rect = None;
        self.name_hit = None;
        self.search_hit = None;
        self.list_rect = None;
        self.save_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        self.row_hits.clear();
        self.action_hits.clear();
    }

    pub fn show_form(&self) -> bool {
        self.show_form
    }

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        control_tx: Option<&crate::ControlSender>,
    ) {
        if self.pending.is_some() {
            return;
        }
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (event.column, event.row);
        let target = self.hit_at(point);
        match event.kind {
            MouseEventKind::Moved => self.hovered = target,
            MouseEventKind::Down(MouseButton::Left) => {
                self.hovered = target;
                match target {
                    Some(Hit::Confirm) => {
                        self.handle_key(&KeyAction::Submit, control_tx);
                    }
                    Some(Hit::Save) => self.handle_key(&KeyAction::Save, control_tx),
                    Some(Hit::Cancel) => self.handle_key(&KeyAction::Escape, control_tx),
                    Some(Hit::New) => self.handle_key(&KeyAction::Char('n'), control_tx),
                    Some(Hit::Edit) => self.handle_key(&KeyAction::Char('e'), control_tx),
                    Some(Hit::Delete) => self.handle_key(&KeyAction::Char('d'), control_tx),
                    Some(Hit::Search) => {
                        self.focus = Focus::Search;
                        if let Some(hit) = self.search_hit
                            && rect_hit(Some(hit.value), point)
                        {
                            self.search.set_cursor_by_display(
                                0,
                                event
                                    .column
                                    .saturating_sub(hit.value.x)
                                    .saturating_add(hit.offset.min(u16::MAX as usize) as u16),
                            );
                        }
                    }
                    Some(Hit::ClearSearch) => {
                        self.search = InputEditor::default();
                        self.filter_models();
                    }
                    Some(Hit::PreviousGroup | Hit::NextGroup) => {
                        self.focus = Focus::Tree;
                        self.handle_key(
                            &if target == Some(Hit::PreviousGroup) {
                                KeyAction::CursorLeft
                            } else {
                                KeyAction::CursorRight
                            },
                            control_tx,
                        );
                    }
                    Some(Hit::Name) => {
                        self.focus = Focus::NameInput;
                        if let Some(hit) = self.name_hit
                            && rect_hit(Some(hit.value), point)
                        {
                            self.editor.set_cursor_by_display(
                                0,
                                event
                                    .column
                                    .saturating_sub(hit.value.x)
                                    .saturating_add(hit.offset.min(u16::MAX as usize) as u16),
                            );
                        }
                    }
                    Some(Hit::Row(index)) if self.show_form => {
                        self.focus = Focus::Tree;
                        if let Some(value) =
                            self.browser.rows().get(index).map(|row| row.value.clone())
                        {
                            self.select_model(&value);
                        }
                    }
                    Some(Hit::Row(index)) => self.selected = index,
                    None => {}
                }
            }
            MouseEventKind::Down(MouseButton::Right) if !self.show_form => {
                if let Some(Hit::Row(index)) = target {
                    self.selected = index;
                    self.handle_key(&KeyAction::Char('d'), control_tx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.confirm_delete.is_none() && rect_hit(self.list_rect, point) =>
            {
                self.hovered = None;
                let action = if event.kind == MouseEventKind::ScrollUp {
                    KeyAction::HistoryUp
                } else {
                    KeyAction::HistoryDown
                };
                if self.show_form {
                    self.focus = Focus::Tree;
                }
                self.handle_key(&action, control_tx);
            }
            _ => {}
        }
    }

    fn hit_at(&self, point: (u16, u16)) -> Option<Hit> {
        if self.confirm_delete.is_some() {
            return if rect_hit(self.confirm_yes_rect, point) {
                Some(Hit::Confirm)
            } else if rect_hit(self.confirm_no_rect, point) {
                Some(Hit::Cancel)
            } else {
                None
            };
        }
        if self
            .name_hit
            .is_some_and(|hit| rect_hit(Some(hit.record), point))
        {
            return Some(Hit::Name);
        }
        if self
            .search_hit
            .is_some_and(|hit| rect_hit(Some(hit.record), point))
        {
            return Some(Hit::Search);
        }
        self.action_hits
            .iter()
            .find_map(|(action, rect)| rect_hit(Some(*rect), point).then_some(*action))
            .or_else(|| {
                self.row_hits.iter().find_map(|(index, rect)| {
                    rect_hit(Some(*rect), point).then_some(Hit::Row(*index))
                })
            })
    }

    pub fn refresh_list(&mut self) {
        self.aliases = self.settings.all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
        self.list_offset = 0;
    }

    pub(crate) fn catalog_changed(&mut self) {
        if !self.open {
            return;
        }
        let alias = self
            .aliases
            .get(self.selected)
            .map(|(name, _)| name.clone());
        let model = self.browser.selected().map(|row| row.value.clone());
        let focus = self.focus;
        let offset = self.list_offset;
        self.refresh_list();
        self.selected = alias
            .and_then(|name| self.aliases.iter().position(|(key, _)| *key == name))
            .unwrap_or(self.selected);
        self.list_offset = offset.min(self.aliases.len().saturating_sub(1));
        self.refresh_groups();
        self.sync_browser(model.as_deref());
        self.focus = focus;
        self.tree_pages.clear();
        self.clear_geometry();
    }

    fn refresh_groups(&mut self) {
        let enabled_providers = self.settings.enabled_provider_names();
        self.groups = self
            .settings
            .all_provider_groups_with_empty()
            .into_iter()
            .filter(|g| enabled_providers.contains(&g.provider_name))
            .collect();
        self.model_idx = vec![0; self.groups.len()];
        self.provider_idx = 0;
        self.sync_browser(None);
        self.focus = Focus::NameInput;
    }

    fn sync_browser(&mut self, selected: Option<&str>) {
        let query = self.search.buf().trim().to_lowercase();
        let entries: std::collections::HashMap<_, _> =
            self.settings.all_model_entries().into_iter().collect();
        let mut rows = Vec::new();
        for group in &self.groups {
            if self
                .group_scope
                .as_ref()
                .is_some_and(|scope| scope != &group.provider_name)
            {
                continue;
            }
            let display_name = self.settings.provider_display_name(&group.provider_name);
            let provider_matches = format!("{} {display_name}", group.provider_name)
                .to_lowercase()
                .contains(&query);
            let models: Vec<_> = group
                .models
                .iter()
                .filter(|model| {
                    provider_matches
                        || model_matches_search(model, entries.get(&model.slug), &query)
                })
                .collect();
            if !query.is_empty() && models.is_empty() && !provider_matches {
                continue;
            }
            rows.push(BrowserRow {
                kind: BrowserRowKind::Provider,
                label: format!("{display_name} · {}", models.len()),
                value: group.provider_name.clone(),
                selectable: false,
            });
            rows.extend(models.into_iter().map(|model| BrowserRow {
                kind: BrowserRowKind::Model,
                label: model.slug.clone(),
                value: model.slug.clone(),
                selectable: true,
            }));
        }
        self.browser.replace_rows(rows, selected);
    }

    fn filter_models(&mut self) {
        let selected = self
            .browser
            .selected()
            .map(|row| row.value.clone())
            .or_else(|| self.target_model.clone());
        self.sync_browser(selected.as_deref());
        self.clear_geometry();
        self.tree_pages.clear();
    }

    pub fn open_form_with_model(&mut self, model: &str) {
        self.open();
        self.open_form(false, None, None);
        self.select_model(model);
    }

    fn open_form(&mut self, is_edit: bool, alias: Option<&str>, model: Option<&str>) {
        self.refresh_groups();
        self.prepare_form(is_edit, alias, model);
    }

    fn prepare_form(&mut self, is_edit: bool, alias: Option<&str>, model: Option<&str>) {
        self.pending = None;
        self.error = None;
        self.clear_geometry();
        self.show_form = true;
        self.is_edit = is_edit;
        self.hovered = None;
        self.search = InputEditor::default();
        self.group_scope = None;
        self.sync_browser(model);
        let mut ed = InputEditor::default();
        if let Some(a) = alias {
            ed.insert_str(a);
        }
        self.editor = ed;
        self.edit_original = alias.map(|s| s.to_string());
        self.target_model = self.browser.selected().map(|row| row.value.clone());
        if let Some(m) = model {
            self.select_model(m);
        }
        self.focus = Focus::NameInput;
    }

    fn select_model(&mut self, slug: &str) {
        self.target_model = Some(slug.to_owned());
        self.sync_browser(Some(slug));
        self.clear_geometry();
        for (pi, g) in self.groups.iter().enumerate() {
            if let Some(mi) = g.models.iter().position(|m| m.slug == slug) {
                self.provider_idx = pi;
                self.model_idx[pi] = mi;
                return;
            }
        }
    }

    fn apply_browser_selection(&mut self) {
        let Some(row) = self.browser.selected() else {
            return;
        };
        let Some((pi, mi)) = self.groups.iter().enumerate().find_map(|(pi, group)| {
            group
                .models
                .iter()
                .position(|model| model.slug == row.value)
                .map(|mi| (pi, mi))
        }) else {
            return;
        };
        self.provider_idx = pi;
        self.model_idx[pi] = mi;
        self.target_model = Some(row.value.clone());
        self.clear_geometry();
    }

    fn current_model(&self) -> Option<&atman_runtime::model_registry::ModelRow> {
        let target = self.target_model.as_deref()?;
        self.groups
            .iter()
            .flat_map(|group| &group.models)
            .find(|model| model.slug == target)
    }

    pub fn handle_key(&mut self, action: &KeyAction, control_tx: Option<&crate::ControlSender>) {
        if self.required_models && matches!(action, KeyAction::Escape) {
            self.close();
            return;
        }
        if self.pending.is_some() {
            if matches!(action, KeyAction::Escape) {
                self.close();
            }
            return;
        }
        if let Some(alias) = self.confirm_delete.clone() {
            match action {
                KeyAction::Submit | KeyAction::Char('y') | KeyAction::Char('Y') => {
                    self.submit(
                        crate::settings::SettingsMutation::RemoveAlias { name: alias },
                        control_tx,
                    );
                }
                KeyAction::Escape | KeyAction::Char('n') | KeyAction::Char('N') => {
                    self.confirm_delete = None;
                    self.clear_geometry();
                }
                _ => {}
            }
            return;
        }
        if !self.show_form {
            match action {
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
                KeyAction::PageUp => {
                    self.selected = self.selected.saturating_sub(self.row_hits.len().max(1));
                }
                KeyAction::PageDown => {
                    self.selected = self
                        .selected
                        .saturating_add(self.row_hits.len().max(1))
                        .min(self.aliases.len().saturating_sub(1));
                }
                KeyAction::Home => self.selected = 0,
                KeyAction::End => self.selected = self.aliases.len().saturating_sub(1),
                KeyAction::Char('n') => self.open_form(false, None, None),
                KeyAction::Char('e') | KeyAction::Submit => {
                    let selected = self.selected;
                    if let Some((alias, model)) = self.aliases.get(selected).cloned() {
                        self.open_form(true, Some(&alias), Some(&model));
                    }
                }
                KeyAction::Char('d') => {
                    if let Some((alias, _)) = self.aliases.get(self.selected) {
                        self.confirm_delete = Some(alias.clone());
                        self.clear_geometry();
                    }
                }
                KeyAction::Escape => self.close(),
                _ => {}
            }
            return;
        }

        if self.focus == Focus::Search {
            match action {
                KeyAction::Save => self.commit_alias(control_tx),
                KeyAction::Tab | KeyAction::Submit | KeyAction::Escape => self.focus = Focus::Tree,
                KeyAction::BackTab => self.focus = Focus::NameInput,
                KeyAction::RemoveAttachment => {
                    self.search = InputEditor::default();
                    self.filter_models();
                }
                _ => {
                    let before = self.search.buf().to_owned();
                    self.search.handle_key(action);
                    if self.search.buf() != before {
                        self.filter_models();
                    }
                }
            }
            return;
        }

        if matches!(action, KeyAction::Save) {
            self.commit_alias(control_tx);
            return;
        }

        if matches!(action, KeyAction::Escape)
            && (self.editor.buf().trim().is_empty() || self.target_model.is_none())
        {
            self.show_form = false;
            self.clear_geometry();
            return;
        }

        match self.focus {
            Focus::NameInput => match action {
                KeyAction::Escape | KeyAction::Submit => self.commit_alias(control_tx),
                KeyAction::Tab | KeyAction::BackTab => {
                    self.focus = Focus::Tree;
                }
                KeyAction::Backspace
                | KeyAction::Delete
                | KeyAction::DeleteWordBackward
                | KeyAction::CursorLeft
                | KeyAction::CursorRight
                | KeyAction::CursorHome
                | KeyAction::CursorEnd
                | KeyAction::Char(_) => {
                    self.editor.handle_key(action);
                }
                _ => {}
            },
            Focus::Tree => match action {
                KeyAction::Char('/') => self.focus = Focus::Search,
                KeyAction::Escape => self.commit_alias(control_tx),
                KeyAction::Submit => {
                    self.apply_browser_selection();
                    self.commit_alias(control_tx);
                }
                KeyAction::Tab | KeyAction::BackTab => self.focus = Focus::NameInput,
                KeyAction::HistoryUp
                | KeyAction::Char('k')
                | KeyAction::Home
                | KeyAction::CursorHome => {
                    self.browser.handle_key(action, 0);
                    self.apply_browser_selection();
                }
                KeyAction::HistoryDown
                | KeyAction::Char('j')
                | KeyAction::End
                | KeyAction::CursorEnd => {
                    self.browser.handle_key(action, 0);
                    self.apply_browser_selection();
                }
                KeyAction::PageUp | KeyAction::PageDown => {
                    self.page_models(matches!(action, KeyAction::PageDown))
                }
                KeyAction::CursorLeft | KeyAction::CursorRight => {
                    let Some(direction) =
                        crate::directional_selector::SelectorDirection::from_key(action)
                    else {
                        return;
                    };
                    let mut selected = self
                        .group_scope
                        .as_ref()
                        .and_then(|scope| {
                            self.groups
                                .iter()
                                .position(|group| &group.provider_name == scope)
                        })
                        .map_or(0, |i| i + 1);
                    crate::directional_selector::move_wrapped(
                        &mut selected,
                        self.groups.len() + 1,
                        direction,
                    );
                    self.group_scope = selected
                        .checked_sub(1)
                        .and_then(|i| self.groups.get(i))
                        .map(|group| group.provider_name.clone());
                    self.provider_idx = selected.saturating_sub(1);
                    self.filter_models();
                }
                _ => {}
            },
            Focus::Search => unreachable!("search input is handled before form navigation"),
        }
    }

    fn page_models(&mut self, forward: bool) {
        let page = self
            .tree_pages
            .iter()
            .position(|page| page.contains(&self.browser.selected_index()))
            .unwrap_or(0);
        let next = if forward {
            (page + 1).min(self.tree_pages.len().saturating_sub(1))
        } else {
            page.saturating_sub(1)
        };
        if next == page {
            return;
        }
        if let Some(indices) = self.tree_pages.get(next) {
            self.browser.focus_page(indices);
            self.apply_browser_selection();
            self.clear_geometry();
        }
    }

    fn submit(
        &mut self,
        action: crate::settings::SettingsMutation,
        tx: Option<&crate::ControlSender>,
    ) {
        match crate::settings::SettingsMutationRequest::send(action, tx) {
            Ok(request) => {
                self.pending = Some(request);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn commit_alias(&mut self, tx: Option<&crate::ControlSender>) {
        let name = self.editor.buf().trim().to_string();
        let Some(model) = self.target_model.clone().filter(|model| !model.is_empty()) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        let action = if self.required_models && name == "smart" {
            crate::settings::SettingsMutation::DefaultModel { model }
        } else if self.is_edit {
            crate::settings::SettingsMutation::UpdateAlias {
                original: self.edit_original.clone(),
                name,
                model,
            }
        } else {
            crate::settings::SettingsMutation::CreateAlias { name, model }
        };
        self.submit(action, tx);
    }

    pub(crate) fn mutation_result(
        &mut self,
        id: uuid::Uuid,
        result: &Result<String, String>,
    ) -> bool {
        if self.pending.as_ref().is_none_or(|request| request.id != id) {
            return false;
        }
        self.pending = None;
        match result {
            Ok(_) => {
                self.error = None;
                self.confirm_delete = None;
                self.show_form = false;
                self.refresh_list();
                self.refresh_groups();
                self.clear_geometry();
                if self.required_models && !self.require_execution_models() {
                    self.close();
                }
            }
            Err(error) => self.error = Some(error.clone()),
        }
        true
    }
}

fn rect_hit(rect: Option<Rect>, point: (u16, u16)) -> bool {
    rect.is_some_and(|rect| {
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    })
}

fn model_matches_search(
    model: &atman_runtime::model_registry::ModelRow,
    entry: Option<&atman_runtime::model_registry::ModelEntry>,
    query: &str,
) -> bool {
    model.slug.to_lowercase().contains(query)
        || entry
            .filter(|entry| entry.provider.as_deref().unwrap_or("unknown") == model.provider_name)
            .is_some_and(|entry| entry.model.to_lowercase().contains(query))
}

fn render_tree_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut AliasManager,
    theme: &crate::theme::Theme,
) {
    mgr.list_rect = Some(area);
    mgr.tree_pages = mgr.browser.grouped_pages(area.height, 1, 3);
    if area.height < 4 {
        render_hint(f, area, "Enlarge to browse models", theme);
        return;
    }
    let page = mgr
        .tree_pages
        .iter()
        .position(|rows| rows.contains(&mgr.browser.selected_index()))
        .unwrap_or(0);
    let indices = mgr.tree_pages.get(page).cloned().unwrap_or_default();
    let models = indices
        .iter()
        .filter(|index| mgr.browser.rows()[**index].selectable)
        .count();
    mgr.browser.set_visible_rows(models.max(1));
    let mut y = area.y;
    for index in indices {
        let row = &mgr.browser.rows()[index];
        if row.kind == BrowserRowKind::Provider {
            render_hint(f, Rect::new(area.x, y, area.width, 1), &row.label, theme);
            y += 1;
        } else {
            let selected = index == mgr.browser.selected_index()
                && (mgr.focus == Focus::Tree
                    || mgr.target_model.as_deref() == Some(row.value.as_str()));
            let rect = Rect::new(area.x, y, area.width, 3);
            render_choice(
                f,
                rect,
                &row.label,
                selected,
                mgr.hovered == Some(Hit::Row(index)),
                theme,
            );
            mgr.row_hits.push((index, rect));
            y += 3;
        }
    }
    if !mgr.browser.rows().iter().any(|row| row.selectable) && y < area.bottom() {
        render_hint(
            f,
            Rect::new(area.x, y, area.width, area.bottom() - y),
            if mgr.search.buf().is_empty() {
                "No enabled models available"
            } else {
                "No matching models"
            },
            theme,
        );
    }
}

fn render_hint(f: &mut ratatui::Frame, rect: Rect, text: &str, t: &crate::theme::Theme) {
    let inset = rect.width.min(2);
    let inner = Rect::new(
        rect.x + inset,
        rect.y,
        rect.width.saturating_sub(inset * 2),
        rect.height,
    );
    f.render_widget(
        Paragraph::new(crate::width::truncate(text, inner.width as usize))
            .style(Style::default().fg(t.subtle_fg.into())),
        inner,
    );
}

fn render_choice(
    f: &mut ratatui::Frame,
    rect: Rect,
    text: &str,
    selected: bool,
    hovered: bool,
    t: &crate::theme::Theme,
) {
    let bg = render_record(f, rect, selected, hovered, t);
    let inset = rect.width.min(2);
    let inner = Rect::new(
        rect.x + inset,
        rect.y + 1,
        rect.width.saturating_sub(inset * 2),
        1,
    );
    f.render_widget(
        Paragraph::new(crate::width::truncate(text, inner.width as usize))
            .style(Style::default().fg(t.tinted_fg.into()).bg(bg)),
        inner,
    );
}

fn render_preview_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &AliasManager,
    theme: &crate::theme::Theme,
) {
    let mut lines = vec![
        Line::from(Span::styled(
            "Selected model",
            Style::default().fg(theme.subtle_fg.into()),
        )),
        Line::from(""),
    ];
    let content_x = area.x.saturating_add(3);

    if let Some(m) = mgr.current_model() {
        lines.push(Line::from(Span::styled(
            &m.slug,
            Style::default()
                .fg(theme.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            mgr.settings.provider_display_name(&m.provider_name),
            Style::default().fg(theme.subtle_fg.into()),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "Context: {}",
                atman_runtime::humanize::format_count(m.context_budget)
            ),
            Style::default().fg(theme.tinted_fg.into()),
        )));
        let max_out = m
            .max_output_tokens
            .map(|n| format!("{}K", n / 1000))
            .unwrap_or_else(|| "—".to_string());
        lines.push(Line::from(Span::styled(
            format!("Output:  {}", max_out),
            Style::default().fg(theme.tinted_fg.into()),
        )));
        lines.push(Line::from(Span::styled(
            format!("Thinking: {}", if m.thinking { "✓" } else { "—" }),
            Style::default().fg(theme.tinted_fg.into()),
        )));
    } else if let Some(target) = mgr.target_model.as_deref() {
        lines.push(Line::from(Span::styled(
            target,
            Style::default().fg(theme.tinted_fg.into()),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Saved target retained. Select a model to replace it.",
            Style::default().fg(theme.subtle_fg.into()),
        )));
    }

    let content_area = Rect {
        x: content_x,
        y: area.y + 1.min(area.height),
        width: area.width.saturating_sub(3),
        height: area.height.saturating_sub(1),
    };
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );
}

fn render_alias_list(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut AliasManager,
    t: &crate::theme::Theme,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let list = rows[1];
    mgr.list_rect = Some(list);
    let visible = usize::from(list.height / 3);
    mgr.selected = mgr.selected.min(mgr.aliases.len().saturating_sub(1));
    if mgr.selected < mgr.list_offset {
        mgr.list_offset = mgr.selected;
    } else if mgr.selected >= mgr.list_offset + visible.max(1) {
        mgr.list_offset = mgr.selected + 1 - visible.max(1);
    }
    mgr.list_offset = mgr
        .list_offset
        .min(mgr.aliases.len().saturating_sub(visible));
    let end = (mgr.list_offset + visible).min(mgr.aliases.len());
    render_hint(f, rows[0], &format!("{} aliases", mgr.aliases.len()), t);
    if mgr.aliases.is_empty() {
        render_hint(f, list, "No aliases yet. Add an alias below.", t);
    }
    for (slot, index) in (mgr.list_offset..end).enumerate() {
        let (name, model) = &mgr.aliases[index];
        let rect = Rect::new(list.x, list.y + slot as u16 * 3, list.width, 3);
        render_choice(
            f,
            rect,
            &format!("{name}  →  {model}"),
            index == mgr.selected,
            mgr.hovered == Some(Hit::Row(index)),
            t,
        );
        mgr.row_hits.push((index, rect));
    }
    if mgr.aliases.len() > visible && visible > 0 {
        render_hint(
            f,
            rows[2],
            &format!(
                "{}–{} of {} · PgUp/PgDn",
                mgr.list_offset + 1,
                end,
                mgr.aliases.len()
            ),
            t,
        );
    }
    let action_width = |label: &str, shortcut: &str| {
        (crate::width::width(label) + crate::width::width(shortcut) + 4) as u16
    };
    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(action_width("New", "n")),
            Constraint::Length(1),
            Constraint::Min(action_width("Edit", "e")),
            Constraint::Length(1),
            Constraint::Min(action_width("Delete", "d")),
        ])
        .split(rows[3]);
    for (hit, label, shortcut, rect) in [
        (Hit::New, "New", "n", buttons[0]),
        (Hit::Edit, "Edit", "e", buttons[2]),
        (Hit::Delete, "Delete", "d", buttons[4]),
    ] {
        let disabled = hit != Hit::New && mgr.aliases.is_empty();
        render_action(
            f,
            rect,
            &Action {
                label,
                shortcut,
                hovered: mgr.hovered == Some(hit),
                disabled,
                ..Default::default()
            },
            t,
        );
        if !disabled {
            mgr.action_hits.push((hit, rect));
        }
    }
    let help = if area.width < 44 {
        "↑↓ select · Esc close"
    } else {
        "↑↓ select · Enter edit · Esc close"
    };
    render_hint(f, rows[4], help, t);
}

fn render_alias_form(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut AliasManager,
    t: &crate::theme::Theme,
) {
    let field_height = if area.width < 68 && area.height >= 12 {
        4
    } else {
        3
    };
    let gap = u16::from(area.height >= 18);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(field_height),
            Constraint::Length(gap),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(gap),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);
    let name = render_field(
        f,
        rows[0],
        &Field {
            label: "Alias name",
            value: mgr.editor.buf(),
            placeholder: "Enter a name",
            focused: mgr.focus == Focus::NameInput,
            hovered: mgr.hovered == Some(Hit::Name),
            cursor: Some(mgr.editor.cursor_display_col()),
            ..Default::default()
        },
        t,
    );
    mgr.last_input_rect = Some(name.value);
    mgr.name_hit = Some(name);
    render_model_search(f, rows[2], mgr, t);
    if area.width >= 74 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(rows[3]);
        render_tree_panel(f, cols[0], mgr, t);
        render_preview_panel(f, cols[1], mgr, t);
    } else {
        render_tree_panel(f, rows[3], mgr, t);
    }
    let page = mgr
        .tree_pages
        .iter()
        .position(|rows| rows.contains(&mgr.browser.selected_index()))
        .unwrap_or(0)
        + 1;
    let info = format!(
        "Page {page}/{} · Target: {}",
        mgr.tree_pages.len().max(1),
        mgr.target_model.as_deref().unwrap_or("none")
    );
    render_hint(f, rows[4], &info, t);
    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 3),
            Constraint::Length(1),
            Constraint::Ratio(1, 3),
            Constraint::Length(1),
            Constraint::Ratio(1, 3),
        ])
        .split(rows[6]);
    for (hit, label, shortcut, rect) in [
        (
            Hit::PreviousGroup,
            if buttons[0].width >= 15 {
                "Prev group"
            } else {
                "Group"
            },
            "←",
            buttons[0],
        ),
        (
            Hit::NextGroup,
            if buttons[2].width >= 15 {
                "Next group"
            } else {
                "Group"
            },
            "→",
            buttons[2],
        ),
        (Hit::Save, "Save", "^S", buttons[4]),
    ] {
        let disabled = match hit {
            Hit::Save => {
                mgr.pending.is_some()
                    || mgr.editor.buf().trim().is_empty()
                    || mgr.target_model.is_none()
            }
            _ => mgr.groups.is_empty(),
        };
        render_action(
            f,
            rect,
            &Action {
                label,
                shortcut,
                hovered: mgr.hovered == Some(hit),
                disabled,
                ..Default::default()
            },
            t,
        );
        if !disabled {
            mgr.action_hits.push((hit, rect));
            if hit == Hit::Save {
                mgr.save_rect = Some(rect);
            }
        }
    }
    let help = match mgr.focus {
        Focus::NameInput => "Tab models · Ctrl+S save",
        Focus::Tree => "↑↓ select · PgUp/PgDn · Tab name",
        Focus::Search => "Tab models · Ctrl+U clear",
    };
    render_hint(f, rows[7], help, t);
}

fn render_model_search(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut AliasManager,
    t: &crate::theme::Theme,
) {
    let inset = area.width.min(2);
    let label_width = 9.min(area.width.saturating_sub(inset));
    let clear_width = if mgr.search.buf().is_empty() { 0 } else { 3 };
    let record = Rect::new(
        area.x + inset,
        area.y,
        area.width.saturating_sub(inset * 2 + clear_width),
        area.height,
    );
    let value = Rect::new(
        record.x + label_width,
        record.y,
        record.width.saturating_sub(label_width),
        record.height,
    );
    let offset = if mgr.focus == Focus::Search {
        mgr.search
            .cursor_display_col()
            .saturating_sub(value.width.saturating_sub(1) as usize)
    } else {
        0
    };
    let bg = if mgr.hovered == Some(Hit::Search) {
        crate::theme::ThemeColor::new(crate::wm::shell::surface_bg(t)).lerp(t.work_hover_bg, 0.45)
    } else {
        crate::wm::shell::surface_bg(t)
    };
    f.render_widget(
        Paragraph::new("Search /").style(Style::default().fg(t.subtle_fg.into()).bg(bg)),
        record,
    );
    let query = if mgr.search.buf().is_empty() {
        "model name or ID"
    } else {
        mgr.search.buf()
    };
    f.render_widget(
        Paragraph::new(crate::width::trim_display_offset(
            query,
            offset,
            value.width as usize,
        ))
        .style(
            Style::default()
                .fg(if mgr.focus == Focus::Search {
                    t.tinted_fg.into()
                } else {
                    t.subtle_fg.into()
                })
                .bg(bg),
        ),
        value,
    );
    mgr.search_hit = Some(FieldHit {
        record,
        value,
        offset,
        arrows: None,
    });
    if clear_width > 0 {
        let clear = Rect::new(record.right(), record.y, clear_width, record.height);
        let clear_bg = if mgr.hovered == Some(Hit::ClearSearch) {
            crate::theme::ThemeColor::new(crate::wm::shell::surface_bg(t))
                .lerp(t.work_hover_bg, 0.45)
        } else {
            bg
        };
        f.render_widget(
            Paragraph::new(" × ").style(Style::default().fg(t.subtle_fg.into()).bg(clear_bg)),
            clear,
        );
        mgr.action_hits.push((Hit::ClearSearch, clear));
    }
    if mgr.focus == Focus::Search && value.width > 0 && value.height > 0 {
        f.set_cursor_position((
            value.x
                + mgr
                    .search
                    .cursor_display_col()
                    .saturating_sub(offset)
                    .min(value.width as usize - 1) as u16,
            value.y,
        ));
    }
}

impl crate::wm::modal::ModalOverlay for AliasManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.clear_geometry();
        let margin = u16::from(area.height >= 18);
        let mut area = Rect::new(
            area.x + area.width.min(1),
            area.y + margin,
            area.width.saturating_sub(2),
            area.height.saturating_sub(margin * 2),
        );
        if self.required_models {
            let hint = Paragraph::new(
                "Please set Smart and Cheap before running. Cheap can point to Smart. Esc cancels.",
            )
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(t.meta_fg.into()));
            let height = (hint.line_count(area.width) as u16).min(area.height);
            f.render_widget(hint, Rect::new(area.x, area.y, area.width, height));
            let offset = (height + 1).min(area.height);
            area.y += offset;
            area.height -= offset;
        }
        if let Some(message) = self
            .error
            .as_deref()
            .or_else(|| self.pending.as_ref().map(|_| "Saving…"))
        {
            let height = area.height.min(3);
            let status = Rect::new(
                area.x,
                area.bottom().saturating_sub(height),
                area.width,
                height,
            );
            f.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(if self.error.is_some() {
                        t.error.into()
                    } else {
                        t.meta_fg.into()
                    })),
                status,
            );
            area.height = area.height.saturating_sub(height + 1);
        }
        if let Some(alias) = self.confirm_delete.as_deref() {
            let (yes, no) = render_delete_confirm(f, area, alias, self.hovered, t);
            self.confirm_yes_rect = Some(yes);
            self.confirm_no_rect = Some(no);
            return;
        }
        if !self.show_form {
            render_alias_list(f, area, self, t);
            return;
        }
        render_alias_form(f, area, self, t);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&crate::ControlSender>,
    ) -> Option<ModalAction> {
        self.handle_key(action, tx);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        if !self.open || !self.show_form || self.confirm_delete.is_some() {
            return None;
        }
        let (rect, cursor, offset) = match self.focus {
            Focus::NameInput => (
                self.last_input_rect?,
                self.editor.cursor_display_col(),
                self.name_hit.map_or(0, |hit| hit.offset),
            ),
            Focus::Search => {
                let hit = self.search_hit?;
                (hit.value, self.search.cursor_display_col(), hit.offset)
            }
            Focus::Tree => return None,
        };
        (rect.width > 0 && rect.height > 0).then(|| {
            (
                rect.x + cursor.saturating_sub(offset).min(rect.width as usize - 1) as u16,
                rect.y,
            )
        })
    }

    fn handle_paste(&mut self, text: &str) {
        if self.pending.is_some() {
            return;
        }
        if self.open && self.show_form {
            match self.focus {
                Focus::NameInput => self.editor.paste_single_line(text),
                Focus::Search => {
                    self.search.paste_single_line(text);
                    self.filter_models();
                }
                Focus::Tree => {}
            }
        }
    }

    fn title(&self) -> Line<'static> {
        Line::from(if self.show_form {
            "Alias Form"
        } else {
            "Aliases"
        })
    }

    fn icon(&self) -> &str {
        "@"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

fn render_delete_confirm(
    f: &mut ratatui::Frame,
    area: Rect,
    name: &str,
    hovered: Option<Hit>,
    theme: &crate::theme::Theme,
) -> (Rect, Rect) {
    let width = area.width.min(60);
    let height = area.height.min(9);
    let inner = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(inner);
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Delete alias \"{name}\"?"),
                Style::default()
                    .fg(theme.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "This change cannot be undone.",
                Style::default().fg(theme.meta_fg.into()),
            )),
        ])
        .alignment(ratatui::layout::Alignment::Center)
        .wrap(Wrap { trim: false }),
        rows[0],
    );
    let buttons = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Ratio(1, 2),
            Constraint::Length(2),
            Constraint::Ratio(1, 2),
        ])
        .split(rows[1]);
    for (hit, label, shortcut, rect) in [
        (Hit::Confirm, "Delete", "Enter", buttons[0]),
        (Hit::Cancel, "Cancel", "Esc", buttons[2]),
    ] {
        render_action(
            f,
            rect,
            &Action {
                label,
                shortcut,
                hovered: hovered == Some(hit),
                ..Default::default()
            },
            theme,
        );
    }
    (buttons[0], buttons[2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_models_follow_alias_chains_and_confirmed_settings_receipts() {
        use crate::{settings::SettingsMutation, wm::modal::ModalOverlay};
        let dir = tempfile::tempdir().unwrap();
        let config = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
        std::fs::write(
            dir.path().join("config.toml"),
            "[providers.vendor]\ntype = 'openai'\napi_key = 'test-key'\n[models.example]\nprovider = 'vendor'\nmodel = 'example'\ncontext_budget = 8192\n[models.unbound]\ncontext_budget = 8192\n[alias.smart]\nmodel = 'unbound'\n[alias.cheap]\nmodel = 'smart'\n",
        )
        .unwrap();
        config.reload().unwrap();
        let mut manager = AliasManager {
            settings: config.clone().into(),
            ..Default::default()
        };
        assert!(manager.require_execution_models());
        assert_eq!(manager.editor.buf(), "smart");
        assert!(manager.open && manager.show_form);
        let (tx, mut rx) = crate::ControlSender::channel();
        manager.handle_key(&KeyAction::Escape, Some(&tx));
        assert!(!manager.open);
        assert!(rx.try_recv().is_err());
        assert!(manager.require_execution_models());
        manager.select_model("example");
        let app = crate::app::AppState::new("test".into(), None);
        let theme = crate::theme::theme();
        for width in [42, 100] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 36)).unwrap();
            terminal
                .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(text.contains("Please set Smart and Cheap"));
            assert!(!manager.row_hits.is_empty());
        }
        let save = manager.save_rect.unwrap();
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                save.x,
                save.y,
            ),
            Some(&tx),
        );
        let crate::TuiControl::MutateSettings(request) = rx.try_recv().unwrap() else {
            panic!("settings request expected");
        };
        assert!(
            matches!(request.action, SettingsMutation::DefaultModel { ref model } if model == "example")
        );
        assert!(!manager.mutation_result(uuid::Uuid::now_v7(), &Ok("saved".into())));
        assert!(manager.open);
        manager.mutation_result(request.id, &Err("write failed".into()));
        assert!(manager.open && manager.show_form);
        assert!(
            manager
                .settings
                .model_entry("smart")
                .unwrap()
                .provider
                .is_none()
        );
        manager.handle_key(&KeyAction::Save, Some(&tx));
        let crate::TuiControl::MutateSettings(request) = rx.try_recv().unwrap() else {
            panic!("retry request expected");
        };
        config.bind_default_model("example").unwrap();
        manager.mutation_result(request.id, &Ok("saved".into()));
        assert!(!manager.open);
        assert!(!manager.require_execution_models());
        assert_eq!(config.models().resolve_alias("cheap"), "example");

        config
            .update_alias(Some("cheap"), "cheap", "missing")
            .unwrap();
        assert!(manager.require_execution_models());
        assert_eq!(manager.editor.buf(), "cheap");
        manager.select_model("example");
        manager.handle_key(&KeyAction::Save, Some(&tx));
        let crate::TuiControl::MutateSettings(request) = rx.try_recv().unwrap() else {
            panic!("cheap settings request expected");
        };
        assert!(
            matches!(request.action, SettingsMutation::UpdateAlias { ref name, .. } if name == "cheap")
        );
        config
            .update_alias(Some("cheap"), "cheap", "example")
            .unwrap();
        manager.mutation_result(request.id, &Ok("saved".into()));
        assert!(!manager.open);

        config.remove_alias("smart").unwrap();
        assert!(manager.require_execution_models());
        manager.select_model("example");
        manager.handle_key(&KeyAction::Save, Some(&tx));
        let crate::TuiControl::MutateSettings(request) = rx.try_recv().unwrap() else {
            panic!("smart settings request expected");
        };
        config.bind_default_model("example").unwrap();
        manager.mutation_result(request.id, &Ok("saved".into()));
        assert_eq!(
            config
                .models()
                .all_aliases()
                .iter()
                .find(|(name, _)| name == "cheap")
                .unwrap()
                .1,
            "example"
        );
        assert!(!manager.open);
    }

    #[test]
    fn tree_directional_selector_wraps_providers() {
        let mut manager = AliasManager {
            show_form: true,
            focus: Focus::Tree,
            groups: vec![provider_group("alpha"), provider_group("beta")],
            model_idx: vec![0, 0],
            ..Default::default()
        };

        manager.handle_key(&KeyAction::CursorLeft, None);
        assert_eq!(manager.provider_idx, 1);
        manager.handle_key(&KeyAction::CursorRight, None);
        assert_eq!(manager.provider_idx, 0);
    }

    #[test]
    fn tab_moves_from_alias_name_to_model_tree() {
        let mut manager = AliasManager {
            show_form: true,
            focus: Focus::NameInput,
            groups: vec![provider_group("alpha")],
            model_idx: vec![0],
            browser: ModelBrowser::new(
                vec![BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "model".into(),
                    value: "vendor/model".into(),
                    selectable: true,
                }],
                None,
            ),
            ..Default::default()
        };

        manager.handle_key(&KeyAction::Tab, None);

        assert_eq!(manager.focus, Focus::Tree);
        assert!(
            <AliasManager as crate::wm::modal::ModalOverlay>::cursor_position(&manager).is_none()
        );

        manager.handle_key(&KeyAction::Tab, None);
        assert_eq!(manager.focus, Focus::NameInput);
    }

    #[test]
    fn alias_cursor_and_paste_follow_name_focus() {
        let mut manager = AliasManager {
            open: true,
            show_form: true,
            focus: Focus::NameInput,
            last_input_rect: Some(Rect::new(10, 4, 20, 1)),
            ..Default::default()
        };

        <AliasManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "fast");
        assert_eq!(manager.editor.buf(), "fast");
        assert_eq!(
            <AliasManager as crate::wm::modal::ModalOverlay>::cursor_position(&manager),
            Some((14, 4))
        );

        manager.focus = Focus::Tree;
        <AliasManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "ignored");
        assert_eq!(manager.editor.buf(), "fast");
        assert!(
            <AliasManager as crate::wm::modal::ModalOverlay>::cursor_position(&manager).is_none()
        );
    }

    #[test]
    fn delete_requires_confirmation_and_escape_cancels_it() {
        let mut manager = AliasManager {
            aliases: vec![("fast".into(), "vendor/model".into())],
            ..Default::default()
        };

        manager.handle_key(&KeyAction::Char('d'), None);
        assert_eq!(manager.confirm_delete.as_deref(), Some("fast"));

        manager.handle_key(&KeyAction::Escape, None);
        assert!(manager.confirm_delete.is_none());
        assert_eq!(manager.aliases.len(), 1);
    }

    #[test]
    fn delete_confirmation_renders_and_mouse_cancel_closes_it() {
        let mut manager = AliasManager {
            confirm_delete: Some("fast".into()),
            ..Default::default()
        };
        let app = crate::app::AppState::new("session".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap();

        terminal
            .draw(|frame| {
                <AliasManager as crate::wm::modal::ModalOverlay>::render_content(
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
        assert!(content.contains("Delete alias \"fast\"?"));
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

    fn populated_manager() -> AliasManager {
        let models = (0..16)
            .map(|index| atman_runtime::model_registry::ModelRow {
                slug: format!("vendor/model-{index:02}"),
                provider_name: "vendor".into(),
                context_budget: 128_000,
                max_output_tokens: Some(16_000),
                thinking: true,
                reasoning: Default::default(),
                capabilities: Default::default(),
                image_detail: Default::default(),
            })
            .collect();
        let mut manager = AliasManager {
            open: true,
            groups: vec![atman_runtime::model_registry::ProviderGroup {
                provider_name: "vendor".into(),
                models,
            }],
            model_idx: vec![0],
            ..Default::default()
        };
        manager.sync_browser(None);
        manager
    }

    #[test]
    fn settings_save_waits_for_matching_ack_and_preserves_failed_draft() {
        let mut manager = populated_manager();
        manager.prepare_form(true, Some("smart"), Some("vendor/model-09"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = crate::ControlSender::from(tx);
        manager.handle_key(&KeyAction::Save, Some(&tx));
        let crate::TuiControl::MutateSettings(first) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert!(manager.show_form);
        manager.handle_key(&KeyAction::Save, Some(&tx));
        assert!(rx.try_recv().is_err());
        assert!(!manager.mutation_result(uuid::Uuid::now_v7(), &Ok("saved".into())));
        assert!(manager.mutation_result(first.id, &Err("disk unavailable".into())));
        assert!(manager.show_form);
        assert_eq!(manager.editor.buf(), "smart");
        assert_eq!(manager.error.as_deref(), Some("disk unavailable"));
        manager.handle_key(&KeyAction::Save, Some(&tx));
        let crate::TuiControl::MutateSettings(retry) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert!(manager.mutation_result(retry.id, &Ok("saved".into())));
        assert!(!manager.show_form);
        manager.prepare_form(false, None, Some("vendor/model-09"));
        assert!(!manager.mutation_result(first.id, &Ok("saved".into())));
        assert!(manager.show_form);
    }

    #[test]
    fn catalog_refresh_preserves_alias_draft_focus_target_and_pending_save() {
        let mut manager = populated_manager();
        manager.prepare_form(true, Some("smart"), Some("vendor/model-09"));
        manager.editor.replace_with("new alias");
        manager.search.replace_with("model-09");
        manager.focus = Focus::Search;
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        manager.commit_alias(Some(&crate::ControlSender::from(tx)));
        let request = manager.pending.as_ref().unwrap().id;
        manager.catalog_changed();
        assert_eq!(manager.pending.as_ref().unwrap().id, request);
        assert_eq!(manager.editor.buf(), "new alias");
        assert_eq!(manager.search.buf(), "model-09");
        assert_eq!(manager.focus, Focus::Search);
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));
        assert!(manager.show_form);
    }

    #[test]
    fn editing_preserves_the_target_until_a_model_is_explicitly_selected() {
        let mut manager = populated_manager();
        manager.prepare_form(true, Some("smart"), Some("vendor/model-09"));
        assert_eq!(manager.current_model().unwrap().slug, "vendor/model-09");
        assert_eq!(manager.focus, Focus::NameInput);
        manager.handle_key(&KeyAction::Char('x'), None);
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));

        manager.prepare_form(true, Some("offline"), Some("missing/model"));
        assert!(manager.current_model().is_none());
        manager.handle_key(&KeyAction::Tab, None);
        manager.handle_key(&KeyAction::BackTab, None);
        assert_eq!(manager.target_model.as_deref(), Some("missing/model"));
        manager.handle_key(&KeyAction::Tab, None);
        manager.handle_key(&KeyAction::HistoryDown, None);
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-01"));
    }

    #[test]
    fn alias_records_keep_scroll_indices_padding_and_mouse_targets_together() {
        use crate::wm::modal::ModalOverlay;
        let mut manager = populated_manager();
        manager.aliases = (0..16)
            .map(|index| (format!("alias-{index}"), format!("vendor/model-{index:02}")))
            .collect();
        manager.selected = 15;
        let app = crate::app::AppState::new("test".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 18)).unwrap();
        terminal
            .draw(|frame| manager.render_content(frame, Rect::new(3, 3, 34, 14), &app, &theme))
            .unwrap();
        let (index, rect) = manager.row_hits[0];
        assert!(index > 0);
        assert_eq!(rect.height, 3);
        for row in rect.y..rect.bottom() {
            assert_eq!(manager.hit_at((rect.x, row)), Some(Hit::Row(index)));
        }
        for &(index, rect) in &manager.row_hits {
            if index == manager.selected {
                for row in rect.y..rect.bottom() {
                    assert_eq!(terminal.backend().buffer()[(rect.x, row)].symbol(), "▌");
                }
            }
        }
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                rect.x,
                rect.y,
            ),
            None,
        );
        assert_eq!(manager.selected, index);
        assert_eq!(manager.action_hits.len(), 3);
        for &(action, rect) in &manager.action_hits {
            let text = (rect.x..rect.right())
                .map(|x| terminal.backend().buffer()[(x, rect.y + 1)].symbol())
                .collect::<String>();
            let label = match action {
                Hit::New => "New  n",
                Hit::Edit => "Edit  e",
                Hit::Delete => "Delete  d",
                _ => unreachable!(),
            };
            assert!(text.contains(label), "{text:?} is missing {label:?}");
        }
        let content = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(content.contains("Esc close"));
        let new = manager
            .action_hits
            .iter()
            .find(|(hit, _)| *hit == Hit::New)
            .unwrap()
            .1;
        manager.prepare_form(false, None, None);
        assert!(manager.row_hits.is_empty());
        assert!(manager.action_hits.is_empty());
        assert_eq!(manager.hit_at((new.x, new.y)), None);
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                new.x,
                new.y,
            ),
            None,
        );
        assert!(manager.editor.buf().is_empty());
        terminal
            .draw(|frame| manager.render_content(frame, Rect::new(3, 3, 34, 14), &app, &theme))
            .unwrap();
        let name = manager.name_hit.unwrap().record;
        manager.handle_key(&KeyAction::Escape, None);
        assert!(!manager.show_form);
        assert!(manager.last_input_rect.is_none());
        assert!(manager.name_hit.is_none());
        assert!(manager.row_hits.is_empty());
        assert_eq!(manager.hit_at((name.x, name.y)), None);
    }

    #[test]
    fn alias_form_cursor_and_model_mouse_selection_use_rendered_geometry() {
        use crate::wm::modal::ModalOverlay;
        for (width, height) in [(40, 14), (80, 24)] {
            let mut manager = populated_manager();
            manager.prepare_form(
                true,
                Some("长名字-alias-name-that-needs-horizontal-scrolling-across-the-entire-input-field"),
                Some("vendor/model-09"),
            );
            let app = crate::app::AppState::new("test".into(), None);
            let theme = crate::theme::theme();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
                .unwrap();
            let hit = manager.name_hit.unwrap();
            let cursor = manager.cursor_position().unwrap();
            assert!(rect_hit(Some(hit.value), cursor));
            assert!(hit.offset > 0);
            let expected = crate::input::cursor_from_wrapped(
                manager.editor.buf(),
                0,
                hit.offset + 1,
                usize::MAX,
            );
            manager.handle_mouse(
                &mouse(
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    hit.value.x + 1,
                    hit.value.y,
                ),
                None,
            );
            assert_eq!(manager.editor.cursor(), expected);
            let (index, rect) = manager.row_hits[0];
            let model = manager.browser.rows()[index].value.clone();
            manager.handle_mouse(
                &mouse(
                    crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                    rect.x,
                    rect.bottom() - 1,
                ),
                None,
            );
            assert_eq!(manager.focus, Focus::Tree);
            assert_eq!(manager.target_model.as_deref(), Some(model.as_str()));
            assert!(manager.cursor_position().is_none());
            terminal
                .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
                .unwrap();
            let save = manager.save_rect.unwrap();
            assert_eq!(save.height, 3);
            assert_eq!(manager.hit_at((save.x, save.bottom() - 1)), Some(Hit::Save));
        }
    }

    fn mouse(
        kind: crossterm::event::MouseEventKind,
        column: u16,
        row: u16,
    ) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn provider_group(name: &str) -> atman_runtime::model_registry::ProviderGroup {
        atman_runtime::model_registry::ProviderGroup {
            provider_name: name.to_string(),
            models: Vec::new(),
        }
    }

    #[test]
    fn search_and_group_changes_never_replace_the_saved_alias_target() {
        let mut manager = populated_manager();
        manager.groups.push(provider_group("empty-group"));
        manager.model_idx.push(0);
        manager.prepare_form(true, Some("smart"), Some("vendor/model-09"));
        manager.handle_key(&KeyAction::Tab, None);
        manager.handle_key(&KeyAction::Char('/'), None);
        <AliasManager as crate::wm::modal::ModalOverlay>::handle_paste(&mut manager, "model-01");
        assert_eq!(
            manager
                .browser
                .rows()
                .iter()
                .filter(|row| row.selectable)
                .count(),
            1
        );
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));
        manager.handle_key(&KeyAction::Tab, None);
        assert_eq!(manager.focus, Focus::Tree);
        manager.handle_key(&KeyAction::CursorLeft, None);
        assert_eq!(manager.group_scope.as_deref(), Some("empty-group"));
        assert!(!manager.browser.rows().iter().any(|row| row.selectable));
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));
        manager.handle_key(&KeyAction::CursorRight, None);
        assert!(manager.group_scope.is_none());
        manager.handle_key(&KeyAction::Char('/'), None);
        manager.handle_key(&KeyAction::RemoveAttachment, None);
        assert!(manager.search.buf().is_empty());
        assert_eq!(
            manager
                .browser
                .rows()
                .iter()
                .filter(|row| row.kind == BrowserRowKind::Provider)
                .count(),
            2
        );
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        manager.commit_alias(Some(&crate::ControlSender::from(tx)));
        assert!(
            matches!(rx.try_recv().unwrap(), crate::TuiControl::MutateSettings(crate::settings::SettingsMutationRequest { action: crate::settings::SettingsMutation::UpdateAlias { name, model, .. }, .. }) if name == "smart" && model == "vendor/model-09")
        );
    }

    #[test]
    fn model_search_matches_configuration_names_and_bound_api_ids() {
        let manager = populated_manager();
        let model = &manager.groups[0].models[0];
        let mut entry = atman_runtime::model_registry::ModelEntry {
            provider: Some("vendor".into()),
            model: "upstream/reasoning-model".into(),
            ..Default::default()
        };
        assert!(model_matches_search(model, Some(&entry), "model-00"));
        assert!(model_matches_search(model, Some(&entry), "reasoning"));
        entry.provider = Some("different-provider".into());
        assert!(!model_matches_search(model, Some(&entry), "reasoning"));
    }

    #[test]
    fn search_cursor_clear_hit_and_grouped_page_geometry_remain_consistent() {
        use crate::wm::modal::ModalOverlay;
        let mut manager = populated_manager();
        manager.prepare_form(true, Some("smart"), Some("vendor/model-09"));
        let app = crate::app::AppState::new("test".into(), None);
        let theme = crate::theme::theme();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 14)).unwrap();
        manager.focus = Focus::Search;
        manager
            .search
            .replace_with("长查询文本-model-which-does-not-exist");
        manager.filter_models();
        terminal
            .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
            .unwrap();
        let hit = manager.search_hit.unwrap();
        assert!(hit.offset > 0);
        assert!(rect_hit(
            Some(hit.value),
            manager.cursor_position().unwrap()
        ));
        let expected =
            crate::input::cursor_from_wrapped(manager.search.buf(), 0, hit.offset + 1, usize::MAX);
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                hit.value.x + 1,
                hit.value.y,
            ),
            None,
        );
        assert_eq!(manager.search.cursor(), expected);
        let clear = manager
            .action_hits
            .iter()
            .find(|(hit, _)| *hit == Hit::ClearSearch)
            .unwrap()
            .1;
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                clear.x,
                clear.y,
            ),
            None,
        );
        assert!(manager.search.buf().is_empty());
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-09"));
        manager.handle_key(&KeyAction::Tab, None);
        terminal
            .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
            .unwrap();
        assert!(!manager.row_hits.is_empty());
        let page = manager
            .tree_pages
            .iter()
            .find(|page| page.contains(&manager.browser.selected_index()))
            .unwrap();
        assert_eq!(
            manager.browser.rows()[page[0]].kind,
            BrowserRowKind::Provider
        );
        assert!(
            manager.row_hits.iter().all(|(_, rect)| rect.height == 3
                && rect.bottom() <= manager.list_rect.unwrap().bottom())
        );
        let before = manager.browser.selected_index();
        manager.handle_key(&KeyAction::PageDown, None);
        assert!(manager.browser.selected_index() > before);
        terminal
            .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
            .unwrap();
        let next = manager
            .action_hits
            .iter()
            .find(|(hit, _)| *hit == Hit::NextGroup)
            .unwrap()
            .1;
        let target = manager.target_model.clone();
        manager.handle_mouse(
            &mouse(
                crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                next.x,
                next.bottom() - 1,
            ),
            None,
        );
        assert_eq!(manager.group_scope.as_deref(), Some("vendor"));
        assert_eq!(manager.target_model, target);
        manager.select_model("vendor/model-15");
        terminal
            .draw(|frame| manager.render_content(frame, frame.area(), &app, &theme))
            .unwrap();
        manager.handle_key(&KeyAction::PageDown, None);
        assert_eq!(manager.target_model.as_deref(), Some("vendor/model-15"));
    }
}
