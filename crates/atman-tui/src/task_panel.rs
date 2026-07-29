use std::time::Instant;

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

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

pub const TASK_PANEL_WIDTH: u16 = 40;
pub const TASK_PANEL_STRIP_WIDTH: u16 = 5;

#[derive(Debug, Default)]
pub struct TaskPanelHitMap {
    pub kill_rects: Vec<(TaskId, Rect)>,
    pub group_header_rects: Vec<(TaskKind, Rect)>,
    pub task_rects: Vec<(String, Rect)>,
    pub task_header_rects: Vec<(String, Rect)>,
    pub activity_rects: Vec<(String, String, Rect)>,
    pub history_btn_rect: Option<Rect>,
    pub hamburger_rect: Option<Rect>,
    pub insert_rects: Vec<(String, Rect)>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskPanelHover {
    pub hovered_task_id: Option<TaskId>,
    pub hovered_kill_id: Option<TaskId>,
    pub hovered_insert_handle: Option<String>,
    pub hovered_activity: Option<(String, String)>,
    pub hovered_history_btn: bool,
    pub hovered_hamburger: bool,
    pub kill_armed_id: Option<TaskId>,
    pub kill_armed_expired: bool,
    pub expanded_tasks: std::collections::HashSet<String>,
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
    let height = area.height.saturating_sub(2);
    if height == 0 {
        return None;
    }
    Some(Rect {
        x: area.x + 1,
        y: area.y + 1,
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

pub fn node_kind_glyph(kind: &atman_runtime::nodegraph::NodeKind) -> (&'static str, Color) {
    let t = crate::theme::theme();
    use atman_runtime::nodegraph::NodeKind;
    match kind {
        NodeKind::Llm { .. } => ("✦", t.accent.into()),
        NodeKind::ToolCall { .. } => ("🔧", t.heading.into()),
        NodeKind::Fanout { .. } => ("⇉", t.accent.into()),
        NodeKind::UserConfirm => ("?", t.warn.into()),
        NodeKind::Subflow { .. } => ("↳", t.accent.into()),
        NodeKind::Message { .. } => ("✉", t.tinted_fg.into()),
        NodeKind::FixUntilTest => ("↻", t.warn.into()),
        NodeKind::When { .. } => ("⋯", t.subtle_fg.into()),
        NodeKind::Return => ("←", t.success.into()),
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

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    activity_nodes: &[ActivityNode],
    items: &[crate::app::OutputItem],
    collapsed: bool,
    collapsed_groups: &std::collections::HashSet<TaskKind>,
    hover: &TaskPanelHover,
) -> TaskPanelHitMap {
    if collapsed {
        let hitmap = render_strip(f, area, snapshots, hover);
        crate::floating_panels::render_shadow(f, area, &crate::theme::theme());
        return hitmap;
    }

    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(Clear, area);

    let panel_bg: Color = t.modal_bg.into();
    let hamburger_fg: Color = if hover.hovered_hamburger {
        t.highlight_bg.lerp(t.heading, 0.3)
    } else {
        t.heading.into()
    };
    let title_line = Line::from(vec![
        Span::styled(
            "  ≡",
            Style::default()
                .fg(hamburger_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            "Tasks",
            Style::default()
                .fg(hamburger_fg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({})", snapshots.iter().filter(|s| s.is_running()).count()),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]);
    let hamburger_rect = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    // top padding row with panel bg
    f.render_widget(
        Block::default().style(Style::default().bg(panel_bg)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
    f.render_widget(
        Block::default()
            .style(Style::default().bg(panel_bg))
            .title(title_line)
            .padding(ratatui::widgets::Padding {
                left: 0,
                right: 0,
                top: 1,
                bottom: 0,
            }),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        },
    );

    let inner = Rect {
        x: area.x + 2,
        y: area.y + 3,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(4),
    };
    let mut hitmap = TaskPanelHitMap::default();
    if inner.height == 0 || inner.width < 3 {
        return hitmap;
    }

    let mut lines: Vec<Line> = Vec::new();
    let mut row = inner.y;

    // --- Upper: Activity (running only, old→new, fg gradient) ---
    let activity_bg: Color = t.code_bg.into();
    let w = inner.width as usize;

    // top padding line
    lines.push(Line::from(vec![Span::styled(
        " ".repeat(w),
        Style::default().bg(activity_bg),
    )]));
    row += 1;

    let running: Vec<&ActivityNode> = activity_nodes
        .iter()
        .filter(|n| n.status == ActivityStatus::Running)
        .collect();
    let running_count = running.len();
    if running_count == 0 {
        let msg = "no active tasks";
        let msg_w = crate::width::width(msg);
        let total_pad = w.saturating_sub(msg_w);
        let left_pad = total_pad / 2;
        let right_pad = total_pad.saturating_sub(left_pad);
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(left_pad), Style::default().bg(activity_bg)),
            Span::styled(msg, Style::default().fg(t.subtle_fg.into()).bg(activity_bg)),
            Span::styled(" ".repeat(right_pad), Style::default().bg(activity_bg)),
        ]));
        row += 1;
    } else {
        let inner_w = w.saturating_sub(2); // left+right padding
        for (i, node) in running.iter().enumerate() {
            let ratio = if running_count <= 1 {
                1.0
            } else {
                i as f64 / (running_count - 1) as f64
            };
            let is_hovered = hover
                .hovered_activity
                .as_ref()
                .is_some_and(|(r, n)| *r == node.run_id && *n == node.node_id);
            let fg = t.code_bg.lerp(t.tinted_fg, 0.15 + ratio * 0.85);
            let label_fg = if is_hovered {
                t.tinted_fg.lerp(t.highlight_bg, 0.3)
            } else {
                fg
            };
            let (kind_icon, _) = node_kind_glyph(&node.kind);
            let elapsed = fmt_activity_elapsed(node.started_at, node.ended_at);
            let elapsed_w = crate::width::width(&elapsed);
            let icon_w = 2;
            let content_w = inner_w.saturating_sub(icon_w + elapsed_w + 2);
            let label = crate::width::truncate(&node.label, content_w);
            let pad = inner_w.saturating_sub(icon_w + crate::width::width(&label) + elapsed_w + 2);
            lines.push(Line::from(vec![
                Span::styled(" ", Style::default().bg(activity_bg)),
                Span::styled(
                    format!(" {kind_icon} "),
                    Style::default().fg(label_fg).bg(activity_bg),
                ),
                Span::styled(label, Style::default().fg(label_fg).bg(activity_bg)),
                Span::styled(" ".repeat(pad), Style::default().bg(activity_bg)),
                Span::styled(elapsed, Style::default().fg(fg).bg(activity_bg)),
                Span::styled("  ", Style::default().bg(activity_bg)),
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

    // bottom padding line
    lines.push(Line::from(vec![Span::styled(
        " ".repeat(w),
        Style::default().bg(activity_bg),
    )]));
    row += 1;

    // blank line separates activity panel from task groups
    lines.push(Line::from(""));
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

    let mut first_group = true;
    for kind in kinds {
        let running_tasks: Vec<&TaskSnapshot> = snapshots
            .iter()
            .filter(|s| s.kind == kind && s.is_running())
            .collect();
        if running_tasks.is_empty() {
            continue;
        }
        if !first_group {
            lines.push(Line::from(""));
            row += 1;
        }
        first_group = false;
        let is_collapsed = collapsed_groups.contains(&kind);
        let glyph = if is_collapsed { "›" } else { "⌄" };
        lines.push(Line::from(vec![
            Span::styled(format!("{glyph} "), Style::default().fg(t.subtle_fg.into())),
            Span::styled(
                kind.label(),
                Style::default()
                    .fg(t.heading.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}", running_tasks.len()),
                Style::default().fg(t.subtle_fg.into()),
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
        let w = inner.width as usize;
        // blank line between group header and first task
        lines.push(Line::from(""));
        row += 1;
        for (task_idx, snap) in running_tasks.iter().enumerate() {
            let is_task_hovered = hover.hovered_task_id == Some(snap.id.clone());
            let is_kill_hovered = hover.hovered_kill_id == Some(snap.id.clone());
            let is_kill_armed =
                hover.kill_armed_id == Some(snap.id.clone()) && !hover.kill_armed_expired;
            let is_expanded = hover.expanded_tasks.contains(&snap.source_handle);
            let header_bg: Color = t.user_msg_bg.into();
            let content_bg: Color = t.code_bg.into();
            let header_bg_hover: Color = t.user_msg_bg.lerp(t.highlight_bg, 0.3);
            let content_bg_hover: Color = t.code_bg.lerp(t.highlight_bg, 0.3);
            let bar_color: Color = t.subtle_fg.into();
            let bar = if is_task_hovered { "▌" } else { "▎" };

            let icon = status_icon(snap.status);
            let elapsed = fmt_elapsed(snap.elapsed_ms());
            let elapsed_w = crate::width::width(&elapsed);
            // layout: bar(1) sp(1) icon(1) sp(1) label(...) pad sp(2) elapsed sp(1) insert(1) sp(1) kill(1) sp(1)
            let right_w = 2 + elapsed_w + 1 + 1 + 1 + 1 + 1; // sp(2) + elapsed + sp + ↵ + sp + ✕ + sp
            let left_w = 1 + 1 + 1 + 1; // bar + sp + icon + sp

            // compute content lines (used for both collapsed summary and expanded view)
            let content_lines: Vec<String> = compute_content_lines(snap, activity_nodes, items);
            let label_max = w.saturating_sub(left_w + right_w);
            let collapse_label_max = if snap.kind == TaskKind::Bash {
                (label_max / 3).max(8)
            } else {
                label_max
            };
            let label = crate::width::truncate(
                &snap.label,
                if is_expanded {
                    label_max
                } else {
                    collapse_label_max
                },
            );
            let summary = if is_expanded {
                String::new()
            } else {
                let actual_label_w = crate::width::width(&label);
                content_lines
                    .last()
                    .map(|l| {
                        let smax = w
                            .saturating_sub(left_w + actual_label_w + right_w + 1)
                            .max(1);
                        crate::width::truncate(l, smax)
                    })
                    .unwrap_or_default()
            };
            let label_w = crate::width::width(&label);
            let pad = w.saturating_sub(left_w + label_w + right_w);

            let h_bg = if is_task_hovered {
                header_bg_hover
            } else {
                header_bg
            };
            let c_bg = if is_task_hovered {
                content_bg_hover
            } else {
                content_bg
            };

            let insert_color: Color = if is_kill_hovered || is_kill_armed {
                t.meta_fg.into()
            } else if hover.hovered_insert_handle.as_deref() == Some(&snap.source_handle) {
                t.tinted_fg.into()
            } else {
                t.meta_fg.into()
            };
            let insert_mod = if hover.hovered_insert_handle.as_deref() == Some(&snap.source_handle)
            {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };

            let (kill_glyph, kill_color, kill_mod) = if is_kill_armed {
                ("⚠", t.warn.into(), Modifier::BOLD)
            } else if is_kill_hovered {
                ("✕", t.error.into(), Modifier::BOLD)
            } else {
                ("✕", t.error.into(), Modifier::empty())
            };

            // kill is at width-2 (with trailing space), insert is 2 chars before
            let kill_x = inner.x + inner.width.saturating_sub(2);
            let insert_x = kill_x.saturating_sub(2);

            if !is_expanded {
                // collapsed: 3 rows = padding + single line + padding
                let collapsed_max = w.saturating_sub(left_w + right_w);
                let collapsed_bg = content_bg;
                let collapsed_bg_hover = content_bg_hover;
                let cbg = if is_task_hovered {
                    collapsed_bg_hover
                } else {
                    collapsed_bg
                };
                // split label and summary for different fg colors
                let (collapsed_label_text, collapsed_summary_text) = if summary.is_empty() {
                    (label.clone(), String::new())
                } else {
                    (label.clone(), summary.clone())
                };
                let label_trunc = crate::width::truncate(&collapsed_label_text, collapsed_max);
                let remaining = collapsed_max.saturating_sub(crate::width::width(&label_trunc) + 1);
                let summary_trunc = if collapsed_summary_text.is_empty() || remaining < 2 {
                    String::new()
                } else {
                    crate::width::truncate(&collapsed_summary_text, remaining)
                };
                let collapsed_label_w = crate::width::width(&label_trunc)
                    + if summary_trunc.is_empty() {
                        0
                    } else {
                        1 + crate::width::width(&summary_trunc)
                    };
                let c_pad = w
                    .saturating_sub(left_w + collapsed_label_w + right_w)
                    .max(0);

                // top padding (with bar)
                let top_y = row;
                lines.push(Line::from(vec![
                    Span::styled(bar, Style::default().fg(bar_color).bg(cbg)),
                    Span::styled(" ".repeat(w.saturating_sub(1)), Style::default().bg(cbg)),
                ]));
                hitmap.task_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: top_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                hitmap.task_header_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: top_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                row += 1;

                // single line (acts as header)
                let mid_y = row;
                hitmap.kill_rects.push((
                    snap.id.clone(),
                    Rect {
                        x: kill_x,
                        y: mid_y,
                        width: 1,
                        height: 1,
                    },
                ));
                hitmap.insert_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: insert_x,
                        y: mid_y,
                        width: 1,
                        height: 1,
                    },
                ));
                lines.push(Line::from({
                    let mut spans = vec![
                        Span::styled(bar, Style::default().fg(bar_color).bg(cbg)),
                        Span::styled(format!(" {icon} "), Style::default().fg(bar_color).bg(cbg)),
                        Span::styled(label_trunc, Style::default().fg(t.subtle_fg.into()).bg(cbg)),
                    ];
                    if !summary_trunc.is_empty() {
                        spans.push(Span::styled(" ", Style::default().bg(cbg)));
                        spans.push(Span::styled(
                            summary_trunc,
                            Style::default().fg(t.tinted_fg.into()).bg(cbg),
                        ));
                    }
                    spans.push(Span::styled(" ".repeat(c_pad), Style::default().bg(cbg)));
                    spans.push(Span::styled("  ", Style::default().bg(cbg)));
                    spans.push(Span::styled(
                        elapsed,
                        Style::default().fg(t.subtle_fg.into()).bg(cbg),
                    ));
                    spans.push(Span::styled(" ", Style::default().bg(cbg)));
                    spans.push(Span::styled(
                        "↵",
                        Style::default()
                            .fg(insert_color)
                            .bg(cbg)
                            .add_modifier(insert_mod),
                    ));
                    spans.push(Span::styled(" ", Style::default().bg(cbg)));
                    spans.push(Span::styled(
                        kill_glyph,
                        Style::default()
                            .fg(kill_color)
                            .bg(cbg)
                            .add_modifier(kill_mod),
                    ));
                    spans.push(Span::styled(" ", Style::default().bg(cbg)));
                    spans
                }));
                hitmap.task_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: mid_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                hitmap.task_header_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: mid_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                row += 1;

                // bottom padding (with bar)
                let bot_y = row;
                lines.push(Line::from(vec![
                    Span::styled(bar, Style::default().fg(bar_color).bg(cbg)),
                    Span::styled(" ".repeat(w.saturating_sub(1)), Style::default().bg(cbg)),
                ]));
                hitmap.task_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: bot_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                hitmap.task_header_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: bot_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                row += 1;

                if task_idx + 1 < running_tasks.len() {
                    lines.push(Line::from(""));
                    row += 1;
                }
                continue;
            }

            // expanded: header + content padding + content + content padding
            let header_row = row;
            lines.push(Line::from(vec![
                Span::styled(bar, Style::default().fg(bar_color).bg(h_bg)),
                Span::styled(format!(" {icon} "), Style::default().fg(bar_color).bg(h_bg)),
                Span::styled(
                    label.clone(),
                    Style::default().fg(t.tinted_fg.into()).bg(h_bg),
                ),
                Span::styled(" ".repeat(pad), Style::default().bg(h_bg)),
                Span::styled("  ", Style::default().bg(h_bg)),
                Span::styled(
                    elapsed.clone(),
                    Style::default().fg(t.meta_fg.into()).bg(h_bg),
                ),
                Span::styled(" ", Style::default().bg(h_bg)),
                Span::styled(
                    "↵",
                    Style::default()
                        .fg(insert_color)
                        .bg(h_bg)
                        .add_modifier(insert_mod),
                ),
                Span::styled(" ", Style::default().bg(h_bg)),
                Span::styled(
                    kill_glyph,
                    Style::default()
                        .fg(kill_color)
                        .bg(h_bg)
                        .add_modifier(kill_mod),
                ),
                Span::styled(" ", Style::default().bg(h_bg)),
            ]));
            hitmap.kill_rects.push((
                snap.id.clone(),
                Rect {
                    x: kill_x,
                    y: header_row,
                    width: 1,
                    height: 1,
                },
            ));
            hitmap.insert_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: insert_x,
                    y: header_row,
                    width: 1,
                    height: 1,
                },
            ));
            hitmap.task_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: inner.x,
                    y: header_row,
                    width: inner.width.saturating_sub(4),
                    height: 1,
                },
            ));
            hitmap.task_header_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: inner.x,
                    y: header_row,
                    width: inner.width.saturating_sub(4),
                    height: 1,
                },
            ));
            row += 1;

            // content lines already computed above
            let content_rows: Vec<String> = if content_lines.is_empty() {
                vec![String::new()]
            } else {
                content_lines
                    .iter()
                    .map(|c| crate::width::truncate(c, w.saturating_sub(4)))
                    .collect()
            };

            // content top padding
            let top_pad_y = row;
            let pad_bg = if is_task_hovered {
                content_bg_hover
            } else {
                content_bg
            };
            lines.push(Line::from(vec![Span::styled(
                " ".repeat(w),
                Style::default().bg(pad_bg),
            )]));
            hitmap.task_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: inner.x,
                    y: top_pad_y,
                    width: inner.width.saturating_sub(4),
                    height: 1,
                },
            ));
            row += 1;

            for (i, content_trunc) in content_rows.iter().enumerate() {
                let content_y = header_row + 2 + i as u16;
                let content_pad = w.saturating_sub(2 + crate::width::width(content_trunc) + 2);
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default().bg(c_bg)),
                    Span::styled(
                        content_trunc.clone(),
                        Style::default().fg(t.meta_fg.into()).bg(c_bg),
                    ),
                    Span::styled(" ".repeat(content_pad), Style::default().bg(c_bg)),
                    Span::styled("  ", Style::default().bg(c_bg)),
                ]));
                hitmap.task_rects.push((
                    snap.source_handle.clone(),
                    Rect {
                        x: inner.x,
                        y: content_y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    },
                ));
                row += 1;
            }

            // content bottom padding
            let bot_pad_y = row;
            lines.push(Line::from(vec![Span::styled(
                " ".repeat(w),
                Style::default().bg(pad_bg),
            )]));
            hitmap.task_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: inner.x,
                    y: bot_pad_y,
                    width: inner.width.saturating_sub(4),
                    height: 1,
                },
            ));
            row += 1;
            if task_idx + 1 < running_tasks.len() {
                lines.push(Line::from(""));
                row += 1;
            }
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
        let history_color: Color = if hover.hovered_history_btn {
            t.tinted_fg.into()
        } else {
            t.subtle_fg.into()
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ", Style::default()),
                Span::styled("⊞ History", Style::default().fg(history_color)),
            ])),
            btn_rect,
        );
        hitmap.history_btn_rect = Some(btn_rect);
    }
    hitmap.hamburger_rect = Some(hamburger_rect);

    crate::floating_panels::render_shadow(f, area, &t);
    hitmap
}

