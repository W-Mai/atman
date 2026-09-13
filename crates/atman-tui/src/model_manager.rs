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
    next_request_id: u64,
    pending: Option<crate::ModelMutationRequest>,
    confirm_delete: Option<String>,
    list_rect: Option<Rect>,
    save_rect: Option<Rect>,
    confirm_yes_rect: Option<Rect>,
    confirm_no_rect: Option<Rect>,
}

impl ModelManager {
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
    }

    pub fn has_text_focus(&self) -> bool {
        self.show_form
    }

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if self.pending.is_some() {
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
            MouseEventKind::Down(MouseButton::Left) if rect_hit(self.save_rect, point) => {
                self.handle_key(&KeyAction::Submit, control_tx);
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

    fn reasoning_choices(&self) -> Vec<String> {
        let selections = self
            .editing
            .as_deref()
            .filter(|model| {
                atman_runtime::model_registry::model_entry(model)
                    .and_then(|entry| entry.provider)
                    .as_deref()
                    == Some(self.selected_provider.as_str())
            })
            .map(atman_runtime::model_registry::reasoning_selections_for_model)
            .unwrap_or_else(|| {
                atman_runtime::model_registry::reasoning_selections_for_provider(
                    &self.selected_provider,
                    &atman_runtime::provider::ModelCapabilities::default(),
                )
            });
        selections
            .into_iter()
            .map(|selection| selection.to_string())
            .collect()
    }

    fn open_form(&mut self) {
        self.show_form = true;
        self.form_field = 0;
        self.editing = None;
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
        if !self.show_form || self.form_field == 2 {
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
    }

    fn handle_form_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        match action {
            KeyAction::Escape | KeyAction::Submit => self.commit_form(control_tx),
            KeyAction::Tab => self.form_field = (self.form_field + 1) % 6,
            KeyAction::BackTab => {
                self.form_field = if self.form_field == 0 {
                    5
                } else {
                    self.form_field - 1
                };
            }
            KeyAction::CursorLeft | KeyAction::CursorRight if matches!(self.form_field, 2 | 4) => {
                let Some(direction) =
                    crate::directional_selector::SelectorDirection::from_key(action)
                else {
                    return;
                };
                if self.form_field == 2 {
                    if self.locked_provider.is_some() {
                        return;
                    }
                    let providers: Vec<&str> = self
                        .groups
                        .iter()
                        .map(|group| group.provider_name.as_str())
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
                        self.selected_provider = providers[selected].to_string();
                    }
                } else {
                    let choices = self.reasoning_choices();
                    let mut selected = choices
                        .iter()
                        .position(|choice| choice == self.thinking_editor.buf().trim())
                        .unwrap_or(0);
                    crate::directional_selector::move_wrapped(
                        &mut selected,
                        choices.len(),
                        direction,
                    );
                    self.thinking_editor.replace_with(&choices[selected]);
                }
            }
            KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Backspace
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward
            | KeyAction::Char(_)
            | KeyAction::Newline
                if matches!(self.form_field, 0 | 1 | 3 | 4 | 5) =>
            {
                let editor = match self.form_field {
                    0 => &mut self.name_editor,
                    1 => &mut self.model_editor,
                    3 => &mut self.context_budget_editor,
                    4 => &mut self.thinking_editor,
                    5 => &mut self.max_tokens_editor,
                    _ => unreachable!(),
                };
                editor.handle_key(action);
            }
            _ => {}
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
        let fields: [(&str, String); 6] = [
            ("Name", self.name_editor.buf().to_string()),
            ("Model ID", self.model_editor.buf().to_string()),
            ("Provider", self.selected_provider.clone()),
            (
                "Context Budget",
                self.context_budget_editor.buf().to_string(),
            ),
            ("Reasoning", self.thinking_editor.buf().to_string()),
            ("Max Tokens", self.max_tokens_editor.buf().to_string()),
        ];
        let label_style = Style::default().fg(t.meta_fg.into());
        let mut cursor_pos: Option<(u16, u16)> = None;
        let mut y = inner.y;
        for (i, (label, val)) in fields.iter().enumerate() {
            if y >= inner.bottom() {
                break;
            }
            let active = i == self.form_field;
            let val_style = if matches!(i, 2 | 4) {
                crate::directional_selector::value_style(t, active)
            } else if active {
                Style::default().fg(t.accent.into())
            } else {
                Style::default()
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {label:<16}"), label_style),
                    Span::styled(val.as_str(), val_style),
                ])),
                Rect { y, ..inner },
            );
            if active && i != 2 {
                let prefix = format!(" {label:<16}");
                let prefix_w = crate::width::width(&prefix) as u16;
                let cursor_w = match i {
                    0 => self.name_editor.cursor_display_col(),
                    1 => self.model_editor.cursor_display_col(),
                    3 => self.context_budget_editor.cursor_display_col(),
                    4 => self.thinking_editor.cursor_display_col(),
                    5 => self.max_tokens_editor.cursor_display_col(),
                    _ => 0,
                } as u16;
                cursor_pos = Some((inner.x + prefix_w + cursor_w, y));
            }
            y += 1;
        }
        y += 1;
        if y < inner.bottom() {
            self.save_rect = Some(Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            });
            let help = if matches!(self.form_field, 2 | 4) {
                crate::directional_selector::footer_help(" Tab: next field", "Enter/Esc: save")
            } else {
                " Tab: next field  Enter/Esc: save".to_string()
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    help,
                    Style::default().fg(t.meta_fg.into()),
                ))),
                Rect { y, ..inner },
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
