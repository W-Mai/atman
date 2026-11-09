//! Shared symbol-first workflow surface, prepared by the existing layout workers.

use super::*;
use crate::workflow::{Action, Presentation, Row, Section};
use atman_runtime::workflow::{NodeStatus, WorkflowNode, WorkflowNodeKind};

pub(super) const TOP_PADDING: usize = 1;
// Header, body gap, footer and the document's trailing separator.
pub(super) const DOCUMENT_CHROME_ROWS: usize = TOP_PADDING * 2 + 4;

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
        let mut style = Style::default().fg(if active { t.accent } else { t.work_meta_fg }.into());
        if active && matches!(action, Action::Section(_)) {
            style = style.bg(t.work_hover_bg.into());
        }
        self.action(
            spans,
            vec![Span::styled(label.to_owned(), style)],
            action.key(),
        );
    }

    fn action(
        &mut self,
        spans: &mut Vec<Span<'static>>,
        mut content: Vec<Span<'static>>,
        key: String,
    ) {
        let Some(first) = content.first_mut() else {
            return;
        };
        first.content = first.content.trim_start().to_owned().into();
        let padding = first.style;
        if let Some(last) = content.last_mut() {
            last.content = last.content.trim_end().to_owned().into();
        }
        let start = crate::width::spans_width(spans.iter()) as u16;
        let available = self.width.saturating_sub(start);
        if available < 3 {
            return;
        }
        // Padding belongs to the painted control and its hit region, including after truncation.
        spans.push(Span::styled(" ", padding));
        spans.extend(crate::width::truncate_spans(
            content,
            (available - 2) as usize,
            None,
        ));
        spans.push(Span::styled(" ", padding));
        self.hit(
            self.lines.len(),
            start,
            crate::width::spans_width(spans.iter()) as u16,
            key,
        );
    }

    fn finish(self, width: u16, inset: u16) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
        let t = crate::theme::theme();
        let background: Color = t.work_bg.into();
        let band = |color: Color| {
            Line::from(Span::styled(
                " ".repeat(width as usize),
                Style::default().bg(color),
            ))
        };
        let mut lines = vec![band(background)];
        for (index, mut line) in self.lines.into_iter().enumerate() {
            if index == 1 {
                lines.push(band(background));
            }
            line.spans.insert(
                0,
                Span::styled(" ".repeat(inset as usize), Style::default().bg(background)),
            );
            for span in &mut line.spans {
                if span.style.bg.is_none_or(|color| color == Color::Reset) {
                    span.style.bg = Some(background);
                }
            }
            pad_spans_to_width(
                &mut line.spans,
                width as usize,
                Style::default().bg(background),
            );
            line.style = line.style.bg(background);
            lines.push(line);
        }
        lines.push(band(background));
        let regions = self
            .regions
            .into_iter()
            .map(|mut region| {
                let shift = TOP_PADDING as u32 + u32::from(region.start_row > 0);
                region.start_row += shift;
                region.end_row += shift;
                region.col_start += inset;
                region.col_end += inset;
                region
            })
            .collect();
        (lines, regions)
    }
}

fn status(node: &WorkflowNode, frame: u32) -> (&'static str, Style) {
    let t = crate::theme::theme();
    let (glyph, color) = match node.status {
        NodeStatus::Running => (spinner_char(frame), t.accent),
        NodeStatus::Pending => ("○", t.work_meta_fg),
        NodeStatus::Ok => ("✓", t.workflow_done_fg),
        NodeStatus::Err => ("×", t.error),
        NodeStatus::Cancelled => ("⊘", t.subtle_fg),
    };
    (glyph, Style::default().fg(color.into()))
}

fn label(node: &WorkflowNode) -> String {
    match &node.kind {
        WorkflowNodeKind::ToolCall {
            tool, call_intent, ..
        } => call_intent
            .as_ref()
            .map(|intent| intent.as_str())
            .unwrap_or(tool)
            .to_owned(),
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::ToolCall { path },
        } => path.clone(),
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("⇉ {branch_index}  {}", node.label)
        }
        _ => node.label.clone(),
    }
}