pub fn compute_content_lines(
    snap: &TaskSnapshot,
    activity_nodes: &[ActivityNode],
    items: &[crate::app::OutputItem],
) -> Vec<String> {
    match snap.kind {
        TaskKind::Flow | TaskKind::Subflow | TaskKind::Agent | TaskKind::Dispatch => {
            let sub = activity_nodes
                .iter()
                .rev()
                .find(|n| n.run_id == snap.source_handle && n.status == ActivityStatus::Running);
            sub.map(|n| vec![n.label.clone()]).unwrap_or_default()
        }
        TaskKind::Bash => {
            let item = items.iter().rev().find(|it| match it {
                crate::app::OutputItem::Bash { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            let mut found: Vec<String> = Vec::new();
            if let Some(crate::app::OutputItem::Bash { output, .. }) = item {
                for line in output.lines().rev() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        found.push(trimmed.to_string());
                        if found.len() >= 3 {
                            break;
                        }
                    }
                }
                found.reverse();
            }
            found
        }
        TaskKind::Terminal => {
            let item = items.iter().rev().find(|it| match it {
                crate::app::OutputItem::Terminal { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            let mut found: Vec<String> = Vec::new();
            if let Some(crate::app::OutputItem::Terminal { screen, .. }) = item {
                let cols = screen.cols as usize;
                let rows = screen.rows as usize;
                if cols > 0 && rows > 0 {
                    for r in (0..rows).rev() {
                        let start = r * cols;
                        let end = start + cols;
                        let line: String = screen
                            .cells
                            .get(start..end)
                            .map(|cells| cells.iter().map(|c| c.chars.as_str()).collect::<String>())
                            .unwrap_or_default();
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            found.push(trimmed.to_string());
                            if found.len() >= 3 {
                                break;
                            }
                        }
                    }
                    found.reverse();
                }
            }
            found
        }
    }
}

fn render_strip(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    hover: &TaskPanelHover,
) -> TaskPanelHitMap {
    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(Clear, area);
    let mut hitmap = TaskPanelHitMap::default();

    let running = snapshots.iter().filter(|s| s.is_running()).count();
    let dot = if running > 0 {
        Span::styled("●", Style::default().fg(t.success.into()))
    } else {
        Span::styled("○", Style::default().fg(t.subtle_fg.into()))
    };

    let panel_bg: Color = t.modal_bg.into();
    // top padding row
    f.render_widget(
        Block::default().style(Style::default().bg(panel_bg)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
    let hamburger_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    let hamburger_fg: Color = if hover.hovered_hamburger {
        t.highlight_bg.lerp(t.heading, 0.3)
    } else {
        t.heading.into()
    };
    f.render_widget(
        Block::default().style(Style::default().bg(panel_bg)).title(
            Line::from(vec![Span::styled(
                "≡",
                Style::default()
                    .fg(hamburger_fg)
                    .add_modifier(Modifier::BOLD),
            )])
            .alignment(Alignment::Center),
        ),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        },
    );
    hitmap.hamburger_rect = Some(hamburger_area);

    // history icon near bottom — keep 1-row gap from edge like expanded mode
    let btn_y = area.y + area.height.saturating_sub(2);
    let dot_y = if area.height > 5 {
        area.y + (area.height / 2)
    } else {
        area.y + 1
    };
    let dot_area = Rect {
        x: area.x,
        y: dot_y,
        width: area.width,
        height: 1,
    };
    f.render_widget(
        Paragraph::new(Line::from(dot)).alignment(Alignment::Center),
        dot_area,
    );

    // history icon at bottom row
    if btn_y > dot_y && btn_y >= area.y {
        let history_color: Color = if hover.hovered_history_btn {
            t.tinted_fg.into()
        } else {
            t.subtle_fg.into()
        };
        let btn_rect = Rect {
            x: area.x,
            y: btn_y,
            width: area.width,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "⊞",
                Style::default().fg(history_color),
            )]))
            .alignment(Alignment::Center),
            btn_rect,
        );
        hitmap.history_btn_rect = Some(btn_rect);
    }

    hitmap
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn hitmap_group_header_not_overlapped_by_task_content() {
        use atman_runtime::task_registry::{TaskKind, TaskStatus};
        use ratatui::backend::TestBackend;
        use std::time::Instant;

        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let snapshots = vec![
            TaskSnapshot {
                id: TaskId::default(),
                kind: TaskKind::Terminal,
                label: "vim".into(),
                status: TaskStatus::Running,
                started_at: Instant::now(),
                ended_at: None,
                source_handle: "term_0".into(),
                session_id: "s".into(),
            },
            TaskSnapshot {
                id: TaskId::default(),
                kind: TaskKind::Flow,
                label: "agent".into(),
                status: TaskStatus::Running,
                started_at: Instant::now(),
                ended_at: None,
                source_handle: "flow_0".into(),
                session_id: "s".into(),
            },
        ];
        let items = vec![crate::app::OutputItem::Terminal {
            handle: "term_0".into(),
            screen: atman_runtime::tools::term::TerminalScreen {
                rows: 3,
                cols: 10,
                cells: vec![
                    atman_runtime::tools::term::TerminalCell {
                        chars: "line1".into(),
                        ..Default::default()
                    };
                    10
                ],
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: vec![],
            mode: crate::app::TerminalViewMode::Capture,
            done: false,
            expanded: false,
            scroll_offset: None,
        }];
        let hover = TaskPanelHover::default();
        let backend = TestBackend::new(40, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hm = TaskPanelHitMap::default();
        terminal
            .draw(|f| {
                hm = render(
                    f,
                    area,
                    &snapshots,
                    &[],
                    &items,
                    false,
                    &std::collections::HashSet::new(),
                    &hover,
                );
            })
            .unwrap();

        let flow_header = hm
            .group_header_rects
            .iter()
            .find(|(k, _)| *k == TaskKind::Flow)
            .expect("Flow group header");

        for (_, tr) in &hm.task_rects {
            assert!(
                !(flow_header.1.y >= tr.y && flow_header.1.y < tr.y + tr.height),
                "Flow group header y={} is inside task_rect y={} height={}",
                flow_header.1.y,
                tr.y,
                tr.height
            );
        }
    }
}
