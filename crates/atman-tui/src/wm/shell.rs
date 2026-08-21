use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use atman_runtime::TaskSnapshot;

use crate::wm::WindowContent;

use super::{PanelBtn, WindowInstance};

#[allow(clippy::too_many_arguments)]
pub fn render_shell(
    f: &mut Frame,
    panel: &WindowInstance,
    is_focused: bool,
    btn_hover: Option<PanelBtn>,
    panel_close_armed: Option<(&str, bool)>,
    snapshots: &[TaskSnapshot],
    t: &crate::theme::Theme,
) {
    let panel_bg: Color = if is_focused {
        t.modal_bg.lerp(t.highlight_bg, 0.15)
    } else {
        t.modal_bg.into()
    };

    let buf_area = f.buffer_mut().area;
    let sanitize_rect = Rect {
        x: panel.rect.x.saturating_sub(2),
        y: panel.rect.y.saturating_add(1),
        width: panel.rect.width + 4,
        height: panel.rect.height.saturating_sub(2),
    }
    .intersection(buf_area);
    crate::sanitize_widget_edges(f, sanitize_rect);
    f.render_widget(Clear, panel.rect);
    f.render_widget(
        Block::default().style(Style::default().bg(panel_bg)),
        panel.rect,
    );

    let title_text = if matches!(panel.content_kind, WindowContent::Mermaid { .. }) {
        let hint = if panel.split {
            "Tab: diagram"
        } else {
            "Tab: split"
        };
        format!("{}  · {}", panel.title, hint)
    } else {
        panel.title.clone()
    };
    let title_line = Line::from(vec![
        Span::styled(
            format!(" {} ", panel.content_kind.icon()),
            Style::default().fg(t.heading.into()).bg(panel_bg),
        ),
        Span::styled(
            crate::width::truncate(&title_text, panel.rect.width as usize - 6),
            Style::default().fg(t.tinted_fg.into()).bg(panel_bg),
        ),
    ]);
    f.render_widget(
        Paragraph::new(title_line),
        Rect {
            x: panel.rect.x,
            y: panel.rect.y,
            width: panel.rect.width.saturating_sub(2),
            height: 1,
        },
    );

    if panel.rect.height >= 6 {
        let hover_bg = t.user_msg_bg.into();

        let btn_area = Rect {
            x: panel.rect.x,
            y: panel.rect.y + 2,
            width: 3,
            height: 1,
        };
        let (min_fg, min_bg) = if btn_hover == Some(PanelBtn::Minimize) {
            (Color::Rgb(180, 130, 0), hover_bg)
        } else {
            (Color::Rgb(200, 150, 20), panel_bg)
        };
        f.render_widget(Clear, btn_area);
        f.render_widget(
            Block::default().style(Style::default().bg(min_bg)),
            btn_area,
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "—",
                Style::default().fg(min_fg).bg(min_bg),
            )]))
            .alignment(ratatui::layout::Alignment::Center),
            btn_area,
        );

        let btn_area = Rect {
            x: panel.rect.x,
            y: panel.rect.y + 3,
            width: 3,
            height: 1,
        };
        let is_max = panel.maximized;
        let (max_fg, max_bg) = if btn_hover == Some(PanelBtn::Maximize) || is_max {
            (Color::Rgb(40, 200, 80), hover_bg)
        } else {
            (Color::Rgb(80, 180, 100), panel_bg)
        };
        f.render_widget(Clear, btn_area);
        f.render_widget(
            Block::default().style(Style::default().bg(max_bg)),
            btn_area,
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "▢",
                Style::default().fg(max_fg).bg(max_bg),
            )]))
            .alignment(ratatui::layout::Alignment::Center),
            btn_area,
        );

        if panel.rect.height >= 8 {
            let killable = match &panel.content_kind {
                WindowContent::Task { handle, .. } => snapshots
                    .iter()
                    .any(|s| s.source_handle == *handle && s.is_running()),
                _ => false,
            };
            if killable {
                let btn_area = Rect {
                    x: panel.rect.x,
                    y: panel.rect.y + 5,
                    width: 3,
                    height: 1,
                };
                let armed = panel_close_armed
                    .as_ref()
                    .is_some_and(|(id, expired)| id == &panel.label && !expired);
                let (close_fg, close_bg) = if btn_hover == Some(PanelBtn::Close) || armed {
                    (t.error.into(), hover_bg)
                } else {
                    (Color::Rgb(220, 90, 90), panel_bg)
                };
                f.render_widget(Clear, btn_area);
                f.render_widget(
                    Block::default().style(Style::default().bg(close_bg)),
                    btn_area,
                );
                let glyph = "✕";
                let mod_add = if armed {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                };
                f.render_widget(
                    Paragraph::new(Line::from(vec![Span::styled(
                        glyph,
                        Style::default()
                            .fg(close_fg)
                            .bg(close_bg)
                            .add_modifier(mod_add),
                    )]))
                    .alignment(ratatui::layout::Alignment::Center),
                    btn_area,
                );
            }
        }

        let btn_area = Rect {
            x: panel.rect.x + panel.rect.width.saturating_sub(3),
            y: panel.rect.y + panel.rect.height.saturating_sub(1),
            width: 3,
            height: 1,
        };
        let (res_fg, res_bg) = if btn_hover == Some(PanelBtn::Resize) {
            (t.tinted_fg.into(), hover_bg)
        } else {
            (t.subtle_fg.into(), panel_bg)
        };
        f.render_widget(Clear, btn_area);
        f.render_widget(
            Block::default().style(Style::default().bg(res_bg)),
            btn_area,
        );
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "⇲",
                Style::default().fg(res_fg).bg(res_bg),
            )]))
            .alignment(ratatui::layout::Alignment::Center),
            btn_area,
        );

        if btn_hover == Some(PanelBtn::Resize) {
            let inner_cols = panel.rect.width.saturating_sub(8);
            let inner_rows = panel.rect.height.saturating_sub(5);
            let dim_text = format!("{inner_cols}×{inner_rows}");
            let dim_w = dim_text.chars().count() as u16 + 2;
            let dim_area = Rect {
                x: btn_area.x.saturating_sub(dim_w),
                y: btn_area.y,
                width: dim_w,
                height: 1,
            };
            let buf_area = f.buffer_mut().area;
            let dim_area = dim_area.intersection(buf_area);
            if dim_area.width > 0 {
                f.render_widget(
                    Paragraph::new(Line::from(vec![Span::styled(
                        format!(" {dim_text}"),
                        Style::default().fg(t.subtle_fg.into()).bg(panel_bg),
                    )])),
                    dim_area,
                );
            }
        }
    }
}

