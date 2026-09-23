use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Insert,
    Edit,
    MoveUp,
    MoveDown,
    Delete,
}

#[derive(Debug, Clone, Default)]
pub struct QueueHitMap {
    pub rows: Vec<(usize, Rect)>,
    pub actions: Vec<(usize, QueueAction, Rect)>,
    pub edit_origin: Option<(u16, u16)>,
}

pub struct QueueRenderState<'a> {
    pub selected: usize,
    pub focused: bool,
    pub hovered: Option<usize>,
    pub edit: Option<&'a crate::app::QueuedSubmissionEdit>,
    pub active_turn: bool,
}

pub(crate) fn unavailable_lines(
    submissions: &[atman_runtime::QueuedSubmissionView],
    selected: usize,
    focused: bool,
    active_turn: bool,
    width: u16,
) -> Vec<String> {
    let reason = focused
        .then(|| submissions.get(selected))
        .flatten()
        .and_then(|submission| {
            submission
                .insert_block_reason
                .as_deref()
                .or((!active_turn).then_some("no active flow"))
        });
    reason.map_or_else(Vec::new, |reason| {
        crate::width::word_wrap(
            &format!("i insert unavailable · {reason}"),
            width.saturating_sub(6).max(1) as usize,
        )
    })
}

impl QueueHitMap {
    pub fn row_at(&self, x: u16, y: u16) -> Option<usize> {
        self.rows
            .iter()
            .find_map(|(index, rect)| contains(*rect, x, y).then_some(*index))
    }

    pub fn action_at(&self, x: u16, y: u16) -> Option<(usize, QueueAction)> {
        self.actions
            .iter()
            .find_map(|(index, action, rect)| contains(*rect, x, y).then_some((*index, *action)))
    }
}

