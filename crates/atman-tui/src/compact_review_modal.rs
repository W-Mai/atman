use crate::wm::modal::ModalAction;

use crate::input::{InputEditor, visual_line_count, wrapped_cursor_col, wrapped_cursor_position};
use crate::keys::KeyAction;
use atman_runtime::PendingCompactReview;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::Line;
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
    slice_rect: Option<Rect>,
    action_hits: Vec<(KeyAction, Rect)>,
    hovered_action: Option<usize>,
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
            slice_rect: None,
            action_hits: Vec::new(),
            hovered_action: None,
            follow_cursor: false,
        }
    }

    pub fn enter_editing(&mut self) {
        self.mode = CompactReviewMode::Editing;
        self.follow_cursor = true;
        self.action_hits.clear();
        self.hovered_action = None;
    }

    pub fn leave_editing(&mut self) {
        self.mode = CompactReviewMode::Viewing;
        self.action_hits.clear();
        self.hovered_action = None;
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

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        use crate::wm::modal::ModalOverlay;
        use crossterm::event::{MouseButton, MouseEventKind};
        let point = (event.column, event.row).into();
        let action = self
            .action_hits
            .iter()
            .position(|(_, rect)| rect.contains(point));
        match event.kind {
            MouseEventKind::Moved => self.hovered_action = action,
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = action {
                    self.handle_key(&self.action_hits[index].0.clone(), app, tx);
                } else if self.mode == CompactReviewMode::Editing
                    && let Some(rect) = self.summary_rect.filter(|rect| rect.contains(point))
                {
                    let cursor = crate::input::cursor_from_wrapped(
                        self.editor.buf(),
                        (event.row - rect.y) as usize + self.summary_scroll as usize,
                        (event.column - rect.x) as usize,
                        rect.width as usize,
                    );
                    self.editor.set_cursor(cursor);
                    self.follow_cursor = true;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let scroll = if self.summary_rect.is_some_and(|rect| rect.contains(point)) {
                    self.follow_cursor = false;
                    &mut self.summary_scroll
                } else if self.slice_rect.is_some_and(|rect| rect.contains(point)) {
                    &mut self.scroll
                } else {
                    return;
                };
                *scroll = if event.kind == MouseEventKind::ScrollUp {
                    scroll.saturating_sub(3)
                } else {
                    scroll.saturating_add(3)
                };
            }
            _ => {}
        }
    }
}

fn render_content_body(
    f: &mut ratatui::Frame,
    inner: Rect,
    modal: &mut CompactReviewModal,
    _theme: &crate::theme::Theme,
) {
    modal.action_hits.clear();
    modal.summary_rect = None;
    modal.slice_rect = None;
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let footer_h: u16 = if inner.height >= 9 {
        4
    } else {
        inner.height.min(3)
    };
    let body_h = inner.height.saturating_sub(footer_h);
    let body_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: body_h,
    };
    let cols = Layout::default()
        .direction(if inner.width >= 60 {
            Direction::Horizontal
        } else {
            Direction::Vertical
        })
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body_rect);
    render_slice_pane(f, cols[0], modal);
    if inner.width >= 60 {
        crate::wm::shell::render_column_divider(
            f,
            cols[0].right(),
            cols[0].y,
            cols[0].height,
            &crate::theme::theme(),
        );
    }
    render_summary_pane(f, cols[1], modal);
    let footer_rect = Rect {
        x: inner.x,
        y: inner.y.saturating_add(body_h),
        width: inner.width,
        height: footer_h,
    };
    render_footer(f, footer_rect, modal);
}

fn render_slice_pane(f: &mut ratatui::Frame, rect: Rect, modal: &mut CompactReviewModal) {
    if rect.height == 0 || rect.width == 0 {
        return;
    }
    let theme = crate::theme::theme();
    crate::wm::shell::render_section_header(f, rect, Line::from("Slice"), &theme);
    let inner = Rect {
        x: rect.x,
        y: rect.y + rect.height.min(2),
        width: rect.width,
        height: rect.height.saturating_sub(2).saturating_sub(1),
    };
    let text = &modal.pending.slice_preview;
    modal.slice_rect = Some(inner);
    modal.scroll = modal.scroll.min(
        visual_line_count(text, inner.width as usize)
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16,
    );
    let para = Paragraph::new(text.as_str())
        .wrap(Wrap { trim: false })
        .scroll((modal.scroll, 0));
    f.render_widget(para, inner);
}

