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
    pub col_start: u16,
    pub col_end: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRect {
    pub row0: u16,
    pub col0: u16,
    pub outer_width: u16,
    pub rows: u16,
}

impl BoxRect {
    pub fn col_end(self) -> u16 {
        self.col0.saturating_add(self.outer_width)
    }

    pub fn end_row(self) -> u16 {
        self.row0.saturating_add(self.rows)
    }
}

pub struct BoxSpec<'a> {
    pub row0: u16,
    pub col0: u16,
    pub outer_width: u16,
    pub inner_lines: Vec<Line<'static>>,
    pub border_style: Style,
    pub status_glyph: &'a str,
    pub kind_glyph: &'a str,
    pub label: &'a str,
    pub approval_hotkey: Option<u8>,
}

pub fn append_box(out: &mut Vec<Line<'static>>, spec: BoxSpec<'_>) -> BoxRect {
    let BoxSpec {
        row0,
        col0,
        outer_width,
        inner_lines,
        border_style,
        status_glyph,
        kind_glyph,
        label,
        approval_hotkey,
    } = spec;
    use unicode_width::UnicodeWidthStr;
    let min_outer: u16 = 6;
    if outer_width < min_outer {
        return BoxRect {
            row0,
            col0,
            outer_width,
            rows: 0,
        };
    }
    let approval_text = approval_hotkey.map(|n| format!("─[{n}]─"));
    let approval_w = approval_text.as_deref().map_or(0, UnicodeWidthStr::width);
    let status_w = UnicodeWidthStr::width(status_glyph);
    let kind_w = UnicodeWidthStr::width(kind_glyph);
    let leading_w = 2usize + 1; // `╭─` + leading space
    let trailing_w = 2usize; // `─╮`
    let status_seg = if status_w > 0 { status_w + 1 } else { 0 };
    let kind_seg = if kind_w > 0 { kind_w + 1 } else { 0 };
    let fixed = leading_w + status_seg + kind_seg + approval_w + trailing_w;
    let label_budget = (outer_width as usize).saturating_sub(fixed).max(1);
    let label_display = middle_truncate(label, label_budget);
    let label_w = UnicodeWidthStr::width(label_display.as_str());
    let content_total = fixed.saturating_add(label_w);
    let fill_w = (outer_width as usize).saturating_sub(content_total);
    let inner_w = (outer_width as usize).saturating_sub(4);
    let mut top_spans: Vec<Span<'static>> = Vec::with_capacity(8);
    top_spans.push(Span::styled("╭─".to_string(), border_style));
    top_spans.push(Span::raw(" "));
    if status_w > 0 {
        top_spans.push(Span::raw(status_glyph.to_string()));
        top_spans.push(Span::raw(" "));
    }
    if kind_w > 0 {
        top_spans.push(Span::raw(kind_glyph.to_string()));
        top_spans.push(Span::raw(" "));
    }
    top_spans.push(Span::raw(label_display));
    if fill_w > 0 {
        top_spans.push(Span::styled(" ".repeat(fill_w), border_style));
    }
    if let Some(text) = approval_text {
        top_spans.push(Span::styled(
            text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    top_spans.push(Span::styled("─╮".to_string(), border_style));
    out.push(Line::from(top_spans));
    let inner_count = inner_lines.len() as u16;
    for line in inner_lines {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
        spans.push(Span::styled("│ ".to_string(), border_style));
        let inner_used: usize = line
            .spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        for s in line.spans {
            spans.push(s);
        }
        let pad_w = inner_w.saturating_sub(inner_used);
        if pad_w > 0 {
            spans.push(Span::raw(" ".repeat(pad_w)));
        }
        spans.push(Span::styled(" │".to_string(), border_style));
        out.push(Line::from(spans));
    }
    let bottom = format!("╰{}╯", "─".repeat((outer_width as usize).saturating_sub(2)));
    out.push(Line::from(Span::styled(bottom, border_style)));
    BoxRect {
        row0,
        col0,
        outer_width,
        rows: 2u16.saturating_add(inner_count),
    }
}

fn middle_truncate(s: &str, max_display: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if UnicodeWidthStr::width(s) <= max_display {
        return s.to_string();
    }
    if max_display <= 1 {
        return "…".into();
    }
    let ell = 1;
    let side_budget = max_display.saturating_sub(ell) / 2;
    let (prefix, prefix_w) = take_display_prefix(s, side_budget);
    let remaining = max_display.saturating_sub(prefix_w + ell);
    let suffix = take_display_suffix(s, remaining);
    format!("{prefix}…{suffix}")
}

fn take_display_prefix(s: &str, max_w: usize) -> (String, usize) {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max_w {
            break;
        }
        out.push(c);
        used += w;
    }
    (out, used)
}

fn take_display_suffix(s: &str, max_w: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut chars: Vec<char> = Vec::new();
    let mut used = 0usize;
    for c in s.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max_w {
            break;
        }
        chars.push(c);
        used += w;
    }
    chars.reverse();
    chars.into_iter().collect()
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

fn wrap_row_offsets(lines: &[Line<'static>], _width: u16) -> (u16, Vec<u16>) {
    // Paragraph is rendered with .scroll() but no .wrap(), so ratatui uses
    // LineTruncator: one Line always renders as one row (long lines get
    // truncated at panel width, not wrapped). Anything else here would over-
    // estimate total_rows, put follow_tail scroll past real content, and
    // produce the "session opens on blank space, scroll up to find text" bug.
    let mut offsets: Vec<u16> = Vec::with_capacity(lines.len() + 1);
    let mut cursor: u16 = 0;
    offsets.push(0);
    for _ in lines {
        cursor = cursor.saturating_add(1);
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
    pub animation_frame: Option<u32>,
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
    let legacy = std::env::var_os("ATMAN_LEGACY_WORKFLOW").is_some();
    if panel_expanded {
        if legacy {
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
            return (lines, regions);
        }
        let child_count = graph.root.len();
        for (i, node) in graph.root.iter().enumerate() {
            let path = format!("{i}");
            let is_last = i + 1 == child_count;
            append_workflow_node_boxed(
                &mut lines,
                &mut regions,
                node,
                expanded_nodes,
                &[],
                is_last,
                panel_width,
                &path,
                animation_frame,
                running,
                &mut pending_counter,
            );
        }
        lines.push(Line::raw(""));
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
            r.col_start = col_start;
            r.col_end = col_end;
            regions.push(r);
        }
    }
    let _ = (fork_row, merge_row);
}

const MAX_BOX_WIDTH: u16 = 100;
const INDENT_PER_DEPTH: u16 = 4;

fn tree_prefix_spans(ancestor_last: &[bool], is_last: Option<bool>) -> Vec<Span<'static>> {
    let style = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ancestor_last.len() + 1);
    for &last in ancestor_last {
        spans.push(Span::styled(
            if last { "    " } else { "┊   " }.to_string(),
            style,
        ));
    }
    if let Some(is_last) = is_last {
        spans.push(Span::styled(
            if is_last { "└┈┈ " } else { "├┈┈ " }.to_string(),
            style,
        ));
    }
    spans
}

fn tree_continuation_spans(ancestor_last: &[bool], is_last: bool) -> Vec<Span<'static>> {
    let style = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ancestor_last.len() + 1);
    for &last in ancestor_last {
        spans.push(Span::styled(
            if last { "    " } else { "┊   " }.to_string(),
            style,
        ));
    }
    spans.push(Span::styled(
        if is_last { "    " } else { "┊   " }.to_string(),
        style,
    ));
    spans
}