pub fn render(
    f: &mut ratatui::Frame,
    area: Rect,
    submissions: &[atman_runtime::QueuedSubmissionView],
    state: QueueRenderState<'_>,
) -> QueueHitMap {
    let QueueRenderState {
        selected,
        focused,
        hovered,
        edit,
        active_turn,
    } = state;
    if submissions.is_empty() || area.height < 3 {
        return QueueHitMap::default();
    }
    let t = crate::theme::theme();
    let border = if focused { t.accent } else { t.subtle_fg };
    let selected = selected.min(submissions.len().saturating_sub(1));
    let reason_lines = unavailable_lines(submissions, selected, focused, active_turn, area.width);
    let show_reason =
        !reason_lines.is_empty() && area.height.saturating_sub(2) as usize > reason_lines.len();
    let hint = if focused && show_reason {
        " Enter/e edit · ↑/↓ select · Del remove · Tab input "
    } else if focused && !reason_lines.is_empty() {
        " i unavailable · click insert for reason "
    } else if focused {
        " i insert (flow settings) · Enter/e edit · ↑/↓ select · Del remove · Tab input "
    } else {
        " Shift+Tab focus "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(border.into()))
        .title(Span::styled(
            format!(" next · {} ", submissions.len()),
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(Span::styled(hint, Style::default().fg(t.subtle_fg.into()))).right_aligned(),
        )
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    let reason_rows = if show_reason { reason_lines.len() } else { 0 };
    let visible = inner.height as usize - reason_rows;
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(submissions.len().saturating_sub(visible));
    let end = (start + visible).min(submissions.len());
    let mut lines = Vec::with_capacity(visible);
    let mut hitmap = QueueHitMap::default();

    for (row, index) in (start..end).enumerate() {
        let submission = &submissions[index];
        let active = focused && index == selected;
        let hovered = hovered == Some(index);
        let row_bg = if active || hovered {
            t.work_hover_bg.into()
        } else {
            ratatui::style::Color::Reset
        };
        let marker = if active { "●" } else { "○" };
        let editing = edit.filter(|editing| editing.id == submission.id);
        let mut text = editing
            .map(|editing| editing.editor.buf())
            .unwrap_or(&submission.text)
            .replace(['\n', '\r'], " ");
        if let Some(quote) = submission
            .presentation
            .as_ref()
            .and_then(|value| value.quote.as_ref())
        {
            let preview = quote.text.lines().next().unwrap_or_default();
            if text.trim().is_empty() {
                text = format!("quote: {preview}");
            } else {
                text = format!("{text}  · quote: {preview}");
            }
        }
        let prefix = format!(" {marker} {}. ", index + 1);
        let actions = if active && edit.is_none() {
            "  insert  edit  ↑  ↓  delete "
        } else if active {
            "  Enter save · Esc cancel "
        } else {
            ""
        };
        let text_width = inner
            .width
            .saturating_sub(crate::width::width(&prefix) as u16)
            .saturating_sub(crate::width::width(actions) as u16) as usize;
        let (display_text, edit_cursor_col) = if let Some(editing) = editing {
            let cursor_col = editing.editor.cursor_display_col();
            let offset = cursor_col.saturating_sub(text_width.saturating_sub(1));
            (
                crate::width::trim_display_offset(&text, offset, text_width),
                Some(cursor_col.saturating_sub(offset)),
            )
        } else {
            (crate::width::truncate(&text, text_width), None)
        };
        let mut spans = vec![
            Span::styled(
                prefix,
                Style::default()
                    .fg(if active { t.accent } else { t.subtle_fg }.into())
                    .bg(row_bg),
            ),
            Span::styled(
                crate::width::pad_right(&display_text, text_width),
                Style::default()
                    .fg(if active { t.heading } else { t.tinted_fg }.into())
                    .bg(row_bg)
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ];
        if !actions.is_empty() {
            spans.push(Span::styled(
                actions,
                Style::default().fg(t.subtle_fg.into()).bg(row_bg),
            ));
        }
        lines.push(Line::from(spans));

        let row_rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        hitmap.rows.push((index, row_rect));
        if active && edit.is_none() {
            let labels = [
                (QueueAction::Insert, "insert"),
                (QueueAction::Edit, "edit"),
                (QueueAction::MoveUp, "↑"),
                (QueueAction::MoveDown, "↓"),
                (QueueAction::Delete, "delete"),
            ];
            let mut x = inner.x
                + inner
                    .width
                    .saturating_sub(crate::width::width(actions) as u16);
            for (action, label) in labels {
                x = x.saturating_add(2);
                let width = crate::width::width(label) as u16;
                hitmap
                    .actions
                    .push((index, action, Rect::new(x, row_rect.y, width, 1)));
                x = x.saturating_add(width);
            }
        }
        if active && let Some(cursor_col) = edit_cursor_col {
            hitmap.edit_origin = Some((
                inner.x
                    + crate::width::width(&format!(" {marker} {}. ", index + 1)) as u16
                    + cursor_col.min(u16::MAX as usize) as u16,
                row_rect.y,
            ));
        }
    }
    if reason_rows > 0 {
        lines.extend(reason_lines.into_iter().map(|line| {
            Line::from(Span::styled(
                format!("│ {line}"),
                Style::default().fg(t.subtle_fg.into()),
            ))
        }));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
    hitmap
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_queue_exposes_mouse_rows_and_actions() {
        let session = atman_runtime::Session::open_ephemeral();
        session
            .enqueue_submission(
                "first",
                Vec::new(),
                atman_runtime::InvocationEnv::default(),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        session
            .enqueue_submission(
                "second",
                Vec::new(),
                atman_runtime::InvocationEnv::default(),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let submissions = session.queued_submissions();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 6)).expect("terminal");
        let mut hitmap = QueueHitMap::default();
        terminal
            .draw(|frame| {
                hitmap = render(
                    frame,
                    Rect::new(0, 0, 80, 6),
                    &submissions,
                    QueueRenderState {
                        selected: 0,
                        focused: true,
                        hovered: None,
                        edit: None,
                        active_turn: true,
                    },
                );
            })
            .unwrap();

        assert_eq!(hitmap.row_at(2, 1), Some(0));
        assert!(
            hitmap
                .actions
                .iter()
                .any(|(index, action, _)| { *index == 0 && *action == QueueAction::Edit })
        );
        assert!(hitmap.actions.iter().all(|(index, _, _)| *index == 0));
    }

    #[test]
    fn unavailable_reason_uses_content_row_without_clipping_its_start() {
        let session = atman_runtime::Session::open_ephemeral();
        session
            .enqueue_submission(
                "@./image.png inspect",
                Vec::new(),
                atman_runtime::InvocationEnv::single(
                    "effort",
                    atman_runtime::Value::Str("medium".into()),
                ),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let submissions = session.queued_submissions();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 5)).unwrap();
        terminal
            .draw(|frame| {
                render(
                    frame,
                    Rect::new(0, 0, 80, 5),
                    &submissions,
                    QueueRenderState {
                        selected: 0,
                        focused: true,
                        hovered: None,
                        edit: None,
                        active_turn: true,
                    },
                );
            })
            .unwrap();
        let screen = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("i insert unavailable · path attachments need a separate turn"));
    }
}
