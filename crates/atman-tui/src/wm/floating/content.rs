use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::message::Message;
use atman_runtime::workflow::WorkflowGraph;
use atman_runtime::{TaskKind, TaskSnapshot};

use crate::app::OutputItem;
use crate::task_panel::{ActivityNode, ActivityStatus};

use super::{PanelBtn, PanelKind, PanelRenderCache, WindowInstance, WmHitmap, task_kind_icon};

#[allow(clippy::too_many_arguments)]
pub fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &mut WindowInstance,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    hovered_history_row: &Option<String>,
    hitmap_out: &mut WmHitmap,
    animation_frame: u32,
    mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    expanded_mcp_servers: &std::collections::HashSet<String>,
    mcp_selected: usize,
    hovered_mcp_row: &Option<String>,
    mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
    items_version: u64,
    expanded_version: u64,
) {
    let _hovered_btn = hovered_btn;
    if area.height == 0 || area.width == 0 {
        return;
    }

    if let Some(ref mut content) = panel.content {
        let _ = content.render_content(
            area,
            f,
            &crate::wm::RenderCtx {
                snapshots,
                items,
                animation_frame,
                panel_width: area.width,
                expanded_tools: &panel.expanded_tools,
            },
        );
        return;
    }

    let area = Rect::new(area.x + 1, area.y, area.width - 2, area.height);

    match panel.kind {
        PanelKind::History => render_history_content(
            f,
            area,
            snapshots,
            items,
            hitmap_out,
            hovered_history_row,
            panel.scroll,
        ),
        PanelKind::Activity => {
            let parts: Vec<&str> = panel.id.splitn(2, ':').collect();
            if parts.len() == 2 {
                let node = activity_nodes
                    .iter()
                    .find(|n| n.run_id == parts[0] && n.node_id == parts[1]);
                if let Some(node) = node {
                    render_activity_content(f, area, node);
                    return;
                }
            }
            render_placeholder(f, area, &panel.title);
        }
        PanelKind::Mermaid => {
            let item_idx: Option<usize> = panel
                .id
                .strip_prefix("mermaid:")
                .and_then(|s| s.parse().ok());
            if let Some(idx) = item_idx
                && let Some(OutputItem::MermaidDiagram { source }) = items.get(idx)
            {
                let pad = 2u16;
                let inner_area = Rect {
                    x: area.x + pad,
                    y: area.y + 1,
                    width: area.width.saturating_sub(pad * 2),
                    height: area.height.saturating_sub(2),
                };
                if inner_area.width == 0 || inner_area.height == 0 {
                    return;
                }
                if panel.split {
                    let half_w = inner_area.width / 2;
                    let left_area = Rect {
                        width: half_w,
                        ..inner_area
                    };
                    let right_area = Rect {
                        x: inner_area.x + half_w,
                        width: inner_area.width.saturating_sub(half_w),
                        ..inner_area
                    };

                    let t = crate::theme::theme();
                    let src_bg: Color = t.code_bg.into();

                    let src_lines: Vec<Line<'static>> = source
                        .lines()
                        .enumerate()
                        .map(|(i, l)| {
                            Line::from(vec![
                                Span::styled(
                                    format!("{:>3} ", i + 1),
                                    Style::default()
                                        .fg(t.subtle_fg.into())
                                        .bg(src_bg)
                                        .add_modifier(Modifier::DIM),
                                ),
                                Span::styled(
                                    l.to_string(),
                                    Style::default().fg(t.tinted_fg.into()).bg(src_bg),
                                ),
                            ])
                        })
                        .collect();
                    let src_max = (src_lines.len() as u16).saturating_sub(left_area.height);
                    panel.scroll = panel.scroll.min(src_max);
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(src_lines).scroll((panel.scroll, 0)),
                        left_area,
                    );

                    let diagram_lines = crate::mermaid::render_mermaid(source, right_area.width);
                    let diag_max = (diagram_lines.len() as u16).saturating_sub(right_area.height);
                    panel.h_scroll = panel.h_scroll.min(diag_max);
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(diagram_lines).scroll((panel.h_scroll, 0)),
                        right_area,
                    );

                    let divider_x = inner_area.x + half_w;
                    for y in inner_area.y..inner_area.y + inner_area.height {
                        if let Some(cell) = f
                            .buffer_mut()
                            .cell_mut(ratatui::layout::Position { x: divider_x, y })
                        {
                            cell.set_char('│');
                        }
                    }
                } else {
                    let lines = crate::mermaid::render_mermaid(source, inner_area.width);
                    let max_scroll = (lines.len() as u16).saturating_sub(inner_area.height);
                    panel.scroll = panel.scroll.min(max_scroll);
                    let content_w = lines
                        .iter()
                        .map(|l| crate::width::spans_width(&l.spans))
                        .max()
                        .unwrap_or(0) as u16;
                    let max_h_scroll = content_w.saturating_sub(inner_area.width);
                    panel.h_scroll = panel.h_scroll.min(max_h_scroll);
                    let scroll = panel.scroll;
                    let h_scroll = panel.h_scroll;
                    let visible_w = inner_area.width as usize;
                    let scrolled_lines: Vec<Line> = lines
                        .iter()
                        .skip(scroll as usize)
                        .map(|l| {
                            let line_str: String =
                                l.spans.iter().map(|s| s.content.as_ref()).collect();
                            let trimmed = crate::width::trim_display_offset(
                                &line_str,
                                h_scroll as usize,
                                visible_w,
                            );
                            Line::from(Span::styled(
                                trimmed,
                                l.spans.first().map(|s| s.style).unwrap_or_default(),
                            ))
                        })
                        .collect();
                    let p = ratatui::widgets::Paragraph::new(scrolled_lines);
                    f.render_widget(p, inner_area);
                }
            } else {
                render_placeholder(f, area, &panel.title);
            }
        }
        PanelKind::Cheatsheet => {
            let lines = crate::completion::cheatsheet_lines();
            let max_scroll = (lines.len() as u16).saturating_sub(area.height);
            panel.scroll = panel.scroll.min(max_scroll);
            let scroll = panel.scroll;
            let visible: Vec<Line> = lines.into_iter().skip(scroll as usize).collect();
            let p = ratatui::widgets::Paragraph::new(visible);
            f.render_widget(p, area);
        }
        PanelKind::Mcp => {
            crate::mcp_manager::render_panel(
                f,
                area,
                &mut panel.scroll,
                mcp_servers,
                expanded_mcp_servers,
                mcp_selected,
                hovered_mcp_row,
                hitmap_out,
                mcp_browser,
            );
        }
        PanelKind::Task(kind) => {
            let snap = snapshots.iter().find(|s| s.source_handle == panel.id);
            match kind {
                TaskKind::Bash => {
                    let item = items.iter().rev().find(|it| match it {
                        OutputItem::Bash { handle, .. } => *handle == panel.id,
                        _ => false,
                    });
                    if let Some(OutputItem::Bash { output, done, .. }) = item {
                        if let Some(snap) = snap {
                            render_bash_content(f, area, snap, output, *done);
                        } else {
                            render_bash_screen(f, area, &panel.title, output, *done);
                        }
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
                TaskKind::Terminal => {
                    let item = items.iter().rev().find(|it| match it {
                        OutputItem::Terminal { handle, .. } => *handle == panel.id,
                        _ => false,
                    });
                    if let Some(OutputItem::Terminal {
                        screen,
                        accumulated_bytes,
                        done,
                        ..
                    }) = item
                    {
                        if let Some(snap) = snap {
                            render_terminal_content(
                                f,
                                area,
                                snap,
                                screen,
                                accumulated_bytes,
                                *done,
                            );
                        } else {
                            render_terminal_screen(f, area, &panel.title, screen);
                        }
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
                TaskKind::Flow => {
                    let item_idx = items
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, it)| match it {
                            OutputItem::SubAgentActivity { handle, .. } => *handle == panel.id,
                            _ => false,
                        })
                        .map(|(i, _)| i);
                    let item = item_idx.and_then(|i| items.get(i));
                    if let Some(OutputItem::SubAgentActivity {
                        handle,
                        goal,
                        model,
                        status,
                        output,
                        iteration,
                        done,
                        messages,
                        workflow_graph,
                        expanded_nodes,
                        workflow_expanded,
                        ..
                    }) = item
                    {
                        render_sub_agent_panel(
                            f,
                            area,
                            handle,
                            goal,
                            model,
                            status,
                            output,
                            *iteration,
                            *done,
                            messages,
                            workflow_graph,
                            expanded_nodes,
                            *workflow_expanded,
                            &panel.expanded_tools,
                            &mut panel.scroll,
                            animation_frame,
                            hitmap_out,
                            item_idx.unwrap(),
                            items_version,
                            expanded_version,
                            &mut panel.render_cache,
                        );
                    } else if let Some((panel_idx, _)) =
                        items.iter().enumerate().rev().find(|(_, it)| {
                            if let OutputItem::WorkflowPanel { graph, .. } = it {
                                graph.root.iter().any(|node| {
                                    matches!(
                                        &node.kind,
                                        atman_runtime::workflow::WorkflowNodeKind::Flow {
                                            run_id,
                                            ..
                                        } if run_id == &panel.id
                                    )
                                })
                            } else {
                                false
                            }
                        })
                        && let Some(OutputItem::WorkflowPanel {
                            graph,
                            expanded_nodes,
                            ..
                        }) = items.get(panel_idx)
                    {
                        let render_width = area.width.max(300);
                        let wf_running = workflow_graph_is_running(graph);
                        let cache_af = if wf_running {
                            Some(animation_frame)
                        } else {
                            None
                        };

                        let cache_hit = panel.render_cache.as_ref().is_some_and(|cache| {
                            cache.items_version == items_version
                                && cache.expanded_version == expanded_version
                                && cache.width == area.width
                                && cache.animation_frame == cache_af
                                && cache.workflow_expanded
                        });

                        if cache_hit {
                            let cache = panel.render_cache.as_ref().unwrap();
                            let max_scroll = (cache.lines.len() as u16).saturating_sub(area.height);
                            let content_w = cache
                                .lines
                                .iter()
                                .map(|l| crate::width::spans_width(&l.spans))
                                .max()
                                .unwrap_or(0) as u16;
                            let max_h_scroll = content_w.saturating_sub(area.width);
                            panel.scroll = panel.scroll.min(max_scroll);
                            panel.h_scroll = panel.h_scroll.min(max_h_scroll);
                            let scroll = panel.scroll;
                            let h_scroll = panel.h_scroll;
                            for r in &cache.regions {
                                let row0 =
                                    area.y as u32 + r.start_row.saturating_sub(scroll as u32);
                                let row1 = area.y as u32 + r.end_row.saturating_sub(scroll as u32);
                                let col0 = area.x + r.col_start.saturating_sub(h_scroll);
                                let col1 = area.x + r.col_end.saturating_sub(h_scroll);
                                if col1 > col0 && row1 > row0 {
                                    hitmap_out.workflow_node_rects.push((
                                        panel_idx,
                                        r.path_key.clone(),
                                        Rect {
                                            x: col0,
                                            y: row0 as u16,
                                            width: col1.saturating_sub(col0),
                                            height: (row1.saturating_sub(row0)) as u16,
                                        },
                                    ));
                                }
                            }
                            let p = ratatui::widgets::Paragraph::new(cache.lines.clone())
                                .scroll((panel.scroll, panel.h_scroll));
                            f.render_widget(p, area);
                        } else {
                            let (lines, regions) =
                                crate::output::render_workflow_panel_with_regions(
                                    graph,
                                    expanded_nodes,
                                    true,
                                    false,
                                    animation_frame,
                                    render_width,
                                    crate::output::MAX_COLLAPSED_BODY_ROWS,
                                );
                            let max_scroll = (lines.len() as u16).saturating_sub(area.height);
                            let content_w = lines
                                .iter()
                                .map(|l| crate::width::spans_width(&l.spans))
                                .max()
                                .unwrap_or(0) as u16;
                            let max_h_scroll = content_w.saturating_sub(area.width);
                            panel.scroll = panel.scroll.min(max_scroll);
                            panel.h_scroll = panel.h_scroll.min(max_h_scroll);
                            let scroll = panel.scroll;
                            let h_scroll = panel.h_scroll;
                            for r in &regions {
                                let row0 =
                                    area.y as u32 + r.start_row.saturating_sub(scroll as u32);
                                let row1 = area.y as u32 + r.end_row.saturating_sub(scroll as u32);
                                let col0 = area.x + r.col_start.saturating_sub(h_scroll);
                                let col1 = area.x + r.col_end.saturating_sub(h_scroll);
                                if col1 > col0 && row1 > row0 {
                                    hitmap_out.workflow_node_rects.push((
                                        panel_idx,
                                        r.path_key.clone(),
                                        Rect {
                                            x: col0,
                                            y: row0 as u16,
                                            width: col1.saturating_sub(col0),
                                            height: (row1.saturating_sub(row0)) as u16,
                                        },
                                    ));
                                }
                            }

                            panel.render_cache = Some(PanelRenderCache {
                                items_version,
                                expanded_version,
                                width: area.width,
                                animation_frame: cache_af,
                                messages_len: 0,
                                workflow_expanded: true,
                                expanded_tools_len: 0,
                                lines: lines.clone(),
                                regions: regions.clone(),
                                wf_offset: 0,
                            });

                            let p = ratatui::widgets::Paragraph::new(lines)
                                .scroll((panel.scroll, panel.h_scroll));
                            f.render_widget(p, area);
                        }
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
            }
        }
    }
}

/// Check if any node in the workflow graph is still running or pending.
/// Used to decide whether to invalidate the render cache on animation ticks.
fn workflow_graph_is_running(graph: &WorkflowGraph) -> bool {
    use atman_runtime::workflow::{NodeStatus, WorkflowNode};
    fn walk(nodes: &[WorkflowNode]) -> bool {
        nodes.iter().any(|n| {
            matches!(n.status, NodeStatus::Running | NodeStatus::Pending) || walk(&n.children)
        })
    }
    walk(&graph.root)
}

fn task_summary_line(snap: &TaskSnapshot, items: &[OutputItem]) -> String {
    match snap.kind {
        TaskKind::Bash => {
            let item = items.iter().rev().find(|it| match it {
                OutputItem::Bash { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            if let Some(OutputItem::Bash { output, .. }) = item {
                for line in output.lines().rev() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
            String::new()
        }
        TaskKind::Terminal => {
            let item = items.iter().rev().find(|it| match it {
                OutputItem::Terminal { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            if let Some(OutputItem::Terminal { screen, .. }) = item {
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
                            return trimmed.to_string();
                        }
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn format_started_at(snap: &TaskSnapshot) -> String {
    if let Some(ts) = snap.id.0.get_timestamp() {
        let (secs, _nanos) = ts.to_unix();
        if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0) {
            return dt
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string();
        }
    }
    "--:--:--".to_string()
}

fn render_history_content(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    hitmap: &mut WmHitmap,
    hovered_row: &Option<String>,
    scroll: u16,
) {
    let t = crate::theme::theme();
    let bar_color: Color = t.subtle_fg.into();
    let content_bg: Color = t.code_bg.into();
    let hover_bg: Color = t.code_bg.lerp(t.highlight_bg, 0.3);
    let mut done: Vec<&TaskSnapshot> = snapshots.iter().filter(|s| !s.is_running()).collect();
    done.sort_by_key(|b| std::cmp::Reverse(b.ended_at));

    let visible_height = area.height;
    let max_scroll = done.len().saturating_sub(visible_height as usize) as u16;
    let scroll = scroll.min(max_scroll);

    let mut lines: Vec<Line> = Vec::new();
    for (i, snap) in done.iter().enumerate() {
        let icon = status_icon(snap.status);
        let elapsed = format_elapsed(snap.elapsed_ms());
        let st_color = status_color(snap.status);
        let visible_i = (i as u16).saturating_sub(scroll);
        let row_y = area.y + visible_i;
        let is_hovered = hovered_row.as_deref() == Some(&snap.source_handle);
        if visible_i < visible_height {
            hitmap.history_row_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: area.x,
                    y: row_y,
                    width: area.width,
                    height: 1,
                },
            ));
        }
        let row_bg = if is_hovered { hover_bg } else { content_bg };
        let bar = if is_hovered { "▌" } else { "▎" };
        let label_fg = if is_hovered {
            t.tinted_fg.into()
        } else {
            t.subtle_fg.into()
        };
        let time_fg: Color = t.meta_fg.into();
        let kind_icon = task_kind_icon(snap.kind);
        let started = format_started_at(snap);
        let summary = task_summary_line(snap, items);
        let bar_str = format!("{bar} ");
        let kind_str = format!("{kind_icon} ");
        let icon_str = format!("{icon} ");
        let prefix_w: u16 = crate::width::width(bar_str.as_str()) as u16
            + crate::width::width(kind_str.as_str()) as u16
            + crate::width::width(icon_str.as_str()) as u16;
        let suffix_w: u16 = crate::width::width(started.as_str()) as u16
            + 1
            + crate::width::width(elapsed.as_str()) as u16
            + 1;
        let content_max = area.width.saturating_sub(prefix_w + suffix_w + 1) as usize;
        let label_text = if summary.is_empty() {
            snap.label.clone()
        } else {
            format!("{} · {}", snap.label, summary)
        };
        let label = crate::width::truncate(&label_text, content_max);
        let label_w = crate::width::width(label.as_str()) as u16;
        let pad = area
            .width
            .saturating_sub(prefix_w)
            .saturating_sub(label_w)
            .saturating_sub(suffix_w)
            .max(1);
        lines.push(Line::from(vec![
            Span::styled(bar_str, Style::default().fg(bar_color).bg(row_bg)),
            Span::styled(kind_str, Style::default().fg(st_color).bg(row_bg)),
            Span::styled(icon_str, Style::default().fg(st_color).bg(row_bg)),
            Span::styled(label, Style::default().fg(label_fg).bg(row_bg)),
            Span::styled(" ".repeat(pad as usize), Style::default().bg(row_bg)),
            Span::styled(started, Style::default().fg(time_fg).bg(row_bg)),
            Span::styled(" ", Style::default().bg(row_bg)),
            Span::styled(elapsed, Style::default().fg(label_fg).bg(row_bg)),
            Span::styled(" ", Style::default().bg(row_bg)),
        ]));
    }
    if done.is_empty() {
        let msg = "no completed tasks";
        let total_pad = area.width as usize;
        let left = total_pad.saturating_sub(msg.len()) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(left), Style::default()),
            Span::styled(msg, Style::default().fg(t.subtle_fg.into())),
        ]));
    }
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

#[allow(clippy::too_many_arguments)]
fn render_sub_agent_panel(
    f: &mut Frame,
    area: Rect,
    handle: &str,
    goal: &str,
    model: &str,
    status: &str,
    _output: &str,
    iteration: u64,
    done: bool,
    messages: &[Message],
    workflow_graph: &WorkflowGraph,
    expanded_nodes: &HashSet<String>,
    workflow_expanded: bool,
    expanded_tools: &HashSet<String>,
    scroll: &mut u16,
    animation_frame: u32,
    hitmap_out: &mut WmHitmap,
    item_idx: usize,
    items_version: u64,
    expanded_version: u64,
    render_cache: &mut Option<PanelRenderCache>,
) {
    let cache_af = if done { None } else { Some(animation_frame) };

    if render_cache.as_ref().is_some_and(|cache| {
        cache.items_version == items_version
            && cache.expanded_version == expanded_version
            && cache.width == area.width
            && cache.animation_frame == cache_af
            && cache.messages_len == messages.len()
            && cache.workflow_expanded == workflow_expanded
            && cache.expanded_tools_len == expanded_tools.len()
    }) {
        let cache = render_cache.as_ref().unwrap();
        let max_scroll = (cache.lines.len() as u16).saturating_sub(area.height);
        *scroll = (*scroll).min(max_scroll);
        for r in &cache.regions {
            let row0 =
                area.y as u32 + (r.start_row + cache.wf_offset).saturating_sub(*scroll as u32);
            let row1 = area.y as u32 + (r.end_row + cache.wf_offset).saturating_sub(*scroll as u32);
            let col0 = area.x + r.col_start;
            let col1 = area.x + r.col_end;
            if col1 > col0 && row1 > row0 {
                hitmap_out.workflow_node_rects.push((
                    item_idx,
                    r.path_key.clone(),
                    Rect {
                        x: col0,
                        y: row0 as u16,
                        width: col1 - col0,
                        height: (row1 - row0) as u16,
                    },
                ));
            }
        }
        f.render_widget(
            Paragraph::new(cache.lines.clone()).scroll((*scroll, 0)),
            area,
        );
        return;
    }

    let t = crate::theme::theme();
    let label_style = Style::default().fg(t.subtle_fg.into());
    let value_style = Style::default().fg(t.tinted_fg.into());
    let accent_style = Style::default().fg(t.accent.into());

    // Section 1: Header table
    let icon = match status {
        "ok" => "✓",
        "err" => "✗",
        "killed" => "⊘",
        "interrupted" => "⚠",
        _ if done => "✓",
        _ => "◐",
    };
    let iter_str = if done {
        String::new()
    } else {
        format!(" (iter {iteration})")
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        format!(" {icon} {handle}{iter_str}"),
        accent_style,
    ));
    lines.push(Line::from(vec![
        Span::styled(" goal:   ", label_style),
        Span::styled(goal.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" model:  ", label_style),
        Span::styled(model.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" status: ", label_style),
        Span::styled(status.to_string(), value_style),
    ]));
    lines.push(Line::from(""));

    // Section 2: Workflow graph — reuse the same renderer as the main transcript
    let wf_offset = lines.len() as u32;
    let (wf_lines, regions) = crate::output::render_workflow_panel_with_regions(
        workflow_graph,
        expanded_nodes,
        workflow_expanded,
        status == "killed",
        animation_frame,
        area.width.max(300),
        crate::output::MAX_COLLAPSED_BODY_ROWS,
    );
    lines.extend(wf_lines);
    lines.push(Line::from(""));

    // Section 3: Sub-document flow — use flatten_message for proper OutputItem
    // types (Bash/DiffPreview/Terminal with collapse support), identical to
    // the main transcript rendering.
    let mut tool_map: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if matches!(msg.role, atman_runtime::message::MessageRole::Assistant) {
            for part in &msg.parts {
                if let atman_runtime::message::MessagePart::ToolUse { id, name, .. } = part {
                    tool_map.insert(id.clone(), name.clone());
                }
            }
        }
    }
    let mut items: Vec<OutputItem> = Vec::new();
    for msg in messages {
        crate::history::flatten_message(msg, &mut items, &tool_map);
    }

    let render_ctx = crate::output::RenderCtx {
        expanded_tools,
        messages,
        animation_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
    };
    let doc_lines = crate::output::build_lines(&items, &render_ctx);
    lines.extend(doc_lines);

    let max_scroll = (lines.len() as u16).saturating_sub(area.height);
    *scroll = (*scroll).min(max_scroll);

    for r in &regions {
        let row0 = area.y as u32 + (r.start_row + wf_offset).saturating_sub(*scroll as u32);
        let row1 = area.y as u32 + (r.end_row + wf_offset).saturating_sub(*scroll as u32);
        let col0 = area.x + r.col_start;
        let col1 = area.x + r.col_end;
        if col1 > col0 && row1 > row0 {
            hitmap_out.workflow_node_rects.push((
                item_idx,
                r.path_key.clone(),
                Rect {
                    x: col0,
                    y: row0 as u16,
                    width: col1 - col0,
                    height: (row1 - row0) as u16,
                },
            ));
        }
    }

    *render_cache = Some(PanelRenderCache {
        items_version,
        expanded_version,
        width: area.width,
        animation_frame: cache_af,
        messages_len: messages.len(),
        workflow_expanded,
        expanded_tools_len: expanded_tools.len(),
        lines: lines.clone(),
        regions: regions.clone(),
        wf_offset,
    });

    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), area);
}

fn render_task_meta(f: &mut Frame, area: Rect, kind: TaskKind, snap: &TaskSnapshot) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            format!("{:?}", snap.status),
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

fn render_bash_content(f: &mut Frame, area: Rect, snap: &TaskSnapshot, output: &str, done: bool) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            if done {
                "done".into()
            } else {
                format_elapsed(snap.elapsed_ms())
            },
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

    let all_lines: Vec<&str> = output.lines().collect();
    let max_visible = body_area.height as usize;
    let start = all_lines.len().saturating_sub(max_visible);
    let visible: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into()),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(visible), body_area);
}

