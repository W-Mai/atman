//! Shared symbol-first workflow surface, prepared by the existing layout workers.

use super::*;
use crate::workflow::{Action, Presentation, Row, Section};
use atman_runtime::workflow::{NodeStatus, WorkflowNode, WorkflowNodeKind};

struct Surface {
    lines: Vec<Line<'static>>,
    regions: Vec<NodeRegion>,
    width: u16,
}

impl Surface {
    fn new(width: u16) -> Self {
        Self {
            lines: Vec::new(),
            regions: Vec::new(),
            width,
        }
    }

    fn line(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(crate::width::truncate_spans(
            spans,
            self.width as usize,
            None,
        )));
    }

    fn hit(&mut self, row: usize, start: u16, end: u16, key: String) {
        if start < end.min(self.width) {
            self.regions.push(NodeRegion {
                panel_item_index: 0,
                path_key: key,
                start_row: row as u32,
                end_row: row as u32 + 1,
                col_start: start,
                col_end: end.min(self.width),
            });
        }
    }

    fn control(
        &mut self,
        spans: &mut Vec<Span<'static>>,
        label: &str,
        action: Action,
        active: bool,
    ) {
        let t = crate::theme::theme();
        let start = spans
            .iter()
            .map(|span| crate::width::width(&span.content))
            .sum::<usize>() as u16;
        let style = Style::default().fg(if active { t.accent } else { t.work_meta_fg }.into());
        spans.push(Span::styled(label.to_owned(), style));
        self.hit(
            self.lines.len(),
            start,
            start + crate::width::width(label) as u16,
            action.key(),
        );
    }
}

fn status(node: &WorkflowNode, frame: u32) -> (&'static str, Style) {
    let t = crate::theme::theme();
    let (glyph, color) = match node.status {
        NodeStatus::Running => (spinner_char(frame), t.accent),
        NodeStatus::Pending => ("○", t.work_meta_fg),
        NodeStatus::Ok => ("✓", t.success),
        NodeStatus::Err => ("×", t.error),
        NodeStatus::Cancelled => ("⊘", t.subtle_fg),
    };
    (glyph, Style::default().fg(color.into()))
}

fn label(node: &WorkflowNode) -> String {
    match &node.kind {
        WorkflowNodeKind::ToolCall {
            tool,
            args_preview,
            call_intent,
            ..
        } => workflow_tool_label(tool, args_preview, call_intent.as_ref(), 64, false),
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("⇉ {branch_index}  {}", node.label)
        }
        _ => node.label.clone(),
    }
}