#[allow(clippy::too_many_arguments)]
fn append_workflow_node_boxed(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    node: &atman_runtime::workflow::WorkflowNode,
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_last: &[bool],
    is_last: bool,
    panel_width: u16,
    path: &str,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
) {
    use atman_runtime::workflow::{ApprovalState, NodeStatus, WorkflowNodeKind};
    let depth = ancestor_last.len() as u16;
    let prefix_w = depth.saturating_mul(INDENT_PER_DEPTH);
    let col0 = prefix_w;
    let budget = panel_width.saturating_sub(prefix_w).min(MAX_BOX_WIDTH);
    if budget < 8 {
        return;
    }
    let mut border_style = match node.status {
        NodeStatus::Ok => Style::default().fg(Color::Green),
        NodeStatus::Err => Style::default().fg(Color::Red),
        NodeStatus::Cancelled => Style::default().fg(Color::DarkGray),
        NodeStatus::Running | NodeStatus::Pending => Style::default().fg(Color::Cyan),
    };
    let status_glyph = match node.status {
        NodeStatus::Ok => "✓",
        NodeStatus::Err => "✗",
        NodeStatus::Cancelled => "⊘",
        NodeStatus::Running | NodeStatus::Pending => {
            if flow_running {
                spinner_char(animation_frame)
            } else {
                "○"
            }
        }
    };
    let (kind_glyph, _kind_color) = match &node.kind {
        WorkflowNodeKind::Flow { .. } => ("⚡", Color::Cyan),
        WorkflowNodeKind::Subflow { .. } => ("↳", Color::Cyan),
        WorkflowNodeKind::Stmt { node_kind } => stmt_kind_glyph(node_kind),
        WorkflowNodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", Color::Magenta),
    };
    let label = match &node.kind {
        WorkflowNodeKind::ToolCall {
            tool, args_preview, ..
        } => format!("{tool}({})", truncate_preview(args_preview, 60)),
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("branch[{branch_index}]  {}", node.label)
        }
        _ => node.label.clone(),
    };
    let mut approval_hotkey: Option<u8> = None;
    let mut auto_expand = false;
    if let Some(ApprovalState::Pending { .. }) = &node.approval {
        *pending_counter = pending_counter.saturating_add(1);
        if *pending_counter <= 9 {
            approval_hotkey = Some(*pending_counter);
        }
        border_style = Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD);
        auto_expand = true;
    } else if matches!(&node.approval, Some(ApprovalState::Denied { .. })) {
        border_style = Style::default().fg(Color::Red);
    }
    let is_expanded = auto_expand || expanded_nodes.contains(path);
    let mut inner_lines: Vec<Line<'static>> = Vec::new();
    if is_expanded {
        collect_boxed_details(node, &mut inner_lines);
    }
    use unicode_width::UnicodeWidthStr;
    let approval_seg = if approval_hotkey.is_some() { 5 } else { 0 };
    let status_seg = if UnicodeWidthStr::width(status_glyph) > 0 {
        UnicodeWidthStr::width(status_glyph) + 1
    } else {
        0
    };
    let kind_seg = if UnicodeWidthStr::width(kind_glyph) > 0 {
        UnicodeWidthStr::width(kind_glyph) + 1
    } else {
        0
    };
    let compact_content =
        3 + status_seg + kind_seg + UnicodeWidthStr::width(label.as_str()) + approval_seg + 2;
    let compact_w = compact_content.min(budget as usize) as u16;
    let outer_width = if is_expanded { budget } else { compact_w };
    let mut scratch: Vec<Line<'static>> = Vec::new();
    let start_row = out.len() as u16;
    let rect = append_box(
        &mut scratch,
        BoxSpec {
            row0: start_row,
            col0,
            outer_width,
            inner_lines,
            border_style,
            status_glyph,
            kind_glyph,
            label: &label,
            approval_hotkey,
        },
    );
    for (row_idx, line) in scratch.into_iter().enumerate() {
        let is_top = row_idx == 0;
        let prefix = if is_top {
            tree_prefix_spans(ancestor_last, Some(is_last))
        } else {
            tree_continuation_spans(ancestor_last, is_last)
        };
        let mut spans = prefix;
        spans.extend(line.spans);
        out.push(Line::from(spans));
    }
    regions.push(NodeRegion {
        panel_item_index: 0,
        path_key: path.to_string(),
        start_row: rect.row0,
        end_row: rect.row0.saturating_add(rect.rows),
        col_start: rect.col0,
        col_end: rect.col_end(),
    });
    let mut child_ancestor_last: Vec<bool> = ancestor_last.to_vec();
    child_ancestor_last.push(is_last);
    let child_count = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_path = format!("{path}/{i}");
        let child_is_last = i + 1 == child_count;
        append_workflow_node_boxed(
            out,
            regions,
            child,
            expanded_nodes,
            &child_ancestor_last,
            child_is_last,
            panel_width,
            &child_path,
            animation_frame,
            flow_running,
            pending_counter,
        );
    }
}

