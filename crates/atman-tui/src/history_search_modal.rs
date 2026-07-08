use crate::input::InputEditor;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistorySearchScope {
    #[default]
    Session,
    Project,
}

impl HistorySearchScope {
    pub fn toggle(self) -> Self {
        match self {
            HistorySearchScope::Session => HistorySearchScope::Project,
            HistorySearchScope::Project => HistorySearchScope::Session,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            HistorySearchScope::Session => "this session",
            HistorySearchScope::Project => "whole project",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryHit {
    pub session_id: String,
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub snippet: String,
}

#[derive(Default)]
pub struct HistorySearchModal {
    pub open: bool,
    pub editor: InputEditor,
    pub scope: HistorySearchScope,
    pub results: Vec<HistoryHit>,
    pub selected: usize,
    pub error: Option<String>,
    pub last_query: String,
}

impl std::fmt::Debug for HistorySearchModal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistorySearchModal")
            .field("open", &self.open)
            .field("scope", &self.scope)
            .field("results", &self.results.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl HistorySearchModal {
    pub fn open(&mut self) {
        self.open = true;
        self.editor.replace_with("");
        self.results.clear();
        self.selected = 0;
        self.error = None;
        self.last_query.clear();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.results.clear();
        self.error = None;
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
        }
    }

    pub fn selected_hit(&self) -> Option<&HistoryHit> {
        self.results.get(self.selected)
    }

    pub fn set_results(&mut self, hits: Vec<HistoryHit>, query: String) {
        self.selected = 0;
        self.results = hits;
        self.last_query = query;
        self.error = None;
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
        self.results.clear();
        self.selected = 0;
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, modal: &HistorySearchModal) {
    let w = area.width.saturating_sub(4).clamp(70, 140);
    let h = area.height.saturating_sub(4).clamp(20, 42);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let title = format!(
        " Search History · scope: {} · Tab to toggle · Enter to search · Esc to close ",
        modal.scope.label()
    );
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.height < 6 {
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(5),
        ])
        .split(inner);
    render_query_row(f, rows[0], modal);
    render_results_row(f, rows[1], modal);
    render_preview_row(f, rows[2], modal);
}

fn render_query_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(" Query ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let cursor_indicator = "▏";
    let text = format!("{}{cursor_indicator}", modal.editor.buf());
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn render_results_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(match modal.error {
            Some(_) => Span::styled(" Error ", Style::default().fg(Color::Red)),
            None => Span::raw(format!(" Results ({}) ", modal.results.len())),
        });
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    if let Some(err) = &modal.error {
        let para = Paragraph::new(err.as_str()).wrap(Wrap { trim: false });
        f.render_widget(para, inner);
        return;
    }
    if modal.results.is_empty() {
        let hint = if modal.last_query.is_empty() {
            "type a query and press Enter to search"
        } else {
            "no matches"
        };
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            ))),
            inner,
        );
        return;
    }
    let items: Vec<ListItem<'static>> = modal
        .results
        .iter()
        .map(|hit| {
            let sid_short: String = hit.session_id.chars().take(8).collect();
            let ts_short: String = hit.ts.chars().take(19).collect();
            let snippet: String = hit.snippet.chars().take(80).collect();
            let line = Line::from(vec![
                Span::styled(
                    format!("{sid_short:<10}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>5} ", hit.seq),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("{ts_short:<19} "), Style::default().fg(Color::Cyan)),
                Span::styled(
                    format!("{:<15} ", hit.kind),
                    Style::default().fg(Color::Green),
                ),
                Span::styled(snippet, Style::default().fg(Color::Gray)),
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
    state.select(Some(modal.selected));
    f.render_stateful_widget(list, inner, &mut state);
}

fn render_preview_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Preview ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let text = modal
        .selected_hit()
        .map(|h| h.snippet.clone())
        .unwrap_or_default();
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(sid: &str, seq: u64, snippet: &str) -> HistoryHit {
        HistoryHit {
            session_id: sid.into(),
            seq,
            ts: "2026-07-08T00:00:00".into(),
            kind: "user_msg".into(),
            snippet: snippet.into(),
        }
    }

    #[test]
    fn open_resets_state() {
        let mut m = HistorySearchModal::default();
        m.results.push(hit("a", 1, "stale"));
        m.error = Some("stale".into());
        m.open();
        assert!(m.open);
        assert!(m.results.is_empty());
        assert!(m.error.is_none());
        assert_eq!(m.editor.buf(), "");
    }

    #[test]
    fn set_results_replaces_and_resets_selection() {
        let mut m = HistorySearchModal {
            selected: 5,
            ..Default::default()
        };
        m.set_results(vec![hit("a", 1, "foo"), hit("b", 2, "bar")], "foo".into());
        assert_eq!(m.selected, 0);
        assert_eq!(m.results.len(), 2);
        assert_eq!(m.last_query, "foo");
    }

    #[test]
    fn move_down_clamps_at_end() {
        let mut m = HistorySearchModal::default();
        m.set_results(vec![hit("a", 1, ""), hit("b", 2, "")], "q".into());
        m.move_down();
        m.move_down();
        m.move_down();
        assert_eq!(m.selected, 1);
    }

    #[test]
    fn scope_toggle_flips() {
        assert_eq!(
            HistorySearchScope::Session.toggle(),
            HistorySearchScope::Project
        );
        assert_eq!(
            HistorySearchScope::Project.toggle(),
            HistorySearchScope::Session
        );
    }
}
