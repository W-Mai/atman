use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};

use crate::SessionPickerRow;

#[derive(Default)]
pub struct SessionSwitcher {
    pub open: bool,
    pub rows: Vec<SessionPickerRow>,
    pub selected: usize,
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
    pub fn open_with(&mut self, rows: Vec<SessionPickerRow>) {
        self.rows = rows;
        self.selected = 0;
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.rows.clear();
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

pub fn render(f: &mut ratatui::Frame, area: Rect, switcher: &SessionSwitcher) {
    let w = area.width.saturating_sub(4).clamp(60, 100);
    let desired = 3 + switcher.rows.len() as u16 + 2;
    let h = area.height.saturating_sub(4).min(desired).max(8);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Switch Session (Enter to swap, Esc to cancel) ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.height == 0 || switcher.rows.is_empty() {
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
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>5} msgs  ", row.message_count),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("{updated:<19}  "), Style::default().fg(Color::Cyan)),
                Span::styled(project_label, Style::default().fg(Color::Green)),
                Span::styled(
                    format!("  {goal_snippet}"),
                    Style::default().fg(Color::Gray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
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

    #[test]
    fn open_with_rows_sets_selection_to_zero() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1), row("b", 2)]);
        assert!(s.open);
        assert_eq!(s.selected, 0);
        assert_eq!(s.selected_id().as_deref(), Some("a"));
    }

    #[test]
    fn move_down_clamps_at_end() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1), row("b", 2)]);
        s.move_down();
        s.move_down();
        s.move_down();
        assert_eq!(s.selected, 1);
        assert_eq!(s.selected_id().as_deref(), Some("b"));
    }

    #[test]
    fn close_clears_state() {
        let mut s = SessionSwitcher::default();
        s.open_with(vec![row("a", 1)]);
        s.close();
        assert!(!s.open);
        assert!(s.rows.is_empty());
    }
}
