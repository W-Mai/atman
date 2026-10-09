use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use crate::theme::Theme;

/// Geometry shared by settings fields, actions, and their mouse targets.
pub(crate) struct FormLayout {
    pub fields: Vec<(usize, Rect)>,
    pub test: Rect,
    pub save: Rect,
    pub info: Rect,
    pub help: Rect,
    pub scroll: Rect,
}

impl FormLayout {
    pub fn new(area: Rect, field_count: usize, selected: usize) -> Self {
        let width = area.width.saturating_sub(2).min(96);
        let top = u16::from(area.height >= 16);
        let content = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + top,
            width,
            area.height.saturating_sub(top * 2),
        );
        let row_height = if width >= 68 { 3 } else { 4 };
        let action_height = 3.min(content.height);
        let spare = content.height.saturating_sub(row_height + action_height);
        let help_height = u16::from(spare > 0);
        let info_height = spare.saturating_sub(help_height + 1).min(3);
        let gap = u16::from(spare > help_height + info_height);
        let footer_height = action_height + help_height + info_height + gap;
        let body_height = content.height.saturating_sub(footer_height);
        let scroll_height = u16::from(
            field_count > (body_height / row_height) as usize && body_height > row_height,
        );
        let visible = ((body_height - scroll_height) / row_height) as usize;
        let visible = visible.min(field_count);
        let start = selected
            .min(field_count.saturating_sub(1))
            .saturating_sub(visible.saturating_sub(1));
        let fields = (start..start + visible)
            .enumerate()
            .map(|(row, index)| {
                (
                    index,
                    Rect::new(
                        content.x,
                        content.y + row as u16 * row_height,
                        width,
                        row_height,
                    ),
                )
            })
            .collect();
        let scroll = Rect::new(
            content.x,
            content.y + body_height.saturating_sub(1),
            width,
            scroll_height,
        );
        let info = Rect::new(content.x, content.y + body_height, width, info_height);
        let action_y = info.bottom() + gap;
        let action_gap = 2.min(width);
        let button_width = width.saturating_sub(action_gap) / 2;
        let test = Rect::new(content.x, action_y, button_width, action_height);
        let save = Rect::new(
            test.right() + action_gap,
            action_y,
            width.saturating_sub(button_width + action_gap),
            action_height,
        );
        let help = Rect::new(content.x, save.bottom(), width, help_height);
        Self {
            fields,
            test,
            save,
            info,
            help,
            scroll,
        }
    }
}

#[derive(Default)]
pub(crate) struct Field<'a> {
    pub label: &'a str,
    pub value: &'a str,
    pub placeholder: &'a str,
    pub focused: bool,
    pub hovered: bool,
    pub cursor: Option<usize>,
    pub selector: bool,
    pub locked: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FieldHit {
    pub record: Rect,
    pub value: Rect,
    pub offset: usize,
    pub arrows: Option<(Rect, Rect)>,
}

fn record_background(t: &Theme, focused: bool, hovered: bool) -> Color {
    let base = crate::wm::shell::surface_bg(t);
    let surface = crate::theme::ThemeColor::new(base);
    if focused {
        surface.lerp(t.accent, 0.12)
    } else if hovered {
        surface.lerp(t.work_hover_bg, 0.45)
    } else {
        base
    }
}

fn render_record(f: &mut Frame, rect: Rect, focused: bool, hovered: bool, t: &Theme) -> Color {
    let bg = record_background(t, focused, hovered);
    f.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    if focused && rect.width > 0 {
        for y in rect.y..rect.bottom() {
            f.render_widget(
                Paragraph::new("▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                Rect::new(rect.x, y, 1, 1),
            );
        }
    }
    bg
}

pub(crate) fn render_field(f: &mut Frame, rect: Rect, field: &Field<'_>, t: &Theme) -> FieldHit {
    let bg = render_record(f, rect, field.focused, field.hovered && !field.locked, t);
    let stacked = rect.height >= 4;
    let inset = 2.min(rect.width);
    let text_width = rect.width.saturating_sub(inset * 2);
    let label_width = if stacked { 0 } else { 23.min(text_width) };
    let value_y = rect.y + (if stacked { 2 } else { 1 }).min(rect.height.saturating_sub(1));
    let arrows_width = if field.selector && text_width > label_width + 10 {
        8
    } else {
        0
    };
    let value = Rect::new(
        rect.x + inset + label_width,
        value_y,
        text_width.saturating_sub(label_width + arrows_width),
        u16::from(rect.height > 0),
    );
    let label = Rect::new(
        rect.x + inset,
        rect.y + 1.min(rect.height.saturating_sub(1)),
        if stacked {
            text_width
        } else {
            label_width.saturating_sub(2)
        },
        u16::from(rect.height > 0),
    );
    f.render_widget(
        Paragraph::new(crate::width::truncate(field.label, label.width as usize))
            .style(Style::default().fg(t.subtle_fg.into()).bg(bg)),
        label,
    );
    let offset = if field.focused {
        field
            .cursor
            .unwrap_or_default()
            .saturating_sub(value.width.saturating_sub(1) as usize)
    } else {
        0
    };
    let empty = field.value.is_empty();
    let text = if empty {
        field.placeholder
    } else {
        field.value
    };
    f.render_widget(
        Paragraph::new(crate::width::trim_display_offset(
            text,
            offset,
            value.width as usize,
        ))
        .style(Style::default().bg(bg).fg(if empty || field.locked {
            t.subtle_fg.into()
        } else {
            t.tinted_fg.into()
        })),
        value,
    );
    let arrows = (arrows_width > 0).then(|| {
        let left = Rect::new(rect.right().saturating_sub(inset + 7), value_y, 3, 1);
        let right = Rect::new(rect.right().saturating_sub(inset + 3), value_y, 3, 1);
        for (area, text) in [(left, " ‹ "), (right, " › ")] {
            f.render_widget(
                Paragraph::new(text).style(Style::default().bg(bg).fg(if field.focused {
                    t.accent.into()
                } else {
                    t.subtle_fg.into()
                })),
                area,
            );
        }
        (left, right)
    });
    if field.focused
        && !field.locked
        && value.width > 0
        && value.height > 0
        && let Some(cursor) = field.cursor
    {
        f.set_cursor_position((
            value.x + cursor.saturating_sub(offset).min(value.width as usize - 1) as u16,
            value.y,
        ));
    }
    FieldHit {
        record: rect,
        value,
        offset,
        arrows,
    }
}

#[derive(Default)]
pub(crate) struct Action<'a> {
    pub label: &'a str,
    pub shortcut: &'a str,
    pub focused: bool,
    pub hovered: bool,
    pub disabled: bool,
}

