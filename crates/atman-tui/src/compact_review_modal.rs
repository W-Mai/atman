use crate::wm::modal::ModalAction;

use crate::input::{InputEditor, visual_line_count, wrapped_cursor_col, wrapped_cursor_position};
use crate::keys::KeyAction;
use atman_runtime::PendingCompactReview;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

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
    summary_scroll: u16,
    summary_rect: Option<Rect>,
    follow_cursor: bool,
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
            summary_scroll: 0,
            summary_rect: None,
            follow_cursor: false,
        }
    }

    pub fn enter_editing(&mut self) {
        self.mode = CompactReviewMode::Editing;
        self.follow_cursor = true;
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

fn render_content_body(
    f: &mut ratatui::Frame,
    inner: Rect,
    modal: &mut CompactReviewModal,
    _theme: &crate::theme::Theme,
) {
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
    crate::wm::shell::render_column_divider(
        f,
        cols[0].right(),
        cols[0].y,
        cols[0].height,
        &crate::theme::theme(),
    );
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
    let theme = crate::theme::theme();
    crate::wm::shell::render_section_header(f, rect, Line::from("Slice"), &theme);
    let inner = Rect {
        x: rect.x,
        y: rect.y + 2,
        width: rect.width,
        height: rect.height.saturating_sub(2).saturating_sub(1),
    };
    let text = &modal.pending.slice_preview;
    let para = Paragraph::new(text.as_str())
        .wrap(Wrap { trim: false })
        .scroll((modal.scroll, 0));
    f.render_widget(para, inner);
}

fn render_summary_pane(f: &mut ratatui::Frame, rect: Rect, modal: &mut CompactReviewModal) {
    let theme = crate::theme::theme();
    let title = match modal.mode {
        CompactReviewMode::Viewing => "Summary",
        CompactReviewMode::Editing => "Summary — editing",
    };
    crate::wm::shell::render_section_header(f, rect, Line::from(title), &theme);
    let inner = Rect {
        x: rect.x,
        y: rect.y + 2,
        width: rect.width,
        height: rect.height.saturating_sub(2).saturating_sub(1),
    };
    modal.summary_rect = Some(inner);
    let mut content = if modal.mode == CompactReviewMode::Editing {
        modal.editor.buf().to_string()
    } else {
        let base = if modal.summary_is_dirty() {
            modal.editor.buf()
        } else {
            modal.pending.summary.as_str()
        };
        base.to_string()
    };
    if modal.mode == CompactReviewMode::Editing
        && !modal.editor.buf().is_empty()
        && modal.editor.cursor() == modal.editor.buf().len()
        && inner.width > 0
        && wrapped_cursor_col(
            modal.editor.buf(),
            modal.editor.cursor(),
            inner.width as usize,
        ) >= inner.width as usize
    {
        content.push(' ');
    }
    let height = visual_line_count(&content, inner.width as usize) as u16;
    modal.summary_scroll = modal
        .summary_scroll
        .min(height.saturating_sub(inner.height));
    if modal.mode == CompactReviewMode::Editing && modal.follow_cursor && inner.height > 0 {
        let (row, _) = wrapped_cursor_position(
            modal.editor.buf(),
            modal.editor.cursor(),
            inner.width as usize,
        );
        let row = row as u16;
        if row < modal.summary_scroll {
            modal.summary_scroll = row;
        } else if row >= modal.summary_scroll.saturating_add(inner.height) {
            modal.summary_scroll = row.saturating_sub(inner.height - 1);
        }
        modal.follow_cursor = false;
    }
    let para = Paragraph::new(content)
        .wrap(Wrap { trim: false })
        .scroll((modal.summary_scroll, 0));
    f.render_widget(para, inner);
}

fn render_footer(f: &mut ratatui::Frame, rect: Rect, modal: &CompactReviewModal) {
    let line = match modal.mode {
        CompactReviewMode::Viewing => Line::from(vec![
            Span::styled(
                "Enter",
                Style::default()
                    .fg(crate::theme::theme().success.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" accept  "),
            Span::styled(
                "e",
                Style::default()
                    .fg(crate::theme::theme().accent.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" edit  "),
            Span::styled(
                "r/Esc",
                Style::default()
                    .fg(crate::theme::theme().error.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" reject  "),
            Span::styled(
                "PgUp/PgDn",
                Style::default().fg(crate::theme::theme().subtle_fg.into()),
            ),
            Span::raw(" scroll slice"),
        ]),
        CompactReviewMode::Editing => Line::from(vec![
            Span::styled(
                "Ctrl+Enter",
                Style::default()
                    .fg(crate::theme::theme().success.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" commit edit  "),
            Span::styled(
                "Esc",
                Style::default().fg(crate::theme::theme().warn.into()),
            ),
            Span::raw(" back to viewing (edits kept)"),
        ]),
    };
    f.render_widget(Paragraph::new(line), rect);
}

impl crate::wm::modal::ModalOverlay for CompactReviewModal {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        render_content_body(f, area, self, t);
    }

    fn handle_key(
        &mut self,
        action: &KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        match self.mode {
            CompactReviewMode::Viewing => match action {
                KeyAction::Submit => {
                    if let Some(tx) = tx {
                        let _ = tx.send(crate::TuiControl::CompactReviewAccept {
                            review_id: self.pending.review_id.clone(),
                            edited: None,
                        });
                    }
                    Some(ModalAction::Consumed)
                }
                KeyAction::Char('e') => {
                    self.enter_editing();
                    Some(ModalAction::Consumed)
                }
                KeyAction::Escape | KeyAction::Char('r') => {
                    if let Some(tx) = tx {
                        let _ = tx.send(crate::TuiControl::CompactReviewReject {
                            review_id: self.pending.review_id.clone(),
                        });
                    }
                    Some(ModalAction::Consumed)
                }
                KeyAction::PageUp => {
                    self.scroll = self.scroll.saturating_sub(1);
                    Some(ModalAction::Consumed)
                }
                KeyAction::PageDown => {
                    self.scroll = self.scroll.saturating_add(1);
                    Some(ModalAction::Consumed)
                }
                _ => Some(ModalAction::Consumed),
            },
            CompactReviewMode::Editing => match action {
                KeyAction::Submit => {
                    let edited = self.edited_summary();
                    if let Some(tx) = tx {
                        let _ = tx.send(crate::TuiControl::CompactReviewAccept {
                            review_id: self.pending.review_id.clone(),
                            edited: Some(edited),
                        });
                    }
                    Some(ModalAction::Consumed)
                }
                KeyAction::Escape => {
                    self.mode = CompactReviewMode::Viewing;
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryUp => {
                    self.editor.move_line_up();
                    self.follow_cursor = true;
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryDown => {
                    self.editor.move_line_down();
                    self.follow_cursor = true;
                    Some(ModalAction::Consumed)
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
                    self.follow_cursor = true;
                    Some(ModalAction::Consumed)
                }
                _ => Some(ModalAction::Consumed),
            },
        }
    }

    fn handle_paste(&mut self, text: &str) {
        if self.mode == CompactReviewMode::Editing {
            self.editor.paste_multiline(text);
            self.follow_cursor = true;
        }
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        if self.mode != CompactReviewMode::Editing {
            return None;
        }
        let rect = self.summary_rect?;
        let (row, col) =
            wrapped_cursor_position(self.editor.buf(), self.editor.cursor(), rect.width as usize);
        let visible_row = row.checked_sub(self.summary_scroll as usize)?;
        (visible_row < rect.height as usize && col < rect.width as usize)
            .then_some((rect.x + col as u16, rect.y + visible_row as u16))
    }

    fn title(&self) -> Line<'static> {
        Line::from("Review Compaction")
    }

    fn icon(&self) -> &str {
        "◫"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.warn.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::modal::ModalOverlay;

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
    fn editing_cursor_tracks_multiline_summary_and_viewport() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut modal = CompactReviewModal::new(sample_pending());
        modal.enter_editing();
        modal.editor.replace_with("first\nsecond\nthird\nfourth");
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        terminal
            .draw(|frame| render_summary_pane(frame, Rect::new(2, 1, 8, 5), &mut modal))
            .unwrap();
        assert_eq!(modal.summary_scroll, 2);
        assert_eq!(modal.cursor_position(), Some((8, 4)));

        modal.editor.replace_with("12345678");
        modal.follow_cursor = true;
        terminal
            .draw(|frame| render_summary_pane(frame, Rect::new(2, 1, 8, 5), &mut modal))
            .unwrap();
        assert_eq!(modal.summary_scroll, 0);
        assert_eq!(modal.cursor_position(), Some((2, 4)));

        modal.leave_editing();
        assert_eq!(modal.cursor_position(), None);
    }

    #[test]
    fn modal_manager_routes_the_editing_cursor() {
        let mut modal = CompactReviewModal::new(sample_pending());
        modal.enter_editing();
        modal.summary_rect = Some(Rect::new(2, 3, 40, 2));
        let manager = crate::wm::modal::ModalManager {
            compact_review: Some(modal),
            ..Default::default()
        };
        assert!(manager.cursor_visible(crate::wm::modal::ModalKind::CompactReview));
        assert_eq!(
            manager.cursor_position(crate::wm::modal::ModalKind::CompactReview),
            Some((22, 3))
        );
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