fn row(surface: &mut Surface, row: &Row<'_>, state: &Presentation, frame: u32) {
    let t = crate::theme::theme();
    let y = surface.lines.len();
    let indent = (row.depth.saturating_mul(2)).min(surface.width.saturating_sub(10) as usize);
    let mut spans = vec![Span::styled(
        format!(" {}", "│ ".repeat(indent / 2)),
        Style::default().fg(t.work_boundary_fg.into()),
    )];
    let branch = !row.node.children.is_empty();
    let x = 1 + indent as u16;
    let marker = if !row.completed.is_empty() {
        if state.revealed.contains(&row.node.id) {
            "−"
        } else {
            "+"
        }
    } else if branch {
        if state.collapsed.contains(&row.node.id) {
            "+"
        } else {
            "−"
        }
    } else {
        " "
    };
    spans.push(Span::styled(
        format!("{marker} "),
        Style::default().fg(t.work_meta_fg.into()),
    ));
    if !row.completed.is_empty() || branch {
        surface.hit(
            y,
            x,
            x + 2,
            if !row.completed.is_empty() {
                Action::Reveal(row.node.id.clone())
            } else {
                Action::Branch(row.node.id.clone())
            }
            .key(),
        );
    }
    let (glyph, style) = status(row.node, frame);
    spans.push(Span::styled(format!("{glyph} "), style));
    let selected = state
        .selected
        .as_deref()
        .is_some_and(|id| id == row.node.id || row.wrapper.is_some_and(|node| node.id == id));
    let mut style = Style::default().fg(if selected { t.accent } else { t.work_title_fg }.into());
    if selected {
        style = style
            .bg(t.work_hover_bg.into())
            .add_modifier(Modifier::BOLD);
    }
    let title = if row.completed.is_empty() {
        label(row.node)
    } else {
        format!("{} ✓", row.completed.len())
    };
    let badge = approval_badge(row.node.approval.as_ref(), None, false);
    let badge_width = badge
        .as_ref()
        .map_or(0, |(text, _)| crate::width::width(text) + 2);
    let duration = match (row.node.started_at, row.node.ended_at) {
        (Some(start), Some(end)) => format!(
            "  {}",
            atman_runtime::humanize::format_secs((end - start).num_seconds().max(0))
        ),
        _ => String::new(),
    };
    let title_width = (surface.width as usize)
        .saturating_sub(indent + 6 + badge_width + crate::width::width(&duration));
    spans.push(Span::styled(
        crate::width::truncate(&title, title_width),
        style,
    ));
    if let Some((badge, style)) = badge {
        spans.push(Span::styled(format!("  {badge}"), style));
    }
    spans.push(Span::styled(
        duration,
        Style::default().fg(t.work_meta_fg.into()),
    ));
    surface.line(spans);
    // Label and branch controls have disjoint hitboxes, including in narrow windows.
    surface.hit(
        y,
        x + 2,
        surface.width,
        if row.completed.is_empty() {
            Action::Inspect(row.node.id.clone(), Section::for_node(row.node))
        } else {
            Action::Reveal(row.node.id.clone())
        }
        .key(),
    );
}