fn duration(node: &WorkflowNode, precise: bool) -> Option<String> {
    let (Some(start), Some(end)) = (node.started_at, node.ended_at) else {
        return None;
    };
    let ms = (end - start).num_milliseconds().max(0);
    Some(if ms < 1000 {
        format!("{ms}ms")
    } else if precise {
        format!("{:.3}s", ms as f64 / 1000.0)
    } else {
        atman_runtime::humanize::format_secs((end - start).num_seconds().max(0))
    })
}

fn row(
    surface: &mut Surface,
    row: &Row<'_>,
    state: &Presentation,
    frame: u32,
    graph: &WorkflowProjection,
) {
    let t = crate::theme::theme();
    let y = surface.lines.len();
    let mut wire = String::from(" ");
    if row.depth > 0 {
        // Roots have no incoming rail; every displayed ancestor below them owns one.
        for last in row.ancestors.iter().skip(1) {
            wire.push_str(if *last { "  " } else { "│ " });
        }
        wire.push_str(if row.last { "└─" } else { "├─" });
    }
    wire = crate::width::truncate_plain(&wire, surface.width.saturating_sub(12) as usize);
    let grouped = !row.completed.is_empty();
    let branch = !row.node.children.is_empty();
    let closed = if grouped {
        !row.completed_open
    } else {
        state.collapsed.contains(&row.node.id)
    };
    if grouped || branch {
        // Borrow the existing connector column for padding without moving the disclosure glyph.
        wire.pop();
    }
    let mut spans = vec![Span::styled(
        wire,
        Style::default().fg(t.work_boundary_fg.into()),
    )];
    if grouped || branch {
        surface.control(
            &mut spans,
            if closed { "+" } else { "−" },
            if grouped {
                Action::Reveal(row.node.id.clone(), closed)
            } else {
                Action::Branch(row.node.id.clone())
            },
            false,
        );
    } else {
        spans.push(Span::raw("  "));
    }
    let content_start = crate::width::spans_width(spans.iter()) as u16;
    let selected = state
        .selected
        .as_deref()
        .is_some_and(|id| id == row.node.id || row.wrapper.is_some_and(|node| node.id == id));
    let current = row.node.status == NodeStatus::Running
        && !row
            .node
            .children
            .iter()
            .any(|node| node.status == NodeStatus::Running);
    let background = if selected {
        t.work_hover_bg.into()
    } else {
        Color::Reset
    };
    let (glyph, mut style) = status(row.node, frame);
    if row.node.status == NodeStatus::Running && !current {
        style = style.fg(t.work_meta_fg.into());
    }
    spans.push(Span::styled(format!("{glyph} "), style.bg(background)));
    let title = if grouped {
        row.completed.len().to_string()
    } else {
        label(row.node)
    };
    let request = graph.permission_request_for_node(&row.node.id);
    let badge = if grouped {
        None
    } else {
        let approval = match row.node.approval.as_ref() {
            Some(atman_runtime::workflow::ApprovalState::Approved)
                if !request.is_some_and(|request| {
                    request.state == atman_runtime::workflow::WorkflowPermissionState::Approved
                }) =>
            {
                None
            }
            approval => approval,
        };
        approval_badge(
            approval,
            None,
            request.is_some_and(|request| !request.payload.group_ids.is_empty()),
        )
        .map(|(glyph, mut style)| {
            if matches!(
                approval,
                Some(atman_runtime::workflow::ApprovalState::Approved)
            ) {
                style = Style::default().fg(t.work_meta_fg.into());
            }
            (glyph, style)
        })
    };
    let time = if grouped {
        None
    } else {
        duration(row.node, false)
    };
    let time_width = if time.is_some() {
        7.min(surface.width as usize / 3)
    } else {
        0
    };
    let badge_width = badge
        .as_ref()
        .map_or(0, |(glyph, _)| crate::width::width(glyph) + 2);
    let title_end = (surface.width as usize).saturating_sub(time_width + badge_width);
    let used = crate::width::spans_width(spans.iter());
    let title_style = Style::default()
        .fg(if selected {
            t.accent
        } else if current {
            t.work_action_fg
        } else {
            t.work_title_fg
        }
        .into())
        .bg(background);
    let title_style = if current {
        title_style.add_modifier(Modifier::BOLD)
    } else {
        title_style
    };
    spans.push(Span::styled(
        crate::width::truncate(&title, title_end.saturating_sub(used)),
        title_style,
    ));
    pad_spans_to_width(&mut spans, title_end, Style::default().bg(background));
    let badge_start = crate::width::spans_width(spans.iter()) as u16;
    let inspect = if grouped {
        Action::Reveal(row.node.id.clone(), closed)
    } else {
        Action::Inspect(row.node.id.clone(), Section::for_node(row.node))
    };
    if let Some((glyph, style)) = badge {
        spans.push(Span::styled(format!(" {glyph} "), style.bg(background)));
        let action = request
            .filter(|request| {
                request.state.is_pending()
                    && matches!(
                        request.payload.target,
                        atman_runtime::permission_audit::PermissionAuditTarget::User
                    )
            })
            .and_then(|request| request.payload.request_id.as_ref())
            .map(|id| Action::Review(id.to_string()))
            .unwrap_or_else(|| Action::Inspect(row.node.id.clone(), Section::Permission));
        surface.hit(
            y,
            badge_start,
            badge_start.saturating_add(badge_width as u16),
            action.key(),
        );
    }
    if let Some(time) = time {
        spans.push(Span::styled(
            format!(
                "{:>width$}",
                crate::width::truncate_plain(&time, time_width),
                width = time_width
            ),
            Style::default().fg(t.work_meta_fg.into()).bg(background),
        ));
    }
    surface.line(spans);
    surface.hit(
        y,
        content_start,
        if badge_width > 0 {
            badge_start
        } else {
            surface.width
        },
        inspect.key(),
    );
    if badge_width > 0 {
        surface.hit(
            y,
            badge_start.saturating_add(badge_width as u16),
            surface.width,
            inspect.key(),
        );
    }
}