pub fn render_overlay_shell(
    f: &mut Frame,
    rect: Rect,
    title: Line<'_>,
    icon: &str,
    accent: Color,
    show_header: bool,
    t: &crate::theme::Theme,
) -> Rect {
    let bg: Color = t.modal_bg.lerp(t.highlight_bg, 0.15);

    let sanitize_rect = Rect {
        x: rect.x.saturating_sub(2),
        y: rect.y.saturating_add(1),
        width: rect.width.saturating_add(4),
        height: rect.height.saturating_sub(2),
    };
    crate::sanitize_widget_edges(f, sanitize_rect);
    crate::wm::shadow::render_shadow(f, rect, t);

    f.render_widget(ratatui::widgets::Clear, rect);
    f.render_widget(
        ratatui::widgets::Block::default().style(ratatui::style::Style::default().bg(bg)),
        rect,
    );

    if !show_header {
        return Rect {
            x: rect.x + 1,
            y: rect.y,
            width: rect.width.saturating_sub(2),
            height: rect.height.saturating_sub(1),
        };
    }

    let title_line = Line::from(
        std::iter::once(Span::styled(
            format!(" {} ", icon),
            Style::default().fg(accent).bg(bg),
        ))
        .chain(title.spans)
        .collect::<Vec<_>>(),
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(title_line),
        Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: 1,
        },
    );

    let sep = std::iter::repeat_n('─', rect.width as usize).collect::<String>();
    f.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(Span::styled(
            sep,
            Style::default().fg(t.border.into()).bg(bg),
        ))),
        Rect {
            x: rect.x,
            y: rect.y + 1,
            width: rect.width,
            height: 1,
        },
    );

    Rect {
        x: rect.x + 1,
        y: rect.y + 2,
        width: rect.width.saturating_sub(2),
        height: rect.height.saturating_sub(3),
    }
}

pub fn render_section_header(f: &mut Frame, area: Rect, title: Line<'_>, t: &crate::theme::Theme) {
    let bg: Color = t.modal_bg.lerp(t.highlight_bg, 0.15);
    let line = Line::from(
        std::iter::once(Span::styled(" ", Style::default().bg(bg)))
            .chain(title.spans)
            .collect::<Vec<_>>(),
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(line),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
    if area.height > 1 {
        let sep = "─".repeat(area.width as usize);
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                sep,
                Style::default().fg(t.border.into()).bg(bg),
            ))),
            Rect {
                x: area.x,
                y: area.y + 1,
                width: area.width,
                height: 1,
            },
        );
    }
}

pub fn render_column_divider(f: &mut Frame, x: u16, y: u16, height: u16, t: &crate::theme::Theme) {
    let bg: Color = t.modal_bg.lerp(t.highlight_bg, 0.15);
    let buf_area = f.buffer_mut().area;
    for ry in 0..height {
        let py = y + ry;
        if x < buf_area.x
            || x >= buf_area.x + buf_area.width
            || py < buf_area.y
            || py >= buf_area.y + buf_area.height
        {
            continue;
        }
        let cell = &mut f.buffer_mut()[(x, py)];
        cell.set_symbol("│");
        cell.set_style(Style::default().fg(t.border.into()).bg(bg));
    }
}