fn collect_boxed_details(
    node: &atman_runtime::workflow::WorkflowNode,
    out: &mut Vec<Line<'static>>,
) {
    use atman_runtime::workflow::{ApprovalState, WorkflowNodeKind};
    if let WorkflowNodeKind::ToolCall {
        args_preview,
        result_preview,
        ..
    } = &node.kind
    {
        if !args_preview.is_empty() {
            push_detail_section(out, "args", args_preview);
        }
        if let Some(r) = result_preview {
            push_detail_section(out, "result", r);
        }
    }
    if let Some(p) = &node.output_preview {
        push_detail_section(out, "output", p);
    }
    if let Some(ApprovalState::Pending {
        level,
        preview: Some(p),
    }) = &node.approval
    {
        push_detail_section(out, &format!("approval ({level})"), p);
    }
    if let (Some(start), Some(end)) = (node.started_at, node.ended_at) {
        let ms = (end - start).num_milliseconds().max(0);
        let text = if ms < 1000 {
            format!("{ms}ms")
        } else {
            format!("{:.3}s", ms as f64 / 1000.0)
        };
        push_detail_section(out, "duration", &text);
    }
}

fn push_detail_section(out: &mut Vec<Line<'static>>, header: &str, body: &str) {
    out.push(Line::from(Span::styled(
        format!("{header}:"),
        Style::default().fg(Color::DarkGray),
    )));
    for line in body.lines().take(20) {
        out.push(Line::from(Span::raw(line.to_string())));
    }
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
        col_start: 0,
        col_end: panel_width,
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

    fn plain_line(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn spec<'a>(
        outer_width: u16,
        inner: Vec<Line<'static>>,
        status: &'a str,
        kind: &'a str,
        label: &'a str,
        approval: Option<u8>,
    ) -> BoxSpec<'a> {
        BoxSpec {
            row0: 0,
            col0: 0,
            outer_width,
            inner_lines: inner,
            border_style: Style::default(),
            status_glyph: status,
            kind_glyph: kind,
            label,
            approval_hotkey: approval,
        }
    }

    #[test]
    fn append_box_produces_rounded_border_and_correct_rect() {
        let mut out = Vec::new();
        let mut s = spec(
            30,
            vec![Line::from(Span::raw("hello"))],
            "○",
            "🔧",
            "read_file",
            None,
        );
        s.row0 = 5;
        s.col0 = 2;
        let rect = append_box(&mut out, s);
        assert_eq!(rect.row0, 5);
        assert_eq!(rect.col0, 2);
        assert_eq!(rect.outer_width, 30);
        assert_eq!(rect.rows, 3);
        assert_eq!(out.len(), 3);
        let top = plain_line(&out[0]);
        let mid = plain_line(&out[1]);
        let bot = plain_line(&out[2]);
        assert!(top.starts_with("╭─"), "top: {top:?}");
        assert!(top.ends_with("─╮"), "top: {top:?}");
        assert!(top.contains("○"), "status glyph missing: {top:?}");
        assert!(top.contains("🔧"), "kind glyph missing: {top:?}");
        assert!(top.contains("read_file"), "label missing: {top:?}");
        assert!(
            mid.starts_with("│ "),
            "mid should have left border: {mid:?}"
        );
        assert!(mid.ends_with(" │"), "mid should have right border: {mid:?}");
        assert!(mid.contains("hello"));
        assert!(bot.starts_with("╰"), "bot: {bot:?}");
        assert!(bot.ends_with("╯"), "bot: {bot:?}");
    }

    #[test]
    fn append_box_adds_approval_hotkey_in_top_right() {
        let mut out = Vec::new();
        let rect = append_box(
            &mut out,
            spec(40, Vec::new(), "⏸", "🔧", "shell.exec", Some(3)),
        );
        assert_eq!(rect.rows, 2);
        let top = plain_line(&out[0]);
        assert!(top.contains("─[3]─"), "approval tag missing: {top:?}");
        let idx_approval = top.find("─[3]─").unwrap();
        let idx_label = top.find("shell.exec").unwrap();
        assert!(
            idx_label < idx_approval,
            "approval must appear after label: {top:?}"
        );
    }

    #[test]
    fn append_box_truncates_long_label_middle() {
        let mut out = Vec::new();
        let long_label = "a".repeat(80);
        append_box(&mut out, spec(20, Vec::new(), "○", "🔧", &long_label, None));
        let top = plain_line(&out[0]);
        assert!(top.contains("…"), "truncation ellipsis missing: {top:?}");
        assert!(!top.contains(&"a".repeat(20)));
    }

    #[test]
    fn append_box_pads_short_content_to_full_inner_width() {
        let mut out = Vec::new();
        let inner = vec![Line::from(Span::raw("x"))];
        append_box(&mut out, spec(20, inner, "", "", "lbl", None));
        let mid = plain_line(&out[1]);
        assert_eq!(
            unicode_width::UnicodeWidthStr::width(mid.as_str()),
            20,
            "middle line should fill outer_width: {mid:?}"
        );
    }

    #[test]
    fn append_box_handles_cjk_label_display_width() {
        let mut out = Vec::new();
        append_box(&mut out, spec(30, Vec::new(), "○", "🔧", "读取文件", None));
        let top = plain_line(&out[0]);
        assert!(top.contains("读取文件"), "CJK label missing: {top:?}");
        let width = unicode_width::UnicodeWidthStr::width(top.as_str());
        assert_eq!(width, 30, "top border must be exactly outer_width: {width}");
    }

    #[test]
    fn append_box_at_min_width_still_renders_all_borders() {
        let mut out = Vec::new();
        let rect = append_box(
            &mut out,
            spec(6, Vec::new(), "○", "🔧", "very-long-label", None),
        );
        assert_eq!(rect.outer_width, 6, "min viable outer_width should render");
        assert_eq!(rect.rows, 2, "empty inner should emit top + bottom only");
        let top = plain_line(&out[0]);
        let bot = plain_line(out.last().unwrap());
        assert!(top.starts_with("╭─"), "top-left border missing: {top:?}");
        assert!(top.ends_with("─╮"), "top-right border missing: {top:?}");
        assert!(bot.starts_with("╰"), "bottom-left: {bot:?}");
        assert!(bot.ends_with("╯"), "bottom-right: {bot:?}");
    }

    #[test]
    fn append_box_below_min_width_emits_no_lines() {
        let mut out = Vec::new();
        let rect = append_box(&mut out, spec(4, Vec::new(), "○", "🔧", "x", None));
        assert_eq!(rect.rows, 0, "sub-minimum width must not emit rows");
        assert!(out.is_empty(), "sub-minimum width leaked lines: {out:?}");
    }

    #[test]
    fn append_box_truncates_mixed_ascii_cjk_at_exact_width() {
        let mut out = Vec::new();
        append_box(
            &mut out,
            spec(24, Vec::new(), "○", "🔧", "read_文件_data_读取", None),
        );
        let top = plain_line(&out[0]);
        let width = unicode_width::UnicodeWidthStr::width(top.as_str());
        assert_eq!(
            width, 24,
            "mixed ASCII+CJK truncation should still hit exact outer_width: {top:?}"
        );
        assert!(top.contains("…"), "expected truncation ellipsis: {top:?}");
    }

    #[test]
    fn append_box_truncates_very_long_cjk_label() {
        let mut out = Vec::new();
        let long_cjk = "读取文件内容并做分析的一个非常长的中文标题名称";
        append_box(&mut out, spec(20, Vec::new(), "○", "🔧", long_cjk, None));
        let top = plain_line(&out[0]);
        let width = unicode_width::UnicodeWidthStr::width(top.as_str());
        assert_eq!(
            width, 20,
            "CJK truncation must respect display width: {top:?}"
        );
        assert!(top.contains("…"), "expected ellipsis: {top:?}");
        assert!(
            !top.contains(long_cjk),
            "full long CJK should have been truncated: {top:?}"
        );
    }

    #[test]
    fn append_box_handles_emoji_dense_label() {
        let mut out = Vec::new();
        append_box(
            &mut out,
            spec(24, Vec::new(), "○", "🔧", "🚀🚀🚀 launch 🚀🚀", None),
        );
        let top = plain_line(&out[0]);
        let width = unicode_width::UnicodeWidthStr::width(top.as_str());
        assert_eq!(
            width, 24,
            "emoji width accounting must land on outer_width: {top:?}"
        );
    }

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

    struct LegacyEnvGuard;
    impl Drop for LegacyEnvGuard {
        fn drop(&mut self) {
            // SAFETY: test-only, restores env after this scope.
            unsafe { std::env::remove_var("ATMAN_LEGACY_WORKFLOW") };
        }
    }

    #[test]
    fn workflow_panel_renders_linear_chain_with_tree_glyphs() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };
        // SAFETY: same rationale as the guard's Drop.
        unsafe { std::env::set_var("ATMAN_LEGACY_WORKFLOW", "1") };
        let _legacy = LegacyEnvGuard;
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