fn render_summary_pane(f: &mut ratatui::Frame, rect: Rect, modal: &mut CompactReviewModal) {
    if rect.height == 0 || rect.width == 0 {
        return;
    }
    let theme = crate::theme::theme();
    let title = match modal.mode {
        CompactReviewMode::Viewing => "Summary",
        CompactReviewMode::Editing => "Summary — editing",
    };
    crate::wm::shell::render_section_header(f, rect, Line::from(title), &theme);
    let inner = Rect {
        x: rect.x,
        y: rect.y + rect.height.min(2),
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
    let height = visual_line_count(&content, inner.width as usize).min(u16::MAX as usize) as u16;
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

fn render_footer(f: &mut ratatui::Frame, rect: Rect, modal: &mut CompactReviewModal) {
    modal.action_hits.clear();
    if rect.height == 0 || rect.width == 0 {
        return;
    }
    let t = crate::theme::theme();
    let actions: &[(&str, &str, KeyAction)] = match modal.mode {
        CompactReviewMode::Viewing => &[
            ("Accept", "Enter", KeyAction::Submit),
            ("Edit", "e", KeyAction::Char('e')),
            ("Reject", "Esc", KeyAction::Escape),
        ],
        CompactReviewMode::Editing => &[
            ("Save", "Enter", KeyAction::Submit),
            ("Back", "Esc", KeyAction::Escape),
        ],
    };
    let gap = u16::from(rect.width >= actions.len() as u16 * 4);
    let available = rect.width.saturating_sub(gap * (actions.len() as u16 - 1));
    let desired: Vec<u16> = actions
        .iter()
        .map(|(label, shortcut, _)| (label.len() + shortcut.len() + 4) as u16)
        .collect();
    let desired_total = desired.iter().sum::<u16>();
    let mut x = rect.x;
    for (index, (label, shortcut, action)) in actions.iter().enumerate() {
        let width = if index + 1 == actions.len() {
            rect.right().saturating_sub(x)
        } else if available >= desired_total {
            desired[index] + (available - desired_total) / actions.len() as u16
        } else {
            (u32::from(available) * u32::from(desired[index]) / u32::from(desired_total)) as u16
        };
        let button = Rect::new(x, rect.y, width, rect.height.min(3));
        crate::settings_form::render_action(
            f,
            button,
            &crate::settings_form::Action {
                label,
                shortcut,
                hovered: modal.hovered_action == Some(index),
                ..Default::default()
            },
            &t,
        );
        modal.action_hits.push((action.clone(), button));
        x = button.right().saturating_add(gap).min(rect.right());
    }
    if rect.height > 3 {
        let hint = match modal.mode {
            CompactReviewMode::Viewing => "↑↓: summary · PgUp/PgDn: slice · Wheel: scroll",
            CompactReviewMode::Editing => "Shift+Enter: newline · Esc: back",
        };
        f.render_widget(
            Paragraph::new(crate::width::truncate(hint, rect.width as usize))
                .style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(rect.x, rect.y + 3, rect.width, 1),
        );
    }
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
                            edited: self.summary_is_dirty().then(|| self.edited_summary()),
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
                    self.scroll = self
                        .scroll
                        .saturating_sub(self.slice_rect.map_or(1, |r| r.height.max(1)));
                    Some(ModalAction::Consumed)
                }
                KeyAction::PageDown => {
                    self.scroll = self
                        .scroll
                        .saturating_add(self.slice_rect.map_or(1, |r| r.height.max(1)));
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryUp | KeyAction::ScrollUp => {
                    self.summary_scroll = self.summary_scroll.saturating_sub(1);
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryDown | KeyAction::ScrollDown => {
                    self.summary_scroll = self.summary_scroll.saturating_add(1);
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
                    self.leave_editing();
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryUp => {
                    self.editor.move_line_up_visual(
                        self.summary_rect.map_or(1, |r| r.width.max(1)) as usize,
                    );
                    self.follow_cursor = true;
                    Some(ModalAction::Consumed)
                }
                KeyAction::HistoryDown => {
                    self.editor.move_line_down_visual(
                        self.summary_rect.map_or(1, |r| r.width.max(1)) as usize,
                    );
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
                | KeyAction::Newline
                | KeyAction::Char(_) => {
                    self.editor.handle_key(action);
                    self.follow_cursor = true;
                    Some(ModalAction::Consumed)
                }
                KeyAction::PageUp | KeyAction::ScrollUp => {
                    let amount = if *action == KeyAction::PageUp {
                        self.summary_rect.map_or(1, |r| r.height.max(1))
                    } else {
                        1
                    };
                    self.summary_scroll = self.summary_scroll.saturating_sub(amount);
                    self.follow_cursor = false;
                    Some(ModalAction::Consumed)
                }
                KeyAction::PageDown | KeyAction::ScrollDown => {
                    let amount = if *action == KeyAction::PageDown {
                        self.summary_rect.map_or(1, |r| r.height.max(1))
                    } else {
                        1
                    };
                    self.summary_scroll = self.summary_scroll.saturating_add(amount);
                    self.follow_cursor = false;
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
    fn action_geometry_stays_inside_the_viewport_and_dispatches_visible_actions() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let mut modal = CompactReviewModal::new(sample_pending());
        let mut app = crate::app::AppState::default();
        let theme = crate::theme::theme();
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        for width in [1, 20, 40, 80] {
            for height in [1, 3, 8, 18] {
                let area = Rect::new(2, 1, width, height);
                terminal
                    .draw(|frame| render_content_body(frame, area, &mut modal, &theme))
                    .unwrap();
                for (_, rect) in &modal.action_hits {
                    assert!(rect.x >= area.x && rect.y >= area.y);
                    assert!(rect.right() <= area.right() && rect.bottom() <= area.bottom());
                    assert_eq!(rect.height, height.min(3));
                }
                for pair in modal.action_hits.windows(2) {
                    assert!(pair[0].1.right() <= pair[1].1.x);
                }
            }
        }

        let area = Rect::new(2, 1, 80, 18);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let click = |rect: Rect| MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        };
        let edit = modal.action_hits[1].1;
        modal.handle_mouse(&click(edit), &mut app, Some(&tx));
        assert_eq!(modal.mode, CompactReviewMode::Editing);
        modal.editor.replace_with("edited summary");
        terminal
            .draw(|frame| render_content_body(frame, area, &mut modal, &theme))
            .unwrap();
        let back = modal.action_hits[1].1;
        modal.handle_mouse(&click(back), &mut app, Some(&tx));
        assert_eq!(modal.mode, CompactReviewMode::Viewing);
        assert_eq!(modal.edited_summary(), "edited summary");
        terminal
            .draw(|frame| render_content_body(frame, area, &mut modal, &theme))
            .unwrap();
        let accept = modal.action_hits[0].1;
        modal.handle_mouse(&click(accept), &mut app, Some(&tx));
        assert!(matches!(
            rx.try_recv().unwrap(),
            crate::TuiControl::CompactReviewAccept { edited: Some(summary), .. }
                if summary == "edited summary"
        ));
        let reject = modal.action_hits[2].1;
        modal.handle_mouse(&click(reject), &mut app, Some(&tx));
        assert!(matches!(
            rx.try_recv().unwrap(),
            crate::TuiControl::CompactReviewReject { .. }
        ));
    }

    #[test]
    fn editing_uses_rendered_wrap_for_cursor_and_keeps_pane_scrolls_independent() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};

        let mut modal = CompactReviewModal::new(sample_pending());
        let mut app = crate::app::AppState::default();
        modal.enter_editing();
        modal.editor.replace_with("abcdefghij");
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal
            .draw(|frame| render_summary_pane(frame, Rect::new(2, 1, 4, 8), &mut modal))
            .unwrap();
        modal.handle_key(&KeyAction::HistoryUp, &mut app, None);
        assert_eq!(modal.editor.cursor(), 6);
        modal.handle_key(&KeyAction::HistoryDown, &mut app, None);
        assert_eq!(modal.editor.cursor(), 10);
        let input = modal.summary_rect.unwrap();
        let mut mouse = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: input.x + 1,
            row: input.y + 1,
            modifiers: KeyModifiers::NONE,
        };
        modal.handle_mouse(&mouse, &mut app, None);
        assert_eq!(modal.editor.cursor(), 5);
        modal.handle_key(&KeyAction::Newline, &mut app, None);
        assert_eq!(modal.editor.buf(), "abcde\nfghij");
        mouse.kind = MouseEventKind::ScrollDown;
        modal.handle_mouse(&mouse, &mut app, None);
        assert_eq!(modal.scroll, 0);
        assert_eq!(modal.summary_scroll, 3);
        terminal
            .draw(|frame| render_summary_pane(frame, Rect::new(2, 1, 4, 8), &mut modal))
            .unwrap();
        assert_eq!(modal.summary_scroll, 0);
        assert!(!modal.follow_cursor);
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
