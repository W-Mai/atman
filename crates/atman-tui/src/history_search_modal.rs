use crate::wm::modal::ModalAction;

use crate::UiState;
use crate::input::InputEditor;
use crate::keys::KeyAction;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

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
    pub last_input_rect: Option<Rect>,
    pub input_focused: bool,
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
        self.input_focused = true;
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

    /// Execute a search using the current editor content as query.
    /// Called on Enter (Submit).
    pub fn run_search(&mut self, app: &crate::app::AppState) {
        let query = self.editor.buf().to_string();
        if query.trim().is_empty() {
            self.results.clear();
            self.last_query.clear();
            self.preview_lines.clear();
            return;
        }
        let Some(session) = app.session.as_ref() else {
            return;
        };
        let Some(idx) = session.project_index() else {
            return;
        };
        let sid = session.id().0.to_string();
        let session_filter = match self.scope {
            HistorySearchScope::Session => Some(sid.as_str()),
            HistorySearchScope::Project => None,
        };
        match idx.fts_search_project_events(&query, session_filter, 50) {
            Ok(rows) => {
                let hits: Vec<HistoryHit> = rows
                    .into_iter()
                    .map(|r| HistoryHit {
                        session_id: r.session_id,
                        seq: r.seq,
                        ts: r.ts,
                        kind: r.kind.clone(),
                        snippet: extract_event_text(&r.kind, &r.payload)
                            .unwrap_or_else(|| r.payload.chars().take(80).collect()),
                    })
                    .collect();
                self.set_results(hits, query);
            }
            Err(e) => {
                self.set_error(format!("search failed: {e}"));
            }
        }
    }
}

pub(crate) fn refresh_history_preview(app: &mut UiState) {
    let (session_id, seq) = match app.wm.modals.history_search.selected_hit() {
        Some(hit) => (hit.session_id.clone(), hit.seq),
        None => {
            app.wm.modals.history_search.set_preview(Vec::new());
            return;
        }
    };
    let Some(session) = app.session.as_ref() else {
        return;
    };
    let Some(idx) = session.project_index() else {
        return;
    };
    let rows = match idx.find_project_events_around(&session_id, seq, 3) {
        Ok(r) => r,
        Err(_) => {
            app.wm.modals.history_search.set_preview(Vec::new());
            return;
        }
    };
    let lines: Vec<String> = rows
        .into_iter()
        .filter_map(|row| {
            let is_hit = row.seq == seq;
            let text = extract_event_text(&row.kind, &row.payload);
            if text.is_none() && !is_hit {
                return None;
            }
            let marker = if is_hit { "▶" } else { " " };
            let body = text.unwrap_or_else(|| format!("<{}>", row.kind));
            Some(format!(
                "{marker} **[{}]** seq={}  \n{}",
                row.kind, row.seq, body
            ))
        })
        .collect();
    app.wm.modals.history_search.set_preview(lines);
}

pub(crate) fn extract_event_text(kind: &str, payload: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    match kind {
        "user_msg" | "assistant_msg" | "system_msg" | "tool_result_msg" => {
            let parts = v.get("message")?.get("parts")?.as_array()?;
            let mut chunks = Vec::new();
            for p in parts {
                if let Some(text) = p.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        chunks.push(text.to_string());
                    }
                } else if let Some(thinking) = p.get("thinking").and_then(|t| t.as_str()) {
                    if !thinking.is_empty() {
                        chunks.push(format!("_{thinking}_"));
                    }
                } else if let Some(summary) = p.get("summary").and_then(|t| t.as_str()) {
                    if !summary.is_empty() {
                        chunks.push(summary.to_string());
                    }
                } else if let Some(content) = p.get("content").and_then(|t| t.as_str()) {
                    if !content.is_empty() {
                        chunks.push(format!("```\n{content}\n```"));
                    }
                }
            }
            if chunks.is_empty() {
                None
            } else {
                Some(chunks.join("\n\n"))
            }
        }
        _ => None,
    }
}

