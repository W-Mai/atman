use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::TaskKind;

pub(crate) fn render_placeholder(f: &mut Frame, area: Rect, title: &str) {
    let t = crate::theme::theme();
    let msg = format!("no data: {title}");
    let total_pad = area.width as usize;
    let left = total_pad.saturating_sub(msg.len()) / 2;
    let top = area.height as usize / 2;
    let mut lines: Vec<Line> = vec![Line::from(""); top];
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(left), Style::default()),
        Span::styled(msg, Style::default().fg(t.subtle_fg.into())),
    ]));
    f.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn render_task_meta(
    f: &mut Frame,
    area: Rect,
    kind: TaskKind,
    snap: &atman_runtime::TaskSnapshot,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            snap.status.display_label(),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 {
        return;
    }

    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("kind   ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(kind.label(), Style::default().fg(t.tinted_fg.into())),
        ]),
        Line::from(vec![
            Span::styled("handle ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(&snap.source_handle, Style::default().fg(t.tinted_fg.into())),
        ]),
        Line::from(vec![
            Span::styled("elapsed", Style::default().fg(t.subtle_fg.into())),
            Span::raw(" "),
            Span::styled(
                format_elapsed(snap.elapsed_ms()),
                Style::default().fg(t.tinted_fg.into()),
            ),
        ]),
    ];
    f.render_widget(Paragraph::new(lines), body_area);
}

pub(crate) fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Killing => "◑",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

pub(crate) fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Killing => t.warn.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

pub(crate) fn format_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    let raw = if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    };
    format!("{:>5}", raw)
}