fn render_terminal_content(
    f: &mut Frame,
    area: Rect,
    snap: &TaskSnapshot,
    screen: &atman_runtime::tools::term::TerminalScreen,
    _accumulated_bytes: &[u8],
    _done: bool,
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
            format_elapsed(snap.elapsed_ms()),
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
    if body_area.height == 0 || screen.cells.is_empty() {
        return;
    }

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let max_rows = (body_area.height as usize).min(total_rows);
    let start_row = total_rows.saturating_sub(max_rows);
    let bg: Color = t.code_bg.into();
    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for row in start_row..total_rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                spans.push(Span::raw(" "));
                continue;
            }
            let cell = &screen.cells[idx];
            if cell.wide_continuation {
                continue;
            }
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_terminal_screen(
    f: &mut Frame,
    area: Rect,
    title: &str,
    screen: &atman_runtime::tools::term::TerminalScreen,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(" ✓ ", Style::default().fg(t.success.into())),
        Span::styled(title, Style::default().fg(t.tinted_fg.into())),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 || screen.cells.is_empty() {
        return;
    }

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let max_rows = (body_area.height as usize).min(total_rows);
    let start_row = total_rows.saturating_sub(max_rows);
    let bg: Color = t.code_bg.into();
    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for row in start_row..total_rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                spans.push(Span::raw(" "));
                continue;
            }
            let cell = &screen.cells[idx];
            if cell.wide_continuation {
                continue;
            }
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_bash_screen(f: &mut Frame, area: Rect, title: &str, output: &str, done: bool) {
    let t = crate::theme::theme();
    let icon = if done { "✓" } else { "◐" };
    let icon_color = if done { t.success } else { t.accent };
    let header = Line::from(vec![
        Span::styled(format!(" {icon} "), Style::default().fg(icon_color.into())),
        Span::styled(title, Style::default().fg(t.tinted_fg.into())),
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

    let all_lines: Vec<&str> = output.lines().collect();
    let max_visible = body_area.height as usize;
    let start = all_lines.len().saturating_sub(max_visible);
    let visible: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into()),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(visible), body_area);
}

