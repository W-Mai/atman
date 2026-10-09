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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Name,
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
    pub open: bool,
    pub aliases: Vec<(String, String)>,
    pub selected: usize,
    pub show_form: bool,
    pub last_input_rect: Option<Rect>,
    is_edit: bool,
    editor: InputEditor,
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

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
        self.confirm_delete = None;
        self.clear_geometry();
        self.hovered = None;
    }

    fn clear_geometry(&mut self) {
        self.last_input_rect = None;
        self.name_hit = None;
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
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (event.column, event.row);
        let target = self.hit_at(point);
        match event.kind {
            MouseEventKind::Moved => self.hovered = target,
            MouseEventKind::Down(MouseButton::Left) => {
                self.hovered = target;
                match target {
                    Some(Hit::Confirm | Hit::Save) => {
                        self.handle_key(&KeyAction::Submit, control_tx);
                    }
                    Some(Hit::Cancel) => self.handle_key(&KeyAction::Escape, control_tx),
                    Some(Hit::New) => self.handle_key(&KeyAction::Char('n'), control_tx),
                    Some(Hit::Edit) => self.handle_key(&KeyAction::Char('e'), control_tx),
                    Some(Hit::Delete) => self.handle_key(&KeyAction::Char('d'), control_tx),
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
        self.aliases = atman_runtime::model_registry::all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
        self.list_offset = 0;
    }

    fn refresh_groups(&mut self) {
        let enabled_providers = atman_runtime::model_registry::enabled_provider_names();
        self.groups = atman_runtime::model_registry::all_provider_groups()
            .into_iter()
            .filter(|g| enabled_providers.contains(&g.provider_name))
            .collect();
        self.model_idx = vec![0; self.groups.len()];
        self.provider_idx = 0;
        self.sync_browser(None);
        self.focus = Focus::NameInput;
    }

    fn sync_browser(&mut self, selected: Option<&str>) {
        let mut rows = Vec::new();
        for group in &self.groups {
            rows.extend(group.models.iter().map(|model| BrowserRow {
                kind: BrowserRowKind::Model,
                label: model.slug.clone(),
                value: model.slug.clone(),
                selectable: true,
            }));
        }
        self.browser.replace_rows(rows, selected);
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
        self.clear_geometry();
        self.show_form = true;
        self.is_edit = is_edit;
        self.hovered = None;
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
    }

    fn current_model(&self) -> Option<&atman_runtime::model_registry::ModelRow> {
        let target = self.target_model.as_deref()?;
        self.groups
            .iter()
            .flat_map(|group| &group.models)
            .find(|model| model.slug == target)
    }

    pub fn handle_key(
        &mut self,
        action: &KeyAction,
        control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        if let Some(alias) = self.confirm_delete.clone() {
            match action {
                KeyAction::Submit | KeyAction::Char('y') | KeyAction::Char('Y') => {
                    match atman_runtime::config_hub::ConfigHub::global()
                        .and_then(|hub| hub.remove_alias(&alias))
                    {
                        Ok(()) => {
                            self.confirm_delete = None;
                            self.clear_geometry();
                            self.refresh_list();
                        }
                        Err(error) => {
                            atman_runtime::notify!(error, "Alias {alias:?} delete failed: {error}");
                        }
                    }
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
                    if !self.browser.rows().is_empty() {
                        self.focus = Focus::Tree;
                    }
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
                KeyAction::Escape | KeyAction::Submit => self.commit_alias(control_tx),
                KeyAction::Tab | KeyAction::BackTab => self.focus = Focus::NameInput,
                KeyAction::HistoryUp
                | KeyAction::Char('k')
                | KeyAction::PageUp
                | KeyAction::Home => {
                    self.browser.handle_key(action, 0);
                    self.apply_browser_selection();
                }
                KeyAction::HistoryDown
                | KeyAction::Char('j')
                | KeyAction::PageDown
                | KeyAction::End => {
                    self.browser.handle_key(action, 0);
                    self.apply_browser_selection();
                }
                KeyAction::CursorLeft | KeyAction::CursorRight => {
                    let Some(direction) =
                        crate::directional_selector::SelectorDirection::from_key(action)
                    else {
                        return;
                    };
                    crate::directional_selector::move_wrapped(
                        &mut self.provider_idx,
                        self.groups.len(),
                        direction,
                    );
                    let selected = self
                        .groups
                        .get(self.provider_idx)
                        .and_then(|group| group.models.get(self.model_idx[self.provider_idx]))
                        .map(|model| model.slug.clone());
                    if let Some(selected) = selected {
                        self.select_model(&selected);
                    }
                }
                _ => {}
            },
        }
    }

    fn commit_alias(
        &mut self,
        _control_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let alias = self.editor.buf().trim().to_string();
        if alias.is_empty() {
            return;
        }
        let Some(slug) = self.target_model.as_deref().filter(|slug| !slug.is_empty()) else {
            return;
        };
        let result = atman_runtime::config_hub::ConfigHub::global().and_then(|hub| {
            if self.is_edit {
                hub.update_alias(self.edit_original.as_deref(), &alias, slug)
            } else {
                hub.add_alias(&alias, slug)
            }
        });
        match result {
            Ok(()) => {
                self.refresh_list();
                self.show_form = false;
                self.clear_geometry();
            }
            Err(error) => {
                atman_runtime::notify!(error, "Alias {alias:?} save failed: {error}");
            }
        }
    }
}

fn rect_hit(rect: Option<Rect>, point: (u16, u16)) -> bool {
    rect.is_some_and(|rect| {
        point.0 >= rect.x && point.0 < rect.right() && point.1 >= rect.y && point.1 < rect.bottom()
    })
}

fn render_tree_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &mut AliasManager,
    theme: &crate::theme::Theme,
) {
    mgr.list_rect = Some(area);
    let visible = usize::from(area.height / 3);
    mgr.browser.set_visible_rows(visible);
    if mgr.browser.rows().is_empty() {
        render_hint(f, area, "No enabled models available", theme);
    }
    for (slot, index) in mgr.browser.visible_rows(visible).take(visible).enumerate() {
        let row = &mgr.browser.rows()[index];
        let selected =
            mgr.focus == Focus::Tree && mgr.target_model.as_deref() == Some(row.value.as_str());
        let rect = Rect::new(area.x, area.y + slot as u16 * 3, area.width, 3);
        render_choice(
            f,
            rect,
            &row.label,
            selected,
            mgr.hovered == Some(Hit::Row(index)),
            theme,
        );
        mgr.row_hits.push((index, rect));
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
            atman_runtime::model_registry::provider_display_name(&m.provider_name),
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
    let gap = u16::from(area.height >= 17);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(field_height),
            Constraint::Length(gap),
            Constraint::Min(0),
            Constraint::Length(u16::from(area.height >= 11)),
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
    if area.width >= 74 {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(rows[2]);
        render_tree_panel(f, cols[0], mgr, t);
        render_preview_panel(f, cols[1], mgr, t);
    } else {
        render_tree_panel(f, rows[2], mgr, t);
    }
    let info = if mgr.current_model().is_none() {
        mgr.target_model.as_ref().map_or_else(
            || "Select a model".to_owned(),
            |target| format!("Retaining saved target: {target}"),
        )
    } else {
        format!(
            "{} / {} models · ←→ provider",
            mgr.browser.selected_index() + 1,
            mgr.browser.rows().len()
        )
    };
    render_hint(f, rows[3], &info, t);
    let disabled = mgr.editor.buf().trim().is_empty() || mgr.target_model.is_none();
    render_action(
        f,
        rows[5],
        &Action {
            label: "Save",
            shortcut: "Enter",
            hovered: mgr.hovered == Some(Hit::Save),
            disabled,
            ..Default::default()
        },
        t,
    );
    if !disabled {
        mgr.save_rect = Some(rows[5]);
        mgr.action_hits.push((Hit::Save, rows[5]));
    }
    let help = if disabled {
        "Tab focus · ↑↓ · Esc back"
    } else if area.width < 55 {
        "Tab focus · ↑↓ · Esc save"
    } else {
        "Tab/Shift+Tab: name / model · ↑↓: model · Esc: save"
    };
    render_hint(f, rows[6], help, t);
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
        let area = Rect::new(
            area.x + area.width.min(1),
            area.y + margin,
            area.width.saturating_sub(2),
            area.height.saturating_sub(margin * 2),
        );
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
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action, tx);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        (self.open
            && self.show_form
            && self.focus == Focus::NameInput
            && self.confirm_delete.is_none())
        .then_some(self.last_input_rect)
        .flatten()
        .filter(|rect| rect.width > 0 && rect.height > 0)
        .map(|rect| {
            let cursor = self.editor.cursor_display_col();
            let offset = self.name_hit.map_or(0, |hit| hit.offset);
            (
                rect.x + cursor.saturating_sub(offset).min(rect.width as usize - 1) as u16,
                rect.y,
            )
        })
    }

    fn handle_paste(&mut self, text: &str) {
        if self.open && self.show_form && self.focus == Focus::NameInput {
            self.editor.paste_single_line(text);
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
}
