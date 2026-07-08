use atman_runtime::message::Message;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{NoteLevel, OutputItem};

const RESET: Style = Style::new();

pub struct RenderCtx<'a> {
    pub expanded_tools: &'a std::collections::HashSet<String>,
    pub messages: &'a [Message],
    pub animation_frame: u32,
    pub panel_width: u16,
}

impl<'a> RenderCtx<'a> {
    pub fn empty() -> RenderCtx<'a> {
        static EMPTY_SET: std::sync::OnceLock<std::collections::HashSet<String>> =
            std::sync::OnceLock::new();
        RenderCtx {
            expanded_tools: EMPTY_SET.get_or_init(std::collections::HashSet::new),
            messages: &[],
            animation_frame: 0,
            panel_width: 80,
        }
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_char(frame: u32) -> &'static str {
    SPINNER[(frame as usize) % SPINNER.len()]
}

pub fn build_lines(items: &[OutputItem], ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(items.len() * 3);
    for item in items {
        out.extend(render_item(item, ctx));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemRange {
    pub item_index: usize,
    pub start_row: u16,
    pub end_row: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRegion {
    pub panel_item_index: usize,
    pub path_key: String,
    pub start_row: u16,
    pub end_row: u16,
    pub col_start: Option<u16>,
    pub col_end: Option<u16>,
}

pub fn build_lines_with_ranges(
    items: &[OutputItem],
    width: u16,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u16) {
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut node_regions: Vec<NodeRegion> = Vec::new();
    let mut cursor: u16 = 0;
    for (idx, item) in items.iter().enumerate() {
        let (item_lines, mut item_regions) = render_item_with_regions(item, ctx);
        let (rows, line_row_offsets) = wrap_row_offsets(&item_lines, width);
        ranges.push(ItemRange {
            item_index: idx,
            start_row: cursor,
            end_row: cursor.saturating_add(rows),
        });
        for r in item_regions.iter_mut() {
            r.panel_item_index = idx;
            let s = r.start_row as usize;
            let e = r.end_row as usize;
            let wrapped_start = line_row_offsets.get(s).copied().unwrap_or(rows);
            let wrapped_end = line_row_offsets.get(e).copied().unwrap_or(rows);
            r.start_row = cursor.saturating_add(wrapped_start);
            r.end_row = cursor.saturating_add(wrapped_end);
        }
        node_regions.extend(item_regions);
        cursor = cursor.saturating_add(rows);
        all_lines.extend(item_lines);
    }
    (all_lines, ranges, node_regions, cursor)
}

fn wrap_row_offsets(lines: &[Line<'static>], width: u16) -> (u16, Vec<u16>) {
    let mut offsets: Vec<u16> = Vec::with_capacity(lines.len() + 1);
    let mut cursor: u16 = 0;
    offsets.push(0);
    if width == 0 {
        for _ in lines {
            cursor = cursor.saturating_add(1);
            offsets.push(cursor);
        }
        return (cursor, offsets);
    }
    for line in lines {
        let single = vec![line.clone()];
        let p = Paragraph::new(single).wrap(Wrap { trim: false });
        let rows = p.line_count(width) as u16;
        cursor = cursor.saturating_add(rows.max(1));
        offsets.push(cursor);
    }
    (cursor, offsets)
}

pub fn render_item_with_regions(
    item: &OutputItem,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    if let OutputItem::WorkflowPanel {
        graph,
        expanded_nodes,
        panel_expanded,
        ..
    } = item
    {
        render_workflow_panel_with_regions(
            graph,
            expanded_nodes,
            *panel_expanded,
            ctx.animation_frame,
            ctx.panel_width,
        )
    } else {
        (render_item(item, ctx), Vec::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub items_version: u64,
    pub expanded_version: u64,
    pub width: u16,
}

#[derive(Default)]
pub struct LayoutCache {
    key: Option<LayoutKey>,
    lines: Vec<Line<'static>>,
    ranges: Vec<ItemRange>,
    node_regions: Vec<NodeRegion>,
    total_rows: u16,
}

impl LayoutCache {
    pub fn get_or_build(
        &mut self,
        key: LayoutKey,
        items: &[OutputItem],
        ctx: &RenderCtx<'_>,
    ) -> (&[Line<'static>], &[ItemRange], &[NodeRegion], u16) {
        if self.key != Some(key) {
            let (lines, ranges, node_regions, total) =
                build_lines_with_ranges(items, key.width, ctx);
            self.lines = lines;
            self.ranges = ranges;
            self.node_regions = node_regions;
            self.total_rows = total;
            self.key = Some(key);
        }
        (
            &self.lines,
            &self.ranges,
            &self.node_regions,
            self.total_rows,
        )
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }
}

impl std::fmt::Debug for LayoutCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCache")
            .field("key", &self.key)
            .field("total_rows", &self.total_rows)
            .finish()
    }
}

pub fn render_item(item: &OutputItem, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut lines = match item {
        OutputItem::UserTurn { text } => vec![Line::from(vec![
            Span::styled(
                "❯ ".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
        ])],
        OutputItem::AssistantMd { md, streaming } => {
            let mut lines = crate::markdown::render_markdown(md);
            if *streaming {
                lines.push(Line::from(Span::styled(
                    "▏".to_string(),
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                )));
            }
            lines
        }
        OutputItem::SystemNote { text, level } => {
            let (glyph, color) = match level {
                NoteLevel::Info => ("·", Color::Blue),
                NoteLevel::Warn => ("!", Color::Yellow),
                NoteLevel::Error => ("✗", Color::Red),
            };
            let cleaned = text
                .strip_prefix("[atman] ")
                .or_else(|| text.strip_prefix("[atman]"))
                .unwrap_or(text)
                .to_string();
            vec![Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::raw(cleaned),
            ])]
        }
        OutputItem::Divider => vec![
            Line::from(""),
            Line::from(Span::styled(
                "─".repeat(60),
                Style::default().fg(Color::DarkGray),
            )),
        ],
        OutputItem::WorkflowPanel {
            graph,
            expanded_nodes,
            panel_expanded,
            started_at,
            ended_at,
            ..
        } => render_workflow_panel(
            graph,
            expanded_nodes,
            *panel_expanded,
            *started_at,
            *ended_at,
            ctx.animation_frame,
            ctx.panel_width,
        ),
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
}

fn render_workflow_panel(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    _started_at: std::time::Instant,
    _ended_at: Option<std::time::Instant>,
    animation_frame: u32,
    panel_width: u16,
) -> Vec<Line<'static>> {
    render_workflow_panel_with_regions(
        graph,
        expanded_nodes,
        panel_expanded,
        animation_frame,
        panel_width,
    )
    .0
}

fn render_workflow_panel_with_regions(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    animation_frame: u32,
    panel_width: u16,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let count = count_workflow_nodes(&graph.root);
    let (status_str, status_style, running) = workflow_overall_status(&graph.root);
    let elapsed = compute_elapsed_secs(&graph.root, running);
    let fold_glyph = if panel_expanded { "▼" } else { "▶" };
    let flow_glyph = if running {
        spinner_char(animation_frame)
    } else {
        "⚡"
    };
    let header = Line::from(vec![
        Span::styled(
            format!(" {fold_glyph} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{flow_glyph} workflow"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · {count} nodes · {} · ",
            crate::humanize::format_secs(elapsed)
        )),
        Span::styled(status_str, status_style),
    ]);
    let mut lines = vec![header];
    let mut regions: Vec<NodeRegion> = Vec::new();
    let mut pending_counter: u8 = 0;
    if panel_expanded {
        let child_count = graph.root.len();
        for (i, node) in graph.root.iter().enumerate() {
            let is_last = i + 1 == child_count;
            let path = format!("{i}");
            append_workflow_node(
                &mut lines,
                &mut regions,
                node,
                expanded_nodes,
                "",
                &path,
                is_last,
                animation_frame,
                running,
                &mut pending_counter,
                panel_width,
            );
        }
    }
    (lines, regions)
}

fn compute_elapsed_secs(nodes: &[atman_runtime::workflow::WorkflowNode], running: bool) -> i64 {
    let mut min: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut max: Option<chrono::DateTime<chrono::Utc>> = None;
    for n in nodes {
        if let Some(t) = n.started_at {
            min = Some(min.map(|m| m.min(t)).unwrap_or(t));
        }
        if let Some(t) = n.ended_at {
            max = Some(max.map(|m| m.max(t)).unwrap_or(t));
        }
    }
    let Some(start) = min else {
        return 0;
    };
    let end = if running {
        chrono::Utc::now()
    } else {
        max.unwrap_or(start)
    };
    (end - start).num_seconds().max(0)
}

fn count_workflow_nodes(nodes: &[atman_runtime::workflow::WorkflowNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_workflow_nodes(&n.children))
        .sum()
}

fn workflow_overall_status(
    nodes: &[atman_runtime::workflow::WorkflowNode],
) -> (String, Style, bool) {
    use atman_runtime::workflow::NodeStatus;
    fn walk(ns: &[atman_runtime::workflow::WorkflowNode], running: &mut bool, err: &mut bool) {
        for n in ns {
            match n.status {
                NodeStatus::Running | NodeStatus::Pending => *running = true,
                NodeStatus::Err => *err = true,
                _ => {}
            }
            walk(&n.children, running, err);
        }
    }
    let mut has_running = false;
    let mut has_err = false;
    walk(nodes, &mut has_running, &mut has_err);
    if has_running {
        ("running…".into(), Style::default().fg(Color::Yellow), true)
    } else if has_err {
        ("err".into(), Style::default().fg(Color::Red), false)
    } else if nodes.is_empty() {
        ("empty".into(), Style::default().fg(Color::DarkGray), false)
    } else {
        ("ok".into(), Style::default().fg(Color::Green), false)
    }
}

const FANOUT_MIN_WIDTH: u16 = 120;
const FANOUT_MAX_BRANCHES: usize = 4;
const FANOUT_MIN_COL_WIDTH: u16 = 20;

fn is_fanout_group(node: &atman_runtime::workflow::WorkflowNode) -> bool {
    use atman_runtime::workflow::WorkflowNodeKind;
    !node.children.is_empty()
        && node
            .children
            .iter()
            .all(|c| matches!(c.kind, WorkflowNodeKind::FanoutBranch { .. }))
}

fn horizontal_layout_feasible(branch_count: usize, panel_width: u16, prefix: &str) -> bool {
    if !(2..=FANOUT_MAX_BRANCHES).contains(&branch_count) {
        return false;
    }
    if panel_width < FANOUT_MIN_WIDTH {
        return false;
    }
    let prefix_cols = prefix.chars().count() as u16;
    let usable = panel_width.saturating_sub(prefix_cols);
    let per_branch = usable / (branch_count as u16).max(1);
    per_branch >= FANOUT_MIN_COL_WIDTH
}

#[allow(clippy::too_many_arguments)]
fn append_fanout_horizontal(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    branches: &[atman_runtime::workflow::WorkflowNode],
    expanded_nodes: &std::collections::HashSet<String>,
    child_prefix: &str,
    parent_path: &str,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
    panel_width: u16,
) {
    let branch_count = branches.len();
    let prefix_cols = child_prefix.chars().count() as u16;
    let usable = panel_width.saturating_sub(prefix_cols);
    let col_width = usable / branch_count as u16;
    let base_col = prefix_cols;
    let mut per_branch_lines: Vec<Vec<Line<'static>>> = Vec::with_capacity(branch_count);
    let mut per_branch_regions: Vec<Vec<NodeRegion>> = Vec::with_capacity(branch_count);
    for (i, branch) in branches.iter().enumerate() {
        let mut b_lines: Vec<Line<'static>> = Vec::new();
        let mut b_regions: Vec<NodeRegion> = Vec::new();
        let branch_path = format!("{parent_path}/{i}");
        append_workflow_node(
            &mut b_lines,
            &mut b_regions,
            branch,
            expanded_nodes,
            "",
            &branch_path,
            i + 1 == branch_count,
            animation_frame,
            flow_running,
            pending_counter,
            col_width,
        );
        per_branch_lines.push(b_lines);
        per_branch_regions.push(b_regions);
    }
    let fork_row = out.len() as u16;
    let mut fork_spans = vec![Span::styled(
        child_prefix.to_string(),
        Style::default().fg(Color::DarkGray),
    )];
    let mut cursor: u16 = 0;
    for i in 0..branch_count {
        let mid = cursor + col_width / 2;
        while cursor < mid {
            fork_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(Color::Magenta),
            ));
            cursor += 1;
        }
        fork_spans.push(Span::styled(
            "┬".to_string(),
            Style::default().fg(Color::Magenta),
        ));
        cursor += 1;
        let _ = i;
        while cursor < ((i + 1) as u16) * col_width {
            fork_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(Color::Magenta),
            ));
            cursor += 1;
        }
    }
    out.push(Line::from(fork_spans));
    let body_start_row = out.len() as u16;
    let max_height = per_branch_lines.iter().map(|b| b.len()).max().unwrap_or(0);
    for row_i in 0..max_height {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(child_prefix.to_string())];
        for (b, branch_lines) in per_branch_lines.iter().enumerate() {
            let mut written: u16 = 0;
            let target = col_width;
            if let Some(line) = branch_lines.get(row_i) {
                for span in line.spans.iter() {
                    let take = span
                        .content
                        .chars()
                        .take((target - written) as usize)
                        .collect::<String>();
                    let taken = take.chars().count() as u16;
                    spans.push(Span::styled(take, span.style));
                    written = written.saturating_add(taken);
                    if written >= target {
                        break;
                    }
                }
            }
            while written < target {
                spans.push(Span::raw(" ".to_string()));
                written += 1;
            }
            let _ = b;
        }
        out.push(Line::from(spans));
    }
    let merge_row = out.len() as u16;
    let mut merge_spans = vec![Span::styled(
        child_prefix.to_string(),
        Style::default().fg(Color::DarkGray),
    )];
    let mut cursor: u16 = 0;
    for i in 0..branch_count {
        let mid = cursor + col_width / 2;
        while cursor < mid {
            merge_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(Color::Magenta),
            ));
            cursor += 1;
        }
        merge_spans.push(Span::styled(
            "┴".to_string(),
            Style::default().fg(Color::Magenta),
        ));
        cursor += 1;
        while cursor < ((i + 1) as u16) * col_width {
            merge_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(Color::Magenta),
            ));
            cursor += 1;
        }
    }
    out.push(Line::from(merge_spans));
    for (b, branch_regions) in per_branch_regions.into_iter().enumerate() {
        let col_start = base_col + (b as u16) * col_width;
        let col_end = col_start + col_width;
        for mut r in branch_regions {
            r.start_row = body_start_row.saturating_add(r.start_row);
            r.end_row = body_start_row.saturating_add(r.end_row);
            r.col_start = Some(col_start);
            r.col_end = Some(col_end);
            regions.push(r);
        }
    }
    let _ = (fork_row, merge_row);
}

