use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use atman_runtime::{TaskKind, TaskSnapshot, TaskStatus};

pub const TASK_PANEL_WIDTH: u16 = 32;
pub const TASK_PANEL_STRIP_WIDTH: u16 = 5;

pub fn compute_task_panel_rect(
    area: Rect,
    show: bool,
    collapsed: bool,
) -> Option<Rect> {
    if !show {
        return None;
    }
    let width = if collapsed {
        TASK_PANEL_STRIP_WIDTH
    } else {
        TASK_PANEL_WIDTH
    };
    if !collapsed && area.width < TASK_PANEL_WIDTH + 40 {
        return None;
    }
    let height = area.height.saturating_sub(4);
    if height == 0 {
        return None;
    }
    Some(Rect {
        x: area.x + 1,
        y: area.y + 2,
        width,
        height,
    })
}

fn status_icon(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "◐",
        TaskStatus::Ok => "✓",
        TaskStatus::Err => "✗",
        TaskStatus::Killed => "⊘",
    }
}

fn status_color(status: TaskStatus) -> Color {
    match status {
        TaskStatus::Running => Color::LightBlue,
        TaskStatus::Ok => Color::Green,
        TaskStatus::Err => Color::Red,
        TaskStatus::Killed => Color::DarkGray,
    }
}

fn saturation_color(index: usize, total: usize) -> Color {
    if total <= 1 {
        return Color::White;
    }
    let ratio = index as f32 / (total - 1) as f32;
    let gray = 80 + (175.0 * ratio) as u8;
    Color::Rgb(gray, gray, gray)
}

fn fmt_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}:{}", s / 60, format!("{:02}", s % 60))
    }
}

pub fn render(f: &mut Frame, area: Rect, snapshots: &[TaskSnapshot], collapsed: bool) {
    if collapsed {
        render_strip(f, area, snapshots);
        return;
    }

    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.subtle_fg))
        .title(" ≡ ")
        .title(
            Line::from(vec![
                Span::raw(" "),
                Span::styled("Tasks", Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(
                    format!("({})", snapshots.iter().filter(|s| s.is_running()).count()),
                    Style::default().fg(t.subtle_fg),
                ),
            ])
            .alignment(Alignment::Right),
        );
    f.render_widget(block, area);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.height == 0 || inner.width < 3 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // --- Upper: Activity (running only, old→new, saturation gradient) ---
    let running: Vec<&TaskSnapshot> = snapshots.iter().filter(|s| s.is_running()).collect();
    let running_count = running.len();
    if running_count == 0 {
        lines.push(Line::from(vec![Span::styled(
            "  no active tasks",
            Style::default().fg(Color::DarkGray),
        )]));
    } else {
        for (i, snap) in running.iter().enumerate() {
            let color = saturation_color(i, running_count);
            let icon = status_icon(snap.status);
            let label = truncate_label(&snap.label, inner.width as usize - 12);
            let elapsed = fmt_elapsed(snap.elapsed_ms());
            lines.push(Line::from(vec![
                Span::styled(" ", Style::default().fg(color)),
                Span::styled(format!("{icon} "), Style::default().fg(status_color(snap.status))),
                Span::styled(label, Style::default().fg(color)),
                Span::raw(" "),
                Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    // separator
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(Color::DarkGray),
    )]));

    // --- Lower: Background tasks grouped by TaskKind ---
    let kinds = [
        TaskKind::Bash,
        TaskKind::Terminal,
        TaskKind::Flow,
        TaskKind::Subflow,
        TaskKind::Agent,
        TaskKind::Dispatch,
    ];

    for kind in kinds {
        let tasks: Vec<&TaskSnapshot> = snapshots.iter().filter(|s| s.kind == kind).collect();
        if tasks.is_empty() {
            continue;
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", kind.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", tasks.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        for snap in &tasks {
            let icon = status_icon(snap.status);
            let label = truncate_label(&snap.label, inner.width as usize - 14);
            let meta = if snap.is_running() {
                fmt_elapsed(snap.elapsed_ms())
            } else {
                "done".to_string()
            };
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    format!("{icon} "),
                    Style::default().fg(status_color(snap.status)),
                ),
                Span::styled(label, Style::default().fg(Color::White)),
                Span::raw(" "),
                Span::styled(meta, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}

fn truncate_label(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn render_strip(f: &mut Frame, area: Rect, snapshots: &[TaskSnapshot]) {
    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(Clear, area);

    let running = snapshots.iter().filter(|s| s.is_running()).count();
    let dot = if running > 0 {
        Span::styled("●", Style::default().fg(t.success))
    } else {
        Span::styled("○", Style::default().fg(t.subtle_fg))
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.subtle_fg))
        .title(" ≡ ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Line::from(dot)).alignment(Alignment::Center),
        inner,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn snap(kind: TaskKind, label: &str, status: TaskStatus) -> TaskSnapshot {
        TaskSnapshot {
            id: atman_runtime::TaskId::now(),
            kind,
            label: label.into(),
            status,
            started_at: Instant::now(),
            ended_at: None,
            source_handle: "h".into(),
            session_id: "s".into(),
        }
    }

    #[test]
    fn truncate_short() {
        assert_eq!(truncate_label("hi", 10), "hi");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate_label("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn saturation_gradient_boundaries() {
        assert_eq!(saturation_color(0, 1), Color::White);
        assert_eq!(saturation_color(0, 2), Color::Rgb(80, 80, 80));
        assert_eq!(saturation_color(1, 2), Color::Rgb(255, 255, 255));
    }
}