impl crate::wm::modal::ModalOverlay for HistorySearchModal {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        _t: &crate::theme::Theme,
    ) {
        let content_h = area.height.saturating_sub(1);
        let content_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: content_h,
        };
        let help_area = Rect {
            x: area.x,
            y: area.y + content_h,
            width: area.width,
            height: 1,
        };
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Min(8),
            ])
            .split(content_area);
        self.last_input_rect = Some(Rect {
            x: rows[0].x,
            y: rows[0].y.saturating_add(2),
            width: rows[0].width,
            height: 1,
        });
        render_query_row(f, rows[0], self);
        render_results_row(f, rows[1], self);
        render_preview_row(f, rows[2], self);
        self.results_rect = Some(rows[1]);
        self.preview_rect = Some(rows[2]);
        render_help_bar(f, help_area);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        _tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp => {
                self.move_up();
            }
            KeyAction::HistoryDown => {
                self.move_down();
            }
            KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Backspace
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward
            | KeyAction::Char(_) => {
                self.input_focused = true;
                self.editor.handle_key(action);
            }
            KeyAction::PageUp => self.scroll_preview(true, 10),
            KeyAction::PageDown => self.scroll_preview(false, 10),
            KeyAction::ScrollUp => self.scroll_preview(true, 3),
            KeyAction::ScrollDown => self.scroll_preview(false, 3),
            KeyAction::Tab => {
                self.scope = self.scope.toggle();
            }
            KeyAction::Submit => {
                self.run_search(app);
                self.input_focused = false;
            }
            _ => {}
        }
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        if !self.input_focused {
            return None;
        }
        self.last_input_rect.map(|r| {
            let before_cursor = &self.editor.buf()[..self.editor.cursor()];
            let col = crate::width::width(before_cursor) as u16;
            (r.x + col, r.y)
        })
    }

    fn title(&self) -> Line<'static> {
        let t = crate::theme::theme();
        let scope_color = match self.scope {
            HistorySearchScope::Session => t.accent,
            HistorySearchScope::Project => t.warn,
        };
        Line::from(vec![
            Span::styled(
                "Search History · ",
                Style::default()
                    .fg(t.accent.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.scope.label(),
                Style::default()
                    .fg(scope_color.into())
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    }

    fn icon(&self) -> &str {
        "⌕"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

fn section_inner(rect: Rect) -> Rect {
    Rect {
        x: rect.x,
        y: rect.y.saturating_add(2),
        width: rect.width,
        height: rect.height.saturating_sub(2),
    }
}

fn render_help_bar(f: &mut ratatui::Frame, area: Rect) {
    let t = crate::theme::theme();
    let hints = [
        ("Enter", "search"),
        ("↑↓", "navigate"),
        ("Tab", "scope"),
        ("Esc", "close"),
        ("regex", "/pattern/"),
    ];
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (key, desc)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", Style::default().fg(t.subtle_fg.into())));
        }
        spans.push(Span::styled(
            *key,
            Style::default()
                .fg(t.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!(" {desc}"),
            Style::default().fg(t.subtle_fg.into()),
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(t.code_bg.into())),
        area,
    );
}

fn render_query_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let t = crate::theme::theme();
    crate::wm::shell::render_section_header(
        f,
        rect,
        Line::from(Span::styled("Query", Style::default().fg(t.warn.into()))),
        &t,
    );
    let inner = section_inner(rect);
    let cursor_indicator = "▏";
    let text = format!("{}{cursor_indicator}", modal.editor.buf());
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
    let content_w = inner.width as usize;
    let col = crate::input::wrapped_cursor_col(modal.editor.buf(), modal.editor.cursor(), content_w)
        as u16;
    let row = crate::input::wrapped_cursor_row(modal.editor.buf(), modal.editor.cursor(), content_w)
        as u16;
    f.set_cursor_position((inner.x + col, inner.y + row));
}

fn render_results_row(f: &mut ratatui::Frame, rect: Rect, modal: &HistorySearchModal) {
    let t = crate::theme::theme();
    crate::wm::shell::render_section_header(
        f,
        rect,
        Line::from(Span::styled(
            "Results",
            Style::default().fg(t.subtle_fg.into()),
        )),
        &t,
    );
    let inner = section_inner(rect);
    if let Some(err) = &modal.error {
        let para = Paragraph::new(err.as_str()).wrap(Wrap { trim: false });
        f.render_widget(para, inner);
        return;
    }
    if modal.results.is_empty() {
        let t = crate::theme::theme();
        let lines = if modal.last_query.is_empty() {
            vec![
                Line::from(""),
                Line::from(Span::styled(
                    "Search across session or project history.",
                    Style::default().fg(t.subtle_fg.into()),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("  try:  ", Style::default().fg(t.subtle_fg.into())),
                    Span::styled("error", Style::default().fg(t.tinted_fg.into())),
                    Span::styled(
                        "           full-text search",
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("        ", Style::default().fg(t.subtle_fg.into())),
                    Span::styled("role:user", Style::default().fg(t.tinted_fg.into())),
                    Span::styled(
                        "        filter by role",
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("        ", Style::default().fg(t.subtle_fg.into())),
                    Span::styled("/regex/", Style::default().fg(t.tinted_fg.into())),
                    Span::styled(
                        "          regex match",
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("        ", Style::default().fg(t.subtle_fg.into())),
                    Span::styled("Tab", Style::default().fg(t.tinted_fg.into())),
                    Span::styled(
                        "              toggle scope",
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
            ]
        } else {
            vec![Line::from(Span::styled(
                "no matches",
                Style::default().fg(t.subtle_fg.into()),
            ))]
        };
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    let items: Vec<ListItem<'static>> = modal
        .results
        .iter()
        .map(|hit| {
            let sid_short: String = hit.session_id.chars().take(8).collect();
            let ts_short: String = chrono::DateTime::parse_from_rfc3339(&hit.ts)
                .ok()
                .map(|dt| {
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%dT%H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| hit.ts.chars().take(19).collect());
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
    let t = crate::theme::theme();
    crate::wm::shell::render_section_header(
        f,
        rect,
        Line::from(Span::styled(
            "Preview",
            Style::default().fg(t.subtle_fg.into()),
        )),
        &t,
    );
    let inner = section_inner(rect);
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