#[allow(clippy::too_many_arguments)]
fn append_workflow_node(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    node: &atman_runtime::workflow::WorkflowNode,
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_prefix: &str,
    path: &str,
    is_last: bool,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
    panel_width: u16,
) {
    use atman_runtime::workflow::{ApprovalState, NodeStatus, WorkflowNodeKind};
    let start_row = out.len() as u16;
    let effective = node;
    let (branch_glyph, branch_color) = if matches!(node.kind, WorkflowNodeKind::FanoutBranch { .. })
    {
        if is_last {
            ("╚═", Color::Magenta)
        } else {
            ("╠═", Color::Magenta)
        }
    } else if is_last {
        ("└─", Color::DarkGray)
    } else {
        ("├─", Color::DarkGray)
    };
    let (status_glyph, status_style) = match effective.status {
        NodeStatus::Ok => ("✓", Style::default().fg(Color::Green)),
        NodeStatus::Err => ("✗", Style::default().fg(Color::Red)),
        NodeStatus::Cancelled => ("⊘", Style::default().fg(Color::DarkGray)),
        NodeStatus::Running | NodeStatus::Pending => {
            if flow_running {
                (
                    spinner_char(animation_frame),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                ("○", Style::default().fg(Color::DarkGray))
            }
        }
    };
    let (kind_glyph, kind_color) = match &effective.kind {
        WorkflowNodeKind::Flow { .. } => ("⚡", Color::Cyan),
        WorkflowNodeKind::Subflow { .. } => ("↳", Color::Cyan),
        WorkflowNodeKind::Stmt { node_kind } => stmt_kind_glyph(node_kind),
        WorkflowNodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", Color::Magenta),
    };
    let base_label = match &effective.kind {
        WorkflowNodeKind::ToolCall {
            tool, args_preview, ..
        } => format!("{tool}({})", truncate_preview(args_preview, 60)),
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::When { condition_preview },
        } if !condition_preview.is_empty() && condition_preview != "when" => {
            format!("when {condition_preview}")
        }
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("branch[{branch_index}]  {}", effective.label)
        }
        _ => effective.label.clone(),
    };
    let expandable = matches!(
        &effective.kind,
        WorkflowNodeKind::ToolCall { .. } | WorkflowNodeKind::Stmt { .. }
    );
    let is_expanded = expanded_nodes.contains(path);
    let expand_glyph = if !expandable {
        "  "
    } else if is_expanded {
        "▾ "
    } else {
        "▸ "
    };
    let (approval_prefix, approval_suffix) = match &effective.approval {
        Some(ApprovalState::Pending { level, .. }) => {
            *pending_counter = pending_counter.saturating_add(1);
            let key = if *pending_counter <= 9 {
                format!("{pending_counter}")
            } else {
                "•".into()
            };
            (
                Some((
                    format!("[{key}] "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Some((
                    format!("  ⏸ waiting approval ({level})"),
                    Style::default().fg(Color::Yellow),
                )),
            )
        }
        Some(ApprovalState::Denied { reason }) => (
            None,
            Some((
                format!("  ⊘ denied: {reason}"),
                Style::default().fg(Color::Red),
            )),
        ),
        _ => (None, None),
    };
    let label = base_label;
    let mut spans = vec![
        Span::styled(
            format!("{ancestor_prefix}{branch_glyph} "),
            Style::default().fg(branch_color),
        ),
        Span::styled(format!("{status_glyph} "), status_style),
        Span::styled(
            expand_glyph.to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some((text, style)) = approval_prefix {
        spans.push(Span::styled(text, style));
    }
    spans.push(Span::styled(
        format!("{kind_glyph} "),
        Style::default().fg(kind_color),
    ));
    spans.push(Span::raw(label));
    if let Some((text, style)) = approval_suffix {
        spans.push(Span::styled(text, style));
    }
    out.push(Line::from(spans));
    regions.push(NodeRegion {
        panel_item_index: 0,
        path_key: path.to_string(),
        start_row,
        end_row: start_row.saturating_add(1),
        col_start: None,
        col_end: None,
    });
    let vertical = if is_last { "   " } else { "│  " };
    let child_prefix = format!("{ancestor_prefix}{vertical}");
    if is_expanded {
        append_expanded_details(out, effective, &child_prefix);
    }
    let child_count = effective.children.len();
    if child_count > 1
        && is_fanout_group(effective)
        && horizontal_layout_feasible(effective.children.len(), panel_width, &child_prefix)
    {
        append_fanout_horizontal(
            out,
            regions,
            &effective.children,
            expanded_nodes,
            &child_prefix,
            path,
            animation_frame,
            flow_running,
            pending_counter,
            panel_width,
        );
        return;
    }
    for (i, child) in effective.children.iter().enumerate() {
        let child_last = i + 1 == child_count;
        let child_path = format!("{path}/{i}");
        append_workflow_node(
            out,
            regions,
            child,
            expanded_nodes,
            &child_prefix,
            &child_path,
            child_last,
            animation_frame,
            flow_running,
            pending_counter,
            panel_width,
        );
    }
}

fn append_expanded_details(
    out: &mut Vec<Line<'static>>,
    node: &atman_runtime::workflow::WorkflowNode,
    prefix: &str,
) {
    use atman_runtime::workflow::WorkflowNodeKind;
    let mut sections: Vec<(&str, String)> = Vec::new();
    if let WorkflowNodeKind::ToolCall {
        args_preview,
        result_preview,
        ..
    } = &node.kind
    {
        if !args_preview.is_empty() {
            sections.push(("args", args_preview.clone()));
        }
        if let Some(r) = result_preview.as_deref()
            && !r.is_empty()
        {
            sections.push(("result", r.to_string()));
        }
    }
    if let Some(preview) = node.output_preview.as_deref()
        && !preview.is_empty()
        && sections.iter().all(|(_, v)| v != preview)
    {
        sections.push(("output", preview.to_string()));
    }
    if let Some(atman_runtime::workflow::ApprovalState::Pending {
        preview: Some(p), ..
    }) = &node.approval
        && !p.is_empty()
    {
        sections.push(("diff", p.clone()));
    }
    for (label, body) in sections {
        out.push(Line::from(vec![Span::styled(
            format!("{prefix}  ▪ {label}:"),
            Style::default().fg(Color::DarkGray),
        )]));
        for line in body.lines().take(20) {
            let trimmed: String = line.chars().take(200).collect();
            out.push(Line::from(vec![
                Span::styled(
                    format!("{prefix}    "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(trimmed, Style::default().fg(Color::Gray)),
            ]));
        }
    }
}

fn stmt_kind_glyph(kind: &atman_runtime::nodegraph::NodeKind) -> (&'static str, Color) {
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

fn truncate_preview(s: &str, max: usize) -> String {
    let mut acc = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            acc.push('…');
            return acc;
        }
        acc.push(ch);
    }
    acc
}

pub fn empty_hint<'a>() -> Paragraph<'a> {
    Paragraph::new("plain text → agent · :help for builtins · Ctrl+C to interrupt")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_ends_with_reset_empty_line() {
        for item in [
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::AssistantMd {
                md: "one line".into(),
                streaming: false,
            },
            OutputItem::SystemNote {
                text: "note".into(),
                level: NoteLevel::Info,
            },
            OutputItem::Divider,
        ] {
            let lines = render_item(&item, &RenderCtx::empty());
            let last = lines.last().expect("non-empty");
            let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.is_empty(),
                "expected empty trailing line, got {text:?}"
            );
        }
    }

    #[test]
    fn divider_produces_three_lines() {
        let lines = render_item(&OutputItem::Divider, &RenderCtx::empty());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn build_lines_concats_all_items() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let out = build_lines(&items, &RenderCtx::empty());
        assert!(out.len() >= 4);
    }

    #[test]
    fn build_lines_with_ranges_gives_one_range_per_item() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let (_lines, ranges, _regions, total) =
            build_lines_with_ranges(&items, 80, &RenderCtx::empty());
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].item_index, 0);
        assert_eq!(ranges[1].item_index, 1);
        assert!(ranges[0].end_row <= ranges[1].start_row);
        assert_eq!(total, ranges[1].end_row);
    }

    #[test]
    fn build_lines_with_ranges_empty_items_returns_empty_vecs() {
        let (lines, ranges, _regions, total) =
            build_lines_with_ranges(&[], 80, &RenderCtx::empty());
        assert!(lines.is_empty());
        assert!(ranges.is_empty());
        assert_eq!(total, 0);
    }

    fn flatten_lines(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workflow_panel_renders_linear_chain_with_tree_glyphs() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };
        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(WorkflowNode {
            id: "r".into(),
            kind: WorkflowNodeKind::Flow {
                run_id: "r".into(),
                flow_name: "f".into(),
            },
            label: "flow".into(),
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: vec![
                WorkflowNode {
                    id: "s0".into(),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                    },
                    label: "step0".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                },
                WorkflowNode {
                    id: "s1".into(),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                    },
                    label: "step1".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                },
            ],
            parallelism: Parallelism::Serial,
            approval: None,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(flat.contains("workflow"), "header missing: {flat}");
        assert!(flat.contains("step0"));
        assert!(flat.contains("step1"));
        assert!(flat.contains("├─"));
        assert!(flat.contains("└─"));
    }

    #[test]
    fn workflow_panel_collapsed_hides_children() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };
        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(WorkflowNode {
            id: "r".into(),
            kind: WorkflowNodeKind::Flow {
                run_id: "r".into(),
                flow_name: "f".into(),
            },
            label: "flow".into(),
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: vec![WorkflowNode {
                id: "child".into(),
                kind: WorkflowNodeKind::Stmt {
                    node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                },
                label: "hidden-child".into(),
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
            }],
            parallelism: Parallelism::Serial,
            approval: None,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: false,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(flat.contains("workflow"));
        assert!(!flat.contains("hidden-child"), "collapsed leaks: {flat}");
    }

    #[test]
    fn recursive_subflow_chain_preserves_every_iteration() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };

        fn subflow_layer(depth: usize, remaining: usize) -> WorkflowNode {
            let deeper = if remaining > 0 {
                vec![subflow_layer(depth + 1, remaining - 1)]
            } else {
                vec![WorkflowNode {
                    id: format!("leaf_{depth}"),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::Return,
                    },
                    label: "final".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: None,
                }]
            };
            WorkflowNode {
                id: format!("loop_{depth}"),
                kind: WorkflowNodeKind::Subflow {
                    run_id: format!("r_{depth}"),
                    flow_name: "agent_loop".into(),
                },
                label: "agent_loop".into(),
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: deeper,
                parallelism: Parallelism::Serial,
                approval: None,
            }
        }

        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(subflow_layer(0, 4));
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(
            flat.matches("agent_loop").count() >= 5,
            "each iteration must render, got: {flat}"
        );
        assert!(flat.contains("final"));
    }
}
