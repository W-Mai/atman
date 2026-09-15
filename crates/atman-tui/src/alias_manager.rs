use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::model_browser::{BrowserRow, BrowserRowKind, ModelBrowser};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    NameInput,
    Tree,
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
    groups: Vec<atman_runtime::model_registry::ProviderGroup>,
    focus: Focus,
    provider_idx: usize,
    model_idx: Vec<usize>,
    browser: ModelBrowser,
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
        self.open = true;
        self.refresh_list();
        self.refresh_groups();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
        self.confirm_delete = None;
        self.last_input_rect = None;
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
                self.selected = usize::from(event.row.saturating_sub(rect.y))
                    .min(self.aliases.len().saturating_sub(1));
                if button == MouseButton::Right {
                    self.handle_key(&KeyAction::Char('d'), control_tx);
                }
            }
            _ => {}
        }
    }

    pub fn refresh_list(&mut self) {
        self.aliases = atman_runtime::model_registry::all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
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
            rows.push(BrowserRow {
                kind: BrowserRowKind::Provider,
                label: atman_runtime::model_registry::provider_display_name(&group.provider_name),
                value: group.provider_name.clone(),
                selectable: false,
            });
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
        self.show_form = true;
        self.is_edit = is_edit;
        let mut ed = InputEditor::default();
        if let Some(a) = alias {
            ed.insert_str(a);
        }
        self.editor = ed;
        self.edit_original = alias.map(|s| s.to_string());
        self.refresh_groups();
        self.focus = Focus::NameInput;
        if let Some(m) = model {
            self.select_model(m);
        }
    }

    fn select_model(&mut self, slug: &str) {
        for (pi, g) in self.groups.iter().enumerate() {
            if let Some(mi) = g.models.iter().position(|m| m.slug == slug) {
                self.provider_idx = pi;
                self.model_idx[pi] = mi;
                self.sync_browser(Some(slug));
                self.focus = Focus::Tree;
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
    }

    fn current_model(&self) -> Option<&atman_runtime::model_registry::ModelRow> {
        self.groups
            .get(self.provider_idx)
            .and_then(|g| g.models.get(self.model_idx[self.provider_idx]))
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
                            self.refresh_list();
                        }
                        Err(error) => {
                            atman_runtime::notify!(error, "Alias {alias:?} delete failed: {error}");
                        }
                    }
                }
                KeyAction::Escape | KeyAction::Char('n') | KeyAction::Char('N') => {
                    self.confirm_delete = None;
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
                KeyAction::Char('n') => self.open_form(false, None, None),
                KeyAction::Char('e') | KeyAction::Submit => {
                    let selected = self.selected;
                    if let Some((alias, _)) = self.aliases.get(selected) {
                        let alias = alias.clone();
                        self.open_form(true, Some(&alias), None);
                    }
                }
                KeyAction::Char('d') => {
                    if let Some((alias, _)) = self.aliases.get(self.selected) {
                        self.confirm_delete = Some(alias.clone());
                    }
                }
                KeyAction::Escape => self.close(),
                _ => {}
            }
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
                    let selected = self.current_model().map(|model| model.slug.clone());
                    self.sync_browser(selected.as_deref());
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
        let slug = {
            self.current_model()
                .map(|m| m.slug.clone())
                .unwrap_or_default()
        };
        if slug.is_empty() {
            return;
        }
        let result = atman_runtime::config_hub::ConfigHub::global().and_then(|hub| {
            if self.is_edit {
                hub.update_alias(self.edit_original.as_deref(), &alias, &slug)
            } else {
                hub.add_alias(&alias, &slug)
            }
        });
        match result {
            Ok(()) => {
                self.refresh_list();
                self.show_form = false;
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
    let mut lines: Vec<Line> = vec![];

    let name_style = if mgr.focus == Focus::NameInput {
        Style::default()
            .fg(theme.accent.into())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.tinted_fg.into())
    };
    lines.push(Line::from(Span::styled(
        format!("Name: {}", mgr.editor.buf()),
        name_style,
    )));
    lines.push(Line::from(""));

    let browser_height = area.height.saturating_sub(2) as usize;
    mgr.browser.set_visible_rows(browser_height);
    for index in mgr.browser.visible_rows(browser_height) {
        let row = &mgr.browser.rows()[index];
        let selected = mgr.focus == Focus::Tree && index == mgr.browser.selected_index();
        let style = if selected {
            Style::default()
                .fg(theme.accent.into())
                .add_modifier(Modifier::BOLD)
        } else if row.kind == BrowserRowKind::Provider {
            Style::default()
                .fg(theme.heading.into())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.tinted_fg.into())
        };
        let prefix = if selected {
            " ▶"
        } else if row.kind == BrowserRowKind::Provider {
            "▸ "
        } else {
            "  "
        };
        lines.push(Line::from(Span::styled(
            format!("{}{}", prefix, row.label),
            style,
        )));
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    if mgr.focus == Focus::NameInput {
        let cursor_x = area.x + 6 + mgr.editor.cursor_display_col() as u16;
        f.set_cursor_position((cursor_x, area.y));
    }
}

