use crate::wm::modal::ModalAction;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use tokio::sync::mpsc;

use crate::key_handler::{enumerate_session_rows, request_session_switch};
use crate::keys::KeyAction;
use crate::{SessionPickerRow, TuiControl};

pub const SESSION_SWITCHER_WIDTH: u16 = 104;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionRowHit {
    index: usize,
    rect: Rect,
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
    viewport_offset: usize,
    hovered: Option<usize>,
    list_rect: Option<Rect>,
    row_hits: Vec<SessionRowHit>,
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
        self.reset_viewport();
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
        self.reset_viewport();
    }

    fn reset_viewport(&mut self) {
        self.viewport_offset = 0;
        self.hovered = None;
        self.list_rect = None;
        self.row_hits.clear();
    }

    fn invalidate_hits(&mut self) {
        self.hovered = None;
        self.row_hits.clear();
    }

    pub(crate) fn desired_list_height(&self) -> u16 {
        self.rows
            .iter()
            .map(session_row_height)
            .fold(0, u16::saturating_add)
            .max(3)
    }

    fn visible_indices(&self, height: u16) -> Vec<usize> {
        let mut used = 0;
        self.rows
            .iter()
            .enumerate()
            .skip(self.viewport_offset)
            .take_while(|(_, row)| {
                let next = used + session_row_height(row);
                if next <= height {
                    used = next;
                    true
                } else {
                    false
                }
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn ensure_selected_visible(&mut self) {
        if self.rows.is_empty() {
            self.viewport_offset = 0;
            return;
        }
        self.selected = self.selected.min(self.rows.len() - 1);
        self.viewport_offset = self.viewport_offset.min(self.selected);
        let height = self.list_rect.map_or(0, |rect| rect.height);
        if height == 0 {
            return;
        }
        while self.viewport_offset < self.selected {
            let selected_visible = self
                .visible_indices(height)
                .last()
                .is_some_and(|last| *last >= self.selected);
            if selected_visible {
                break;
            }
            self.viewport_offset += 1;
        }
    }

    fn row_at(&self, column: u16, row: u16) -> Option<usize> {
        self.row_hits
            .iter()
            .find(|hit| crate::render::rect_contains(hit.rect, column, row))
            .map(|hit| hit.index)
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) {
        let over_list = self
            .list_rect
            .is_some_and(|rect| crate::render::rect_contains(rect, event.column, event.row));
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = self.row_at(event.column, event.row);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                self.hovered = self.row_at(event.column, event.row);
                if let Some(index) = self.hovered {
                    self.selected = index;
                    self.delete_armed = None;
                    self.ensure_selected_visible();
                }
            }
            MouseEventKind::ScrollUp if over_list => self.move_up(),
            MouseEventKind::ScrollDown if over_list => self.move_down(),
            _ => {
                if !over_list {
                    self.hovered = None;
                }
            }
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
        self.invalidate_hits();
        self.ensure_selected_visible();
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
        self.invalidate_hits();
        self.ensure_selected_visible();
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
        self.ensure_selected_visible();
        self.invalidate_hits();
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.rows.len() {
            self.selected += 1;
        }
        self.ensure_selected_visible();
        self.invalidate_hits();
    }

    pub fn selected_id(&self) -> Option<String> {
        self.rows.get(self.selected).map(|r| r.id.clone())
    }
}

fn session_row_height(row: &SessionPickerRow) -> u16 {
    if row.goal.as_deref().is_some_and(|goal| !goal.is_empty()) {
        4
    } else {
        3
    }
}

fn format_local_timestamp(raw: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|timestamp| {
            timestamp
                .with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| crate::width::truncate(raw, 11))
}

fn row_matches_filter(row: &SessionPickerRow, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    if row.id.to_lowercase().contains(needle) {
        return true;
    }
    if let Some(name) = row.name.as_deref()
        && name.to_lowercase().contains(needle)
    {
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
        app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        if area.height == 0 {
            return;
        }
        let identity_height = area.height.min(4);
        let content_width = area.width.saturating_sub(6) as usize;
        let name = crate::width::truncate(
            app.session_name.as_deref().unwrap_or("Untitled session"),
            content_width,
        );
        let goal = crate::width::truncate(app.goal.as_deref().unwrap_or("No goal"), content_width);
        let project = crate::width::middle_truncate(
            app.project_root.as_deref().unwrap_or("-"),
            content_width,
        );
        let identity = vec![
            Line::from(vec![
                Span::styled(
                    "∴ ATMAN",
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {name}"),
                    Style::default()
                        .fg(t.heading.into())
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(Span::styled(
                format!("  Goal: {goal}"),
                Style::default().fg(t.tinted_fg.into()),
            )),
            Line::from(Span::styled(
                format!("  Project: {project}"),
                Style::default().fg(t.success.into()),
            )),
        ];
        let identity_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: identity_height,
        };
        f.render_widget(
            Paragraph::new(identity)
                .style(Style::default().bg(t.panel_bg.into()))
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(Style::default().fg(t.border.into()))
                        .padding(Padding::horizontal(1)),
                ),
            identity_rect,
        );
        let area = Rect {
            x: area.x,
            y: area.y.saturating_add(identity_height),
            width: area.width,
            height: area.height.saturating_sub(identity_height),
        };
        if area.height == 0 {
            return;
        }
        if !self.rename_mode && self.delete_armed.is_none() && !self.filter_mode && area.height >= 2
        {
            let footer_rect = Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(2),
                width: area.width,
                height: 2,
            };
            let key_style = Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD);
            let hint_style = Style::default().fg(t.subtle_fg.into());
            let footer = Line::from(vec![
                Span::styled(" s/f", key_style),
                Span::styled(" sort/filter  ", hint_style),
                Span::styled("r", key_style),
                Span::styled(" rename  ", hint_style),
                Span::styled("a", key_style),
                Span::styled(" auto  ", hint_style),
                Span::styled("Enter", key_style),
                Span::styled(" open  ", hint_style),
                Span::styled("Tab", key_style),
                Span::styled(" scope  ", hint_style),
                Span::styled("Esc", key_style),
                Span::styled(" close", hint_style),
            ]);
            f.render_widget(
                Paragraph::new(footer)
                    .style(Style::default().bg(t.code_bg.into()))
                    .block(
                        Block::default()
                            .borders(Borders::TOP)
                            .border_style(Style::default().fg(t.border.into()))
                            .padding(Padding::horizontal(1)),
                    ),
                footer_rect,
            );
        }
        let list_height = if !self.rename_mode
            && self.delete_armed.is_none()
            && !self.filter_mode
            && area.height >= 2
        {
            area.height.saturating_sub(2)
        } else {
            area.height
        };
        let list_area = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: list_height.saturating_sub(2),
        };
        self.list_rect = Some(list_area);
        self.row_hits.clear();
        if self.rows.is_empty() {
            self.hovered = None;
            let hint = match self.scope {
                SessionScope::Project => {
                    "no sessions found in this project · press Tab to see all projects"
                }
                SessionScope::All => "no other sessions exist yet",
            };
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    hint,
                    Style::default().fg(t.subtle_fg.into()),
                )))
                .style(Style::default().bg(t.modal_bg.into())),
                list_area,
            );
            return;
        }
        self.ensure_selected_visible();
        let visible = self.visible_indices(list_area.height);
        let mut y = list_area.y;
        for index in visible {
            let row = self.rows[index].clone();
            let selected = index == self.selected;
            let hovered = self.hovered == Some(index);
            let height = session_row_height(&row);
            let rect = Rect::new(list_area.x, y, list_area.width, height);
            let background = if selected {
                t.modal_bg.lerp(t.accent, 0.22)
            } else if hovered {
                t.modal_bg.lerp(t.highlight_bg, 0.24)
            } else {
                t.modal_bg.lerp(t.panel_bg, 0.14)
            };
            let marker = if selected { "▌ " } else { "  " };
            let marker_style = Style::default().fg(t.accent.into()).bg(background);
            let inner_width = rect.width.saturating_sub(4) as usize;
            let meta_width = 24.min(inner_width / 3);
            let project_width = 30.min(inner_width / 3);
            let name_width = inner_width
                .saturating_sub(meta_width)
                .saturating_sub(project_width)
                .saturating_sub(4)
                .max(12);
            let name_column_width = name_width.saturating_sub(2);
            let name = crate::width::pad_right(
                &crate::width::truncate(
                    row.name.as_deref().unwrap_or("Untitled session"),
                    name_column_width,
                ),
                name_column_width,
            );
            let timestamp = crate::width::pad_right(&format_local_timestamp(&row.updated_at), 11);
            let message_label = format!("{} msgs", row.message_count);
            let message_width = meta_width.saturating_sub(14);
            let meta = format!(
                "{} · {timestamp}  ",
                crate::width::pad_right(&message_label, message_width),
            );
            let project_label = crate::width::pad_right(
                &crate::width::middle_truncate(
                    row.project.as_deref().unwrap_or("-"),
                    project_width,
                ),
                project_width,
            );
            let current = if row.is_current { "● " } else { "  " };
            let mut lines = vec![Line::from(Span::styled(marker, marker_style))];
            lines.push(Line::from(vec![
                Span::styled(marker, marker_style),
                Span::styled(
                    current,
                    Style::default()
                        .fg(t.accent.into())
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    name,
                    Style::default()
                        .fg(t.heading.into())
                        .bg(background)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(meta, Style::default().fg(t.subtle_fg.into()).bg(background)),
                Span::styled(
                    project_label,
                    Style::default().fg(t.success.into()).bg(background),
                ),
            ]));
            if let Some(goal) = row.goal.as_deref().filter(|goal| !goal.is_empty()) {
                let goal = crate::width::truncate(goal, inner_width.saturating_sub(4));
                lines.push(Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        format!("  {goal}"),
                        Style::default().fg(t.tinted_fg.into()).bg(background),
                    ),
                ]));
            }
            lines.push(Line::from(Span::styled(marker, marker_style)));
            f.render_widget(
                Paragraph::new(lines).style(Style::default().bg(background)),
                rect,
            );
            self.row_hits.push(SessionRowHit { index, rect });
            y = y.saturating_add(height);
        }
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if self.rename_mode {
            match action {
                KeyAction::Escape => {
                    self.cancel_rename();
                    app.push_note("rename cancelled", crate::app::NoteLevel::Info);
                }
                KeyAction::Submit => {
                    if let Some((sid, title)) = self.commit_rename() {
                        if sid == app.session_id {
                            app.session_name = title.clone();
                        }
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
            return Some(ModalAction::Consumed);
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
            return Some(ModalAction::Consumed);
        }
        if let KeyAction::Char('d') | KeyAction::Char('D') = action {
            if self.delete_armed_matches_selected() {
                if let Some(sid) = self.remove_selected() {
                    if let Some(tx) = tx {
                        let _ = tx.send(TuiControl::DeleteSession(sid.clone()));
                    }
                    app.push_note(
                        format!("deleted session {sid}"),
                        crate::app::NoteLevel::Info,
                    );
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
            return Some(ModalAction::Consumed);
        }
        if self.delete_armed.is_some() {
            self.clear_delete_arm();
            app.push_note("delete cancelled", crate::app::NoteLevel::Info);
        }
        if let KeyAction::Char('s') | KeyAction::Char('S') = action {
            self.toggle_sort();
            return Some(ModalAction::Consumed);
        }
        if let KeyAction::Char('f') | KeyAction::Char('F') = action {
            self.enter_filter_mode();
            return Some(ModalAction::Consumed);
        }
        if let KeyAction::Char('a') | KeyAction::Char('A') = action {
            if let Some(tx) = tx {
                let _ = tx.send(TuiControl::AutoNameSession);
                app.push_note("generating session name…", crate::app::NoteLevel::Info);
            }
            return Some(ModalAction::Consumed);
        }
        if let KeyAction::Char('r') | KeyAction::Char('R') = action {
            if self.begin_rename().is_none() {
                app.push_note("no session selected", crate::app::NoteLevel::Warn);
            }
            return Some(ModalAction::Consumed);
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
                if let Some(row) = self.rows.get(self.selected) {
                    let sid = row.id.clone();
                    let is_current = row.is_current;
                    self.close();
                    if !is_current {
                        request_session_switch(app, tx, sid);
                    }
                }
            }
            _ => {}
        }
        Some(ModalAction::Consumed)
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
    use crate::wm::modal::ModalOverlay;

    fn row(id: &str, msgs: usize) -> SessionPickerRow {
        SessionPickerRow {
            id: id.into(),
            is_current: false,
            name: None,
            project: None,
            message_count: msgs,
            updated_at: "2026-07-08T00:00:00Z".into(),
            goal: None,
        }
    }

    fn row_with(id: &str, msgs: usize, updated: &str, goal: Option<&str>) -> SessionPickerRow {
        SessionPickerRow {
            id: id.into(),
            is_current: false,
            name: None,
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
    fn current_session_supports_manual_and_auto_naming_without_switching() {
        let mut current = row("current", 1);
        current.is_current = true;
        current.name = Some("Old name".into());
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(vec![current], SessionScope::Project);
        let mut app = crate::app::AppState::new("current".into(), None);
        let (tx, mut rx) = mpsc::unbounded_channel();

        switcher.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        assert!(rx.try_recv().is_err());

        let mut current = row("current", 1);
        current.is_current = true;
        current.name = Some("Old name".into());
        switcher.open_with(vec![current], SessionScope::Project);
        switcher.handle_key(&KeyAction::Char('a'), &mut app, Some(&tx));
        assert!(matches!(rx.try_recv(), Ok(TuiControl::AutoNameSession)));

        switcher.handle_key(&KeyAction::Char('r'), &mut app, Some(&tx));
        assert!(switcher.rename_mode);
        switcher.rename_buf = "New name".into();
        switcher.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        assert_eq!(app.session_name.as_deref(), Some("New name"));
        assert!(matches!(
            rx.try_recv(),
            Ok(TuiControl::RenameSession { session_id, title })
                if session_id == "current" && title.as_deref() == Some("New name")
        ));
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
    fn filter_matches_session_name() {
        let mut named = row("aaa11111", 1);
        named.name = Some("Fix authentication flow".into());
        let mut s = SessionSwitcher::default();
        s.open_with(vec![named, row("bbb22222", 2)], SessionScope::Project);
        for ch in "authentication".chars() {
            s.filter_push(ch);
        }
        assert_eq!(s.rows.len(), 1);
        assert_eq!(s.rows[0].id, "aaa11111");
    }

    #[test]
    fn scope_toggle_flips_between_project_and_all() {
        assert_eq!(SessionScope::Project.toggle(), SessionScope::All);
        assert_eq!(SessionScope::All.toggle(), SessionScope::Project);
    }

    #[test]
    fn row_height_tracks_optional_goal() {
        assert_eq!(session_row_height(&row("plain", 1)), 3);
        assert_eq!(
            session_row_height(&row_with(
                "goal",
                1,
                "2026-07-08T00:00:00Z",
                Some("ship it")
            )),
            4
        );
    }

    #[test]
    fn mixed_rows_only_include_complete_records() {
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(
            vec![
                row_with("a", 1, "2026-07-10T00:00:00Z", Some("goal")),
                row_with("b", 1, "2026-07-09T00:00:00Z", None),
                row_with("c", 1, "2026-07-08T00:00:00Z", Some("goal")),
            ],
            SessionScope::Project,
        );

        assert_eq!(switcher.visible_indices(6), vec![0]);
        assert_eq!(switcher.visible_indices(7), vec![0, 1]);
    }

    #[test]
    fn selection_moves_viewport_to_keep_whole_row_visible() {
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(
            vec![
                row_with("a", 1, "2026-07-10T00:00:00Z", Some("goal")),
                row_with("b", 1, "2026-07-09T00:00:00Z", None),
                row_with("c", 1, "2026-07-08T00:00:00Z", Some("goal")),
            ],
            SessionScope::Project,
        );
        switcher.list_rect = Some(Rect::new(2, 3, 40, 7));

        switcher.move_down();
        assert_eq!(switcher.viewport_offset, 0);
        switcher.move_down();
        assert_eq!(switcher.viewport_offset, 1);
        assert_eq!(switcher.visible_indices(7), vec![1, 2]);
    }

    #[test]
    fn mouse_hits_every_line_and_click_only_selects() {
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(
            vec![
                row_with("a", 1, "2026-07-10T00:00:00Z", Some("goal")),
                row_with("b", 1, "2026-07-09T00:00:00Z", None),
            ],
            SessionScope::Project,
        );
        switcher.list_rect = Some(Rect::new(5, 10, 30, 7));
        switcher.row_hits = vec![
            SessionRowHit {
                index: 0,
                rect: Rect::new(5, 10, 30, 4),
            },
            SessionRowHit {
                index: 1,
                rect: Rect::new(5, 14, 30, 3),
            },
        ];

        for row in 10..14 {
            assert_eq!(switcher.row_at(8, row), Some(0));
        }
        switcher.handle_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 8,
            row: 16,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(switcher.open);
        assert_eq!(switcher.selected, 1);
    }

    #[test]
    fn hover_clears_outside_list_and_wheel_clamps() {
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(
            vec![
                row_with("a", 1, "2026-07-10T00:00:00Z", None),
                row_with("b", 1, "2026-07-09T00:00:00Z", None),
            ],
            SessionScope::Project,
        );
        switcher.list_rect = Some(Rect::new(5, 10, 30, 6));
        switcher.row_hits = vec![SessionRowHit {
            index: 0,
            rect: Rect::new(5, 10, 30, 3),
        }];
        let event = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        switcher.handle_mouse(&event(MouseEventKind::Moved, 8, 11));
        assert_eq!(switcher.hovered, Some(0));
        switcher.handle_mouse(&event(MouseEventKind::Moved, 1, 1));
        assert_eq!(switcher.hovered, None);
        switcher.handle_mouse(&event(MouseEventKind::ScrollUp, 8, 11));
        assert_eq!(switcher.selected, 0);
        switcher.handle_mouse(&event(MouseEventKind::ScrollDown, 8, 11));
        switcher.handle_mouse(&event(MouseEventKind::ScrollDown, 8, 11));
        assert_eq!(switcher.selected, 1);
    }

    #[test]
    fn selected_marker_and_background_cover_the_whole_record() {
        let mut switcher = SessionSwitcher::default();
        switcher.open_with(
            vec![row_with(
                "selected",
                1,
                "2026-07-10T00:00:00Z",
                Some("goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal goal"),
            )],
            SessionScope::Project,
        );
        let app = crate::app::AppState::new("selected".into(), None);
        let theme = crate::theme::theme();
        let expected_background = theme.modal_bg.lerp(theme.accent, 0.22);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 20)).unwrap();

        terminal
            .draw(|frame| {
                <SessionSwitcher as ModalOverlay>::render_content(
                    &mut switcher,
                    frame,
                    frame.area(),
                    &app,
                    &theme,
                );
            })
            .unwrap();

        let rect = switcher.row_hits[0].rect;
        assert_eq!(rect.height, 4);
        let buffer = terminal.backend().buffer();
        for y in rect.y..rect.y + rect.height {
            assert_eq!(buffer[(rect.x, y)].symbol(), "▌");
            for x in rect.x..rect.x + rect.width {
                assert_eq!(buffer[(x, y)].bg, expected_background);
            }
        }
        let goal_y = rect.y + 2;
        for x in rect.x + rect.width - 4..rect.x + rect.width {
            assert_eq!(buffer[(x, goal_y)].symbol(), " ");
        }
    }
}
