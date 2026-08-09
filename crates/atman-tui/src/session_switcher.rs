use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use tokio::sync::mpsc;

use crate::key_handler::{enumerate_session_rows, request_session_switch};
use crate::keys::KeyAction;
use crate::{SessionPickerRow, TuiControl};

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


impl crate::wm::modal::ModalOverlay for SessionSwitcher {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        if area.height == 0 {
            return;
        }
        if !self.rename_mode
            && self.delete_armed.is_none()
            && !self.filter_mode
            && area.height >= 2
        {
            let footer_rect = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            };
            let footer = Line::from(Span::styled(
                " s:sort  f:filter  r:rename  Enter:open  d:delete  Tab:scope  Esc:close ",
                Style::default().fg(t.subtle_fg.into()),
            ));
            f.render_widget(Paragraph::new(footer), footer_rect);
        }
        let list_height = if !self.rename_mode
            && self.delete_armed.is_none()
            && !self.filter_mode
            && area.height >= 2
        {
            area.height.saturating_sub(1)
        } else {
            area.height
        };
        if self.rows.is_empty() {
            let hint = match self.scope {
                SessionScope::Project => {
                    "no sessions found in this project · press Tab to see all projects"
                }
                SessionScope::All => "no other sessions exist yet",
            };
            f.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                    hint,
                    Style::default().fg(t.subtle_fg.into()),
                ))),
                area,
            );
            return;
        }
        let items: Vec<ListItem<'static>> = self
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
                            .fg(t.warn.into())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("{:>5} msgs  ", row.message_count),
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                    Span::styled(
                        format!("{updated:<19}  "),
                        Style::default().fg(t.accent.into()),
                    ),
                    Span::styled(
                        project_label,
                        Style::default().fg(t.success.into()),
                    ),
                    Span::styled(
                        format!("  {goal_snippet}"),
                        Style::default().fg(t.tinted_fg.into()),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ListState::default();
        if !self.rows.is_empty() {
            state.select(Some(self.selected));
        }
        let list_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: list_height,
        };
        f.render_stateful_widget(list, list_rect, &mut state);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> bool {
        if self.rename_mode {
            match action {
                KeyAction::Escape => {
                    self.cancel_rename();
                    app.push_note("rename cancelled", crate::app::NoteLevel::Info);
                }
                KeyAction::Submit => {
                    if let Some((sid, title)) = self.commit_rename() {
                        if let Some(tx) = tx {
                            let _ = tx.send(TuiControl::RenameSession {
                                session_id: sid.clone(),
                                title: title.clone(),
                            });
                        }
                        let msg = match &title {
                            Some(t) => format!("renamed {sid} → {t}"),
                            None => format!("cleared title on {sid}"),
                        };
                        app.push_note(msg, crate::app::NoteLevel::Info);
                    }
                }
                KeyAction::Backspace => self.rename_pop(),
                KeyAction::Char(c) => self.rename_push(*c),
                _ => {}
            }
            return true;
        }
        if self.filter_mode {
            match action {
                KeyAction::Escape | KeyAction::Submit => {
                    self.leave_filter_mode();
                }
                KeyAction::Backspace => self.filter_pop(),
                KeyAction::Char(c) => self.filter_push(*c),
                _ => {}
            }
            return true;
        }
        if let KeyAction::Char('d') | KeyAction::Char('D') = action {
            if self.delete_armed_matches_selected() {
                if let Some(sid) = self.remove_selected() {
                    if let Some(tx) = tx {
                        let _ = tx.send(TuiControl::DeleteSession(sid.clone()));
                    }
                    app.push_note(format!("deleted session {sid}"), crate::app::NoteLevel::Info);
                }
            } else {
                let armed = self.arm_delete().map(str::to_owned);
                match armed {
                    Some(sid) => app.push_note(
                        format!("press d again to confirm delete {sid}"),
                        crate::app::NoteLevel::Warn,
                    ),
                    None => app.push_note("no session selected", crate::app::NoteLevel::Warn),
                }
            }
            return true;
        }
        if self.delete_armed.is_some() {
            self.clear_delete_arm();
            app.push_note("delete cancelled", crate::app::NoteLevel::Info);
        }
        if let KeyAction::Char('s') | KeyAction::Char('S') = action {
            self.toggle_sort();
            return true;
        }
        if let KeyAction::Char('f') | KeyAction::Char('F') = action {
            self.enter_filter_mode();
            return true;
        }
        if let KeyAction::Char('r') | KeyAction::Char('R') = action {
            if self.begin_rename().is_none() {
                app.push_note("no session selected", crate::app::NoteLevel::Warn);
            }
            return true;
        }
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp | KeyAction::CursorLeft => self.move_up(),
            KeyAction::HistoryDown | KeyAction::CursorRight => self.move_down(),
            KeyAction::Tab => {
                let new_scope = self.scope.toggle();
                let rows = enumerate_session_rows(app, new_scope);
                self.scope = new_scope;
                self.set_rows(rows);
            }
            KeyAction::Submit => {
                if let Some(sid) = self.selected_id() {
                    self.close();
                    request_session_switch(app, tx, sid.clone());
                }
            }
            _ => {}
        }
        true
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
    }

    fn title(&self) -> Line<'static> {
        if self.rename_mode {
            Line::from(format!(
                " Rename · {}▏ · Enter save · Esc cancel ",
                self.rename_buf
            ))
        } else if self.delete_armed.is_some() {
            Line::from(" Delete? · d again to confirm · any other key cancels ")
        } else if self.filter_mode {
            Line::from(format!(" Filter · {}▏ · Esc/Enter done ", self.filter))
        } else {
            Line::from(format!(" Sessions · {} ", self.scope.label()))
        }
    }

    fn icon(&self) -> &str {
        "⟲"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
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