fn render_preview_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &AliasManager,
    theme: &crate::theme::Theme,
) {
    crate::wm::shell::render_column_divider(f, area.x, area.y, area.height, theme);
    crate::wm::shell::render_section_header(
        f,
        Rect {
            x: area.x + 3,
            y: area.y,
            width: area.width.saturating_sub(3),
            height: 1,
        },
        Line::from(Span::styled(
            "Preview",
            Style::default().fg(theme.tinted_fg.into()),
        )),
        theme,
    );

    let mut lines = vec![];
    let content_x = area.x.saturating_add(3);

    // Show current alias mapping if editing alias name
    if mgr.focus == Focus::NameInput && !mgr.editor.buf().trim().is_empty() {
        let alias_name = mgr.editor.buf().trim();
        if let Some((_, target)) = mgr.aliases.iter().find(|(a, _)| a == alias_name) {
            lines.push(Line::from(Span::styled(
                format!("→ {}", target),
                Style::default().fg(theme.meta_fg.into()),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "New alias",
                Style::default().fg(theme.meta_fg.into()),
            )));
        }
    }

    if let Some(m) = mgr.current_model() {
        lines.push(Line::from(Span::styled(
            &m.slug,
            Style::default().add_modifier(Modifier::BOLD),
        )));
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
    }

    let content_area = Rect {
        x: content_x,
        y: area.y + 2,
        width: area.width.saturating_sub(3),
        height: area.height.saturating_sub(2),
    };
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );
}

impl crate::wm::modal::ModalOverlay for AliasManager {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        self.last_input_rect = None;
        self.list_rect = None;
        self.save_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        if let Some(alias) = self.confirm_delete.as_deref() {
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
            render_delete_confirm(f, area, "alias", alias, t);
            return;
        }
        if !self.show_form {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(area);
            let list_area = rows[0];
            let footer_area = rows[1];
            self.list_rect = Some(list_area);
            let items: Vec<ListItem> = self
                .aliases
                .iter()
                .enumerate()
                .map(|(i, (a, m))| {
                    let style = if i == self.selected {
                        Style::default()
                            .fg(t.accent.into())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    ListItem::new(Line::from(Span::styled(format!(" {} → {}", a, m), style)))
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(self.selected));
            f.render_stateful_widget(List::new(items), list_area, &mut state);
            let help = if self.aliases.is_empty() {
                "n:add alias · e:edit · d:delete · Esc:close  (no aliases yet)"
            } else {
                "n:add  e:edit  d:delete  ↑↓:navigate  Esc:close"
            };
            let footer = Paragraph::new(Line::from(Span::styled(
                help,
                Style::default().fg(t.meta_fg.into()),
            )))
            .alignment(ratatui::layout::Alignment::Right);
            f.render_widget(footer, footer_area);
            return;
        }
        if area.height < 5 {
            return;
        }
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);
        let panels = rows[0];
        let footer_area = rows[1];
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(panels);
        self.last_input_rect = Some(Rect {
            x: cols[0].x.saturating_add(6),
            y: cols[0].y,
            width: cols[0].width.saturating_sub(6),
            height: 1,
        });
        render_tree_panel(f, cols[0], self, t);
        render_preview_panel(f, cols[1], self, t);
        let help = match self.focus {
            Focus::NameInput => "Tab:model tree  Enter/Esc:save".to_string(),
            Focus::Tree => crate::directional_selector::footer_help(
                "Tab:name input  ↑↓/jk:navigate",
                "Enter/Esc:save",
            ),
        };
        self.save_rect = Some(footer_area);
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
        self.handle_key(action, tx);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        (self.open && self.show_form && self.focus == Focus::NameInput)
            .then_some(self.last_input_rect)
            .flatten()
            .map(|r| (r.x + self.editor.cursor_display_col() as u16, r.y))
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

    fn provider_group(name: &str) -> atman_runtime::model_registry::ProviderGroup {
        atman_runtime::model_registry::ProviderGroup {
            provider_name: name.to_string(),
            models: Vec::new(),
        }
    }
}
