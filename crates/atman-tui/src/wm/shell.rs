use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use atman_runtime::TaskSnapshot;

use crate::wm::WindowContent;

use super::{PanelBtn, WindowInstance};

pub(crate) const DOUBLE_CLICK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

pub(crate) fn surface_bg(t: &crate::theme::Theme) -> Color {
    t.modal_bg.lerp(t.highlight_bg, 0.15)
}

#[allow(clippy::too_many_arguments)]
pub fn render_shell(
    f: &mut Frame,
    panel: &WindowInstance,
    is_focused: bool,
    btn_hover: Option<PanelBtn>,
    panel_close_armed: Option<(&str, bool)>,
    snapshots: &[TaskSnapshot],
    task_handle_index: &std::collections::HashMap<String, usize>,
    t: &crate::theme::Theme,
) {
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
    let killable = match &panel.content_kind {
        WindowContent::Task { handle, .. } => task_handle_index
            .get(handle)
            .and_then(|&index| snapshots.get(index))
            .is_some_and(|task| task.is_running()),
        _ => false,
    };
    let armed = panel_close_armed.is_some_and(|(id, expired)| id == panel.label && !expired);
    render_surface(
        f,
        Shell {
            rect: panel.rect,
            title: &title_text,
            icon: panel.content_kind.icon(),
            maximized: panel.maximized,
            close: killable.then_some(armed),
        },
        is_focused,
        btn_hover,
        t,
    );
}

/// Shared floating chrome; execution controls remain the caller's responsibility.
pub(crate) struct Shell<'a> {
    pub rect: Rect,
    pub title: &'a str,
    pub icon: &'a str,
    pub maximized: bool,
    /// None hides the close control; the value indicates an armed confirmation.
    pub close: Option<bool>,
}

pub(crate) fn content_area(rect: Rect) -> Rect {
    Rect::new(
        rect.x.saturating_add(3),
        rect.y.saturating_add(2),
        rect.width.saturating_sub(6),
        rect.height.saturating_sub(3),
    )
}

pub(crate) fn button_area(rect: Rect, button: PanelBtn) -> Rect {
    if rect.height < 6 || rect.width < 3 || (button == PanelBtn::Close && rect.height < 8) {
        return Rect::default();
    }
    match button {
        PanelBtn::Minimize => Rect::new(rect.x, rect.y + 2, 3, 1),
        PanelBtn::Maximize => Rect::new(rect.x, rect.y + 3, 3, 1),
        PanelBtn::Close => Rect::new(rect.x, rect.y + 5, 3, 1),
        PanelBtn::Resize => Rect::new(rect.right() - 3, rect.bottom() - 1, 3, 1),
    }
}

pub(crate) fn hit_button(rect: Rect, point: ratatui::layout::Position) -> Option<PanelBtn> {
    [
        PanelBtn::Minimize,
        PanelBtn::Maximize,
        PanelBtn::Close,
        PanelBtn::Resize,
    ]
    .into_iter()
    .find(|button| {
        let area = button_area(rect, *button);
        if *button == PanelBtn::Resize && !area.is_empty() {
            // Keep the existing one-cell grip tolerance around the outer corner.
            Rect::new(
                area.x + 1,
                area.y.saturating_sub(1),
                area.width,
                area.height + 2,
            )
            .contains(point)
        } else {
            area.contains(point)
        }
    })
}

pub(crate) fn hit_titlebar(rect: Rect, point: ratatui::layout::Position) -> bool {
    let outer = Rect::new(
        rect.x.saturating_sub(2),
        rect.y.saturating_sub(1),
        rect.width.saturating_add(4),
        rect.height.saturating_add(2),
    );
    outer.contains(point)
        && !content_area(rect).contains(point)
        && hit_button(rect, point).is_none()
}

pub(crate) fn render_surface(
    f: &mut Frame,
    shell: Shell<'_>,
    is_focused: bool,
    btn_hover: Option<PanelBtn>,
    t: &crate::theme::Theme,
) {
    let panel_bg: Color = if is_focused {
        surface_bg(t)
    } else {
        t.modal_bg.into()
    };

    let buf_area = f.buffer_mut().area;
    let sanitize_rect = Rect {
        x: shell.rect.x.saturating_sub(2),
        y: shell.rect.y.saturating_add(1),
        width: shell.rect.width + 4,
        height: shell.rect.height.saturating_sub(2),
    }
    .intersection(buf_area);
    crate::sanitize_widget_edges(f, sanitize_rect);
    f.render_widget(Clear, shell.rect);
    f.render_widget(
        Block::default().style(Style::default().bg(panel_bg)),
        shell.rect,
    );

    let title_line = Line::from(vec![
        Span::styled(
            format!(" {} ", shell.icon),
            Style::default().fg(t.heading.into()).bg(panel_bg),
        ),
        Span::styled(
            crate::width::truncate(shell.title, shell.rect.width.saturating_sub(6) as usize),
            Style::default().fg(t.tinted_fg.into()).bg(panel_bg),
        ),
    ]);
    f.render_widget(
        Paragraph::new(title_line),
        Rect {
            x: shell.rect.x,
            y: shell.rect.y,
            width: shell.rect.width.saturating_sub(2),
            height: 1,
        },
    );

    if shell.rect.height >= 6 {
        let hover_bg = t.user_msg_bg.into();

        let btn_area = button_area(shell.rect, PanelBtn::Minimize);
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

        let btn_area = button_area(shell.rect, PanelBtn::Maximize);
        let is_max = shell.maximized;
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

        if shell.rect.height >= 8 {
            if let Some(armed) = shell.close {
                let btn_area = button_area(shell.rect, PanelBtn::Close);
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

        let btn_area = button_area(shell.rect, PanelBtn::Resize);
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
            let inner_cols = shell.rect.width.saturating_sub(8);
            let inner_rows = shell.rect.height.saturating_sub(5);
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
    let bg = surface_bg(t);

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
    let bg = surface_bg(t);
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
    let bg = surface_bg(t);
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
