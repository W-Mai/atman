use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use crate::input::InputEditor;
use crate::keys::KeyAction;

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
    }

    pub fn close(&mut self) {
        self.open = false;
        self.show_form = false;
    }

    pub fn show_form(&self) -> bool {
        self.show_form
    }

    pub fn refresh_list(&mut self) {
        self.aliases = atman_runtime::model_registry::all_aliases();
        self.aliases.sort_by(|a, b| a.0.cmp(&b.0));
        self.selected = 0;
    }

    fn refresh_groups(&mut self) {
        self.groups = atman_runtime::model_registry::all_provider_groups();
        self.model_idx = vec![0; self.groups.len()];
        self.provider_idx = 0;
        self.focus = Focus::NameInput;
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
                self.focus = Focus::Tree;
                return;
            }
        }
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
        if !self.show_form {
            // Alias list mode
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
                KeyAction::Char('a') => self.open_form(false, None, None),
                KeyAction::Char('e') | KeyAction::Submit => {
                    let selected = self.selected;
                    if let Some((alias, _)) = self.aliases.get(selected) {
                        let alias = alias.clone();
                        self.open_form(true, Some(&alias), None);
                    }
                }
                KeyAction::Char('d') => {
                    if let Some((a, _)) = self.aliases.get(self.selected) {
                        let a = a.clone();
                        atman_runtime::model_registry::remove_alias_from_config(&a).ok();
                        self.refresh_list();
                    }
                }
                KeyAction::Escape => self.close(),
                _ => {}
            }
            return;
        }

        match self.focus {
            Focus::NameInput => match action {
                KeyAction::Escape => self.show_form = false,
                KeyAction::Tab => {
                    self.focus = Focus::Tree;
                }
                KeyAction::Submit => self.commit_alias(control_tx),
                KeyAction::Backspace => {
                    self.editor.backspace();
                }
                KeyAction::CursorLeft => {
                    self.editor.move_left();
                }
                KeyAction::CursorRight => {
                    self.editor.move_right();
                }
                KeyAction::CursorHome => {
                    self.editor.move_home();
                }
                KeyAction::CursorEnd => {
                    self.editor.move_end();
                }
                KeyAction::Char(c) => {
                    self.editor.insert_char(*c);
                }
                _ => {}
            },
            Focus::Tree => match action {
                KeyAction::Escape => self.show_form = false,
                KeyAction::Tab => {
                    let total = self.groups.len();
                    if total == 0 {
                        self.focus = Focus::NameInput;
                        return;
                    }
                    self.provider_idx = (self.provider_idx + 1) % total;
                    if self.provider_idx == 0 && total > 0 {
                        self.focus = Focus::NameInput;
                    }
                }
                KeyAction::HistoryUp | KeyAction::Char('k') => {
                    if self.groups.is_empty() {
                        return;
                    }
                    if self.model_idx[self.provider_idx] > 0 {
                        self.model_idx[self.provider_idx] -= 1;
                    } else if self.provider_idx > 0 {
                        self.provider_idx -= 1;
                        let prev = &self.groups[self.provider_idx];
                        self.model_idx[self.provider_idx] = prev.models.len().saturating_sub(1);
                    }
                }
                KeyAction::HistoryDown | KeyAction::Char('j') => {
                    if self.groups.is_empty() {
                        return;
                    }
                    let g = &self.groups[self.provider_idx];
                    if self.model_idx[self.provider_idx] + 1 < g.models.len() {
                        self.model_idx[self.provider_idx] += 1;
                    } else if self.provider_idx + 1 < self.groups.len() {
                        self.provider_idx += 1;
                        self.model_idx[self.provider_idx] = 0;
                    }
                }
                KeyAction::Submit => self.commit_alias(control_tx),
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
        if self.is_edit {
            if let Some(ref old) = self.edit_original {
                atman_runtime::model_registry::update_alias_in_config(old, &alias, &slug).ok();
            }
        } else {
            atman_runtime::model_registry::add_alias_to_config(&alias, &slug).ok();
        }
        self.refresh_list();
        self.show_form = false;
    }
}

fn render_tree_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    mgr: &AliasManager,
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
    let cursor = if mgr.focus == Focus::NameInput {
        "█"
    } else {
        ""
    };
    lines.push(Line::from(Span::styled(
        format!("Name: {} {}", mgr.editor.buf(), cursor),
        name_style,
    )));
    lines.push(Line::from(""));

    for (pi, grp) in mgr.groups.iter().enumerate() {
        let is_active = mgr.focus == Focus::Tree && mgr.provider_idx == pi;
        let hdr_style = if is_active {
            Style::default()
                .fg(theme.heading.into())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.heading.into())
        };
        lines.push(Line::from(Span::styled(
            format!("▸ {}", grp.provider_name),
            hdr_style,
        )));

        for (mi, m) in grp.models.iter().enumerate() {
            let is_sel = is_active && mgr.model_idx[pi] == mi;
            let style = if is_sel {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.tinted_fg.into())
            };
            let prefix = if is_sel { " ▶" } else { "  " };
            lines.push(Line::from(Span::styled(
                format!("{} {}", prefix, m.slug),
                style,
            )));
        }
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    if mgr.focus == Focus::NameInput {
        let cursor_x = area.x + 6 + crate::width::width(mgr.editor.buf()) as u16;
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
        if !self.show_form {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(area);
            let list_area = rows[0];
            let footer_area = rows[1];
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
                "a:add alias · e:edit · d:delete · Esc:close  (no aliases yet)"
            } else {
                "a:add  e:edit  d:delete  ↑↓:navigate  Esc:close"
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
            Focus::NameInput => "Tab:model tree  Enter:save  Esc:cancel",
            Focus::Tree => "Tab:name input  ↑↓/jk:navigate  Enter:save  Esc:cancel",
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
    ) -> bool {
        self.handle_key(action, tx);
        true
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        self.last_input_rect
            .map(|r| (r.x + self.editor.buf().chars().count() as u16, r.y))
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
