use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Intervene,
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
    selected: usize,
    focused: bool,
    hovered: Option<usize>,
    edit: Option<&crate::app::QueuedSubmissionEdit>,
) -> QueueHitMap {
    if submissions.is_empty() || area.height < 3 {
        return QueueHitMap::default();
    }
    let t = crate::theme::theme();
    let border = if focused { t.accent } else { t.subtle_fg };
    let hint = if focused {
        " Enter interrupt · e edit · alt+↑/↓ move · Del remove · Tab input "
    } else {
        " Shift+Tab focus "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
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
    let visible = inner.height as usize;
    let selected = selected.min(submissions.len().saturating_sub(1));
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(submissions.len().saturating_sub(visible));
    let end = (start + visible).min(submissions.len());
    let mut lines = Vec::with_capacity(visible);
    let mut hitmap = QueueHitMap::default();

    for (row, index) in (start..end).enumerate() {
        let submission = &submissions[index];
        let active = focused && index == selected;
        let highlighted = active || hovered == Some(index);
        let marker = if active { "●" } else { "○" };
        let editing = edit.filter(|editing| editing.id == submission.id);
        let text = editing
            .map(|editing| editing.editor.buf())
            .unwrap_or(&submission.text)
            .replace(['\n', '\r'], " ");
        let prefix = format!(" {marker} {}. ", index + 1);
        let actions = if active && edit.is_none() {
            "  interrupt  edit  ↑  ↓  delete "
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
                Style::default().fg(if highlighted { t.accent } else { t.subtle_fg }.into()),
            ),
            Span::styled(
                crate::width::pad_right(&display_text, text_width),
                Style::default()
                    .fg(if highlighted { t.heading } else { t.tinted_fg }.into())
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
                Style::default().fg(t.subtle_fg.into()),
            ));
        }
        lines.push(Line::from(spans));

        let row_rect = Rect::new(inner.x, inner.y + row as u16, inner.width, 1);
        hitmap.rows.push((index, row_rect));
        if active && edit.is_none() {
            let labels = [
                (QueueAction::Intervene, "interrupt"),
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
                    0,
                    true,
                    None,
                    None,
                );
            })
            .unwrap();

        assert_eq!(hitmap.row_at(2, 1), Some(0));
        assert!(
            hitmap
                .actions
                .iter()
                .any(|(index, action, _)| { *index == 0 && *action == QueueAction::Intervene })
        );
        assert!(hitmap.actions.iter().all(|(index, _, _)| *index == 0));
    }
}
