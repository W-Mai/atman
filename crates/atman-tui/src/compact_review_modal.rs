use crate::input::InputEditor;
use atman_runtime::PendingCompactReview;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReviewMode {
    Viewing,
    Editing,
}

pub struct CompactReviewModal {
    pub pending: PendingCompactReview,
    pub mode: CompactReviewMode,
    pub editor: InputEditor,
    pub scroll: u16,
}

impl std::fmt::Debug for CompactReviewModal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompactReviewModal")
            .field("review_id", &self.pending.review_id)
            .field("mode", &self.mode)
            .field("summary_len", &self.pending.summary.len())
            .finish()
    }
}

impl CompactReviewModal {
    pub fn new(pending: PendingCompactReview) -> Self {
        let mut editor = InputEditor::default();
        editor.replace_with(&pending.summary);
        Self {
            pending,
            mode: CompactReviewMode::Viewing,
            editor,
            scroll: 0,
        }
    }

    pub fn enter_editing(&mut self) {
        self.mode = CompactReviewMode::Editing;
    }

    pub fn leave_editing(&mut self) {
        self.mode = CompactReviewMode::Viewing;
    }

    pub fn edited_summary(&self) -> String {
        self.editor.buf().to_string()
    }

    pub fn summary_is_dirty(&self) -> bool {
        self.editor.buf() != self.pending.summary
    }

    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(4);
    }

    pub fn scroll_down(&mut self) {
        self.scroll = self.scroll.saturating_add(4);
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, modal: &CompactReviewModal) {
    let w = area.width.saturating_sub(4).clamp(60, 140);
    let h = area.height.saturating_sub(4).clamp(16, 40);
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
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            format!(
                " Review Compaction — slice {}..{} ({} msgs, ~{} tokens) ",
                modal.pending.range_start,
                modal.pending.range_end,
                modal.pending.slice_count,
                modal.pending.tokens_before,
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.height < 4 {
        return;
    }
    let footer_h: u16 = 2;
    let body_h = inner.height.saturating_sub(footer_h);
    let body_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: body_h,
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body_rect);
    render_slice_pane(f, cols[0], modal);
    render_summary_pane(f, cols[1], modal);
    let footer_rect = Rect {
        x: inner.x,
        y: inner.y.saturating_add(body_h),
        width: inner.width,
        height: footer_h,
    };
    render_footer(f, footer_rect, modal);
}

fn render_slice_pane(f: &mut ratatui::Frame, rect: Rect, modal: &CompactReviewModal) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" Being replaced ");
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let text = &modal.pending.slice_preview;
    let para = Paragraph::new(text.as_str())
        .wrap(Wrap { trim: false })
        .scroll((modal.scroll, 0));
    f.render_widget(para, inner);
}

fn render_summary_pane(f: &mut ratatui::Frame, rect: Rect, modal: &CompactReviewModal) {
    let (title, colour) = match modal.mode {
        CompactReviewMode::Viewing => (" Summary ", Color::Gray),
        CompactReviewMode::Editing => (" Summary — editing ", Color::Green),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colour))
        .title(title);
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let content = if modal.mode == CompactReviewMode::Editing {
        modal.editor.buf().to_string()
    } else {
        let base = if modal.summary_is_dirty() {
            modal.editor.buf()
        } else {
            modal.pending.summary.as_str()
        };
        base.to_string()
    };
    let para = Paragraph::new(content).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn render_footer(f: &mut ratatui::Frame, rect: Rect, modal: &CompactReviewModal) {
    let line = match modal.mode {
        CompactReviewMode::Viewing => Line::from(vec![
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" accept  "),
            Span::styled(
                "e",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" edit  "),
            Span::styled(
                "r/Esc",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" reject  "),
            Span::styled("PgUp/PgDn", Style::default().fg(Color::DarkGray)),
            Span::raw(" scroll slice"),
        ]),
        CompactReviewMode::Editing => Line::from(vec![
            Span::styled(
                "Ctrl+Enter",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" commit edit  "),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::raw(" back to viewing (edits kept)"),
        ]),
    };
    f.render_widget(Paragraph::new(line), rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pending() -> PendingCompactReview {
        PendingCompactReview {
            review_id: "r-test".into(),
            summary: "initial summary text".into(),
            slice_preview: "[0] user: hi\n[1] assistant: yo\n".into(),
            slice_count: 2,
            range_start: 1,
            range_end: 3,
            tokens_before: 1234,
            emitted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn new_starts_in_viewing_with_editor_preloaded() {
        let modal = CompactReviewModal::new(sample_pending());
        assert_eq!(modal.mode, CompactReviewMode::Viewing);
        assert_eq!(modal.editor.buf(), "initial summary text");
        assert!(!modal.summary_is_dirty());
    }

    #[test]
    fn enter_and_leave_editing_toggle_mode() {
        let mut modal = CompactReviewModal::new(sample_pending());
        modal.enter_editing();
        assert_eq!(modal.mode, CompactReviewMode::Editing);
        modal.leave_editing();
        assert_eq!(modal.mode, CompactReviewMode::Viewing);
    }

    #[test]
    fn edited_summary_returns_editor_buffer() {
        let mut modal = CompactReviewModal::new(sample_pending());
        modal.editor.replace_with("edited by user");
        assert_eq!(modal.edited_summary(), "edited by user");
        assert!(modal.summary_is_dirty());
    }
}
