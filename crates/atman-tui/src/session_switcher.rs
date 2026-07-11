use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::SessionPickerRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionScope {
    #[default]
    Project,
    All,
}

impl SessionScope {
    pub fn toggle(self) -> Self {
        match self {
            SessionScope::Project => SessionScope::All,
            SessionScope::All => SessionScope::Project,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionScope::Project => "project only",
            SessionScope::All => "all projects",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionSortMode {
    #[default]
    Recent,
    Busiest,
}

impl SessionSortMode {
    pub fn toggle(self) -> Self {
        match self {
            SessionSortMode::Recent => SessionSortMode::Busiest,
            SessionSortMode::Busiest => SessionSortMode::Recent,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionSortMode::Recent => "recent",
            SessionSortMode::Busiest => "busiest",
        }
    }
}

#[derive(Default)]
pub struct SessionSwitcher {
    pub open: bool,
    pub scope: SessionScope,
    pub all_rows: Vec<SessionPickerRow>,
    pub rows: Vec<SessionPickerRow>,
    pub selected: usize,
    pub delete_armed: Option<String>,
    pub sort_mode: SessionSortMode,
    pub filter: String,
    pub filter_mode: bool,
    pub rename_mode: bool,
    pub rename_buf: String,
    pub rename_target: Option<String>,
}

impl std::fmt::Debug for SessionSwitcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionSwitcher")
            .field("open", &self.open)
            .field("rows", &self.rows.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl SessionSwitcher {
    pub fn open_with(&mut self, rows: Vec<SessionPickerRow>, scope: SessionScope) {
        self.scope = scope;
        self.all_rows = rows;
        self.filter.clear();
        self.filter_mode = false;
        self.selected = 0;
        self.open = true;
        self.rebuild_view();
    }

    pub fn set_rows(&mut self, rows: Vec<SessionPickerRow>) {
        self.all_rows = rows;
        self.selected = 0;
        self.rebuild_view();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.rows.clear();
        self.all_rows.clear();
        self.filter.clear();
        self.filter_mode = false;
        self.delete_armed = None;
    }

    pub fn toggle_sort(&mut self) {
        self.sort_mode = self.sort_mode.toggle();
        self.rebuild_view();
    }

    pub fn enter_filter_mode(&mut self) {
        self.filter_mode = true;
    }

    pub fn leave_filter_mode(&mut self) {
        self.filter_mode = false;
    }

    pub fn filter_push(&mut self, c: char) {
        self.filter.push(c);
        self.rebuild_view();
    }

    pub fn filter_pop(&mut self) {
        self.filter.pop();
        self.rebuild_view();
    }

    pub fn filter_clear(&mut self) {
        self.filter.clear();
        self.rebuild_view();
    }

    fn rebuild_view(&mut self) {
        let needle = self.filter.to_lowercase();
        let mut view: Vec<SessionPickerRow> = self
            .all_rows
            .iter()
            .filter(|r| row_matches_filter(r, &needle))
            .cloned()
            .collect();
        match self.sort_mode {
            SessionSortMode::Recent => {
                view.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
            }
            SessionSortMode::Busiest => {
                view.sort_by(|a, b| {
                    b.message_count
                        .cmp(&a.message_count)
                        .then_with(|| b.updated_at.cmp(&a.updated_at))
                });
            }
        }
        self.rows = view;
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    pub fn remove_selected(&mut self) -> Option<String> {
        if self.selected >= self.rows.len() {
            return None;
        }
        let removed = self.rows.remove(self.selected);
        self.all_rows.retain(|r| r.id != removed.id);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
        self.delete_armed = None;
        Some(removed.id)
    }

    pub fn arm_delete(&mut self) -> Option<&str> {
        let sid = self.rows.get(self.selected)?.id.clone();
        self.delete_armed = Some(sid);
        self.delete_armed.as_deref()
    }

    pub fn delete_armed_matches_selected(&self) -> bool {
        match (&self.delete_armed, self.rows.get(self.selected)) {
            (Some(armed), Some(row)) => armed == &row.id,
            _ => false,
        }
    }

    pub fn clear_delete_arm(&mut self) {
        self.delete_armed = None;
    }

    pub fn begin_rename(&mut self) -> Option<&str> {
        let row = self.rows.get(self.selected)?;
        self.rename_target = Some(row.id.clone());
        self.rename_buf = row.goal.clone().unwrap_or_default();
        self.rename_mode = true;
        self.rename_target.as_deref()
    }

    pub fn commit_rename(&mut self) -> Option<(String, Option<String>)> {
        let sid = self.rename_target.take()?;
        let title = self.rename_buf.trim();
        let value = if title.is_empty() {
            None
        } else {
            Some(title.to_string())
        };
        if let Some(row) = self.all_rows.iter_mut().find(|r| r.id == sid) {
            row.goal = value.clone();
        }
        if let Some(row) = self.rows.iter_mut().find(|r| r.id == sid) {
            row.goal = value.clone();
        }
        self.rename_mode = false;
        self.rename_buf.clear();
        Some((sid, value))
    }

    pub fn cancel_rename(&mut self) {
        self.rename_mode = false;
        self.rename_buf.clear();
        self.rename_target = None;
    }

    pub fn rename_push(&mut self, c: char) {
        self.rename_buf.push(c);
    }

    pub fn rename_pop(&mut self) {
        self.rename_buf.pop();
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
    }

    pub fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|r| r.id.clone())
    }
}

fn row_matches_filter(row: &SessionPickerRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if row.id.to_lowercase().contains(needle) {
        return true;
    }
    if let Some(g) = row.goal.as_deref()
        && g.to_lowercase().contains(needle)
    {
        return true;
    }
    if let Some(p) = row.project.as_deref()
        && p.to_lowercase().contains(needle)
    {
        return true;
    }
    false
}

pub fn render(f: &mut ratatui::Frame, area: Rect, switcher: &SessionSwitcher) {
    let w = area.width.saturating_sub(4).clamp(60, 100);
    let desired = 3 + switcher.rows.len().max(1) as u16 + 2;
    let h = area.height.saturating_sub(4).min(desired).max(8);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);
    let title = if switcher.rename_mode {
        format!(
            " Rename Session · {}▏ · Enter save · Esc cancel ",
            switcher.rename_buf
        )
    } else if switcher.delete_armed.is_some() {
        format!(
            " Switch Session · {} · d again to DELETE · any other key cancels ",
            switcher.scope.label()
        )
    } else if switcher.filter_mode {
        format!(
            " Switch Session · filter: {}▏ · Esc/Enter done ",
            switcher.filter
        )
    } else {
        format!(
            " Switch Session · {} · sort:{} · filter:{} · Tab scope · s sort · f filter · r rename · Enter open · d delete · Esc ",
            switcher.scope.label(),
            switcher.sort_mode.label(),
            if switcher.filter.is_empty() {
                "-".to_string()
            } else {
                switcher.filter.clone()
            },
        )
    };
    let border_color = if switcher.rename_mode {
        crate::theme::theme().warn
    } else if switcher.delete_armed.is_some() {
        crate::theme::theme().error
    } else {
        crate::theme::theme().accent
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            title,
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.height == 0 {
        return;
    }
    if switcher.rows.is_empty() {
        let hint = match switcher.scope {
            SessionScope::Project => {
                "no sessions found in this project · press Tab to see all projects"
            }
            SessionScope::All => "no other sessions exist yet",
        };
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(crate::theme::theme().subtle_fg),
            ))),
            inner,
        );
        return;
    }
    let items: Vec<ListItem<'static>> = switcher
        .rows
        .iter()
        .map(|row| {
            let sid_short: String = row.id.chars().take(8).collect();
            let goal_snippet: String = row
                .goal
                .as_deref()
                .unwrap_or("-")
                .chars()
                .take(50)
                .collect();
            let project_label = row.project.clone().unwrap_or_else(|| "-".into());
            let updated: String = row.updated_at.chars().take(19).collect();
            let line = Line::from(vec![
                Span::styled(
                    format!("{sid_short:<10}"),
                    Style::default()
                        .fg(crate::theme::theme().warn)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>5} msgs  ", row.message_count),
                    Style::default().fg(crate::theme::theme().subtle_fg),
                ),
                Span::styled(
                    format!("{updated:<19}  "),
                    Style::default().fg(crate::theme::theme().accent),
                ),
                Span::styled(
                    project_label,
                    Style::default().fg(crate::theme::theme().success),
                ),
                Span::styled(
                    format!("  {goal_snippet}"),
                    Style::default().fg(crate::theme::theme().tinted_fg),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(crate::theme::theme().subtle_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    if !switcher.rows.is_empty() {
        state.select(Some(switcher.selected));
    }
    f.render_stateful_widget(list, inner, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, msgs: usize) -> SessionPickerRow {
        SessionPickerRow {
            id: id.into(),
            project: None,
            message_count: msgs,
            updated_at: "2026-07-08T00:00:00Z".into(),
            goal: None,
        }
    }

    fn row_with(id: &str, msgs: usize, updated: &str, goal: Option<&str>) -> SessionPickerRow {
        SessionPickerRow {
            id: id.into(),
            project: None,
            message_count: msgs,
            updated_at: updated.into(),
            goal: goal.map(|s| s.into()),
        }
    }

    #[test]
    fn recent_sort_default_puts_newest_first() {
        let mut s = SessionSwitcher::default();
        s.open_with(
            vec![
                row_with("old", 500, "2026-01-01T00:00:00Z", None),
                row_with("new", 3, "2026-07-08T00:00:00Z", None),
            ],
            SessionScope::Project,
        );
        assert_eq!(s.rows[0].id, "new");
    }

    #[test]
    fn busiest_sort_puts_most_messages_first() {
        let mut s = SessionSwitcher::default();
        s.open_with(
            vec![
                row_with("recent-small", 3, "2026-07-08T00:00:00Z", None),
                row_with("old-big", 500, "2026-01-01T00:00:00Z", None),
            ],
            SessionScope::Project,
        );
        s.toggle_sort();
        assert_eq!(s.sort_mode, SessionSortMode::Busiest);
        assert_eq!(s.rows[0].id, "old-big");
    }

    #[test]
    fn filter_narrows_visible_rows() {
        let mut s = SessionSwitcher::default();
        s.open_with(
            vec![
                row_with("aaa11111", 1, "2026-07-01T00:00:00Z", Some("refactor auth")),
                row_with("bbb22222", 2, "2026-07-02T00:00:00Z", Some("write tests")),
            ],
            SessionScope::Project,
        );
        s.filter_push('a');
        s.filter_push('u');
        s.filter_push('t');
        s.filter_push('h');
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.rows[0].id, "aaa11111");
        s.filter_clear();
        assert_eq!(s.rows.len(), 2);
    }

    #[test]
    fn open_with_rows_sets_selection_to_zero() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1), row("b", 2)], SessionScope::Project);
        assert!(s.open);
        assert_eq!(s.selected, 0);
        assert_eq!(s.selected_id().as_deref(), Some("a"));
        assert_eq!(s.scope, SessionScope::Project);
    }

    #[test]
    fn move_down_clamps_at_end() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1), row("b", 2)], SessionScope::Project);
        s.move_down();
        s.move_down();
        s.move_down();
        assert_eq!(s.selected, 1);
        assert_eq!(s.selected_id().as_deref(), Some("b"));
    }

    #[test]
    fn close_clears_state() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1)], SessionScope::All);
        s.close();
        assert!(!s.open);
        assert!(s.rows.is_empty());
    }

    #[test]
    fn scope_toggle_flips_between_project_and_all() {
        assert_eq!(SessionScope::Project.toggle(), SessionScope::All);
        assert_eq!(SessionScope::All.toggle(), SessionScope::Project);
    }
}