fn body(
    surface: &mut Surface,
    rows: &[Row<'_>],
    state: &Presentation,
    frame: u32,
    graph: &WorkflowProjection,
) {
    let mut index = 0;
    while let Some(value) = rows.get(index) {
        row(surface, value, state, frame, graph);
        index += 1;
        let branches = &value.node.children;
        if !state.lanes
            || surface.width < 100
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
            body(&mut column, &branch_rows, state, frame, graph);
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
    if !title.is_empty() {
        surface.line(Vec::new());
    }
    for line in body.lines() {
        // Recorded previews remain previews; no hidden 20-line truncation is applied.
        for wrapped in
            crate::width::word_wrap(line, surface.width.saturating_sub(2).max(1) as usize)
        {
            surface.line(vec![Span::raw(format!(" {wrapped}"))]);
        }
    }
}

fn field(surface: &mut Surface, name: &str, value: &str) {
    let t = crate::theme::theme();
    let name = match name {
        "approval" => "state",
        "execution" => "mode",
        "provenance" => "origin",
        name => name,
    };
    let label_width = 7.min(surface.width.saturating_sub(4) as usize);
    let prefix = label_width + 2;
    let wrapped = crate::width::word_wrap(
        value,
        (surface.width as usize).saturating_sub(prefix).max(1),
    );
    for (index, line) in wrapped.into_iter().enumerate() {
        surface.line(vec![
            Span::styled(
                format!(
                    " {:<width$}",
                    if index == 0 { name } else { "" },
                    width = label_width
                ),
                Style::default().fg(t.work_meta_fg.into()),
            ),
            Span::raw(" "),
            Span::styled(line, Style::default().fg(t.work_action_fg.into())),
        ]);
    }
}

fn short_id(value: &str, expanded: bool) -> String {
    if expanded || crate::width::width(value) <= 19 {
        return value.into();
    }
    format!(
        "{}…{}",
        value.chars().take(8).collect::<String>(),
        value
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    )
}

fn kind_name(node: &WorkflowNode) -> &'static str {
    use atman_runtime::nodegraph::NodeKind;
    match &node.kind {
        WorkflowNodeKind::Flow { .. } => "Flow",
        WorkflowNodeKind::Subflow { .. } => "Subflow",
        WorkflowNodeKind::ToolCall { .. } => "ToolCall",
        WorkflowNodeKind::FanoutBranch { .. } => "Branch",
        WorkflowNodeKind::Stmt { node_kind } => match node_kind {
            NodeKind::Llm { .. } => "LLM",
            NodeKind::ToolCall { .. } => "Statement",
            NodeKind::Fanout => "Fanout",
            NodeKind::UserConfirm => "UserConfirm",
            NodeKind::FlowFuture { .. } => "Future",
            NodeKind::FlowAwait { .. } => "Await",
            NodeKind::Subflow { .. } => "Subflow",
            NodeKind::Message { .. } => "Message",
            NodeKind::FixUntilTest => "FixUntilTest",
            NodeKind::When { .. } => "When",
            NodeKind::Loop => "Loop",
            NodeKind::Yield => "Yield",
            NodeKind::Return => "Return",
        },
    }
}

fn inspector(
    surface: &mut Surface,
    graph: &WorkflowProjection,
    state: &Presentation,
    selected_visible: bool,
) {
    let Some(id) = state.selected.as_deref() else {
        return;
    };
    let Some(node) = graph.find_node(id) else {
        return;
    };
    let wrapper = crate::workflow::wrapper_for(&graph.root, id);
    let t = crate::theme::theme();
    let mut head = vec![Span::styled(
        format!(
            " {}",
            crate::width::truncate(&label(node), surface.width.saturating_sub(5) as usize)
        ),
        Style::default()
            .fg(t.work_action_fg.into())
            .add_modifier(Modifier::BOLD),
    )];
    pad_spans_to_width(
        &mut head,
        surface.width.saturating_sub(3) as usize,
        Style::default(),
    );
    surface.control(&mut head, " × ", Action::Close, false);
    surface.line(head);
    if !selected_visible {
        surface.line(vec![Span::styled(
            " + folded path",
            Style::default().fg(t.work_meta_fg.into()),
        )]);
    }
    let mut tabs = vec![Span::raw(" ")];
    let request = graph.permission_request_for_node(id);
    for (glyph, section) in [
        ("{ }", Section::Arguments),
        ("↳", Section::Result),
        ("!", Section::Permission),
        ("#", Section::Source),
    ] {
        if section == Section::Permission
            && request.is_none()
            && !matches!(
                node.approval,
                Some(
                    atman_runtime::workflow::ApprovalState::Pending { .. }
                        | atman_runtime::workflow::ApprovalState::Denied { .. }
                )
            )
        {
            continue;
        }
        surface.control(
            &mut tabs,
            glyph,
            Action::Section(section),
            state.section == section,
        );
        tabs.push(Span::raw("   "));
    }
    surface.line(tabs);
    surface.line(vec![Span::styled(
        "─".repeat(surface.width as usize),
        Style::default().fg(t.work_boundary_fg.into()),
    )]);
    surface.line(Vec::new());
    match state.section {
        Section::Arguments => {
            let args = match &node.kind {
                WorkflowNodeKind::ToolCall { args_preview, .. } => Some(args_preview.as_str()),
                WorkflowNodeKind::Stmt {
                    node_kind: atman_runtime::nodegraph::NodeKind::When { condition_preview },
                } => Some(condition_preview.as_str()),
                _ => None,
            };
            text(
                surface,
                "Recorded arguments",
                args.filter(|value| !value.is_empty()).unwrap_or("—"),
            );
        }
        Section::Result => text(
            surface,
            "Recorded result",
            crate::workflow::result_preview(node, wrapper).unwrap_or("—"),
        ),
        Section::Permission => {
            let details = permission_detail_sections(graph.graph(), Some(graph), node);
            if details.is_empty() {
                field(surface, "state", "—");
            }
            for (name, value) in details {
                field(surface, name, &value);
            }
            if let Some(request) = request.filter(|request| {
                request.state.is_pending()
                    && matches!(
                        request.payload.target,
                        atman_runtime::permission_audit::PermissionAuditTarget::User
                    )
            }) {
                if let Some(id) = &request.payload.request_id {
                    surface.line(Vec::new());
                    let mut controls = Vec::new();
                    surface.control(
                        &mut controls,
                        " ! ↗ ",
                        Action::Review(id.to_string()),
                        false,
                    );
                    surface.line(controls);
                }
            }
        }
        Section::Source => {
            field(surface, "kind", kind_name(node));
            let execution = match node.status {
                NodeStatus::Pending => "○ Pending",
                NodeStatus::Running => "◌ Running",
                NodeStatus::Ok => "✓ Completed",
                NodeStatus::Err => "× Failed",
                NodeStatus::Cancelled => "⊘ Cancelled",
            };
            field(surface, "state", execution);
            if let Some(time) = duration(node, true) {
                field(surface, "time", &time);
            }
            match &node.kind {
                WorkflowNodeKind::ToolCall {
                    tool, tool_use_id, ..
                } => {
                    field(surface, "tool", tool);
                    field(
                        surface,
                        "call",
                        &short_id(tool_use_id, state.source_details),
                    );
                }
                WorkflowNodeKind::Flow { run_id, .. }
                | WorkflowNodeKind::Subflow { run_id, .. } => {
                    field(surface, "run", &short_id(run_id, state.source_details))
                }
                WorkflowNodeKind::Stmt { node_kind } => match node_kind {
                    atman_runtime::nodegraph::NodeKind::ToolCall { path } => {
                        field(surface, "tool", path)
                    }
                    atman_runtime::nodegraph::NodeKind::When { condition_preview } => {
                        field(surface, "when", condition_preview)
                    }
                    atman_runtime::nodegraph::NodeKind::Llm { model: Some(model) } => {
                        field(surface, "model", model)
                    }
                    atman_runtime::nodegraph::NodeKind::FlowFuture { name }
                    | atman_runtime::nodegraph::NodeKind::Subflow { name } => {
                        field(surface, "flow", name)
                    }
                    atman_runtime::nodegraph::NodeKind::FlowAwait { target } => {
                        field(surface, "await", target)
                    }
                    atman_runtime::nodegraph::NodeKind::Message { role } => {
                        field(surface, "role", role)
                    }
                    _ => {}
                },
                WorkflowNodeKind::FanoutBranch { branch_index } => {
                    field(surface, "branch", &branch_index.to_string())
                }
            }
            field(surface, "node", &short_id(id, state.source_details));
            if let Some(wrapper) = wrapper {
                field(
                    surface,
                    "parent",
                    &short_id(&wrapper.id, state.source_details),
                );
            }
            if state.source_details {
                if let Some(start) = node.started_at {
                    field(surface, "started", &start.to_rfc3339());
                }
                if let Some(end) = node.ended_at {
                    field(surface, "ended", &end.to_rfc3339());
                }
            }
            if let Some(stats) = node
                .llm_stats
                .as_ref()
                .or_else(|| wrapper.and_then(|node| node.llm_stats.as_ref()))
            {
                surface.line(Vec::new());
                field(surface, "model", &stats.model);
                field(
                    surface,
                    "route",
                    &format!(
                        "{} · {:?} / {:?}",
                        stats.provider, stats.context_call_purpose, stats.context_call_scope
                    ),
                );
                field(
                    surface,
                    "tokens",
                    &format!(
                        "↑{}  ↓{}",
                        stats
                            .input_tokens
                            .saturating_add(stats.cache_read)
                            .saturating_add(stats.cache_write),
                        stats.output_tokens
                    ),
                );
                field(
                    surface,
                    "cache",
                    &format!("read {} / write {}", stats.cache_read, stats.cache_write),
                );
                field(surface, "TTFT", &format!("{}ms", stats.ttft_ms));
                field(
                    surface,
                    "speed",
                    &format!("{:.1} tok/s", stats.tokens_per_second),
                );
                field(surface, "wall", &format!("{}ms", stats.wallclock_ms));
            }
            surface.line(Vec::new());
            let mut controls = Vec::new();
            surface.control(
                &mut controls,
                if state.source_details {
                    " − # "
                } else {
                    " + # "
                },
                Action::SourceDetails,
                state.source_details,
            );
            surface.line(controls);
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
    hovered: Option<&str>,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let outer_width = width;
    let inset = 2.min(width.saturating_sub(1) / 2);
    let width = width.saturating_sub(inset * 2);
    let mut surface = Surface::new(width);
    if width == 0 {
        return (surface.lines, surface.regions);
    }
    let t = crate::theme::theme();
    let summary = graph.summary();
    let running = !cancelled && summary.status() == WorkflowAggregateStatus::Running;
    let measurements = MetricSnapshot::capture(graph);
    let animated_metrics = frame == LAYOUT_ANIMATION_FRAME && running;
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
    // Keep elapsed immediately after its title for semantic paint lookup.
    head.push(Span::styled(
        format!("{glyph} "),
        Style::default().fg(if running { t.accent } else { t.work_meta_fg }.into()),
    ));
    if width >= 56 {
        head.push(Span::styled(
            "workflow  ",
            Style::default()
                .fg(t.work_action_fg.into())
                .add_modifier(Modifier::BOLD),
        ));
    }
    if width >= 24 {
        head.push(Span::styled(
            if animated_metrics && summary.started_at().is_some() {
                metrics::CLOCK_MARKER.to_string().repeat(7)
            } else {
                format!(
                    "{:>7}",
                    atman_runtime::humanize::format_secs(summary.elapsed_secs(chrono::Utc::now()))
                )
            },
            Style::default().fg(t.work_meta_fg.into()),
        ));
    }
    let used = crate::width::spans_width(head.iter());
    let budget = (width as usize).saturating_sub(used + 16);
    let values = metrics::spans(measurements, animated_metrics, budget);
    if !values.is_empty() {
        head.push(Span::raw("  "));
        surface.action(&mut head, values, Action::Metrics.key());
    }
    pad_spans_to_width(
        &mut head,
        width.saturating_sub(12) as usize,
        Style::default(),
    );
    surface.control(&mut head, " ∑ ", Action::Metrics, state.metrics);
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
    surface.action(
        &mut head,
        vec![Span::styled(
            "↗",
            Style::default().fg(t.work_meta_fg.into()),
        )],
        COLLAPSED_CARD_FULLSCREEN_KEY.into(),
    );
    surface.line(head);
    if state.hidden {
        highlight(&mut surface, hovered);
        return surface.finish(outer_width, inset);
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
    let inspector_width = (width / 3).clamp(30, 42);
    surface.width = if split {
        width.saturating_sub(inspector_width + 3)
    } else {
        width
    };
    let divider = surface.width;
    let row_frame = if running { frame } else { 0 };
    if visible == rows.len() {
        body(&mut surface, &rows, state, row_frame, graph);
    } else {
        for (i, value) in rows.iter().enumerate() {
            if i == 0 || i >= start {
                row(&mut surface, value, state, row_frame, graph);
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
    let mut detail = Surface::new(if split { inspector_width } else { width });
    let selected_visible = state.selected.as_deref().is_some_and(|id| {
        rows.iter()
            .any(|row| row.node.id == id || row.wrapper.is_some_and(|wrapper| wrapper.id == id))
    });
    inspector(&mut detail, graph, state, selected_visible);
    if split && !detail.lines.is_empty() {
        let offset = divider + 3;
        let rows = surface.lines.len().max(detail.lines.len() + 1);
        surface.lines.resize_with(rows, Line::default);
        for (i, line) in detail.lines.into_iter().enumerate() {
            let left = &mut surface.lines[i + 1];
            left.spans = crate::width::truncate_spans(
                std::mem::take(&mut left.spans),
                divider as usize,
                None,
            );
            pad_spans_to_width(&mut left.spans, divider as usize, Style::default());
            left.spans.push(Span::styled(
                " │ ",
                Style::default().fg(t.work_boundary_fg.into()),
            ));
            left.spans.extend(line.spans);
        }
        for region in &mut surface.regions {
            if region.start_row > 0 {
                region.col_end = region.col_end.min(divider);
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
    surface.action(
        &mut footer,
        metrics::spans(
            measurements,
            animated_metrics,
            width.saturating_sub(2) as usize,
        ),
        Action::Metrics.key(),
    );
    if let Some(key) = hovered {
        let hint = Action::parse(key)
            .map(|action| action.hint())
            .or_else(|| (key == COLLAPSED_CARD_FULLSCREEN_KEY).then_some("Open read window"));
        if let Some(hint) = hint {
            // The existing footer keeps its geometry while revealing control detail.
            if key != Action::Metrics.key() {
                footer.clear();
            } else if !footer.is_empty() {
                footer.push(Span::raw("  "));
            }
            footer.push(Span::styled(
                hint,
                Style::default().fg(t.work_meta_fg.into()),
            ));
        }
    }
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
        let edits = graph.applied_edits();
        if edits.applied_edits > 0 {
            text(
                &mut surface,
                "Applied",
                &format!(
                    "+{} −{} lines · {} files · {} edits",
                    edits.insertions, edits.deletions, edits.files, edits.applied_edits
                ),
            );
        }
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
                    "{} / {} · {:?} / {:?}\n{} calls  ↑{}  ↓{}  cache write {} / read {}  TTFT {}ms  {:.1} tok/s",
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
    highlight(&mut surface, hovered);
    surface.finish(outer_width, inset)
}

fn highlight(surface: &mut Surface, hovered: Option<&str>) {
    let Some(key) = hovered else {
        return;
    };
    for region in surface
        .regions
        .iter()
        .filter(|region| region.path_key == key)
    {
        let Some(line) = surface.lines.get_mut(region.start_row as usize) else {
            continue;
        };
        let mut column = 0;
        for span in &mut line.spans {
            let end = column + crate::width::width(&span.content) as u16;
            if end > region.col_start && column < region.col_end {
                span.style = span.style.bg(crate::theme::theme().work_hover_bg.into());
            }
            column = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_rails_continue_until_the_next_visible_sibling() {
        fn node(label: &str, children: Vec<WorkflowNode>) -> WorkflowNode {
            WorkflowNode {
                id: label.into(),
                label: label.into(),
                kind: WorkflowNodeKind::Stmt {
                    node_kind: atman_runtime::nodegraph::NodeKind::Loop,
                },
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children,
                parallelism: atman_runtime::workflow::Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }
        }
        for root_sibling in [false, true] {
            let mut graph = atman_runtime::workflow::WorkflowGraph::new(atman_rt::TurnId::now());
            graph.root.push(node(
                "loop",
                vec![
                    node(
                        "iteration 0",
                        vec![
                            node(
                                "nested",
                                vec![node("first leaf", vec![]), node("last leaf", vec![])],
                            ),
                            node("after branch", vec![]),
                        ],
                    ),
                    node("iteration 1", vec![node("terminal leaf", vec![])]),
                ],
            ));
            if root_sibling {
                graph.root.push(node("other root", vec![]));
            }
            let graph = WorkflowProjection::from(graph);
            for structure in [false, true] {
                for width in [60, 120] {
                    let state = Presentation {
                        structure,
                        ..Default::default()
                    };
                    let (lines, _) = render(&graph, &state, true, false, 0, width, 100, 0, None);
                    let rows: Vec<String> = lines
                        .iter()
                        .map(|line| {
                            line.spans
                                .iter()
                                .map(|span| span.content.as_ref())
                                .collect()
                        })
                        .collect();
                    let index =
                        |label: &str| rows.iter().position(|row| row.contains(label)).unwrap();
                    let stem = |label: &str| {
                        let row = &rows[index(label)];
                        crate::width::width(&row[..row.rfind(['├', '└']).unwrap()])
                    };
                    let iteration = stem("iteration 0");
                    for row in &rows[index("iteration 0") + 1..index("iteration 1")] {
                        assert_eq!(row.chars().nth(iteration), Some('│'), "{row}");
                    }
                    let nested = stem("nested");
                    for row in &rows[index("nested") + 1..index("after branch")] {
                        assert_eq!(row.chars().nth(nested), Some('│'), "{row}");
                    }
                    assert_eq!(rows[index("iteration 1")].chars().nth(iteration), Some('└'));
                    assert_eq!(
                        rows[index("terminal leaf")].chars().nth(iteration),
                        Some(' ')
                    );
                }
            }
        }
    }

    #[test]
    fn inspector_separates_typed_source_identity_from_argument_payload() {
        let run = "00000000-0000-7000-8000-000000000001";
        let call = "00000000-0000-7000-8000-000000000002";
        let id = format!("tool:{run}:{call}");
        let mut graph = WorkflowProjection::new(atman_rt::TurnId::now());
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::FlowStart {
            run_id: run.into(),
            flow_name: "agent".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::FlowNodeStart {
            run_id: run.into(),
            node_id: "statement".into(),
            parent_node_id: None,
            kind: atman_runtime::nodegraph::NodeKind::ToolCall {
                path: "fs.read".into(),
            },
            label: "read".into(),
        });
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::ToolNode {
            run_id: run.into(),
            parent_node_id: "statement".into(),
            tool_use_id: call.into(),
            tool: "fs.read".into(),
            args_preview: "argument-payload-marker".into(),
            call_intent: None,
        });
        let mut state = Presentation {
            selected: Some(id.clone()),
            section: Section::Source,
            ..Default::default()
        };
        let mut view = Surface::new(35);
        inspector(&mut view, &graph, &state, true);
        assert!(
            view.lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| !span.style.add_modifier.contains(Modifier::UNDERLINED))
        );
        let selected = view
            .regions
            .iter()
            .find(|region| region.path_key == Action::Section(Section::Source).key())
            .unwrap();
        let mut column = 0;
        for span in &view.lines[selected.start_row as usize].spans {
            let end = column + crate::width::width(&span.content) as u16;
            if end > selected.col_start && column < selected.col_end {
                assert_eq!(
                    span.style.bg,
                    Some(crate::theme::theme().work_hover_bg.into())
                );
            }
            column = end;
        }
        let text = view
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            !text.contains("ToolCall {")
                && !text.contains("args_preview")
                && !text.contains("argument-payload-marker")
        );
        assert!(!text.contains(call));
        state.apply(Action::SourceDetails, None);
        let mut details = Surface::new(100);
        inspector(&mut details, &graph, &state, true);
        let text = details
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains(&id) && text.contains(call));
        assert!(
            view.lines
                .iter()
                .all(|line| crate::width::spans_width(line.spans.iter()) <= 35)
        );
    }

    #[test]
    fn workflow_elapsed_paint_cannot_move_control_columns() {
        let mut graph = WorkflowProjection::new(atman_rt::TurnId::now());
        graph.apply_stream_frame(&atman_runtime::stream::StreamFrame::FlowStart {
            run_id: "run".into(),
            flow_name: "flow".into(),
            parent_run_id: None,
            parent_node_id: None,
        });
        let (mut lines, _) = render(
            &graph,
            &Presentation::default(),
            false,
            false,
            LAYOUT_ANIMATION_FRAME,
            80,
            19,
            0,
            None,
        );
        let mut paint = workflow_dynamic_paint(&graph, false, &lines, 0);
        let header = paint.elapsed.as_ref().expect("live header clock").line;
        let columns = lines[header]
            .spans
            .iter()
            .map(|span| crate::width::width(&span.content))
            .collect::<Vec<_>>();
        for seconds in [9, 10, 59, 60, 3599, 3600, 86400] {
            paint
                .elapsed
                .as_mut()
                .expect("live header clock")
                .started_at = chrono::Utc::now() - chrono::Duration::seconds(seconds);
            patch_animation_lines(&mut lines, &paint, 1, 0);
            assert_eq!(
                columns,
                lines[header]
                    .spans
                    .iter()
                    .map(|span| crate::width::width(&span.content))
                    .collect::<Vec<_>>()
            );
        }
    }

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
        for width in [1, 8, 20, 34, 64, 100, 160, 240] {
            for section in [
                Section::Source,
                Section::Arguments,
                Section::Permission,
                Section::Result,
            ] {
                let state = Presentation {
                    selected: Some(graph.root[0].children[0].id.clone()),
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
                    None,
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
                    let fullscreen = regions
                        .iter()
                        .find(|region| region.path_key == COLLAPSED_CARD_FULLSCREEN_KEY)
                        .unwrap();
                    assert_eq!(fullscreen.col_end, width - 2);
                    let parent = regions
                        .iter()
                        .find(|region| region.path_key == Action::Branch("root".into()).key())
                        .unwrap();
                    let child = regions.iter().find(|region| matches!(Action::parse(&region.path_key), Some(Action::Inspect(id, _)) if id == graph.root[0].children[0].id)).unwrap();
                    let plain = |region: &NodeRegion| {
                        lines[region.start_row as usize]
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>()
                    };
                    assert_eq!(
                        plain(parent).chars().position(|ch| ch == '−'),
                        plain(child).chars().position(|ch| ch == '├' || ch == '└'),
                        "disclosure padding must preserve parent/child tree alignment"
                    );
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