pub(crate) fn render_action(f: &mut Frame, rect: Rect, action: &Action<'_>, t: &Theme) {
    let base = crate::theme::ThemeColor::new(crate::wm::shell::surface_bg(t));
    let bg = if action.focused {
        base.lerp(t.accent, 0.16)
    } else if action.hovered && !action.disabled {
        base.lerp(t.work_hover_bg, 0.85)
    } else {
        base.lerp(t.work_hover_bg, 0.45)
    };
    f.render_widget(Block::default().style(Style::default().bg(bg)), rect);
    if action.focused && rect.width > 0 {
        for y in rect.y..rect.bottom() {
            f.render_widget(
                Paragraph::new("▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                Rect::new(rect.x, y, 1, 1),
            );
        }
    }
    let inner = Rect::new(
        rect.x + 1.min(rect.width),
        rect.y + rect.height.saturating_sub(1) / 2,
        rect.width.saturating_sub(2),
        u16::from(rect.height > 0),
    );
    let label = if action.shortcut.is_empty() {
        action.label.to_owned()
    } else {
        format!("{}  {}", action.label, action.shortcut)
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            crate::width::truncate(&label, inner.width as usize),
            Style::default()
                .bg(bg)
                .fg(if action.disabled {
                    t.subtle_fg.into()
                } else {
                    t.tinted_fg.into()
                })
                .add_modifier(if action.focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        )))
        .alignment(Alignment::Center),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_geometry_keeps_padded_records_and_actions_inside_the_viewport() {
        for width in [24, 40, 68, 96, 140] {
            for height in [7, 14, 24, 40] {
                let area = Rect::new(3, 2, width, height);
                for selected in 0..8 {
                    let layout = FormLayout::new(area, 8, selected);
                    let mut rects = vec![
                        layout.test,
                        layout.save,
                        layout.info,
                        layout.help,
                        layout.scroll,
                    ];
                    rects.extend(layout.fields.iter().map(|(_, rect)| *rect));
                    for rect in rects {
                        assert!(rect.x >= area.x && rect.y >= area.y);
                        assert!(rect.right() <= area.right() && rect.bottom() <= area.bottom());
                    }
                    for (_, rect) in &layout.fields {
                        assert!(rect.height >= 3);
                        assert!(rect.bottom() <= layout.test.y);
                    }
                    if !layout.fields.is_empty() {
                        assert!(layout.fields.iter().any(|(index, _)| *index == selected));
                    }
                    assert!(layout.test.right() < layout.save.x);
                }
            }
        }
    }

    #[test]
    fn field_rendering_and_hits_share_padding_background_and_text_geometry() {
        let t = crate::theme::theme();
        for width in [40, 90] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
            let rect = Rect::new(1, 1, width - 2, if width >= 70 { 3 } else { 4 });
            let mut hit = None;
            terminal
                .draw(|frame| {
                    hit = Some(render_field(
                        frame,
                        rect,
                        &Field {
                            label: "Base URL",
                            value: "https://gateway.example/v1",
                            focused: true,
                            cursor: Some(8),
                            ..Default::default()
                        },
                        &t,
                    ));
                })
                .unwrap();
            let hit = hit.unwrap();
            assert_eq!(hit.record, rect);
            assert!(hit.value.x >= rect.x + 2 && hit.value.right() <= rect.right() - 2);
            assert!(hit.value.y > rect.y && hit.value.bottom() < rect.bottom());
            let buffer = terminal.backend().buffer();
            for y in rect.y..rect.bottom() {
                assert_eq!(buffer[(rect.x, y)].symbol(), "▌");
                assert_eq!(buffer[(rect.x + 1, y)].symbol(), " ");
            }
            for x in rect.x + 1..rect.right() {
                assert_eq!(buffer[(x, rect.y)].symbol(), " ");
                assert_eq!(buffer[(x, rect.bottom() - 1)].symbol(), " ");
            }
        }
    }
}
