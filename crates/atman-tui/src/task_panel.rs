use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

#[derive(Debug, Clone)]
pub struct ActivityNode {
    pub run_id: String,
    pub node_id: String,
    pub label: String,
    pub kind: atman_runtime::nodegraph::NodeKind,
    pub status: ActivityStatus,
    pub started_at: Instant,
    pub ended_at: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityStatus {
    Running,
    Ok,
    Err,
    Cancelled,
}

use atman_runtime::{TaskId, TaskKind, TaskSnapshot, TaskStatus};

pub const TASK_PANEL_WIDTH: u16 = 32;
pub const TASK_PANEL_STRIP_WIDTH: u16 = 5;

#[derive(Debug, Default)]
pub struct TaskPanelHitMap {
    pub kill_rects: Vec<(TaskId, Rect)>,
    pub group_header_rects: Vec<(TaskKind, Rect)>,
    pub task_rects: Vec<(String, Rect)>,
    pub activity_rects: Vec<(String, String, Rect)>,
    pub history_btn_rect: Option<Rect>,
}

pub fn compute_task_panel_rect(area: Rect, show: bool, collapsed: bool) -> Option<Rect> {
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

fn fmt_activity_elapsed(started: std::time::Instant, ended: Option<std::time::Instant>) -> String {
    let end = ended.unwrap_or_else(std::time::Instant::now);
    let ms = end.duration_since(started).as_millis() as u64;
    fmt_elapsed(ms)
}

fn status_color(status: TaskStatus) -> Color {
    match status {
        TaskStatus::Running => Color::LightBlue,
        TaskStatus::Ok => Color::Green,
        TaskStatus::Err => Color::Red,
        TaskStatus::Killed => Color::DarkGray,
    }
}

pub fn node_kind_glyph(kind: &atman_runtime::nodegraph::NodeKind) -> (&'static str, Color) {
    use atman_runtime::nodegraph::NodeKind;
    match kind {
        NodeKind::Llm { .. } => ("✦", Color::Magenta),
        NodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        NodeKind::Fanout { .. } => ("⇉", Color::Magenta),
        NodeKind::UserConfirm => ("?", Color::LightCyan),
        NodeKind::Subflow { .. } => ("↳", Color::Cyan),
        NodeKind::Message { .. } => ("✉", Color::White),
        NodeKind::FixUntilTest => ("↻", Color::LightMagenta),
        NodeKind::When { .. } => ("⋯", Color::DarkGray),
        NodeKind::Return => ("←", Color::Green),
    }
}

fn fmt_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn render(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    activity_nodes: &[ActivityNode],
    collapsed: bool,
    collapsed_groups: &std::collections::HashSet<TaskKind>,
) -> TaskPanelHitMap {
    if collapsed {
        render_strip(f, area, snapshots);
        return TaskPanelHitMap::default();
    }

    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.subtle_fg.into()))
        .title(" ≡ ")
        .title(
            Line::from(vec![
                Span::raw(" "),
                Span::styled("Tasks", Style::default().fg(Color::Yellow)),
                Span::raw(" "),
                Span::styled(
                    format!("({})", snapshots.iter().filter(|s| s.is_running()).count()),
                    Style::default().fg(t.subtle_fg.into()),
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
    let mut hitmap = TaskPanelHitMap::default();
    if inner.height == 0 || inner.width < 3 {
        return hitmap;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut row = inner.y;

    // --- Upper: Activity (running only, old→new, gradient bg) ---
    let running: Vec<&ActivityNode> = activity_nodes
        .iter()
        .filter(|n| n.status == ActivityStatus::Running)
        .collect();
    let running_count = running.len();
    if running_count == 0 {
        lines.push(Line::from(vec![Span::styled(
            "  no active tasks",
            Style::default().fg(t.subtle_fg.into()),
        )]));
        row += 1;
    } else {
        let w = inner.width as usize;
        for (i, node) in running.iter().enumerate() {
            let ratio = if running_count <= 1 {
                0.5
            } else {
                i as f64 / (running_count - 1) as f64
            };
            let bg = t.panel_bg.lerp(t.highlight_bg, ratio);
            let (kind_icon, kind_color) = node_kind_glyph(&node.kind);
            let elapsed = fmt_activity_elapsed(node.started_at, node.ended_at);
            let elapsed_w = elapsed.chars().count() + 1;
            let icon_w = 2;
            let content_w = w.saturating_sub(icon_w + elapsed_w + 1);
            let label = truncate_label(&node.label, content_w);
            let pad = w.saturating_sub(icon_w + label.chars().count() + elapsed_w);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {kind_icon} "),
                    Style::default().fg(kind_color).bg(bg),
                ),
                Span::styled(label, Style::default().fg(t.tinted_fg.into()).bg(bg)),
                Span::styled(" ".repeat(pad), Style::default().bg(bg)),
                Span::styled(elapsed, Style::default().fg(t.meta_fg.into()).bg(bg)),
            ]));
            hitmap.activity_rects.push((
                node.run_id.clone(),
                node.node_id.clone(),
                Rect {
                    x: inner.x,
                    y: row,
                    width: inner.width,
                    height: 1,
                },
            ));
            row += 1;
        }
    }

    // separator
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(inner.width as usize),
        Style::default().fg(t.subtle_fg.into()),
    )]));
    row += 1;

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
        let is_collapsed = collapsed_groups.contains(&kind);
        let glyph = if is_collapsed { "▸" } else { "▾" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {glyph} {} ", kind.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("({})", tasks.len()),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        hitmap.group_header_rects.push((
            kind,
            Rect {
                x: inner.x,
                y: row,
                width: inner.width,
                height: 1,
            },
        ));
        row += 1;

        if is_collapsed {
            continue;
        }
        for snap in &tasks {
            let icon = status_icon(snap.status);
            let label = truncate_label(&snap.label, inner.width as usize - 16);
            let meta = if snap.is_running() {
                fmt_elapsed(snap.elapsed_ms())
            } else {
                "done".to_string()
            };
            let kill_span = if snap.is_running() {
                Span::styled(" ✕", Style::default().fg(Color::Red))
            } else {
                Span::raw("  ")
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
                kill_span,
            ]));
            if snap.is_running() {
                let kill_x = inner.x + inner.width.saturating_sub(2);
                hitmap.kill_rects.push((
                    snap.id.clone(),
                    Rect {
                        x: kill_x,
                        y: row,
                        width: 2,
                        height: 1,
                    },
                ));
            }
            hitmap.task_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: inner.x,
                    y: row,
                    width: inner.width.saturating_sub(3),
                    height: 1,
                },
            ));
            row += 1;
        }
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);

    let btn_y = inner.y + inner.height.saturating_sub(1);
    if btn_y >= inner.y {
        let btn_rect = Rect {
            x: inner.x,
            y: btn_y,
            width: inner.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled(
                    "⊞ History",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ])),
            btn_rect,
        );
        hitmap.history_btn_rect = Some(btn_rect);
    }

    hitmap
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
        Span::styled("●", Style::default().fg(t.success.into()))
    } else {
        Span::styled("○", Style::default().fg(t.subtle_fg.into()))
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(t.subtle_fg.into()))
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

    #[test]
    fn truncate_short() {
        assert_eq!(truncate_label("hi", 10), "hi");
    }

    #[test]
    fn truncate_long() {
        assert_eq!(truncate_label("abcdefghij", 5), "abcd…");
    }

    #[test]
    fn node_kind_glyph_returns_tool_for_toolcall() {
        use atman_runtime::nodegraph::NodeKind;
        let (icon, _) = node_kind_glyph(&NodeKind::ToolCall {
            path: "fs.read".into(),
        });
        assert_eq!(icon, "🔧");
    }

    #[test]
    fn node_kind_glyph_returns_llm_for_llm() {
        use atman_runtime::nodegraph::NodeKind;
        let (icon, _) = node_kind_glyph(&NodeKind::Llm { model: None });
        assert_eq!(icon, "✦");
    }
}
