#[cfg(test)]
use atman_rt::Value as CoreValue;
mod model;
#[cfg(test)]
#[path = "submission_queue/tests.rs"]
mod ownership_tests;
pub use model::QueueItem;
pub(crate) use model::{QueueKey, QueueRequest, payload_text};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};

#[cfg(test)]
type Value = CoreValue<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueAction {
    Recover,
    History,
    PreviousPage,
    NextPage,
    Insert,
    Edit,
    MoveUp,
    MoveDown,
    Delete,
    Save,
    Cancel,
}

#[derive(Debug, Clone, Default)]
pub struct QueueHitMap {
    pub rows: Vec<(usize, Rect)>,
    pub actions: Vec<(usize, QueueAction, Rect)>,
    pub edit_origin: Option<(u16, u16)>,
    pub edit_area: Option<(Rect, usize)>,
}

pub struct QueueRenderState<'a> {
    pub recovery: Option<RecoveryView<'a>>,
    pub page: Option<QueuePage>,
    pub selected: usize,
    pub focused: bool,
    pub hovered: Option<usize>,
    pub hovered_action: Option<QueueAction>,
    pub edit: Option<&'a crate::app::QueuedSubmissionEdit>,
    pub active_turn: bool,
    pub saving: bool,
}

pub const HEADER_ROWS: u16 = 3;

#[derive(Clone, Copy)]
pub struct RecoveryView<'a> {
    pub reason: &'static str,
    pub pending: bool,
    pub blocked: Option<&'static str>,
    pub error: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub struct QueuePage {
    pub offset: usize,
    pub total: usize,
}

impl QueuePage {
    pub fn paged(self) -> bool {
        self.total > atman_runtime::flow_runtime::PENDING_VIEW_PAGE_SIZE
    }
}

pub(crate) fn height(submissions: &[QueueItem], editing: bool, paged: bool, width: u16) -> u16 {
    if editing {
        11
    } else if submissions.is_empty() {
        0
    } else {
        HEADER_ROWS
            + submissions.len().min(3) as u16 * 3
            + 1
            + if paged { 3 } else { 0 }
            + if width < 80 && submissions.iter().any(QueueItem::editable) {
                3
            } else {
                0
            }
    }
}