fn body(surface: &mut Surface, rows: &[Row<'_>], state: &Presentation, frame: u32) {
    let mut index = 0;
    while let Some(value) = rows.get(index) {
        row(surface, value, state, frame);
        index += 1;
        let branches = &value.node.children;
        if !state.lanes
            || state.collapsed.contains(&value.node.id)
            || !is_fanout_group(value.node)
            || !(2..=4).contains(&branches.len())
            || surface.width / (branches.len() as u16) < 24
        {
            continue;
        }
        let column_width = surface.width / branches.len() as u16;
        let mut columns = Vec::new();
        for branch in branches {
            let mut column = Surface::new(column_width.saturating_sub(1));
            let branch_rows = crate::workflow::rows(std::slice::from_ref(branch), state);
            body(&mut column, &branch_rows, state, frame);
            columns.push(column);
        }
        let offset = surface.lines.len();
        let height = columns
            .iter()
            .map(|column| column.lines.len())
            .max()
            .unwrap_or(0);
        let t = crate::theme::theme();
        for y in 0..height {
            let mut spans = Vec::new();
            for column in &columns {
                let mut cell = column
                    .lines
                    .get(y)
                    .map(|line| line.spans.clone())
                    .unwrap_or_default();
                pad_spans_to_width(
                    &mut cell,
                    column_width.saturating_sub(1) as usize,
                    Style::default(),
                );
                spans.extend(cell);
                spans.push(Span::styled(
                    "│",
                    Style::default().fg(t.work_boundary_fg.into()),
                ));
            }
            surface.line(spans);
        }
        for (column, content) in columns.into_iter().enumerate() {
            for mut region in content.regions {
                region.start_row += offset as u32;
                region.end_row += offset as u32;
                region.col_start += column as u16 * column_width;
                region.col_end += column as u16 * column_width;
                surface.regions.push(region);
            }
        }
        // The source fanout owns its join. A launched future does not get one.
        if matches!(
            value.node.kind,
            WorkflowNodeKind::Stmt {
                node_kind: atman_runtime::nodegraph::NodeKind::Fanout
            }
        ) {
            surface.line(vec![Span::styled(
                " └─┴─",
                Style::default().fg(t.work_boundary_fg.into()),
            )]);
        }
        while rows.get(index).is_some_and(|row| row.depth > value.depth) {
            index += 1;
        }
    }
}

fn text(surface: &mut Surface, title: &str, body: &str) {
    let t = crate::theme::theme();
    surface.line(vec![Span::styled(
        format!(" {title}"),
        Style::default().fg(t.work_meta_fg.into()),
    )]);
    for line in body.lines() {
        // Recorded previews remain previews; no hidden 20-line truncation is applied.
        for wrapped in
            crate::width::word_wrap(line, surface.width.saturating_sub(2).max(1) as usize)
        {
            surface.line(vec![Span::raw(format!(" {wrapped}"))]);
        }
    }
}

fn inspector(surface: &mut Surface, graph: &WorkflowProjection, state: &Presentation) {
    let Some(id) = state.selected.as_deref() else {
        return;
    };
    let Some(node) = graph.find_node(id) else {
        return;
    };
    let t = crate::theme::theme();
    let mut head = Vec::new();
    surface.control(&mut head, " × ", Action::Close, false);
    head.push(Span::styled(
        label(node),
        Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD),
    ));
    surface.line(head);
    let mut tabs = Vec::new();
    for (label, section) in [
        (" # ", Section::Source),
        (" {…} ", Section::Arguments),
        (" = ", Section::Result),
        (" ◷ ", Section::Permission),
    ] {
        surface.control(
            &mut tabs,
            label,
            Action::Section(section),
            state.section == section,
        );
    }
    surface.line(tabs);
    match state.section {
        Section::Arguments => {
            if let WorkflowNodeKind::ToolCall { args_preview, .. } = &node.kind {
                text(surface, "Arguments · recorded preview", args_preview);
            } else {
                text(surface, "Arguments", "—");
            }
        }
        Section::Result => {
            let result = match &node.kind {
                WorkflowNodeKind::ToolCall { result_preview, .. } => {
                    result_preview.as_deref().or(node.output_preview.as_deref())
                }
                _ => node.output_preview.as_deref(),
            };
            text(surface, "Result · recorded preview", result.unwrap_or("—"));
        }
        Section::Permission => {
            let details = permission_detail_sections(graph.graph(), Some(graph), node);
            if details.is_empty() {
                text(surface, "Permission", "—");
            }
            for (label, body) in details {
                text(surface, label, &body);
            }
            if permission_request_for_node(graph.graph(), Some(graph), node)
                .is_some_and(|request| request.state.is_pending())
            {
                text(
                    surface,
                    "",
                    "◷ Review the current request below the document",
                );
            }
        }
        Section::Source => {
            text(surface, "Source", &node.id);
            text(surface, "Kind", &format!("{:?}", node.kind));
            text(surface, "Execution", &format!("{:?}", node.status));
            if state.collapsed.contains(id) {
                text(surface, "", "+ descendants folded");
            }
            if let Some(stats) = &node.llm_stats {
                text(
                    surface,
                    "Model",
                    &format!(
                        "{} / {} · {:?} / {:?}\n↑{}  ↓{}  cache +{} / {}  TTFT {}ms  {:.1} tok/s  {}ms",
                        stats.provider,
                        stats.model,
                        stats.context_call_purpose,
                        stats.context_call_scope,
                        stats
                            .input_tokens
                            .saturating_add(stats.cache_read)
                            .saturating_add(stats.cache_write),
                        stats.output_tokens,
                        stats.cache_write,
                        stats.cache_read,
                        stats.ttft_ms,
                        stats.tokens_per_second,
                        stats.wallclock_ms
                    ),
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    graph: &WorkflowProjection,
    state: &Presentation,
    full: bool,
    cancelled: bool,
    frame: u32,
    width: u16,
    limit: usize,
    min_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let mut surface = Surface::new(width);
    if width == 0 {
        return (surface.lines, surface.regions);
    }
    let t = crate::theme::theme();
    let summary = graph.summary();
    let running = !cancelled && summary.status() == WorkflowAggregateStatus::Running;
    let glyph = if cancelled {
        "⊘"
    } else if running {
        spinner_char(frame)
    } else if summary.status() == WorkflowAggregateStatus::Error {
        "×"
    } else {
        "✓"
    };
    let mut head = Vec::new();
    surface.control(
        &mut head,
        if state.hidden { " + " } else { " − " },
        Action::Content,
        !state.hidden,
    );
    // Keep dynamic elapsed in its own semantic span for the existing paint patch.
    head.push(Span::styled(
        format!("{glyph} "),
        Style::default().fg(if running { t.accent } else { t.work_meta_fg }.into()),
    ));
    if width >= 34 {
        head.push(Span::styled(
            "workflow  ",
            Style::default()
                .fg(t.work_title_fg.into())
                .add_modifier(Modifier::BOLD),
        ));
        head.push(Span::styled(
            atman_runtime::humanize::format_secs(summary.elapsed_secs(chrono::Utc::now())),
            Style::default().fg(t.work_meta_fg.into()),
        ));
    }
    surface.control(
        &mut head,
        if state.structure { " ┆ " } else { " ≡ " },
        Action::Structure,
        state.structure,
    );
    surface.control(
        &mut head,
        if state.lanes { " ╂ " } else { " ⇄ " },
        Action::Layout,
        state.lanes,
    );
    let window_x = head
        .iter()
        .map(|span| crate::width::width(&span.content))
        .sum::<usize>() as u16;
    head.push(Span::styled(
        " ↗ ",
        Style::default().fg(t.work_meta_fg.into()),
    ));
    surface.hit(
        0,
        window_x,
        window_x + 3,
        COLLAPSED_CARD_FULLSCREEN_KEY.into(),
    );
    surface.line(head);
    if state.hidden {
        return (surface.lines, surface.regions);
    }
    let rows = crate::workflow::rows(&graph.root, state);
    let visible = if full || state.structure || state.selected.is_some() {
        rows.len()
    } else {
        rows.len().min(limit)
    };
    // Retain the root and most recent context in the bounded document overview.
    let start = rows.len().saturating_sub(visible.saturating_sub(1));
    let split = width >= 100
        && state
            .selected
            .as_deref()
            .is_some_and(|id| graph.find_node(id).is_some());
    surface.width = if split { width / 2 - 1 } else { width };
    let row_frame = if running { frame } else { 0 };
    if visible == rows.len() {
        body(&mut surface, &rows, state, row_frame);
    } else {
        for (i, value) in rows.iter().enumerate() {
            if i == 0 || i >= start {
                row(&mut surface, value, state, row_frame);
            }
        }
    }
    if visible < rows.len() {
        let row = surface.lines.len();
        surface.line(vec![Span::styled(
            format!(" ⋯ {}", rows.len() - visible),
            Style::default().fg(t.work_meta_fg.into()),
        )]);
        surface.hit(row, 0, width, String::new());
    }
    while surface.lines.len() < min_rows.saturating_add(1) {
        surface.line(Vec::new());
    }
    let mut detail = Surface::new(if width >= 100 { width / 2 } else { width });
    inspector(&mut detail, graph, state);
    if split && !detail.lines.is_empty() {
        let offset = width / 2;
        let rows = surface.lines.len().max(detail.lines.len() + 1);
        surface.lines.resize_with(rows, Line::default);
        for (i, line) in detail.lines.into_iter().enumerate() {
            let left = &mut surface.lines[i + 1];
            left.spans = crate::width::truncate_spans(
                std::mem::take(&mut left.spans),
                offset.saturating_sub(1) as usize,
                None,
            );
            pad_spans_to_width(
                &mut left.spans,
                offset.saturating_sub(1) as usize,
                Style::default(),
            );
            left.spans.push(Span::styled(
                "│",
                Style::default().fg(t.work_boundary_fg.into()),
            ));
            left.spans.extend(line.spans);
        }
        for region in &mut surface.regions {
            if region.start_row > 0 {
                region.col_end = region.col_end.min(offset.saturating_sub(1));
            }
        }
        for mut region in detail.regions {
            region.start_row += 1;
            region.end_row += 1;
            region.col_start += offset;
            region.col_end += offset;
            surface.regions.push(region);
        }
    } else {
        let offset = surface.lines.len() as u32;
        surface.lines.extend(detail.lines);
        for mut region in detail.regions {
            region.start_row += offset;
            region.end_row += offset;
            surface.regions.push(region);
        }
    }
    surface.width = width;
    let mut footer = Vec::new();
    surface.control(&mut footer, " ∑ ", Action::Metrics, state.metrics);
    surface.line(footer);
    if state.metrics {
        let counts = summary.counts();
        text(
            &mut surface,
            "Metrics",
            &format!(
                "{} nodes  {} tools  {} agents  {} edit calls",
                counts.nodes, counts.tools, counts.agents, counts.edits
            ),
        );
        let mut routes = summary.llm_routes().iter().collect::<Vec<_>>();
        routes.sort_by_key(|(route, _)| {
            (
                &route.provider,
                &route.model,
                format!("{:?}/{:?}", route.purpose, route.scope),
            )
        });
        for (route, stats) in routes {
            text(
                &mut surface,
                if route.is_primary() {
                    "Primary"
                } else {
                    "Auxiliary"
                },
                &format!(
                    "{} / {} · {:?} / {:?}\n{} calls  ↑{}  ↓{}  cache +{} / {}  TTFT {}ms  {:.1} tok/s",
                    route.provider,
                    route.model,
                    route.purpose,
                    route.scope,
                    stats.calls,
                    stats.total_in,
                    stats.total_out,
                    stats.cache_write,
                    stats.cache_read,
                    stats
                        .total_ttft_ms
                        .checked_div(stats.calls as u64)
                        .unwrap_or(0),
                    stats.average_speed()
                ),
            );
        }
    }
    (surface.lines, surface.regions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_geometry_uses_display_columns_and_separate_controls() {
        let mut graph = WorkflowProjection::new(atman_rt::TurnId::now());
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::FlowStart {
            run_id: "root".into(),
            flow_name: "流程 🇨🇳".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::FlowNodeStart {
            run_id: "root".into(),
            node_id: "step".into(),
            parent_node_id: None,
            kind: atman_runtime::nodegraph::NodeKind::Loop,
            label: "Loop".into(),
        });
        for width in [1, 8, 20, 34, 64, 100, 160] {
            for section in [
                Section::Source,
                Section::Arguments,
                Section::Permission,
                Section::Result,
            ] {
                let state = Presentation {
                    selected: Some("root:step".into()),
                    section,
                    ..Default::default()
                };
                let (lines, regions) = render(
                    &graph,
                    &state,
                    true,
                    false,
                    LAYOUT_ANIMATION_FRAME,
                    width,
                    19,
                    0,
                );
                assert!(
                    lines
                        .iter()
                        .all(|line| crate::width::spans_width(line.spans.iter()) <= width as usize)
                );
                for region in &regions {
                    assert!(region.col_start < region.col_end && region.col_end <= width);
                    assert!(
                        region.start_row < region.end_row && region.end_row <= lines.len() as u32
                    );
                }
                let keys = regions
                    .iter()
                    .map(|region| region.path_key.as_str())
                    .collect::<Vec<_>>();
                if width >= 34 {
                    assert!(keys.contains(&"workflow:content"));
                    assert!(keys.contains(&"workflow:structure"));
                    assert!(keys.contains(&"workflow:layout"));
                }
                for (i, a) in regions.iter().enumerate() {
                    for b in &regions[i + 1..] {
                        assert!(
                            a.start_row != b.start_row
                                || a.col_end <= b.col_start
                                || b.col_end <= a.col_start,
                            "overlapping actions at {width} columns"
                        );
                    }
                }
            }
        }
    }
}
