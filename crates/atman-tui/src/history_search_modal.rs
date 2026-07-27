use crate::input::InputEditor;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryArea {
    Results,
    Preview,
}

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
    pub preview_lines: Vec<String>,
    pub preview_scroll: u16,
    pub preview_rect: Option<Rect>,
    pub results_rect: Option<Rect>,
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
        self.preview_lines.clear();
        self.preview_scroll = 0;
        self.preview_rect = None;
        self.results_rect = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.results.clear();
        self.error = None;
        self.preview_lines.clear();
    }

    pub fn set_preview(&mut self, lines: Vec<String>) {
        self.preview_lines = lines;
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
        self.preview_scroll = 0;
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.results.len() {
            self.selected += 1;
            self.preview_scroll = 0;
        }
    }

    pub fn scroll_preview(&mut self, up: bool, amount: u16) {
        if up {
            self.preview_scroll = self.preview_scroll.saturating_sub(amount);
        } else {
            self.preview_scroll = self.preview_scroll.saturating_add(amount);
        }
    }

    pub fn hit_test(&self, col: u16, row: u16) -> Option<HistoryArea> {
        if let Some(r) = self.preview_rect {
            if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
                return Some(HistoryArea::Preview);
            }
        }
        if let Some(r) = self.results_rect {
            if col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height {
                return Some(HistoryArea::Results);
            }
        }
        None
    }

    pub fn click_result(&self, col: u16, row: u16) -> Option<usize> {
        let r = self.results_rect?;
        if col < r.x || col >= r.x + r.width || row <= r.y || row >= r.y + r.height {
            return None;
        }
        let idx = (row - r.y - 1) as usize;
        if idx < self.results.len() {
            Some(idx)
        } else {
            None
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
        self.preview_lines.clear();
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
        self.results.clear();
        self.selected = 0;
        self.preview_lines.clear();
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, modal: &mut HistorySearchModal) {
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
    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);
    let title = format!(
        " Search History · {} · Enter=search · ↑↓/jk=navigate · Tab=scope · Esc=close ",
        modal.scope.label()
    );
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::theme().accent.into()))
        .title(Span::styled(
            title,
            Style::default()
                .fg(crate::theme::theme().accent.into())
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
            Constraint::Min(8),
        ])
        .split(inner);
    render_query_row(f, rows[0], modal);
    render_results_row(f, rows[1], modal);
    render_preview_row(f, rows[2], modal);
    modal.results_rect = Some(rows[1]);
    modal.preview_rect = Some(rows[2]);
}

fn render_query_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::theme().warn.into()))
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
        .border_style(Style::default().fg(crate::theme::theme().subtle_fg.into()))
        .title(match modal.error {
            Some(_) => Span::styled(
                " Error ",
                Style::default().fg(crate::theme::theme().error.into()),
            ),
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
                Style::default().fg(crate::theme::theme().subtle_fg.into()),
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
            let mut spans = vec![
                Span::styled(
                    format!("{sid_short:<10}"),
                    Style::default()
                        .fg(crate::theme::theme().warn.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:>5} ", hit.seq),
                    Style::default().fg(crate::theme::theme().subtle_fg.into()),
                ),
                Span::styled(
                    format!("{ts_short:<19} "),
                    Style::default().fg(crate::theme::theme().accent.into()),
                ),
                Span::styled(
                    format!("{:<15} ", hit.kind),
                    Style::default().fg(crate::theme::theme().success.into()),
                ),
            ];
            spans.extend(highlight_snippet(&snippet, &modal.last_query));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(crate::theme::theme().subtle_fg.into())
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    state.select(Some(modal.selected));
    f.render_stateful_widget(list, inner, &mut state);
}

fn highlight_snippet(snippet: &str, query: &str) -> Vec<Span<'static>> {
    let base_style = Style::default().fg(crate::theme::theme().tinted_fg.into());
    let hl_style = Style::default()
        .fg(crate::theme::theme().accent.into())
        .add_modifier(Modifier::BOLD | Modifier::REVERSED);
    if query.is_empty() {
        return vec![Span::styled(snippet.to_string(), base_style)];
    }
    let mut spans = Vec::new();
    let mut remaining = snippet;
    let needle = query.to_lowercase();
    while !remaining.is_empty() {
        if let Some(pos) = remaining.to_lowercase().find(&needle) {
            if pos > 0 {
                spans.push(Span::styled(remaining[..pos].to_string(), base_style));
            }
            let end = pos + needle.len();
            spans.push(Span::styled(remaining[pos..end].to_string(), hl_style));
            remaining = &remaining[end..];
        } else {
            spans.push(Span::styled(remaining.to_string(), base_style));
            break;
        }
    }
    spans
}

fn render_preview_row(f: &mut ratatui::Frame, rect: Rect, modal: &mut HistorySearchModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::theme().subtle_fg.into()))
        .title(" Context (±3 events) ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let text = if modal.preview_lines.is_empty() {
        modal
            .selected_hit()
            .map(|h| h.snippet.clone())
            .unwrap_or_default()
    } else {
        modal.preview_lines.join("\n\n")
    };
    if text.trim().is_empty() {
        return;
    }
    let lines = crate::markdown::render_markdown_with_width(&text, inner.width);
    let max_scroll = lines.len().saturating_sub(inner.height as usize);
    if modal.preview_scroll as usize > max_scroll {
        modal.preview_scroll = max_scroll as u16;
    }
    let scroll = modal.preview_scroll as usize;
    for (i, line) in lines.iter().enumerate().skip(scroll) {
        let display_row = i - scroll;
        if display_row as u16 >= inner.height {
            break;
        }
        f.render_widget(
            line.clone(),
            Rect {
                x: inner.x,
                y: inner.y + display_row as u16,
                width: inner.width,
                height: 1,
            },
        );
    }
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

    #[test]
    fn highlight_snippet_splits_at_match() {
        let spans = highlight_snippet("hello world foo", "world");
        assert_eq!(spans.len(), 3);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world foo");
    }

    #[test]
    fn highlight_snippet_case_insensitive() {
        let spans = highlight_snippet("Hello WORLD", "world");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "Hello ");
        assert_eq!(spans[1].content.as_ref(), "WORLD");
    }

    #[test]
    fn highlight_snippet_empty_query_returns_single_span() {
        let spans = highlight_snippet("plain text", "");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "plain text");
    }

    #[test]
    fn click_result_maps_row_to_index() {
        let mut m = HistorySearchModal::default();
        m.set_results(
            vec![hit("a", 1, "x"), hit("b", 2, "y"), hit("c", 3, "z")],
            "q".into(),
        );
        m.results_rect = Some(Rect {
            x: 10,
            y: 5,
            width: 80,
            height: 10,
        });
        assert_eq!(m.click_result(10, 6), Some(0));
        assert_eq!(m.click_result(10, 7), Some(1));
        assert_eq!(m.click_result(10, 8), Some(2));
        assert_eq!(m.click_result(10, 5), None, "border row should not select");
        assert_eq!(m.click_result(10, 9), None, "past last result");
        assert_eq!(m.click_result(5, 6), None, "outside rect");
    }
}