fn render_activity_content(f: &mut Frame, area: Rect, node: &ActivityNode) {
    let t = crate::theme::theme();
    let (kind_icon, kind_color) = crate::task_panel::node_kind_glyph(&node.kind);
    let icon = match node.status {
        ActivityStatus::Running => "◐",
        ActivityStatus::Ok => "✓",
        ActivityStatus::Err => "✗",
        ActivityStatus::Cancelled => "⊘",
    };
    let color: Color = match node.status {
        ActivityStatus::Running => t.accent.into(),
        ActivityStatus::Ok => t.success.into(),
        ActivityStatus::Err => t.error.into(),
        ActivityStatus::Cancelled => t.subtle_fg.into(),
    };
    let elapsed = node
        .ended_at
        .map(|e| e.duration_since(node.started_at).as_millis() as u64)
        .unwrap_or_else(|| node.started_at.elapsed().as_millis() as u64);
    let elapsed_str = format_elapsed(elapsed);

    let header = Line::from(vec![
        Span::styled(format!(" {kind_icon} "), Style::default().fg(kind_color)),
        Span::styled(&node.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(&elapsed_str, Style::default().fg(t.subtle_fg.into())),
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

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("status ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(format!("{:?}", node.status), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("run_id ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(&node.run_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    lines.push(Line::from(vec![
        Span::styled("node_id", Style::default().fg(t.subtle_fg.into())),
        Span::raw(" "),
        Span::styled(&node.node_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_placeholder(f: &mut Frame, area: Rect, title: &str) {
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

fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Killing => "◑",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Killing => t.warn.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

fn format_elapsed(ms: u64) -> String {
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