fn hint(
    item: Option<&QueueItem>,
    focused: bool,
    active_turn: bool,
    recovery: Option<RecoveryView<'_>>,
) -> String {
    if let Some(recovery) = recovery {
        if let Some(error) = recovery.error {
            format!("Recover failed: {error}")
        } else if let Some(blocked) = recovery.blocked {
            blocked.to_owned()
        } else if recovery.pending {
            "Restoring execution".into()
        } else {
            format!("{} · queued input retained", recovery.reason)
        }
    } else if !focused {
        "Shift+Tab focus · click to select".into()
    } else if let Some(item) = item {
        if !item.editable() {
            "Read-only notification".into()
        } else if !active_turn {
            "Queued for next turn".into()
        } else if item.is_inserted() {
            "Waiting for next model call".into()
        } else {
            item.insert_block_reason()
                .unwrap_or("Queued for next turn")
                .into()
        }
    } else {
        String::new()
    }
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
    submissions: &[QueueItem],
    state: QueueRenderState<'_>,
) -> QueueHitMap {
    let QueueRenderState {
        recovery,
        page,
        selected,
        focused,
        hovered,
        hovered_action,
        edit,
        active_turn,
        saving,
    } = state;
    let mut hitmap = QueueHitMap::default();
    if (submissions.is_empty() && edit.is_none()) || area.height < 3 || area.width < 8 {
        return hitmap;
    }
    let t = crate::theme::theme();
    f.render_widget(
        Block::default().style(Style::default().bg(t.work_bg.into())),
        area,
    );
    let mut inner = Rect::new(
        area.x + 1,
        area.y + HEADER_ROWS.min(area.height.saturating_sub(1)),
        area.width.saturating_sub(2),
        area.height.saturating_sub(HEADER_ROWS + 1),
    );
    let recovery = recovery.filter(|_| edit.is_none());
    let mut heading_right = inner.right();
    if let Some(recovery) = recovery {
        let height = HEADER_ROWS.min(area.height);
        let button_width = 14.min(inner.width / 2);
        let button = Rect::new(inner.right() - button_width, area.y, button_width, height);
        heading_right = button.x.saturating_sub(1);
        let enabled = !recovery.pending && recovery.blocked.is_none();
        let bg = if enabled && hovered_action == Some(QueueAction::Recover) {
            t.work_hover_bg
        } else {
            t.work_detail_bg
        };
        f.render_widget(
            Block::default().style(Style::default().bg(bg.into())),
            button,
        );
        if height > 0 {
            f.render_widget(
                Paragraph::new(crate::width::truncate(
                    if recovery.pending {
                        "Recovering"
                    } else {
                        "Recover R"
                    },
                    button.width as usize,
                ))
                .centered()
                .style(
                    Style::default()
                        .fg(if enabled {
                            t.tinted_fg.into()
                        } else {
                            t.subtle_fg.into()
                        })
                        .bg(bg.into()),
                ),
                Rect::new(button.x, button.y + height / 2, button.width, 1),
            );
            hitmap
                .actions
                .push((selected, QueueAction::Recover, button));
        }
    }
    if let Some(page) = page.filter(|page| page.paged() && edit.is_none()) {
        let height = 3.min(inner.height.saturating_sub(3));
        let width = inner.width / 2;
        let size = atman_runtime::flow_runtime::PENDING_VIEW_PAGE_SIZE;
        for (index, action, label, enabled) in [
            (0, QueueAction::PreviousPage, "Prev  PgUp", page.offset > 0),
            (
                1,
                QueueAction::NextPage,
                "Next  PgDn",
                page.offset + size < page.total,
            ),
        ] {
            if height == 0 {
                break;
            }
            let rect = Rect::new(
                inner.x + index * width,
                inner.bottom() - height,
                width.saturating_sub(1),
                height,
            );
            let bg = if enabled && hovered_action == Some(action) {
                t.work_hover_bg
            } else {
                t.work_detail_bg
            };
            f.render_widget(Block::default().style(Style::default().bg(bg.into())), rect);
            f.render_widget(
                Paragraph::new(crate::width::truncate(label, rect.width as usize))
                    .centered()
                    .style(
                        Style::default()
                            .fg(if enabled {
                                t.tinted_fg.into()
                            } else {
                                t.subtle_fg.into()
                            })
                            .bg(bg.into()),
                    ),
                Rect::new(rect.x, rect.y + height / 2, rect.width, 1),
            );
            hitmap.actions.push((selected, action, rect));
        }
        inner.height = inner.height.saturating_sub(height);
    }
    let history = crate::panel_registry::EXTENDED
        && edit.is_none()
        && submissions
            .iter()
            .any(|item| matches!(item, QueueItem::Instance { .. }));
    let title = if edit.is_some() {
        " Edit queued input ".to_owned()
    } else {
        let page_label = page
            .filter(|page| page.paged())
            .map_or(String::new(), |page| {
                let size = atman_runtime::flow_runtime::PENDING_VIEW_PAGE_SIZE;
                format!(
                    " · Page {}/{}",
                    page.offset / size + 1,
                    page.total.div_ceil(size)
                )
            });
        format!(
            " Inbox · {}{} · {} / {}{} ",
            page.map_or(submissions.len(), |page| page.total),
            page_label,
            page.map_or(0, |page| page.offset) + selected.saturating_add(1).min(submissions.len()),
            page.map_or(submissions.len(), |page| page.total),
            if history { " · History :inbox" } else { "" }
        )
    };
    let heading_width = heading_right.saturating_sub(inner.x);
    let title_width = (crate::width::width(&title) as u16).min(if edit.is_some() {
        heading_width
    } else {
        heading_width / 2
    });
    let title_rect = Rect::new(
        inner.x,
        area.y + HEADER_ROWS.min(area.height) / 2,
        title_width,
        1,
    );
    if history {
        hitmap.actions.push((0, QueueAction::History, title_rect));
    }
    f.render_widget(
        Paragraph::new(crate::width::truncate(&title, area.width as usize)).style(
            Style::default().fg(t.accent.into()).bg(
                if hovered_action == Some(QueueAction::History) {
                    t.work_bg.lerp(t.work_hover_bg, 0.5)
                } else {
                    t.work_bg.into_inner()
                },
            ),
        ),
        title_rect,
    );
    if edit.is_none() {
        let x = title_rect.right().saturating_add(1).min(heading_right);
        let width = heading_right.saturating_sub(x);
        let mut text = hint(submissions.get(selected), focused, active_turn, recovery);
        if !focused && recovery.is_none() && crate::width::width(&text) > width as usize {
            text = "Shift+Tab focus".into();
        }
        f.render_widget(
            Paragraph::new(crate::width::truncate(&text, width as usize))
                .right_aligned()
                .style(Style::default().fg(t.subtle_fg.into()).bg(t.work_bg.into())),
            Rect::new(x, title_rect.y, width, 1),
        );
    }
    if let Some(edit) = edit {
        let error = edit.error.as_deref().unwrap_or("");
        let error_lines = crate::width::word_wrap(error, inner.width as usize);
        let error_height = if error.is_empty() {
            0
        } else {
            error_lines
                .len()
                .min(3)
                .min(inner.height.saturating_sub(4) as usize) as u16
        };
        let button_height = 3.min(inner.height.saturating_sub(error_height).saturating_sub(1));
        let editor_height = inner
            .height
            .saturating_sub(button_height + error_height)
            .max(1);
        let text_area = Rect::new(
            inner.x + 2,
            inner.y,
            inner.width.saturating_sub(4).max(1),
            editor_height,
        );
        let width = text_area.width as usize;
        let (row, col) =
            crate::input::wrapped_cursor_position(edit.editor.buf(), edit.editor.cursor(), width);
        let scroll = row.saturating_sub(editor_height.saturating_sub(1) as usize);
        f.render_widget(
            Paragraph::new(edit.editor.buf())
                .wrap(Wrap { trim: false })
                .scroll((scroll.min(u16::MAX as usize) as u16, 0))
                .style(
                    Style::default()
                        .fg(t.heading.into())
                        .bg(t.work_detail_bg.into()),
                ),
            text_area,
        );
        hitmap.edit_area = Some((text_area, scroll));
        hitmap.edit_origin = Some((
            text_area.x + col.min(width.saturating_sub(1)) as u16,
            text_area.y + row.saturating_sub(scroll) as u16,
        ));
        let y = text_area.bottom();
        for (index, line) in error_lines.iter().take(error_height as usize).enumerate() {
            f.render_widget(
                Paragraph::new(line.as_str()).style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(inner.x, y + index as u16, inner.width, 1),
            );
        }
        let buttons_y = y + error_height;
        let half = inner.width.saturating_sub(1) / 2;
        for (action, label, x, width) in [
            (
                QueueAction::Save,
                if saving {
                    "Saving…"
                } else if half < 12 {
                    "Save ^S"
                } else if half < 20 {
                    "Save Ctrl+S"
                } else {
                    "Save  Ctrl+S / Enter"
                },
                inner.x,
                half,
            ),
            (
                QueueAction::Cancel,
                "Cancel  Esc",
                inner.x + half + 1,
                inner.width.saturating_sub(half + 1),
            ),
        ] {
            if button_height == 0 || width == 0 {
                continue;
            }
            let rect = Rect::new(x, buttons_y, width, button_height);
            f.render_widget(
                Block::default().style(Style::default().bg(if hovered_action == Some(action) {
                    t.work_hover_bg.into()
                } else {
                    t.work_detail_bg.into()
                })),
                rect,
            );
            f.render_widget(
                Paragraph::new(crate::width::truncate(label, width as usize))
                    .centered()
                    .style(Style::default().fg(t.tinted_fg.into())),
                Rect::new(x, buttons_y + button_height / 2, width, 1),
            );
            hitmap.actions.push((selected, action, rect));
        }
        return hitmap;
    }
    let selected = selected.min(submissions.len().saturating_sub(1));
    let footer_rows = if submissions.iter().any(QueueItem::editable) && area.width < 80 {
        3.min(inner.height.saturating_sub(3))
    } else {
        0
    };
    let list_height = inner.height.saturating_sub(footer_rows);
    let visible = (list_height / 3).max(1) as usize;
    let start = selected
        .saturating_sub(visible.saturating_sub(1))
        .min(submissions.len().saturating_sub(visible));
    let end = (start + visible).min(submissions.len());
    for (row, index) in (start..end).enumerate() {
        let item = &submissions[index];
        let active = focused && index == selected;
        let bg = if hovered == Some(index) {
            if item.is_inserted() {
                t.queue_insert_bg.lerp(t.work_hover_bg, 0.65).into()
            } else {
                t.work_hover_bg
            }
        } else if active {
            t.work_detail_bg
        } else if item.is_inserted() {
            t.queue_insert_bg
        } else {
            t.work_bg
        };
        let rect = Rect::new(
            inner.x,
            inner.y + row as u16 * 3,
            inner.width,
            3.min(list_height.saturating_sub(row as u16 * 3)),
        );
        f.render_widget(Block::default().style(Style::default().bg(bg.into())), rect);
        if active {
            f.render_widget(
                Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg.into())),
                Rect::new(rect.x, rect.y, 1, rect.height),
            );
        }
        let actions: &[(QueueAction, &str)] = if active && item.editable() {
            &[
                (
                    QueueAction::Insert,
                    if item.is_inserted() {
                        "i Undo insert"
                    } else {
                        "i Insert"
                    },
                ),
                (QueueAction::Edit, "e edit"),
                (QueueAction::MoveUp, "Ctrl↑"),
                (QueueAction::MoveDown, "Ctrl↓"),
                (QueueAction::Delete, "Del"),
            ]
        } else {
            &[]
        };
        let action_width: usize = actions
            .iter()
            .map(|(action, label)| {
                if *action == QueueAction::Insert {
                    15
                } else {
                    crate::width::width(label) + 2
                }
            })
            .sum();
        let show_actions = area.width >= 80 && action_width + 20 < rect.width as usize;
        let body_width = rect
            .width
            .saturating_sub(7 + if show_actions { action_width as u16 } else { 0 });
        let body = item.text().replace(['\n', '\r'], " ");
        let text = if item.editable() {
            body
        } else {
            format!("{} · {body}", item.label())
        };
        let fg = if item.is_inserted() {
            t.queue_insert_fg
        } else {
            t.tinted_fg
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("{}  ", item.symbol()),
                    Style::default().fg(if item.is_inserted() { fg } else { t.subtle_fg }.into()),
                ),
                Span::styled(
                    crate::width::truncate(&text, body_width as usize),
                    Style::default().fg(fg.into()),
                ),
            ]))
            .style(Style::default().bg(bg.into())),
            Rect::new(rect.x + 2, rect.y + rect.height / 2, body_width + 3, 1),
        );
        hitmap.rows.push((index, rect));
        if show_actions {
            let mut x = rect.right().saturating_sub(action_width as u16);
            for &(action, label) in actions {
                let width = if action == QueueAction::Insert {
                    15
                } else {
                    (crate::width::width(label) + 2) as u16
                };
                let button = Rect::new(x, rect.y, width, rect.height);
                f.render_widget(
                    Paragraph::new(label)
                        .centered()
                        .style(Style::default().fg(t.subtle_fg.into()).bg(bg.into())),
                    Rect::new(x, rect.y + rect.height / 2, width, 1),
                );
                hitmap.actions.push((index, action, button));
                x += width;
            }
        } else if active && footer_rows > 0 {
            let labels = ["i", "e", "^↑", "^↓", "Del"];
            let width = inner.width / 5;
            for (column, &(action, label)) in actions.iter().enumerate() {
                let x = inner.x + column as u16 * width;
                let button = Rect::new(x, inner.bottom() - footer_rows, width, footer_rows);
                let label = if crate::width::width(label) > width as usize {
                    if action == QueueAction::Insert && item.is_inserted() && width >= 6 {
                        "i Undo"
                    } else {
                        labels[column]
                    }
                } else {
                    label
                };
                f.render_widget(
                    Block::default().style(Style::default().bg(t.work_detail_bg.into())),
                    button,
                );
                f.render_widget(
                    Paragraph::new(label)
                        .centered()
                        .style(Style::default().fg(t.subtle_fg.into())),
                    Rect::new(x, button.y + button.height / 2, width, 1),
                );
                hitmap.actions.push((index, action, button));
            }
        }
    }
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
    fn recovery_banner_keeps_a_complete_button_hitbox_and_visible_queue_rows() {
        let session = atman_runtime::Session::open_ephemeral();
        session
            .enqueue_submission(
                "retained input",
                vec![],
                Default::default(),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let items = session
            .queued_submissions()
            .into_iter()
            .map(QueueItem::Session)
            .collect::<Vec<_>>();
        for width in [36, 60, 96] {
            for pending in [false, true] {
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 14)).unwrap();
                let mut hits = QueueHitMap::default();
                terminal
                    .draw(|frame| {
                        hits = render(
                            frame,
                            frame.area(),
                            &items,
                            QueueRenderState {
                                recovery: Some(RecoveryView {
                                    reason: "Execution cancelled",
                                    pending,
                                    blocked: None,
                                    error: None,
                                }),
                                page: None,
                                selected: 0,
                                focused: false,
                                hovered: None,
                                hovered_action: Some(QueueAction::Recover),
                                edit: None,
                                active_turn: false,
                                saving: false,
                            },
                        );
                    })
                    .unwrap();
                let (_, _, button) = hits
                    .actions
                    .iter()
                    .find(|(_, action, _)| *action == QueueAction::Recover)
                    .unwrap();
                assert_eq!(button.height, HEADER_ROWS);
                assert_eq!(button.y, 0, "no outer top margin above the button");
                for y in button.y..button.bottom() {
                    assert_eq!(hits.action_at(button.x, y), Some((0, QueueAction::Recover)));
                }
                let buffer = terminal.backend().buffer();
                let button_bg = if pending {
                    crate::theme::theme().work_detail_bg
                } else {
                    crate::theme::theme().work_hover_bg
                };
                for y in [button.y, button.bottom() - 1] {
                    for x in button.x..button.right() {
                        assert_eq!(buffer[(x, y)].symbol(), " ", "internal padding is blank");
                        assert_eq!(buffer[(x, y)].bg, button_bg.into());
                    }
                }
                let label = (button.x..button.right())
                    .map(|x| buffer[(x, button.y + 1)].symbol())
                    .collect::<String>();
                assert!(label.contains(if pending { "Recovering" } else { "Recover R" }));
                assert!(
                    hits.rows
                        .iter()
                        .all(|(_, rect)| rect.intersection(*button).is_empty())
                );
                let text = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(text.contains(if pending { "Recovering" } else { "Recover R" }));
                assert!(text.contains("retained input"));
            }
        }
    }

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
        let submissions: Vec<_> = session
            .queued_submissions()
            .into_iter()
            .map(QueueItem::Session)
            .collect();
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
                        recovery: None,
                        page: None,
                        selected: 0,
                        focused: true,
                        hovered: None,
                        hovered_action: None,
                        edit: None,
                        active_turn: true,
                        saving: false,
                    },
                );
            })
            .unwrap();

        assert_eq!(hitmap.row_at(2, HEADER_ROWS), Some(0));
        assert!(
            hitmap
                .actions
                .iter()
                .any(|(index, action, _)| { *index == 0 && *action == QueueAction::Edit })
        );
        assert!(hitmap.actions.iter().all(|(index, _, _)| *index == 0));
    }

    #[test]
    fn hints_share_the_padded_header_without_changing_list_geometry() {
        let session = atman_runtime::Session::open_ephemeral();
        session
            .enqueue_submission(
                "@./image.png inspect",
                Vec::new(),
                atman_runtime::InvocationEnv::single("effort", Value::Str("medium".into())),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let submissions: Vec<_> = session
            .queued_submissions()
            .into_iter()
            .map(QueueItem::Session)
            .collect();
        for width in [36, 60, 96] {
            let rows = height(&submissions, false, false, width);
            for focused in [false, true] {
                for active_turn in [false, true] {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, rows))
                            .unwrap();
                    let mut hits = QueueHitMap::default();
                    terminal
                        .draw(|frame| {
                            hits = render(
                                frame,
                                frame.area(),
                                &submissions,
                                QueueRenderState {
                                    recovery: None,
                                    page: None,
                                    selected: 0,
                                    focused,
                                    hovered: None,
                                    hovered_action: None,
                                    edit: None,
                                    active_turn,
                                    saving: false,
                                },
                            );
                        })
                        .unwrap();
                    assert_eq!(hits.rows[0].1.y, HEADER_ROWS);
                    let buffer = terminal.backend().buffer();
                    for y in [0, HEADER_ROWS - 1] {
                        assert!((0..width).all(|x| buffer[(x, y)].symbol() == " "));
                    }
                    let body = (0..width)
                        .map(|x| buffer[(x, HEADER_ROWS + 1)].symbol())
                        .collect::<String>();
                    assert!(body.contains("≡  "));
                    assert!(!body.contains("next ·"));
                    let bottom = (0..width)
                        .map(|x| buffer[(x, rows - 1)].symbol())
                        .collect::<String>();
                    assert!(
                        bottom.trim().is_empty(),
                        "hints never touch the composer border"
                    );
                }
            }
        }
    }
}
