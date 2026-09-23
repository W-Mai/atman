use std::sync::Arc;
use std::time::Instant;

use atman_runtime::message::Message;
use atman_runtime::projection::workflow::{
    WorkflowAggregateStatus, WorkflowLlmAggregate as LlmStatsAggregate,
    WorkflowLlmRoute as LlmStatsRoute, WorkflowProjection, WorkflowSummary,
};
use atman_runtime::stream::CompactionPhase;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{
    Disclosure, FsDetail, FsSearchHit, NoteLevel, OutputItem, OutputStore, ToolCallStatus,
    ToolCallView,
};

const RESET: Style = Style::new();

#[cfg(test)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PerfCounters {
    semantic_item_visits: u64,
    item_renders: u64,
    animation_item_visits: u64,
    retention_item_visits: u64,
    bash_source_bytes: u64,
    bash_materialized_rows: u64,
    permission_table_entries: u64,
    panel_projection_builds: u64,
    workflow_node_renders: u64,
}

#[cfg(test)]
thread_local! {
    static PERF_COUNTERS: std::cell::Cell<PerfCounters> = const {
        std::cell::Cell::new(PerfCounters {
            semantic_item_visits: 0,
            item_renders: 0,
            animation_item_visits: 0,
            retention_item_visits: 0,
            bash_source_bytes: 0,
            bash_materialized_rows: 0,
            permission_table_entries: 0,
            panel_projection_builds: 0,
            workflow_node_renders: 0,
        })
    };
}

#[cfg(test)]
fn reset_perf_counters() {
    PERF_COUNTERS.with(|counters| counters.set(PerfCounters::default()));
}

#[cfg(test)]
fn perf_counters() -> PerfCounters {
    PERF_COUNTERS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn update_perf_counters(update: impl FnOnce(&mut PerfCounters)) {
    PERF_COUNTERS.with(|counters| {
        let mut value = counters.get();
        update(&mut value);
        counters.set(value);
    });
}

#[derive(Clone, Copy)]
pub struct RenderCtx<'a> {
    pub expanded_tools: &'a std::collections::HashSet<String>,
    pub messages: &'a [Message],
    pub animation_frame: u32,
    pub panel_width: u16,
    pub hovered_thinking_idx: Option<usize>,
    pub hovered_output_node: Option<&'a (usize, String)>,
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
            hovered_thinking_idx: None,
            hovered_output_node: None,
        }
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DYNAMIC_SPINNER_MARKER: &str = "\u{e000}";
pub(crate) const LAYOUT_ANIMATION_FRAME: u32 = u32::MAX;
const WORK_FOLD_RAIL_WIDTH: u16 = 1;
const WORK_FOLD_CONTENT_PADDING: u16 = 2;
const WORK_FOLD_BOUNDARY_ROWS: usize = 2;

fn spinner_char(frame: u32) -> &'static str {
    if frame == LAYOUT_ANIMATION_FRAME {
        DYNAMIC_SPINNER_MARKER
    } else {
        SPINNER[(frame as usize) % SPINNER.len()]
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DynamicPaint {
    active: bool,
    elapsed: Option<ElapsedPaint>,
    running_rows: Vec<usize>,
}

#[derive(Clone, Debug)]
struct ElapsedPaint {
    line: usize,
    span: usize,
    prefix: String,
    suffix: String,
    started_at: chrono::DateTime<chrono::Utc>,
}

pub(crate) fn patch_animation_lines(
    lines: &mut [Line<'static>],
    paint: &DynamicPaint,
    animation_frame: u32,
    line_offset: usize,
) {
    if !paint.active {
        return;
    }
    for (line_index, line) in lines.iter_mut().enumerate() {
        patch_animation_line(
            line,
            line_offset.saturating_add(line_index),
            paint,
            animation_frame,
        );
    }
}

fn patch_animation_line(
    line: &mut Line<'static>,
    line_index: usize,
    paint: &DynamicPaint,
    animation_frame: u32,
) {
    let spinner = spinner_char(animation_frame);
    for span in &mut line.spans {
        if span.content.contains(DYNAMIC_SPINNER_MARKER) {
            span.content =
                std::borrow::Cow::Owned(span.content.replace(DYNAMIC_SPINNER_MARKER, spinner));
        }
    }
    if let Some(elapsed) = &paint.elapsed
        && elapsed.line == line_index
        && let Some(span) = line.spans.get_mut(elapsed.span)
    {
        let seconds = (chrono::Utc::now() - elapsed.started_at)
            .num_seconds()
            .max(0);
        span.content = std::borrow::Cow::Owned(format!(
            "{}{}{}",
            elapsed.prefix,
            atman_runtime::humanize::format_secs(seconds),
            elapsed.suffix
        ));
    }
    if paint.running_rows.contains(&line_index) {
        paint_running_foreground(line, animation_frame, crate::theme::theme().accent.into());
    }
}

pub fn build_lines(items: &[OutputItem], ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(items.len() * 3);
    for item in items {
        out.extend(render_item(item, ctx));
    }
    out
}

/// A collapsible tool-output block rendered into the transcript. `row` is the
/// block's header line relative to the enclosing `build_lines` output, and
/// `tool_id` identifies the block for hit-testing (expand/collapse).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolHeaderSpot {
    pub row: u32,
    pub tool_id: String,
}

/// Like [`build_lines`] but also reports the header row of every collapsible
/// tool-output block so callers can build clickable hit regions.
pub fn build_lines_with_tool_headers(
    items: &[OutputItem],
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<ToolHeaderSpot>) {
    let mut out = Vec::with_capacity(items.len() * 3);
    let mut spots = Vec::new();
    for (item_index, item) in items.iter().enumerate() {
        let start = out.len() as u32;
        let (lines, regions) = render_item_with_regions(item, ctx, item_index);
        out.extend(lines);
        if matches!(item, OutputItem::ToolDispatch { .. }) {
            spots.extend(regions.iter().filter_map(|region| {
                region
                    .path_key
                    .strip_prefix(TOOL_CALL_REGION_PREFIX)
                    .map(|tool_id| ToolHeaderSpot {
                        row: start.saturating_add(region.start_row).saturating_add(1),
                        tool_id: tool_id.to_string(),
                    })
            }));
            continue;
        }
        if let Some(tool_id) = tool_block_id(item) {
            // Every collapsible block renderer emits a blank row at `start`,
            // then its header at `start + 1`.
            spots.push(ToolHeaderSpot {
                row: start.saturating_add(1),
                tool_id,
            });
        }
    }
    (out, spots)
}

fn tool_block_id(item: &OutputItem) -> Option<String> {
    match item {
        OutputItem::Bash { handle, .. } | OutputItem::Terminal { handle, .. } => {
            Some(handle.clone())
        }
        OutputItem::DiffPreview { title, .. } => Some(title.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemRange {
    pub item_index: usize,
    pub start_row: u32,
    pub end_row: u32,
}

/// Everything the renderer needs for one viewport: visual lines plus the metadata addressing them.
pub struct VisibleSlice {
    pub lines: Vec<Line<'static>>,
    pub ranges: Vec<ItemRange>,
    pub regions: Vec<NodeRegion>,
    pub selection: crate::selection::VisibleSelectionProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRegion {
    pub panel_item_index: usize,
    pub path_key: String,
    pub start_row: u32,
    pub end_row: u32,
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
    pub approval_badge: Option<(String, Style)>,
}

struct CompactionSummaryRender<'a> {
    phase: CompactionPhase,
    range_start: usize,
    range_end: usize,
    summary: &'a str,
    before_tokens: u64,
    after_tokens: u64,
    compacted_count: usize,
    disclosure: Disclosure,
    animation_frame: u32,
    panel_width: u16,
    hovered: bool,
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
        approval_badge,
    } = spec;
    let min_outer: u16 = 6;
    if outer_width < min_outer {
        return BoxRect {
            row0,
            col0,
            outer_width,
            rows: 0,
        };
    }
    let approval_text = approval_badge.map(|(badge, style)| (format!("─{badge}─"), style));
    let approval_w = approval_text
        .as_ref()
        .map_or(0, |(text, _)| crate::width::width(text));
    let status_w = crate::width::width(status_glyph);
    let kind_w = crate::width::width(kind_glyph);
    let leading_w = 2usize + 1; // `╭─` + leading space
    let trailing_w = 2usize; // `─╮`
    let status_seg = if status_w > 0 { status_w + 1 } else { 0 };
    let kind_seg = if kind_w > 0 { kind_w + 1 } else { 0 };
    let fixed = leading_w + status_seg + kind_seg + approval_w + trailing_w;
    let label_budget = (outer_width as usize).saturating_sub(fixed).max(1);
    let label_display = crate::width::middle_truncate(label, label_budget);
    let label_w = crate::width::width(label_display.as_str());
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
    if let Some((text, style)) = approval_text {
        top_spans.push(Span::styled(text, style));
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
            .map(|s| crate::width::width(s.content.as_ref()))
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

pub fn build_lines_with_ranges(
    items: &[OutputItem],
    width: u16,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u32) {
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut node_regions: Vec<NodeRegion> = Vec::new();
    let mut cursor: u32 = 0;
    for (idx, item) in items.iter().enumerate() {
        let is_hovered = ctx.hovered_thinking_idx == Some(idx);
        let item_ctx = RenderCtx {
            expanded_tools: ctx.expanded_tools,
            messages: ctx.messages,
            animation_frame: ctx.animation_frame,
            panel_width: ctx.panel_width,
            hovered_thinking_idx: if is_hovered
                && matches!(
                    item,
                    OutputItem::Thinking { .. } | OutputItem::CompactionSummary { .. }
                ) {
                Some(idx)
            } else {
                None
            },
            hovered_output_node: ctx
                .hovered_output_node
                .filter(|(item_index, _)| *item_index == idx),
        };
        let (item_lines, mut item_regions) = render_item_with_regions(item, &item_ctx, idx);
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
        node_regions.extend(item_regions.iter().cloned());
        cursor = cursor.saturating_add(rows);
        all_lines.extend(item_lines);
    }
    (all_lines, ranges, node_regions, cursor)
}

fn wrap_row_offsets(lines: &[Line<'static>], _width: u16) -> (u32, Vec<u32>) {
    // Paragraph is rendered with .scroll() but no .wrap(), so ratatui uses
    // LineTruncator: one Line always renders as one row (long lines get
    // truncated at panel width, not wrapped). Anything else here would over-
    // estimate total_rows, put follow_tail scroll past real content, and
    // produce the "session opens on blank space, scroll up to find text" bug.
    let mut offsets: Vec<u32> = Vec::with_capacity(lines.len() + 1);
    let mut cursor: u32 = 0;
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
    item_index: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    render_item_with_regions_min_workflow_rows(item, ctx, item_index, 0)
}

fn render_item_with_regions_min_workflow_rows(
    item: &OutputItem,
    ctx: &RenderCtx<'_>,
    item_index: usize,
    min_workflow_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    #[cfg(test)]
    update_perf_counters(|counters| {
        counters.item_renders = counters.item_renders.saturating_add(1);
        if matches!(
            item,
            OutputItem::WorkflowPanel { .. } | OutputItem::SubAgentActivity { .. }
        ) {
            counters.panel_projection_builds = counters.panel_projection_builds.saturating_add(1);
        }
    });
    let (mut lines, regions) = if let OutputItem::WorkflowPanel {
        graph,
        expanded_nodes,
        panel_expanded,
        cancelled,
        ..
    } = item
    {
        render_workflow_projection_with_regions_min_body_rows(
            graph,
            expanded_nodes,
            *panel_expanded,
            *cancelled,
            ctx.animation_frame,
            ctx.panel_width,
            MAX_COLLAPSED_BODY_ROWS,
            min_workflow_body_rows,
        )
    } else if let OutputItem::ToolDispatch { calls } = item {
        render_tool_dispatch(calls, ctx, item_index)
    } else {
        let lines = render_item(item, ctx);
        let regions = match item {
            OutputItem::Terminal { .. } if lines.len() >= 2 => {
                let panel_width = ctx.panel_width as usize;
                vec![NodeRegion {
                    panel_item_index: item_index,
                    path_key: TERMINAL_FULLSCREEN_KEY.to_string(),
                    start_row: 1,
                    end_row: 2,
                    col_start: panel_width.saturating_sub(6) as u16,
                    col_end: panel_width as u16,
                }]
            }
            OutputItem::Bash { .. } if lines.len() >= 2 => {
                let panel_width = ctx.panel_width as usize;
                vec![NodeRegion {
                    panel_item_index: item_index,
                    path_key: BASH_FULLSCREEN_KEY.to_string(),
                    start_row: 1,
                    end_row: 2,
                    col_start: panel_width.saturating_sub(6) as u16,
                    col_end: panel_width as u16,
                }]
            }
            OutputItem::MermaidDiagram { .. } if lines.len() >= 2 => {
                let panel_width = ctx.panel_width as usize;
                vec![NodeRegion {
                    panel_item_index: item_index,
                    path_key: MERMAID_FULLSCREEN_KEY.to_string(),
                    start_row: 1,
                    end_row: 2,
                    col_start: panel_width.saturating_sub(6) as u16,
                    col_end: panel_width as u16,
                }]
            }
            OutputItem::SubAgentActivity { .. } if lines.len() >= 2 => {
                let panel_width = ctx.panel_width as usize;
                vec![NodeRegion {
                    panel_item_index: item_index,
                    path_key: SUB_AGENT_FULLSCREEN_KEY.to_string(),
                    start_row: 1,
                    end_row: 2,
                    col_start: panel_width.saturating_sub(6) as u16,
                    col_end: panel_width as u16,
                }]
            }
            _ => Vec::new(),
        };
        (lines, regions)
    };
    ensure_external_document_gap(&mut lines);
    (lines, regions)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub width: u16,
    pub theme: crate::theme::ThemeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutRequest {
    pub scroll_offset: u32,
    pub viewport_rows: u32,
    pub follow_tail_rows: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutMetrics {
    pub total_rows: u32,
    pub scroll_offset: u32,
}

#[derive(Default)]
pub struct LayoutCache {
    key: Option<LayoutKey>,
    entries: Vec<ItemCacheEntry>,
    row_ends: Vec<u32>,
    total_rows: u32,
    store_revision: u64,
    structure_revision: u64,
    pending_layout: std::collections::BTreeSet<usize>,
    pending_paint: std::collections::BTreeSet<usize>,
    pending_structure_from: Option<usize>,
    retention_protected: std::ops::Range<usize>,
    retention_dirty: bool,
    access_clock: u64,
    work_folds: Vec<WorkFoldProjection>,
    workflow_body_rows: std::collections::HashMap<u64, usize>,
}

#[derive(Clone, Default)]
struct ItemCacheEntry {
    revision: crate::app::OutputRevision,
    rows: u32,
    lines: Option<Arc<[Line<'static>]>>,
    streaming_markdown: Option<crate::markdown::StreamingMarkdownProjection>,
    bash_output: Option<BashOutputProjection>,
    regions: Arc<[NodeRegion]>,
    semantic: Option<Arc<crate::selection::ItemSemanticSource>>,
    dynamic: DynamicPaint,
    last_used: u64,
    prefix_lines: Arc<[Line<'static>]>,
    content_hidden: bool,
    work_framed: bool,
    outer_width: u16,
    work_boundary_end: bool,
}

impl ItemCacheEntry {
    fn has_retained_lines(&self) -> bool {
        let content_retained =
            self.lines.is_some() || self.streaming_markdown.is_some() || self.bash_output.is_some();
        if self.content_hidden || self.content_row_end() <= self.prefix_lines.len() {
            !self.prefix_lines.is_empty() || content_retained
        } else {
            content_retained
        }
    }

    fn append_line_range(&self, start: usize, end: usize, out: &mut Vec<Line<'static>>) -> bool {
        let prefix_len = self.prefix_lines.len();
        let prefix_start = start.min(prefix_len);
        let prefix_end = end.min(prefix_len).max(prefix_start);
        out.extend(self.prefix_lines[prefix_start..prefix_end].iter().cloned());
        if self.content_hidden || end <= prefix_len {
            return true;
        }
        let boundary_rows = self.boundary_rows();
        let content_rows = (self.rows as usize)
            .saturating_sub(prefix_len)
            .saturating_sub(boundary_rows);
        let content_start = start.saturating_sub(prefix_len).min(content_rows);
        let content_end = end.saturating_sub(prefix_len).min(content_rows);
        let prepared = if content_start >= content_end {
            true
        } else if let Some(projection) = &self.streaming_markdown {
            let projection_rows = projection.rows();
            projection.append_range(content_start, content_end.min(projection_rows), out);
            if content_start <= projection_rows && content_end > projection_rows {
                out.push(Line::from(Span::styled(String::new(), RESET)));
            }
            true
        } else if let Some(projection) = &self.bash_output {
            projection.append_prepared_range(content_start, content_end, out)
        } else if let Some(lines) = &self.lines {
            let content_start = content_start.min(lines.len());
            let content_end = content_end.min(lines.len()).max(content_start);
            out.extend(lines[content_start..content_end].iter().cloned());
            true
        } else {
            false
        };
        if self.work_boundary_end {
            let boundary_start = prefix_len.saturating_add(content_rows);
            let suffix_start = start.saturating_sub(boundary_start).min(boundary_rows);
            let suffix_end = end.saturating_sub(boundary_start).min(boundary_rows);
            out.extend((suffix_start..suffix_end).map(|_| Line::default()));
        }
        prepared
    }

    fn boundary_rows(&self) -> usize {
        if self.work_boundary_end {
            WORK_FOLD_BOUNDARY_ROWS
        } else {
            0
        }
    }

    fn content_row_end(&self) -> usize {
        (self.rows as usize).saturating_sub(self.boundary_rows())
    }
}

impl LayoutCache {
    const OVERSCAN_ITEMS: usize = 3;
    const RECENT_ITEM_BUDGET: usize = 64;

    pub(crate) fn mark_layout_dirty(&mut self, index: usize) {
        self.pending_layout.insert(index);
    }

    pub(crate) fn mark_paint_dirty(&mut self, index: usize) {
        self.pending_paint.insert(index);
    }

    pub(crate) fn mark_structure_dirty(&mut self, index: usize) {
        self.pending_structure_from = Some(
            self.pending_structure_from
                .map_or(index, |current| current.min(index)),
        );
    }

    pub fn set_work_folds(&mut self, folds: Vec<WorkFoldProjection>) {
        if self.work_folds == folds {
            return;
        }
        let mut dirty = std::collections::BTreeSet::new();
        for old in &self.work_folds {
            let Some(new) = folds.iter().find(|fold| fold.key == old.key) else {
                dirty.extend(old.start_index..=old.end_index);
                continue;
            };
            if old.start_index != new.start_index || old.end_index != new.end_index {
                dirty.extend(old.start_index..=old.end_index);
                dirty.extend(new.start_index..=new.end_index);
                continue;
            }
            if old.completed_steps != new.completed_steps
                || old.total_steps != new.total_steps
                || old.title != new.title
                || old.stats != new.stats
                || old.hovered != new.hovered
            {
                dirty.insert(old.start_index);
            }
            let changed_start = old.visible_members.min(new.visible_members);
            let changed_end = old.visible_members.max(new.visible_members);
            for member in changed_start..changed_end {
                let index = old.start_index.saturating_add(member);
                if index <= old.end_index {
                    dirty.insert(index);
                }
            }
            if old.visible_members != new.visible_members {
                for visible_members in [old.visible_members, new.visible_members] {
                    let Some(member) = visible_members.checked_sub(1) else {
                        continue;
                    };
                    let index = old.start_index.saturating_add(member);
                    if index <= old.end_index {
                        dirty.insert(index);
                    }
                }
            }
            for member in [old.boundary_member, new.boundary_member]
                .into_iter()
                .flatten()
            {
                let index = old.start_index.saturating_add(member);
                if index <= old.end_index {
                    dirty.insert(index);
                }
            }
        }
        for new in &folds {
            if !self.work_folds.iter().any(|fold| fold.key == new.key) {
                dirty.extend(new.start_index..=new.end_index);
            }
        }
        if let Some(dirty_from) = dirty.first().copied() {
            self.pending_layout.extend(dirty);
            self.pending_structure_from = Some(
                self.pending_structure_from
                    .map_or(dirty_from, |current| current.min(dirty_from)),
            );
        }
        self.work_folds = folds;
    }

    pub(crate) fn total_rows(&self) -> u32 {
        self.total_rows
    }

    pub(crate) fn item_row_end(&self, index: usize) -> Option<u32> {
        self.row_ends.get(index).copied()
    }

    pub(crate) fn item_row_start(&self, index: usize) -> Option<u32> {
        (index < self.entries.len()).then(|| self.row_start(index))
    }

    pub fn update_dirty(
        &mut self,
        key: LayoutKey,
        items: &OutputStore,
        ctx: &RenderCtx<'_>,
        request: LayoutRequest,
    ) -> LayoutMetrics {
        if items.is_empty() {
            self.entries.clear();
            self.row_ends.clear();
            self.workflow_body_rows.clear();
            self.total_rows = 0;
            self.store_revision = items.revision_clock();
            self.structure_revision = items.structure_revision();
            self.pending_layout.clear();
            self.pending_paint.clear();
            self.pending_structure_from = None;
            self.retention_protected = 0..0;
            self.retention_dirty = false;
            self.key = Some(key);
            return LayoutMetrics {
                total_rows: 0,
                scroll_offset: 0,
            };
        }

        let full_invalidation = self
            .key
            .is_none_or(|previous| previous.width != key.width || previous.theme != key.theme);
        if full_invalidation {
            self.entries.clear();
            self.row_ends.clear();
            self.pending_layout.clear();
            self.pending_paint.clear();
            self.pending_structure_from = Some(0);
            self.retention_protected = 0..0;
            self.retention_dirty = false;
        }

        let revisions = items.revisions();
        let structure_changed = self.structure_revision != items.structure_revision()
            || self.entries.len() != items.len();
        if structure_changed {
            self.retention_dirty = true;
            let live_ids = revisions
                .iter()
                .map(|revision| revision.id)
                .collect::<std::collections::HashSet<_>>();
            self.workflow_body_rows
                .retain(|id, _| live_ids.contains(id));
            let changed_from = self.pending_structure_from.unwrap_or(0).min(items.len());
            if changed_from == self.entries.len() && self.entries.len() <= items.len() {
                self.entries.resize(items.len(), ItemCacheEntry::default());
                self.row_ends.resize(items.len(), self.total_rows);
            } else {
                let mut by_id = std::mem::take(&mut self.entries)
                    .into_iter()
                    .filter(|entry| entry.revision.id != 0)
                    .map(|entry| (entry.revision.id, entry))
                    .collect::<std::collections::HashMap<_, _>>();
                self.entries = revisions
                    .iter()
                    .map(|revision| by_id.remove(&revision.id).unwrap_or_default())
                    .collect();
                self.row_ends.resize(items.len(), 0);
            }
            for (idx, revision) in revisions.iter().enumerate().skip(changed_from) {
                #[cfg(test)]
                update_perf_counters(|counters| {
                    counters.semantic_item_visits = counters.semantic_item_visits.saturating_add(1);
                });
                let cached = self.entries[idx].revision;
                if cached.id != revision.id || cached.layout != revision.layout {
                    self.pending_layout.insert(idx);
                } else if cached.paint != revision.paint {
                    self.pending_paint.insert(idx);
                }
            }
            self.pending_structure_from = Some(changed_from);
        }

        if self.store_revision != items.revision_clock()
            && self.pending_layout.is_empty()
            && self.pending_paint.is_empty()
            && !structure_changed
        {
            for (idx, revision) in revisions.iter().enumerate() {
                #[cfg(test)]
                update_perf_counters(|counters| {
                    counters.semantic_item_visits = counters.semantic_item_visits.saturating_add(1);
                });
                let cached = self.entries[idx].revision;
                if cached.layout != revision.layout {
                    self.pending_layout.insert(idx);
                } else if cached.paint != revision.paint {
                    self.pending_paint.insert(idx);
                }
            }
        }

        let mut offsets_dirty_from = self.pending_structure_from.take();
        let layout_dirty = std::mem::take(&mut self.pending_layout);
        for idx in layout_dirty {
            if idx >= items.len() {
                continue;
            }
            let retain = idx.saturating_add(Self::RECENT_ITEM_BUDGET) >= items.len();
            if self.render_entry(
                idx,
                &items[idx],
                revisions[idx],
                ctx,
                retain,
                full_invalidation || structure_changed,
            ) {
                offsets_dirty_from =
                    Some(offsets_dirty_from.map_or(idx, |current| current.min(idx)));
            } else {
                self.pending_layout.insert(idx);
            }
        }

        let paint_dirty = std::mem::take(&mut self.pending_paint);
        for idx in paint_dirty {
            if idx >= items.len() {
                continue;
            }
            let old_rows = self.entries[idx].rows;
            let retain = self.entries[idx].has_retained_lines()
                || idx.saturating_add(Self::RECENT_ITEM_BUDGET) >= items.len();
            if !self.render_entry(idx, &items[idx], revisions[idx], ctx, retain, false) {
                self.pending_layout.insert(idx);
            } else if self.entries[idx].rows != old_rows {
                offsets_dirty_from =
                    Some(offsets_dirty_from.map_or(idx, |current| current.min(idx)));
            }
        }

        if let Some(from) = offsets_dirty_from {
            self.rebuild_row_offsets(from);
        } else if self.row_ends.len() != self.entries.len() {
            self.rebuild_row_offsets(0);
        }

        let scroll_offset = request
            .follow_tail_rows
            .map_or(request.scroll_offset, |visible_rows| {
                self.total_rows.saturating_sub(visible_rows.max(1))
            });
        let (visible_start, visible_end) = self.item_window(scroll_offset, request.viewport_rows);
        let retain_start = visible_start.saturating_sub(Self::OVERSCAN_ITEMS);
        let retain_end = visible_end
            .saturating_add(Self::OVERSCAN_ITEMS)
            .min(items.len());
        for idx in retain_start..retain_end {
            if !self.entries[idx].has_retained_lines() {
                let old_rows = self.entries[idx].rows;
                let rendered = self.render_entry(idx, &items[idx], revisions[idx], ctx, true, true);
                debug_assert!(rendered);
                debug_assert_eq!(self.entries[idx].rows, old_rows);
            } else {
                self.touch_entry(idx);
            }
        }
        for idx in visible_start..visible_end {
            let start = self.row_start(idx);
            let end = self.row_ends[idx];
            let local_start = scroll_offset.saturating_sub(start) as usize;
            let local_end = end
                .min(scroll_offset.saturating_add(request.viewport_rows))
                .saturating_sub(start) as usize;
            if let OutputItem::Bash { output, .. } = &items[idx] {
                let entry = &mut self.entries[idx];
                let content_end = entry.content_row_end();
                if !entry.content_hidden
                    && let Some(projection) = &mut entry.bash_output
                {
                    let prefix = entry.prefix_lines.len();
                    let _materialized = projection.prepare_range(
                        output,
                        local_start.saturating_sub(prefix),
                        local_end.min(content_end).saturating_sub(prefix),
                    );
                    #[cfg(test)]
                    update_perf_counters(|counters| {
                        counters.bash_materialized_rows = counters
                            .bash_materialized_rows
                            .saturating_add(_materialized as u64);
                    });
                }
            }
        }
        self.prune_lines(retain_start..retain_end);

        self.key = Some(key);
        self.store_revision = items.revision_clock();
        self.structure_revision = items.structure_revision();
        LayoutMetrics {
            total_rows: self.total_rows,
            scroll_offset,
        }
    }

    pub fn visible_slice(
        &self,
        scroll_offset: u32,
        viewport_rows: u32,
        animation_frame: u32,
    ) -> VisibleSlice {
        let (start_idx, end_idx) = self.item_window(scroll_offset, viewport_rows);
        let vis_bottom = scroll_offset.saturating_add(viewport_rows);
        let mut lines = Vec::new();
        let mut ranges = Vec::with_capacity(end_idx.saturating_sub(start_idx));
        let mut regions = Vec::new();
        let mut surfaces = Vec::new();
        for idx in start_idx..end_idx {
            let entry = &self.entries[idx];
            let work_fold_hovered = self
                .work_folds
                .iter()
                .find(|fold| idx >= fold.start_index && idx <= fold.end_index)
                .is_some_and(|fold| fold.hovered);
            let start = self.row_start(idx);
            let end = self.row_ends[idx];
            if !entry.has_retained_lines() {
                debug_assert!(false, "visible entries must be prepared by update_dirty");
                continue;
            }
            let skip = scroll_offset.saturating_sub(start) as usize;
            let take = end.min(vis_bottom).saturating_sub(start.max(scroll_offset)) as usize;
            let row_count = entry.rows as usize;
            let lo = skip.min(row_count);
            let hi = skip.saturating_add(take).min(row_count);
            if entry.dynamic.active {
                #[cfg(test)]
                update_perf_counters(|counters| {
                    counters.animation_item_visits =
                        counters.animation_item_visits.saturating_add(1);
                });
            }
            let slice_start = lines.len();
            let prepared = entry.append_line_range(lo, hi, &mut lines);
            debug_assert!(prepared);
            for (line_index, line) in lines[slice_start..].iter_mut().enumerate() {
                patch_animation_line(
                    line,
                    lo.saturating_add(line_index),
                    &entry.dynamic,
                    animation_frame,
                );
                let local_row = lo.saturating_add(line_index);
                if entry.work_framed
                    && local_row >= entry.prefix_lines.len()
                    && local_row < entry.content_row_end()
                {
                    frame_work_fold_content_line(line, entry.outer_width, work_fold_hovered);
                } else if entry.work_boundary_end && local_row == entry.content_row_end() {
                    *line = render_work_fold_footer(entry.outer_width, work_fold_hovered);
                }
            }
            ranges.push(ItemRange {
                item_index: idx,
                start_row: start,
                end_row: end,
            });
            regions.extend(entry.regions.iter().map(|region| NodeRegion {
                panel_item_index: idx,
                path_key: region.path_key.clone(),
                start_row: region.start_row.saturating_add(start),
                end_row: region.end_row.saturating_add(start),
                col_start: region.col_start,
                col_end: region.col_end,
            }));
            if let Some(source) = &entry.semantic {
                let row_offset = entry.prefix_lines.len().min(u32::MAX as usize) as u32;
                let col_offset = if entry.work_framed {
                    work_fold_content_offset(entry.outer_width)
                } else {
                    0
                };
                let prose_atoms = if entry.content_hidden {
                    Vec::new()
                } else {
                    source
                        .prose_atoms
                        .iter()
                        .filter_map(|atom| {
                            let screen_row = start
                                .saturating_add(row_offset)
                                .saturating_add(u32::from(atom.row));
                            (screen_row >= scroll_offset && screen_row < vis_bottom).then(|| {
                                crate::selection::VisibleProseAtom {
                                    atom: crate::selection::RelativeProseAtom {
                                        row: atom.row,
                                        cols: atom.cols.start.saturating_add(col_offset)
                                            ..atom.cols.end.saturating_add(col_offset),
                                        cell_width: atom.cell_width,
                                        event: atom.event,
                                        event_graphemes: atom.event_graphemes.clone(),
                                    },
                                    screen_row,
                                }
                            })
                        })
                        .collect()
                };
                let code_atoms = if entry.content_hidden {
                    Vec::new()
                } else {
                    source
                        .code_atoms
                        .iter()
                        .filter_map(|code| {
                            let screen_row = start
                                .saturating_add(row_offset)
                                .saturating_add(u32::from(code.atom.row));
                            (screen_row >= scroll_offset && screen_row < vis_bottom).then(|| {
                                crate::selection::VisibleCodeAtom {
                                    domain: code.domain.clone(),
                                    grapheme_start: code.grapheme_start,
                                    atom: crate::selection::VisibleAtom {
                                        screen_row,
                                        cols: code.atom.cols.start.saturating_add(col_offset)
                                            ..code.atom.cols.end.saturating_add(col_offset),
                                        cell_width: code.atom.cell_width,
                                        source: code.atom.source.clone(),
                                    },
                                }
                            })
                        })
                        .collect()
                };
                let isolated_atoms = if entry.content_hidden {
                    Vec::new()
                } else {
                    source
                        .isolated_atoms
                        .iter()
                        .filter_map(|isolated| match isolated {
                            crate::selection::RelativeIsolatedAtom::Markdown { domain, atom } => {
                                let screen_row = start
                                    .saturating_add(row_offset)
                                    .saturating_add(u32::from(atom.row));
                                (screen_row >= scroll_offset && screen_row < vis_bottom).then(
                                    || crate::selection::VisibleIsolatedAtom::Markdown {
                                        domain: domain.clone(),
                                        atom: crate::selection::RelativeProseAtom {
                                            row: atom.row,
                                            cols: atom.cols.start.saturating_add(col_offset)
                                                ..atom.cols.end.saturating_add(col_offset),
                                            cell_width: atom.cell_width,
                                            event: atom.event,
                                            event_graphemes: atom.event_graphemes.clone(),
                                        },
                                        screen_row,
                                    },
                                )
                            }
                            crate::selection::RelativeIsolatedAtom::Raw { domain, atom } => {
                                let screen_row = start
                                    .saturating_add(row_offset)
                                    .saturating_add(u32::from(atom.row));
                                (screen_row >= scroll_offset && screen_row < vis_bottom).then(
                                    || crate::selection::VisibleIsolatedAtom::Raw {
                                        domain: domain.clone(),
                                        atom: crate::selection::VisibleAtom {
                                            screen_row,
                                            cols: atom.cols.start.saturating_add(col_offset)
                                                ..atom.cols.end.saturating_add(col_offset),
                                            cell_width: atom.cell_width,
                                            source: atom.source.clone(),
                                        },
                                    },
                                )
                            }
                        })
                        .collect()
                };
                surfaces.push(crate::selection::VisibleSurface {
                    item_index: idx,
                    revision: entry.revision,
                    start_row: start,
                    end_row: end,
                    source: Arc::clone(source),
                    prose_atoms,
                    code_atoms,
                    isolated_atoms,
                });
            }
        }
        VisibleSlice {
            lines,
            ranges,
            regions,
            selection: crate::selection::VisibleSelectionProjection {
                structure_revision: self.structure_revision,
                surfaces,
            },
        }
    }

    fn render_entry(
        &mut self,
        idx: usize,
        item: &OutputItem,
        revision: crate::app::OutputRevision,
        ctx: &RenderCtx<'_>,
        retain_lines: bool,
        force_streaming: bool,
    ) -> bool {
        let retained_before = self.entries[idx].has_retained_lines();
        let work_fold = self
            .work_folds
            .iter()
            .find(|fold| idx >= fold.start_index && idx <= fold.end_index);
        let content_width = if work_fold.is_some() {
            work_fold_content_width(ctx.panel_width)
        } else {
            ctx.panel_width
        };
        if let OutputItem::AssistantMd {
            md,
            streaming: true,
            retried: false,
        } = item
        {
            let mut projection = self.entries[idx]
                .streaming_markdown
                .take()
                .unwrap_or_else(|| {
                    crate::markdown::StreamingMarkdownProjection::new(revision.source_generation)
                });
            if matches!(
                projection.update(
                    md,
                    revision.source_generation,
                    content_width,
                    std::time::Instant::now(),
                    force_streaming,
                ),
                crate::markdown::StreamingProjectionUpdate::Deferred
            ) {
                self.entries[idx].streaming_markdown = Some(projection);
                return false;
            }
            #[cfg(test)]
            update_perf_counters(|counters| {
                counters.item_renders = counters.item_renders.saturating_add(1);
            });
            self.access_clock = self.access_clock.wrapping_add(1);
            self.entries[idx] = ItemCacheEntry {
                revision,
                rows: projection.rows().saturating_add(1).min(u32::MAX as usize) as u32,
                lines: None,
                streaming_markdown: Some(projection),
                bash_output: None,
                regions: Arc::from([]),
                // Streaming tails reparse per chunk; projecting here would be O(n²).
                // The finalized item re-renders through the generic path and projects then.
                semantic: None,
                dynamic: DynamicPaint::default(),
                last_used: self.access_clock,
                prefix_lines: Arc::from([]),
                content_hidden: false,
                work_framed: false,
                outer_width: ctx.panel_width,
                work_boundary_end: false,
            };
            apply_work_fold_to_entry(&mut self.entries[idx], work_fold, idx, ctx.panel_width);
            self.retention_dirty |= !retained_before;
            return true;
        }
        if let OutputItem::Bash {
            handle,
            title,
            command,
            output,
            done,
            expanded,
        } = item
        {
            let mut projection = self.entries[idx].bash_output.take().unwrap_or_default();
            let _indexed_bytes = projection.update(BashProjectionInput {
                handle,
                title: title.as_deref(),
                command: command.as_deref(),
                output,
                generation: revision.source_generation,
                done: *done,
                expanded: *expanded,
                panel_width: content_width,
                fullscreen_hovered: ctx.hovered_output_node.is_some_and(|(_, key)| {
                    key == BASH_FULLSCREEN_KEY
                        || key.starts_with(TOOL_DETAIL_FULLSCREEN_REGION_PREFIX)
                }),
            });
            #[cfg(test)]
            update_perf_counters(|counters| {
                counters.item_renders = counters.item_renders.saturating_add(1);
                counters.bash_source_bytes = counters
                    .bash_source_bytes
                    .saturating_add(_indexed_bytes as u64);
            });
            self.access_clock = self.access_clock.wrapping_add(1);
            let rows = projection.rows().min(u32::MAX as usize) as u32;
            self.entries[idx] = ItemCacheEntry {
                revision,
                rows,
                lines: None,
                streaming_markdown: None,
                bash_output: retain_lines.then_some(projection),
                regions: if retain_lines {
                    Arc::from([NodeRegion {
                        panel_item_index: idx,
                        path_key: BASH_FULLSCREEN_KEY.to_string(),
                        start_row: 1,
                        end_row: 2,
                        col_start: content_width.saturating_sub(6),
                        col_end: content_width,
                    }])
                } else {
                    Arc::from([])
                },
                semantic: retain_lines.then(|| {
                    let mut semantic = crate::selection::item_semantic_source(item, revision);
                    populate_isolated_geometry(item, &mut semantic, content_width);
                    Arc::new(semantic)
                }),
                dynamic: DynamicPaint {
                    active: !*done,
                    elapsed: None,
                    running_rows: Vec::new(),
                },
                last_used: self.access_clock,
                prefix_lines: Arc::from([]),
                content_hidden: false,
                work_framed: false,
                outer_width: ctx.panel_width,
                work_boundary_end: false,
            };
            apply_work_fold_to_entry(&mut self.entries[idx], work_fold, idx, ctx.panel_width);
            self.retention_dirty |= retained_before != retain_lines;
            return true;
        }
        let hovered = ctx.hovered_thinking_idx == Some(idx);
        let item_ctx = RenderCtx {
            expanded_tools: ctx.expanded_tools,
            messages: ctx.messages,
            animation_frame: if item.has_dynamic_paint() {
                LAYOUT_ANIMATION_FRAME
            } else {
                ctx.animation_frame
            },
            panel_width: content_width,
            hovered_thinking_idx: (hovered
                && matches!(
                    item,
                    OutputItem::Thinking { .. } | OutputItem::CompactionSummary { .. }
                ))
            .then_some(idx),
            hovered_output_node: ctx
                .hovered_output_node
                .filter(|(item_index, _)| *item_index == idx),
        };
        let min_workflow_body_rows = if matches!(
            item,
            OutputItem::WorkflowPanel {
                panel_expanded: false,
                ..
            }
        ) {
            self.workflow_body_rows
                .get(&revision.id)
                .copied()
                .unwrap_or(0)
        } else {
            0
        };
        let (lines, regions, semantic) = if let OutputItem::AssistantMd {
            md,
            streaming,
            retried,
        } = item
        {
            let rendered = render_assistant_with_geometry(md, *streaming, *retried, content_width);
            let mut semantic = crate::selection::item_semantic_source(item, revision);
            semantic.prose_atoms = rendered.prose_atoms.clone();
            semantic.code_atoms =
                crate::selection::code_atoms(&semantic.code_blocks, &rendered.code_blocks);
            let mut lines = rendered.lines;
            lines.push(Line::from(Span::styled(String::new(), RESET)));
            (lines, Vec::new(), semantic)
        } else if let OutputItem::UserTurn { text, presentation }
        | OutputItem::Interjection { text, presentation } = item
        {
            let lines = render_user_turn(
                text,
                presentation.as_ref(),
                content_width,
                matches!(item, OutputItem::Interjection { .. }),
            );
            let mut semantic = crate::selection::item_semantic_source(item, revision);
            semantic.prose_atoms = if let Some(presentation) = presentation.as_ref() {
                presented_user_prose_atoms(
                    presentation,
                    usize::from(content_width),
                    matches!(item, OutputItem::Interjection { .. }),
                )
            } else {
                let mut atoms = user_turn_prose_atoms(text, usize::from(content_width));
                if matches!(item, OutputItem::Interjection { .. }) {
                    for atom in &mut atoms {
                        atom.row = atom.row.saturating_add(1);
                    }
                }
                atoms
            };
            (lines, Vec::new(), semantic)
        } else {
            let (lines, regions) = render_item_with_regions_min_workflow_rows(
                item,
                &item_ctx,
                idx,
                min_workflow_body_rows,
            );
            let mut semantic = crate::selection::item_semantic_source(item, revision);
            populate_isolated_geometry(item, &mut semantic, content_width);
            (lines, regions, semantic)
        };
        if matches!(
            item,
            OutputItem::WorkflowPanel {
                panel_expanded: false,
                ..
            }
        ) {
            self.workflow_body_rows.insert(
                revision.id,
                min_workflow_body_rows.max(lines.len().saturating_sub(3)),
            );
        }
        let rows = lines.len().min(u32::MAX as usize) as u32;
        let dynamic = dynamic_paint_for_item(item, &lines, &item_ctx);
        self.access_clock = self.access_clock.wrapping_add(1);
        self.entries[idx] = ItemCacheEntry {
            revision,
            rows,
            lines: retain_lines.then(|| Arc::from(lines)),
            streaming_markdown: None,
            bash_output: None,
            regions: if retain_lines {
                Arc::from(regions)
            } else {
                Arc::from([])
            },
            semantic: retain_lines.then(|| Arc::new(semantic)),
            dynamic,
            last_used: self.access_clock,
            prefix_lines: Arc::from([]),
            content_hidden: false,
            work_framed: false,
            outer_width: ctx.panel_width,
            work_boundary_end: false,
        };
        apply_work_fold_to_entry(&mut self.entries[idx], work_fold, idx, ctx.panel_width);
        self.retention_dirty |= retained_before != retain_lines;
        true
    }

    fn rebuild_row_offsets(&mut self, from: usize) {
        self.row_ends.resize(self.entries.len(), 0);
        let mut cursor = if from == 0 {
            0
        } else {
            self.row_ends[from.saturating_sub(1)]
        };
        for idx in from..self.entries.len() {
            cursor = cursor.saturating_add(self.entries[idx].rows);
            self.row_ends[idx] = cursor;
        }
        self.total_rows = self.row_ends.last().copied().unwrap_or(0);
    }

    fn row_start(&self, index: usize) -> u32 {
        index
            .checked_sub(1)
            .and_then(|previous| self.row_ends.get(previous).copied())
            .unwrap_or(0)
    }

    fn item_window(&self, scroll_offset: u32, viewport_rows: u32) -> (usize, usize) {
        if viewport_rows == 0 || self.entries.is_empty() {
            return (0, 0);
        }
        let bottom = scroll_offset.saturating_add(viewport_rows);
        let start = self.row_ends.partition_point(|end| *end <= scroll_offset);
        let end = self
            .row_ends
            .partition_point(|end| *end < bottom)
            .saturating_add(1)
            .min(self.entries.len());
        (start.min(end), end)
    }

    fn touch_entry(&mut self, index: usize) {
        self.access_clock = self.access_clock.wrapping_add(1);
        self.entries[index].last_used = self.access_clock;
    }

    fn prune_lines(&mut self, protected: std::ops::Range<usize>) {
        if self.retention_protected != protected {
            self.retention_protected = protected.clone();
            self.retention_dirty = true;
        }
        if !self.retention_dirty {
            return;
        }
        self.retention_dirty = false;
        #[cfg(test)]
        update_perf_counters(|counters| {
            counters.retention_item_visits = counters
                .retention_item_visits
                .saturating_add(self.entries.len() as u64);
        });
        let mut recent = self
            .entries
            .iter()
            .enumerate()
            .filter(|(idx, entry)| !protected.contains(idx) && entry.has_retained_lines())
            .map(|(idx, entry)| (entry.last_used, idx))
            .collect::<Vec<_>>();
        recent.sort_unstable_by(|left, right| right.cmp(left));
        for (_, idx) in recent.into_iter().skip(Self::RECENT_ITEM_BUDGET) {
            self.entries[idx].lines = None;
            self.entries[idx].streaming_markdown = None;
            self.entries[idx].bash_output = None;
            self.entries[idx].regions = Arc::from([]);
            self.entries[idx].semantic = None;
        }
    }

    pub fn invalidate(&mut self) {
        self.key = None;
        self.pending_structure_from = Some(0);
        self.workflow_body_rows.clear();
    }

    #[cfg(test)]
    fn retained_item_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.has_retained_lines())
            .count()
    }
}

impl std::fmt::Debug for LayoutCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCache")
            .field("key", &self.key)
            .field("total_rows", &self.total_rows)
            .field("retained_items", &self.retained_item_count_for_debug())
            .finish()
    }
}

impl LayoutCache {
    fn retained_item_count_for_debug(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.has_retained_lines())
            .count()
    }
}

fn apply_work_fold_to_entry(
    entry: &mut ItemCacheEntry,
    fold: Option<&WorkFoldProjection>,
    item_index: usize,
    panel_width: u16,
) {
    let Some(fold) = fold else {
        return;
    };
    let member = item_index.saturating_sub(fold.start_index);
    let visible = member < fold.visible_members;
    let mut prefix = if item_index == fold.start_index {
        render_work_fold_header(fold, panel_width)
    } else {
        Vec::new()
    };
    let header_rows = prefix.len().min(u32::MAX as usize) as u32;
    if item_index == fold.start_index {
        if visible {
            prefix.push(render_work_fold_top_padding(panel_width, fold.hovered));
        } else {
            // The colored row belongs to the header's internal padding. This
            // unstyled row is the block margin before whatever follows it.
            prefix.push(Line::default());
        }
    }
    let prefix_rows = prefix.len().min(u32::MAX as usize) as u32;
    entry.prefix_lines = Arc::from(prefix);
    entry.content_hidden = !visible;
    entry.work_framed = true;
    entry.outer_width = panel_width;
    entry.work_boundary_end = false;
    if visible {
        entry.work_boundary_end = member.saturating_add(1) == fold.visible_members;
        if fold.boundary_member == Some(member) {
            fade_work_fold_boundary(entry, fold.boundary_level);
        }
        entry.rows = entry
            .rows
            .saturating_add(prefix_rows)
            .saturating_add(entry.boundary_rows().min(u32::MAX as usize) as u32);
        entry.regions = Arc::from(
            entry
                .regions
                .iter()
                .cloned()
                .map(|mut region| {
                    region.start_row = region.start_row.saturating_add(prefix_rows);
                    region.end_row = region.end_row.saturating_add(prefix_rows);
                    let content_offset = work_fold_content_offset(panel_width);
                    region.col_start = region
                        .col_start
                        .saturating_add(content_offset)
                        .min(panel_width);
                    region.col_end = region
                        .col_end
                        .saturating_add(content_offset)
                        .min(panel_width.saturating_sub(content_offset))
                        .max(region.col_start);
                    region
                })
                .collect::<Vec<_>>(),
        );
        if prefix_rows > 0 {
            for row in &mut entry.dynamic.running_rows {
                *row = row.saturating_add(prefix_rows as usize);
            }
            if let Some(elapsed) = entry.dynamic.elapsed.as_mut() {
                elapsed.line = elapsed.line.saturating_add(prefix_rows as usize);
            }
        }
    } else {
        entry.rows = prefix_rows;
        entry.regions = Arc::from([]);
        entry.dynamic = DynamicPaint::default();
    }
    if item_index == fold.start_index {
        let mut regions = entry.regions.to_vec();
        regions.push(NodeRegion {
            panel_item_index: item_index,
            path_key: format!("{WORK_FOLD_REGION_PREFIX}{}", fold.key),
            start_row: 0,
            end_row: header_rows,
            col_start: 0,
            col_end: panel_width,
        });
        entry.regions = Arc::from(regions);
    }
}

fn work_fold_frame_widths(outer_width: u16) -> Option<(usize, usize)> {
    if outer_width < 3 {
        return None;
    }
    let between_rails = usize::from(outer_width.saturating_sub(WORK_FOLD_RAIL_WIDTH * 2));
    let padding = usize::from(WORK_FOLD_CONTENT_PADDING).min(between_rails.saturating_sub(1) / 2);
    let inner_width = between_rails.saturating_sub(padding.saturating_mul(2));
    Some((padding, inner_width))
}

fn work_fold_content_width(outer_width: u16) -> u16 {
    work_fold_frame_widths(outer_width).map_or(outer_width.max(1), |(_, inner_width)| {
        inner_width.min(u16::MAX as usize) as u16
    })
}

fn work_fold_content_offset(outer_width: u16) -> u16 {
    work_fold_frame_widths(outer_width).map_or(0, |(padding, _)| {
        WORK_FOLD_RAIL_WIDTH.saturating_add(padding.min(u16::MAX as usize) as u16)
    })
}

fn work_fold_boundary_style(hovered: bool) -> Style {
    let t = crate::theme::theme();
    let foreground = if hovered {
        t.work_boundary_fg.lerp(t.accent, 0.28)
    } else {
        t.work_boundary_fg.into()
    };
    Style::default().fg(foreground)
}

fn frame_work_fold_content_line(line: &mut Line<'static>, outer_width: u16, hovered: bool) {
    let Some((padding, inner_width)) = work_fold_frame_widths(outer_width) else {
        return;
    };
    let outer_width = outer_width as usize;
    let content = crate::width::truncate_spans(std::mem::take(&mut line.spans), inner_width, None);
    let used = crate::width::spans_width(content.iter());
    let mut spans = Vec::with_capacity(content.len() + 5);
    spans.push(Span::styled("⠇", work_fold_boundary_style(hovered)));
    spans.push(Span::raw(" ".repeat(padding)));
    spans.extend(content);
    spans.push(Span::raw(" ".repeat(inner_width.saturating_sub(used))));
    spans.push(Span::raw(" ".repeat(padding)));
    spans.push(Span::styled("⠸", work_fold_boundary_style(hovered)));
    debug_assert_eq!(
        crate::width::spans_width(spans.iter()),
        outer_width,
        "work fold frame must preserve the panel width"
    );
    line.spans = spans;
}

fn render_work_fold_top_padding(outer_width: u16, hovered: bool) -> Line<'static> {
    let mut line = Line::default();
    frame_work_fold_content_line(&mut line, outer_width, hovered);
    line
}

fn render_work_fold_footer(outer_width: u16, hovered: bool) -> Line<'static> {
    let Some(between_rails) = outer_width.checked_sub(WORK_FOLD_RAIL_WIDTH * 2) else {
        return Line::from(Span::raw(" ".repeat(outer_width as usize)));
    };
    Line::from(Span::styled(
        format!("⠧{}⠼", "⠤".repeat(between_rails as usize)),
        work_fold_boundary_style(hovered),
    ))
}

fn fade_work_fold_boundary(entry: &mut ItemCacheEntry, level: u8) {
    let Some(lines) = entry.lines.as_ref() else {
        return;
    };
    let t = crate::theme::theme();
    let brightness = f64::from(level.clamp(1, 3)) / 3.0;
    let mut faded = lines.to_vec();
    for line in &mut faded {
        for span in &mut line.spans {
            let foreground = span.style.fg.unwrap_or_else(|| t.tinted_fg.into());
            span.style.fg = Some(t.code_bg.lerp(foreground, brightness));
            if level == 1 {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
        }
    }
    entry.lines = Some(Arc::from(faded));
}

fn render_work_fold_header(fold: &WorkFoldProjection, panel_width: u16) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let background = if fold.hovered {
        t.work_hover_bg.into()
    } else {
        t.work_bg.into()
    };
    let panel_width = panel_width as usize;
    let title = format!("  work · {}/{}", fold.completed_steps, fold.total_steps);
    let title_spans = vec![
        Span::styled("∴", Style::default().fg(t.accent.into()).bg(background)),
        Span::styled(
            title,
            Style::default().fg(t.work_title_fg.into()).bg(background),
        ),
    ];
    let stats_spans = if fold.stats.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(
            fold.stats.clone(),
            Style::default()
                .fg(t.work_meta_fg.into())
                .bg(background)
                .add_modifier(Modifier::DIM),
        )]
    };
    let title_line = aligned_document_row(title_spans, stats_spans, panel_width, background);

    let summary_style = Style::default()
        .fg(t.work_meta_fg.into())
        .bg(background)
        .add_modifier(Modifier::DIM);
    let summary_prefix = format!("{DOCUMENT_PAD}   ");
    let summary_lines =
        wrap_with_prefix(&fold.title, panel_width, &summary_prefix, &summary_prefix)
            .into_iter()
            .map(|row| {
                line_with_right_pad(
                    &row.prefix,
                    &row.body,
                    panel_width,
                    Style::default().bg(background),
                    summary_style,
                )
            });

    let blank = Line::from(Span::styled(
        " ".repeat(panel_width),
        Style::default().bg(background),
    ));
    let mut lines = vec![blank.clone(), title_line, blank.clone()];
    lines.extend(summary_lines);
    lines.push(blank);
    lines
}

// Subtle stripe behind user messages so they visually separate from
// assistant markdown without a heavy border or gutter glyph.
fn user_message_bg() -> Color {
    crate::theme::theme().user_msg_bg.into()
}

const DOCUMENT_PAD: &str = "  ";
const DOCUMENT_PAD_X: usize = DOCUMENT_PAD.len();
const RIGHT_PAD: usize = DOCUMENT_PAD_X;

pub struct PaddedRow {
    pub prefix: String,
    pub body: String,
}

pub(crate) fn user_turn_prose_atoms(
    text: &str,
    target: usize,
) -> Vec<crate::selection::RelativeProseAtom> {
    let first_prefix = format!("{DOCUMENT_PAD}❯{DOCUMENT_PAD}");
    let continuation = " ".repeat(crate::width::width(&first_prefix));
    let rows = wrap_with_prefix(text, target, &first_prefix, &continuation);
    let mut source_cursor = 0usize;
    let mut source_grapheme = 0usize;
    let mut atoms = Vec::new();

    for (row, padded) in rows.into_iter().enumerate() {
        if padded.body.is_empty() {
            if text[source_cursor..].starts_with('\n') {
                source_cursor = source_cursor.saturating_add(1);
                source_grapheme = source_grapheme.saturating_add(1);
            }
            continue;
        }
        let Some(relative) = text[source_cursor..].find(&padded.body) else {
            continue;
        };
        let body_start = source_cursor.saturating_add(relative);
        let grapheme_start = source_grapheme
            .saturating_add(crate::width::graphemes(&text[source_cursor..body_start]).count());
        atoms.extend(crate::selection::prose_atom_runs(
            u16::try_from(row.saturating_add(1)).unwrap_or(u16::MAX),
            u16::try_from(crate::width::width(&padded.prefix)).unwrap_or(u16::MAX),
            &padded.body,
            0,
            grapheme_start,
        ));
        source_cursor = body_start.saturating_add(padded.body.len());
        source_grapheme =
            grapheme_start.saturating_add(crate::width::graphemes(&padded.body).count());
    }
    atoms
}

fn presented_user_prose_atoms(
    presentation: &atman_runtime::user_input::UserInputPresentation,
    target: usize,
    inserted: bool,
) -> Vec<crate::selection::RelativeProseAtom> {
    let source = presentation.model_text();
    let mut source_cursor = 0usize;
    let mut row = 1usize + usize::from(inserted);
    let mut atoms = Vec::new();
    let mut append_rows = |text: &str, prefix: &str, continuation: &str, row: &mut usize| {
        for padded in wrap_with_prefix(text, target, prefix, continuation) {
            if !padded.body.is_empty()
                && let Some(relative) = source[source_cursor..].find(&padded.body)
            {
                let start = source_cursor + relative;
                let grapheme_start = crate::width::graphemes(&source[..start]).count();
                atoms.extend(crate::selection::prose_atom_runs(
                    u16::try_from(*row).unwrap_or(u16::MAX),
                    u16::try_from(crate::width::width(&padded.prefix)).unwrap_or(u16::MAX),
                    &padded.body,
                    0,
                    grapheme_start,
                ));
                source_cursor = start + padded.body.len();
            }
            *row += 1;
        }
    };
    if let Some(quote) = &presentation.quote {
        row += 1;
        let prefix = format!("{DOCUMENT_PAD}│ ");
        for line in quote.text.lines() {
            append_rows(line, &prefix, &prefix, &mut row);
        }
    }
    if !presentation.prompt.is_empty() {
        let first = format!("{DOCUMENT_PAD}❯{DOCUMENT_PAD}");
        let continuation = " ".repeat(crate::width::width(&first));
        append_rows(&presentation.prompt, &first, &continuation, &mut row);
    }
    atoms
}

pub fn wrap_with_prefix(
    text: &str,
    target: usize,
    first_prefix: &str,
    cont_prefix: &str,
) -> Vec<PaddedRow> {
    let prefix_w = crate::width::width(cont_prefix);
    let body_w = target
        .saturating_sub(prefix_w)
        .saturating_sub(RIGHT_PAD)
        .max(1);
    let first_prefix_w = crate::width::width(first_prefix);
    let first_body_w = target
        .saturating_sub(first_prefix_w)
        .saturating_sub(RIGHT_PAD)
        .max(1);

    let mut out = Vec::new();
    let mut first_row = true;
    for row in text.split('\n') {
        let limit = if first_row { first_body_w } else { body_w };
        if row.is_empty() {
            let prefix = if first_row { first_prefix } else { cont_prefix };
            out.push(PaddedRow {
                prefix: prefix.to_string(),
                body: String::new(),
            });
            first_row = false;
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for (g, gw) in crate::width::graphemes(row) {
            if cur_w + gw > limit && !cur.is_empty() {
                let prefix = if first_row { first_prefix } else { cont_prefix };
                out.push(PaddedRow {
                    prefix: prefix.to_string(),
                    body: std::mem::take(&mut cur),
                });
                first_row = false;
                cur_w = 0;
            }
            cur.push_str(g);
            cur_w += gw;
        }
        let prefix = if first_row { first_prefix } else { cont_prefix };
        out.push(PaddedRow {
            prefix: prefix.to_string(),
            body: cur,
        });
        first_row = false;
    }
    if out.is_empty() {
        out.push(PaddedRow {
            prefix: first_prefix.to_string(),
            body: String::new(),
        });
    }
    out
}

fn raw_wrapped_atoms(
    source: &str,
    body_start_row: usize,
    target: usize,
    prefix: &str,
    visible_rows: std::ops::Range<usize>,
    domain: crate::selection::SelectionDomain,
) -> Vec<crate::selection::RelativeIsolatedAtom> {
    let body_width = target
        .saturating_sub(crate::width::width(prefix))
        .saturating_sub(RIGHT_PAD)
        .max(1);
    let mut rows = Vec::new();
    let mut line_start = 0usize;
    for raw in source.split_inclusive('\n') {
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            rows.push(Vec::new());
        } else {
            let mut row_start = 0usize;
            let mut cursor = 0usize;
            let mut width = 0usize;
            for (grapheme, cells) in crate::width::graphemes(line) {
                if width.saturating_add(cells) > body_width && cursor > row_start {
                    let text = &line[row_start..cursor];
                    rows.push(
                        crate::selection::atom_runs(
                            0,
                            u16::try_from(crate::width::width(prefix)).unwrap_or(u16::MAX),
                            text,
                            line_start.saturating_add(row_start),
                        )
                        .0,
                    );
                    row_start = cursor;
                    width = 0;
                }
                cursor = cursor.saturating_add(grapheme.len());
                width = width.saturating_add(cells);
            }
            let text = &line[row_start..cursor];
            rows.push(
                crate::selection::atom_runs(
                    0,
                    u16::try_from(crate::width::width(prefix)).unwrap_or(u16::MAX),
                    text,
                    line_start.saturating_add(row_start),
                )
                .0,
            );
        }
        line_start = line_start.saturating_add(raw.len());
    }
    if source.is_empty() {
        rows.push(Vec::new());
    }
    rows.into_iter()
        .enumerate()
        .filter(|(row, _)| visible_rows.contains(row))
        .flat_map(|(source_row, atoms)| {
            let row = body_start_row.saturating_add(source_row.saturating_sub(visible_rows.start));
            let domain = domain.clone();
            atoms.into_iter().map(move |mut atom| {
                atom.row = u16::try_from(row).unwrap_or(u16::MAX);
                crate::selection::RelativeIsolatedAtom::Raw {
                    domain: domain.clone(),
                    atom,
                }
            })
        })
        .collect()
}

fn command_render_rows(command: Option<&str>, expanded: bool, target: usize) -> usize {
    let Some(command) = command.filter(|command| !command.is_empty()) else {
        return 0;
    };
    let content_rows = if expanded {
        let prefix = format!("{DOCUMENT_PAD}${DOCUMENT_PAD}");
        let continuation = " ".repeat(crate::width::width(&prefix));
        wrap_with_prefix(command, target, &prefix, &continuation).len()
    } else {
        1
    };
    content_rows.saturating_add(1)
}

fn wrap_tail_with_prefix(
    text: &str,
    target: usize,
    prefix: &str,
    max_rows: usize,
) -> (usize, Vec<PaddedRow>) {
    let mut total_rows = 0usize;
    let mut tail = std::collections::VecDeque::with_capacity(max_rows);
    for line in text.lines() {
        for row in wrap_with_prefix(line, target, prefix, prefix) {
            total_rows = total_rows.saturating_add(1);
            if tail.len() == max_rows {
                tail.pop_front();
            }
            tail.push_back(row);
        }
    }
    (total_rows, tail.into())
}

pub fn line_with_right_pad(
    prefix: &str,
    body: &str,
    target: usize,
    prefix_style: Style,
    body_style: Style,
) -> Line<'static> {
    let body = crate::width::truncate(
        body,
        target
            .saturating_sub(crate::width::width(prefix))
            .saturating_sub(RIGHT_PAD),
    );
    let used = crate::width::width(prefix) + crate::width::width(&body);
    let fill = target.saturating_sub(used);
    let mut spans = vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::styled(body, body_style),
    ];
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), body_style));
    }
    Line::from(spans)
}

// The overlay is a self-contained composition rendered on top of the
// transcript area. Content is laid out as:
//   banner (8 rows)
//   1 pad row
const STARTUP_INPUT_SLOT_ROWS: u16 = 8;
const STARTUP_INPUT_MAX_WIDTH: u16 = 72;
const STARTUP_INPUT_TOP_ROWS: u16 = STARTUP_BANNER.len() as u16 + 4;
const STARTUP_INPUT_RECENT_GAP_ROWS: u16 = 3;
pub const STARTUP_BANNER: &[&str] = &[
    "      ⢀⡤⣾⢿⡿⢿⡿⣷⢤⡀                                           ",
    "     ⢠⢯⢎⠞⡵⠚⠓⢮⠳⡱⡽⡄                                          ",
    "     ⡟⡏⡏⣀⣳⣀⣀⣞⣀⡰⢹⢻    ████████╗███╗   ███╗ █████╗ ███╗   ██╗",
    "  ⢀⣠⡄⣧⣇⡇⠻⠿⠿⠿⠿⠿⢿⡿⣷⣦⣄⡀ ╚══██╔══╝████╗ ████║██╔══██╗████╗  ██║",
    "⢀⡴⡫⡪⠕⠹⡼⡜⡄    ⢠⢢⢮⠍⠺⢗⢝⢦⡀  ██║   ██╔████╔██║███████║██╔██╗ ██║",
    "⡞⡞⡞   ⠙⣝⢞⢦⡀⢀⡴⡳⣫⠋   ⢳⢳⢳  ██║   ██║╚██╔╝██║██╔══██║██║╚██╗██║",
    "⢧⢧⡣⡀   ⠈⣓⡡⣔⣽⡪⢞⠁   ⢀⢜⡼⡼  ██║   ██║     ██║██║  ██║██║ ╚████║",
    "⠈⠓⠿⣾⣿⣿⣿⣿⡿⠿⠛⠙⠾⢷⣿⣿⣿⣿⣷⠿⠚⠁  ╚═╝   ╚═╝     ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝",
];

pub struct StartupOverlayLayout {
    pub area: ratatui::layout::Rect,
    pub input_slot: ratatui::layout::Rect,
    pub overlay_width: u16,
    pub banner_rect: ratatui::layout::Rect,
    pub recent_container: Option<ratatui::layout::Rect>,
    pub all_projects_rect: Option<ratatui::layout::Rect>,
    pub help_rect: ratatui::layout::Rect,
    pub visible_session_count: usize,
    pub session_rects: Vec<ratatui::layout::Rect>,
}

const STARTUP_PREFIX_ROWS: u16 =
    STARTUP_INPUT_TOP_ROWS + STARTUP_INPUT_SLOT_ROWS + STARTUP_INPUT_RECENT_GAP_ROWS;
const STARTUP_FOOTER_ROWS: u16 = 2;
const STARTUP_RECENT_FIXED_ROWS: u16 = 6;
const STARTUP_SESSION_ROWS: u16 = 4;
const STARTUP_OVERLAY_MAX_WIDTH: u16 = 84;

pub fn compute_startup_overlay(
    area: ratatui::layout::Rect,
    recent: &[crate::app::StartupSessionEntry],
) -> StartupOverlayLayout {
    let available_for_sessions = area
        .height
        .saturating_sub(STARTUP_PREFIX_ROWS + STARTUP_FOOTER_ROWS + STARTUP_RECENT_FIXED_ROWS);
    let visible_session_count = if recent.is_empty() {
        0
    } else {
        usize::from(available_for_sessions / STARTUP_SESSION_ROWS).min(recent.len())
    };
    let show_recent_section =
        area.height >= STARTUP_PREFIX_ROWS + STARTUP_FOOTER_ROWS + STARTUP_RECENT_FIXED_ROWS;
    let recent_height = if show_recent_section {
        STARTUP_RECENT_FIXED_ROWS
            + STARTUP_SESSION_ROWS * visible_session_count.min(u16::MAX as usize) as u16
    } else {
        0
    };
    let total_h = (STARTUP_PREFIX_ROWS + recent_height + STARTUP_FOOTER_ROWS).min(area.height);
    let overlay_width = STARTUP_OVERLAY_MAX_WIDTH.min(area.width);
    let overlay_x = area.x + area.width.saturating_sub(overlay_width) / 2;
    let overlay_y = area.y + area.height.saturating_sub(total_h) / 2;
    let overlay = ratatui::layout::Rect::new(overlay_x, overlay_y, overlay_width, total_h);
    let input_width = STARTUP_INPUT_MAX_WIDTH.min(overlay.width);
    let input_x = overlay.x + overlay.width.saturating_sub(input_width) / 2;
    let input_y = overlay
        .y
        .saturating_add(STARTUP_INPUT_TOP_ROWS)
        .min(overlay.bottom());
    let input_slot = ratatui::layout::Rect::new(
        input_x,
        input_y,
        input_width,
        STARTUP_INPUT_SLOT_ROWS.min(overlay.bottom().saturating_sub(input_y)),
    );
    let banner_y = overlay.y.saturating_add(1).min(overlay.bottom());
    let banner_rect = ratatui::layout::Rect::new(
        overlay.x,
        banner_y,
        overlay.width,
        (STARTUP_BANNER.len() as u16 + 2).min(overlay.bottom().saturating_sub(banner_y)),
    );
    let recent_container = show_recent_section.then(|| {
        ratatui::layout::Rect::new(
            input_slot.x,
            input_slot
                .bottom()
                .saturating_add(STARTUP_INPUT_RECENT_GAP_ROWS),
            input_slot.width,
            recent_height,
        )
    });
    let session_rects = recent_container
        .into_iter()
        .flat_map(|container| {
            (0..visible_session_count).map(move |index| {
                ratatui::layout::Rect::new(
                    container.x.saturating_add(1),
                    container
                        .y
                        .saturating_add(2 + STARTUP_SESSION_ROWS * index as u16),
                    container.width.saturating_sub(2),
                    STARTUP_SESSION_ROWS,
                )
            })
        })
        .collect();
    let all_projects_rect = recent_container.map(|container| {
        ratatui::layout::Rect::new(
            container.x.saturating_add(1),
            container
                .y
                .saturating_add(2 + STARTUP_SESSION_ROWS * visible_session_count as u16),
            container.width.saturating_sub(2),
            STARTUP_SESSION_ROWS,
        )
    });
    let help_y = overlay.bottom().saturating_sub(1);
    let help_rect = ratatui::layout::Rect::new(overlay.x, help_y, overlay.width, 1);
    StartupOverlayLayout {
        area: overlay,
        input_slot,
        overlay_width: overlay.width,
        banner_rect,
        recent_container,
        all_projects_rect,
        help_rect,
        visible_session_count,
        session_rects,
    }
}

// Intro fade: banner + sessions ghost out as the new session's
// transcript appears underneath. progress 0=fully visible, 1=fully gone.
// Ratatui has no alpha channel, so we bucket into three fade steps.
pub fn render_startup_intro_fade(
    f: &mut ratatui::Frame,
    transcript_area: ratatui::layout::Rect,
    version: &str,
    recent: &[crate::app::StartupSessionEntry],
    progress: f32,
) -> StartupOverlayLayout {
    if progress >= 0.9 {
        return compute_startup_overlay(transcript_area, recent);
    }
    render_startup_overlay(
        f,
        StartupOverlayRender {
            area: transcript_area,
            version,
            recent,
            dim: progress >= 0.33,
            reveal_count: recent.len(),
            focus: crate::app::StartupFocus::Input,
            selected: 0,
            hovered: None,
            projects_hovered: false,
        },
    )
}

pub struct StartupOverlayRender<'a> {
    pub area: ratatui::layout::Rect,
    pub version: &'a str,
    pub recent: &'a [crate::app::StartupSessionEntry],
    pub dim: bool,
    pub reveal_count: usize,
    pub focus: crate::app::StartupFocus,
    pub selected: usize,
    pub hovered: Option<usize>,
    pub projects_hovered: bool,
}

pub fn render_startup_overlay(
    f: &mut ratatui::Frame,
    render: StartupOverlayRender<'_>,
) -> StartupOverlayLayout {
    let StartupOverlayRender {
        area,
        version,
        recent,
        dim,
        reveal_count,
        focus,
        selected,
        hovered,
        projects_hovered,
    } = render;
    let t = crate::theme::theme();
    let recent = &recent[..reveal_count.min(recent.len())];
    let layout = compute_startup_overlay(area, recent);
    f.render_widget(ratatui::widgets::Clear, area);
    let extra = if dim {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let logo_style = Style::default()
        .fg(t.accent.into())
        .add_modifier(Modifier::BOLD | extra);
    let subtle = Style::default().fg(t.subtle_fg.into()).add_modifier(extra);
    let mut chrome = Vec::with_capacity(STARTUP_PREFIX_ROWS as usize);
    chrome.push(Line::from(""));
    for row in STARTUP_BANNER {
        chrome.push(Line::from(Span::styled((*row).to_string(), logo_style)).centered());
    }
    chrome.push(Line::from(""));
    chrome.push(
        Line::from(Span::styled(
            format!("atman witnesses; code exists · v{version}"),
            subtle,
        ))
        .centered(),
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(chrome).alignment(ratatui::layout::Alignment::Center),
        layout.area,
    );

    if let Some(container) = layout.recent_container {
        f.render_widget(ratatui::widgets::Clear, container);
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                "Recent Sessions",
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD | extra),
            ))),
            ratatui::layout::Rect::new(
                container.x.saturating_add(2),
                container.y.saturating_add(1),
                container.width.saturating_sub(4),
                1,
            ),
        );
        for (index, rect) in layout.session_rects.iter().copied().enumerate() {
            f.render_widget(
                ratatui::widgets::Paragraph::new(render_session_card(
                    index + 1,
                    &recent[index],
                    rect.width as usize,
                    dim,
                    focus == crate::app::StartupFocus::Recent && selected == index,
                    hovered == Some(index),
                )),
                rect,
            );
        }
    }

    if let Some(card) = layout.all_projects_rect {
        f.render_widget(
            ratatui::widgets::Paragraph::new(render_all_projects_card(
                card.width as usize,
                dim,
                projects_hovered,
            )),
            card,
        );
    }

    let help = if layout.visible_session_count == 0 {
        "Start typing to begin a new session"
    } else if focus == crate::app::StartupFocus::Recent {
        "↑↓ / 1-9 select · Enter open · Tab input"
    } else {
        "Shift+Tab browse recent sessions · start typing for a new session"
    };
    f.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(Span::styled(help, subtle)))
            .alignment(ratatui::layout::Alignment::Center),
        layout.help_rect,
    );
    layout
}

fn startup_record_background(selected: bool, hovered: bool) -> Color {
    let t = crate::theme::theme();
    if selected {
        t.highlight_bg.into()
    } else if hovered {
        t.modal_bg.lerp(t.work_hover_bg, 0.72)
    } else {
        Color::Reset
    }
}

fn render_all_projects_card(width: usize, dim: bool, hovered: bool) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = startup_record_background(false, hovered);
    let extra = if dim {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let bg_only = Style::default().bg(bg).add_modifier(extra);
    let icon_style = Style::default()
        .fg(t.accent.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let title_style = Style::default()
        .fg(t.tinted_fg.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let meta_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(extra);
    let shortcut_style = Style::default()
        .fg(t.accent.into())
        .bg(bg)
        .add_modifier(extra);
    let content_width = width.saturating_sub(8);
    let shortcut = "[Ctrl+L]";
    let title_width = content_width.saturating_sub(crate::width::width(shortcut) + 1);
    let title = crate::width::pad_right(
        &crate::width::truncate("All Projects", title_width),
        title_width,
    );
    let meta = crate::width::pad_right(
        &crate::width::truncate("Browse every workspace", content_width),
        content_width,
    );
    let blank = Line::from(Span::styled(" ".repeat(width), bg_only));
    let fit = |spans: Vec<Span<'static>>| {
        let mut spans = crate::width::truncate_spans(spans, width, Some(bg));
        let used = crate::width::spans_width(spans.iter());
        if used < width {
            spans.push(Span::styled(" ".repeat(width - used), bg_only));
        }
        Line::from(spans)
    };
    vec![
        blank.clone(),
        fit(vec![
            Span::styled("  ", bg_only),
            Span::styled("[▦]", icon_style),
            Span::styled(" ", bg_only),
            Span::styled(title, title_style),
            Span::styled(" ", bg_only),
            Span::styled(shortcut, shortcut_style),
            Span::styled("  ", bg_only),
        ]),
        fit(vec![
            Span::styled("      ", bg_only),
            Span::styled(meta, meta_style),
            Span::styled("  ", bg_only),
        ]),
        blank,
    ]
}

fn render_session_card(
    n: usize,
    entry: &crate::app::StartupSessionEntry,
    width: usize,
    dim: bool,
    selected: bool,
    hovered: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = startup_record_background(selected, hovered);
    let extra = if dim {
        Modifier::DIM
    } else {
        Modifier::empty()
    };
    let bg_only = Style::default().bg(bg).add_modifier(extra);
    let interactive = selected || hovered;
    let index_style = Style::default()
        .fg(if interactive {
            t.tinted_fg.into()
        } else {
            t.accent.into()
        })
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let title_style = Style::default()
        .fg(t.tinted_fg.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let meta_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(extra);
    let content_width = width.saturating_sub(8);
    let title_source = entry
        .goal
        .as_deref()
        .filter(|goal| !goal.is_empty())
        .unwrap_or(&entry.short_id);
    let title = crate::width::pad_right(
        &crate::width::truncate(title_source, content_width),
        content_width,
    );
    let meta_source = entry
        .project
        .as_deref()
        .filter(|project| !project.is_empty())
        .map(|project| {
            format!(
                "{}  ·  {} events  ·  {project}",
                entry.age_label, entry.event_count
            )
        })
        .unwrap_or_else(|| format!("{}  ·  {} events", entry.age_label, entry.event_count));
    let meta = crate::width::pad_right(
        &crate::width::truncate(&meta_source, content_width),
        content_width,
    );
    let blank = Line::from(Span::styled(" ".repeat(width), bg_only));
    vec![
        blank.clone(),
        Line::from(vec![
            Span::styled("  ", bg_only),
            Span::styled(format!("[{n}]"), index_style),
            Span::styled(" ", bg_only),
            Span::styled(title, title_style),
            Span::styled("  ", bg_only),
        ]),
        Line::from(vec![
            Span::styled("      ", bg_only),
            Span::styled(meta, meta_style),
            Span::styled("  ", bg_only),
        ]),
        blank,
    ]
}

fn make_dashed_divider(panel_width: u16) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let side_gap = 4u16;
    let dash_width = panel_width.saturating_sub(side_gap * 2).max(4) as usize;
    let pad = " ".repeat(side_gap as usize);
    let dash_style = Style::default()
        .fg(t.subtle_fg.into())
        .add_modifier(Modifier::DIM);
    vec![
        Line::from(""),
        Line::from(vec![
            Span::raw(pad.clone()),
            Span::styled("╌".repeat(dash_width), dash_style),
            Span::raw(pad),
        ]),
        Line::from(""),
    ]
}

fn render_thinking(
    text: &str,
    done: bool,
    disclosure: Disclosure,
    hovered: bool,
    animation_frame: u32,
    panel_width: u16,
    retried: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg: Color = if hovered {
        t.work_hover_bg.into()
    } else {
        t.work_bg.into()
    };
    let header_style = Style::default().fg(t.work_title_fg.into()).bg(bg);
    let glyph_style = Style::default()
        .fg(if done {
            t.success.into()
        } else {
            t.accent.into()
        })
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(t.work_title_fg.into()).bg(bg);
    let hint_style = Style::default()
        .fg(t.work_meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let glyph = if done {
        "⣿"
    } else {
        spinner_char(animation_frame)
    };
    let label = if done {
        if retried {
            "thinking (retry)"
        } else {
            "thinking"
        }
    } else {
        "thinking…"
    };
    let header_prefix = vec![
        Span::styled(format!("{glyph}{DOCUMENT_PAD}"), glyph_style),
        Span::styled(label.to_owned(), header_style),
    ];
    render_markdown_disclosure(MarkdownDisclosureRender {
        text,
        disclosure,
        header_prefix,
        header_right: Vec::new(),
        bg,
        body_style,
        hint_style,
        panel_width,
    })
}

pub(crate) fn next_thinking_disclosure(
    text: &str,
    disclosure: Disclosure,
    panel_width: u16,
) -> Disclosure {
    next_markdown_disclosure(text, disclosure, panel_width)
}

pub(crate) fn next_compaction_disclosure(
    text: &str,
    disclosure: Disclosure,
    panel_width: u16,
) -> Disclosure {
    next_markdown_disclosure(text, disclosure, panel_width)
}

fn next_markdown_disclosure(text: &str, disclosure: Disclosure, panel_width: u16) -> Disclosure {
    let preview_has_less_content = crate::markdown::render_markdown_with_width(
        text,
        panel_width.saturating_sub((DOCUMENT_PAD_X + RIGHT_PAD) as u16),
    )
    .len()
        > 6;
    match disclosure {
        Disclosure::Summary if preview_has_less_content => Disclosure::Preview,
        Disclosure::Summary => Disclosure::Full,
        Disclosure::Preview => Disclosure::Full,
        Disclosure::Full => Disclosure::Summary,
    }
}

struct MarkdownDisclosureRender<'a> {
    text: &'a str,
    disclosure: Disclosure,
    header_prefix: Vec<Span<'static>>,
    header_right: Vec<Span<'static>>,
    bg: Color,
    body_style: Style,
    hint_style: Style,
    panel_width: u16,
}

fn render_markdown_disclosure(render: MarkdownDisclosureRender<'_>) -> Vec<Line<'static>> {
    let MarkdownDisclosureRender {
        text,
        disclosure,
        header_prefix,
        header_right,
        bg,
        body_style,
        hint_style,
        panel_width,
    } = render;
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines = vec![blank.clone()];
    let all_lines =
        crate::markdown::render_markdown_with_width(text, panel_width.saturating_sub(4).max(1));

    if disclosure == Disclosure::Summary {
        let mut body = Vec::new();
        for line in all_lines.iter().filter(|line| {
            line.spans
                .iter()
                .any(|span| !span.content.trim().is_empty())
        }) {
            if !body.is_empty() {
                body.push(Span::styled(" · ", hint_style));
            }
            body.extend(
                line.spans
                    .iter()
                    .map(|span| Span::styled(span.content.clone(), body_style.patch(span.style))),
            );
        }
        lines.push(aligned_ticker_document_row_with_control(
            header_prefix,
            body,
            DOCUMENT_PAD,
            header_right,
            Vec::new(),
            target,
            bg,
            TickerFade::Always,
        ));
        lines.push(blank);
        return lines;
    }

    lines.push(aligned_document_row(
        header_prefix,
        header_right,
        target,
        bg,
    ));
    lines.push(blank.clone());
    let visible = match disclosure {
        Disclosure::Preview => all_lines.len().min(6),
        Disclosure::Full => all_lines.len(),
        Disclosure::Summary => unreachable!("summary returns above"),
    };
    for md_line in &all_lines[crate::width::tail_row_range(all_lines.len(), visible)] {
        let content_w = crate::width::spans_width(md_line.spans.iter());
        let used = content_w + DOCUMENT_PAD_X;
        let mut spans = Vec::with_capacity(md_line.spans.len() + 2);
        spans.push(Span::styled(DOCUMENT_PAD, body_style));
        spans.extend(
            md_line
                .spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), body_style.patch(span.style))),
        );
        if target > used {
            spans.push(Span::styled(" ".repeat(target - used), body_style));
        }
        lines.push(Line::from(spans));
    }
    if disclosure != Disclosure::Full && all_lines.len() > visible {
        let hint = format!(
            "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{} more lines — click to expand",
            all_lines.len() - visible
        );
        lines.push(line_with_right_pad(
            "", &hint, target, hint_style, hint_style,
        ));
    } else if disclosure == Disclosure::Full && all_lines.len() > 6 {
        let hint = format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse");
        lines.push(line_with_right_pad(
            "", &hint, target, hint_style, hint_style,
        ));
    }
    lines.push(blank);
    lines
}

fn populate_isolated_geometry(
    item: &OutputItem,
    semantic: &mut crate::selection::ItemSemanticSource,
    panel_width: u16,
) {
    let Some(isolated) = semantic.isolated.first() else {
        return;
    };
    let domain = isolated.domain.clone();
    let target = panel_width.max(20) as usize;
    semantic.isolated_atoms = match item {
        OutputItem::Thinking {
            text, disclosure, ..
        } if *disclosure != Disclosure::Summary => {
            let rendered = crate::markdown::render_markdown_with_geometry(
                text,
                panel_width.saturating_sub(4).max(1),
            );
            let visible = match disclosure {
                Disclosure::Preview => rendered.lines.len().min(6),
                Disclosure::Full => rendered.lines.len(),
                Disclosure::Summary => 0,
            };
            let source_start = rendered.lines.len().saturating_sub(visible);
            rendered
                .prose_atoms
                .into_iter()
                .filter(|atom| usize::from(atom.row) >= source_start)
                .map(|mut atom| {
                    atom.row = u16::try_from(
                        3usize.saturating_add(usize::from(atom.row).saturating_sub(source_start)),
                    )
                    .unwrap_or(u16::MAX);
                    atom.cols = atom.cols.start.saturating_add(DOCUMENT_PAD_X as u16)
                        ..atom.cols.end.saturating_add(DOCUMENT_PAD_X as u16);
                    crate::selection::RelativeIsolatedAtom::Markdown {
                        domain: domain.clone(),
                        atom,
                    }
                })
                .collect()
        }
        OutputItem::Bash {
            command,
            output,
            expanded,
            ..
        } => {
            let total = output
                .lines()
                .flat_map(|line| wrap_with_prefix(line, target, DOCUMENT_PAD, DOCUMENT_PAD))
                .count();
            let start = if *expanded {
                0
            } else {
                total.saturating_sub(8)
            };
            raw_wrapped_atoms(
                output,
                3usize.saturating_add(command_render_rows(command.as_deref(), *expanded, target)),
                target,
                DOCUMENT_PAD,
                start..total,
                domain,
            )
        }
        _ => Vec::new(),
    };
}

fn render_assistant_with_geometry(
    md: &str,
    streaming: bool,
    retried: bool,
    panel_width: u16,
) -> crate::markdown::MarkdownRender {
    let mut rendered = crate::markdown::render_markdown_with_geometry(md, panel_width);
    if retried {
        let t = crate::theme::theme();
        let retry_style = Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::DIM);
        let retry = format!("{DOCUMENT_PAD}↻{DOCUMENT_PAD}retry");
        rendered.lines.insert(
            0,
            line_with_right_pad("", &retry, panel_width as usize, retry_style, retry_style),
        );
        for block in &mut rendered.code_blocks {
            block.first_body_row = block.first_body_row.saturating_add(1);
        }
        for atom in &mut rendered.prose_atoms {
            atom.row = atom.row.saturating_add(1);
        }
    }
    if streaming {
        let cursor = Span::styled(
            "▏".to_string(),
            Style::default().add_modifier(Modifier::SLOW_BLINK),
        );
        if let Some(last) = rendered.lines.last_mut() {
            last.spans.push(cursor);
        } else {
            rendered.lines.push(Line::from(cursor));
        }
    }
    rendered
}

fn render_assistant(
    md: &str,
    streaming: bool,
    retried: bool,
    panel_width: u16,
) -> Vec<Line<'static>> {
    render_assistant_with_geometry(md, streaming, retried, panel_width).lines
}

fn render_system_note(text: &str, level: NoteLevel, panel_width: u16) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let (glyph, fg, bg) = match level {
        NoteLevel::Info => ("·", t.accent.into(), t.note_info_bg),
        NoteLevel::Warn => ("!", t.warn.into(), t.note_warn_bg),
        NoteLevel::Error => ("✗", t.error.into(), t.note_error_bg),
        NoteLevel::Success => ("✓", t.success.into(), t.note_success_bg),
        NoteLevel::Debug => ("›", t.tinted_fg.into(), t.note_debug_bg),
    };
    let cleaned = text
        .strip_prefix("[atman] ")
        .or_else(|| text.strip_prefix("[atman]"))
        .unwrap_or(text);
    let body_style = Style::default().fg(t.tinted_fg.into()).bg(bg.into());
    let glyph_style = Style::default()
        .fg(fg)
        .bg(bg.into())
        .add_modifier(Modifier::BOLD);
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());
    let first = format!("{DOCUMENT_PAD}{glyph}{DOCUMENT_PAD}");
    let continuation = " ".repeat(crate::width::width(&first));
    let rows = wrap_with_prefix(cleaned, target, &first, &continuation);
    for row in rows {
        lines.push(line_with_right_pad(
            &row.prefix,
            &row.body,
            target,
            glyph_style,
            body_style,
        ));
    }
    lines.push(blank);
    lines
}

fn render_user_turn(
    text: &str,
    presentation: Option<&atman_runtime::user_input::UserInputPresentation>,
    panel_width: u16,
    inserted: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = user_message_bg();
    let prompt_style = Style::default()
        .fg(t.accent.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().bg(bg);
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());
    if inserted {
        lines.push(Line::from(Span::styled(
            crate::width::pad_right(&format!("{DOCUMENT_PAD}inserted into current flow"), target),
            Style::default().fg(t.subtle_fg.into()).bg(bg),
        )));
    }
    let first = format!("{DOCUMENT_PAD}❯{DOCUMENT_PAD}");
    let continuation = " ".repeat(crate::width::width(&first));
    if let Some(presentation) = presentation
        && let Some(quote) = presentation.quote.as_ref()
    {
        let quote_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
        let quote_header = format!(
            "{DOCUMENT_PAD}│ QUOTED · {} lines",
            quote.text.lines().count()
        );
        lines.push(Line::from(Span::styled(
            crate::width::pad_right(&quote_header, target),
            quote_style,
        )));
        for quote_line in quote.text.lines() {
            let prefix = format!("{DOCUMENT_PAD}│ ");
            for row in wrap_with_prefix(quote_line, target, &prefix, &prefix) {
                lines.push(line_with_right_pad(
                    &row.prefix,
                    &row.body,
                    target,
                    quote_style,
                    quote_style,
                ));
            }
        }
        if !presentation.prompt.is_empty() {
            for row in wrap_with_prefix(&presentation.prompt, target, &first, &continuation) {
                lines.push(line_with_right_pad(
                    &row.prefix,
                    &row.body,
                    target,
                    prompt_style,
                    body_style,
                ));
            }
        }
    } else {
        let body = presentation.map_or(text, |value| value.prompt.as_str());
        for row in wrap_with_prefix(body, target, &first, &continuation) {
            lines.push(line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                prompt_style,
                body_style,
            ));
        }
    }
    lines.push(blank);
    lines
}

pub fn render_item(item: &OutputItem, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut lines = match item {
        OutputItem::UserTurn { text, presentation }
        | OutputItem::Interjection { text, presentation } => render_user_turn(
            text,
            presentation.as_ref(),
            ctx.panel_width,
            matches!(item, OutputItem::Interjection { .. }),
        ),
        OutputItem::Thinking {
            text,
            done,
            disclosure,
            retried,
        } => {
            let hovered = ctx.hovered_thinking_idx.is_some();
            render_thinking(
                text,
                *done,
                *disclosure,
                hovered,
                ctx.animation_frame,
                ctx.panel_width,
                *retried,
            )
        }
        OutputItem::StartupCard { .. } => Vec::new(),
        OutputItem::AssistantMd {
            md,
            streaming,
            retried,
        } => render_assistant(md, *streaming, *retried, ctx.panel_width),
        OutputItem::ToolDispatch { calls } => render_tool_dispatch(calls, ctx, 0).0,
        OutputItem::ActivitySummary { turn, .. } => render_activity_summary(turn, ctx.panel_width),
        OutputItem::SystemNote { text, level } => render_system_note(text, *level, ctx.panel_width),
        OutputItem::WorkFoldMarker { .. } => Vec::new(),
        OutputItem::Divider => make_dashed_divider(ctx.panel_width),
        OutputItem::WorkflowPanel {
            graph,
            expanded_nodes,
            panel_expanded,
            started_at,
            ended_at,
            cancelled,
            ..
        } => render_workflow_panel(
            graph,
            expanded_nodes,
            *panel_expanded,
            *cancelled,
            *started_at,
            *ended_at,
            ctx.animation_frame,
            ctx.panel_width,
        ),
        OutputItem::Terminal {
            handle,
            title,
            command,
            screen,
            accumulated_bytes,
            mode,
            done,
            expanded,
            scroll_offset: _,
        } => render_terminal(
            handle,
            title.as_deref(),
            command.as_deref(),
            screen,
            accumulated_bytes,
            *mode,
            *done,
            *expanded,
            ctx.animation_frame,
            ctx.panel_width,
            output_fullscreen_hovered(ctx),
        ),
        OutputItem::Bash {
            handle,
            title,
            command,
            output,
            done,
            expanded,
        } => render_bash(
            handle,
            title.as_deref(),
            command.as_deref(),
            output,
            *done,
            *expanded,
            ctx.animation_frame,
            ctx.panel_width,
            output_fullscreen_hovered(ctx),
        ),
        OutputItem::CompactionSummary {
            phase,
            range_start,
            range_end,
            summary,
            before_tokens,
            after_tokens,
            compacted_count,
            disclosure,
        } => render_compaction_summary(CompactionSummaryRender {
            phase: *phase,
            range_start: *range_start,
            range_end: *range_end,
            summary,
            before_tokens: *before_tokens,
            after_tokens: *after_tokens,
            compacted_count: *compacted_count,
            disclosure: *disclosure,
            animation_frame: ctx.animation_frame,
            panel_width: ctx.panel_width,
            hovered: ctx.hovered_thinking_idx.is_some(),
        }),
        OutputItem::DiffPreview {
            title,
            old_content,
            new_content,
            unified_diff,
            expanded,
        } => render_diff_preview(
            title,
            old_content.as_deref(),
            new_content.as_deref(),
            unified_diff.as_deref(),
            *expanded,
            ctx.panel_width,
        ),
        OutputItem::FsDetail { view, expanded } => {
            render_fs_detail(view, *expanded, ctx.panel_width)
        }
        OutputItem::MermaidDiagram { source } => render_mermaid_preview(
            source,
            ctx.panel_width,
            ctx.animation_frame,
            output_fullscreen_hovered(ctx),
        ),
        OutputItem::SubAgentActivity {
            handle,
            goal,
            status,
            output,
            iteration,
            done,
            expanded,
            ..
        } => render_sub_agent_activity(
            handle,
            goal,
            status,
            output,
            *iteration,
            *done,
            *expanded,
            ctx.panel_width,
            ctx.animation_frame,
            output_fullscreen_hovered(ctx),
        ),
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
}

pub const TOOL_CALL_REGION_PREFIX: &str = "__tool_call__:";
pub const TOOL_DETAIL_REGION_PREFIX: &str = "__tool_detail__:";
pub const TOOL_FULLSCREEN_REGION_PREFIX: &str = "__tool_fullscreen__:";
pub const TOOL_DETAIL_FULLSCREEN_REGION_PREFIX: &str = "__tool_detail_fullscreen__:";
pub const WORKING_GROUP_REGION_PREFIX: &str = "__working_group__:";
pub const WORK_FOLD_REGION_PREFIX: &str = "__work_fold__:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkFoldProjection {
    pub key: u64,
    pub start_index: usize,
    pub end_index: usize,
    pub visible_members: usize,
    pub total_members: usize,
    pub boundary_member: Option<usize>,
    pub boundary_level: u8,
    pub completed_steps: usize,
    pub total_steps: usize,
    pub expanded: bool,
    pub animating: bool,
    pub hovered: bool,
    pub title: String,
    pub stats: String,
}
const TOOL_CONTROL_WIDTH: usize = 3;
const TOOL_INPUT_PREVIEW_ROWS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ToolDisclosureDepth {
    Summary,
    Preview,
    Full,
}

fn preferred_tool_input_fields(tool: &str) -> &'static [&'static str] {
    if tool.starts_with("fs.") || tool.starts_with("hunk.") {
        &["path", "file", "target"]
    } else if tool.starts_with("bash.") || tool.starts_with("term.") || tool == "terminal" {
        &["cmd", "cwd"]
    } else if tool.starts_with("flow.") || tool.starts_with("agent.") {
        &["goal", "handle", "flow", "name"]
    } else if tool.starts_with("web.") || tool.starts_with("search.") {
        &["url", "query"]
    } else {
        &[
            "path", "cmd", "query", "url", "handle", "name", "id", "key", "target",
        ]
    }
}

fn scalar_preview(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn decode_partial_json_string(raw: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\"");
    let key_end = raw.find(&marker)?.saturating_add(marker.len());
    let value = raw[key_end..].split_once(':')?.1.trim_start();
    let mut chars = value.strip_prefix('"')?.chars();
    let mut out = String::new();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            match ch {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{0008}'),
                'f' => out.push('\u{000c}'),
                'u' => {
                    let digits = chars.by_ref().take(4).collect::<String>();
                    if digits.len() == 4
                        && let Ok(value) = u32::from_str_radix(&digits, 16)
                        && let Some(decoded) = char::from_u32(value)
                    {
                        out.push(decoded);
                    }
                }
                other => out.push(other),
            }
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            break;
        } else {
            out.push(ch);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn tool_input_summary(call: &ToolCallView) -> Option<String> {
    if let Some(object) = call.input.as_object() {
        for field in preferred_tool_input_fields(&call.tool) {
            if let Some(value) = object.get(*field).and_then(scalar_preview) {
                return Some(value);
            }
        }
        let mut scalar_fields = object
            .iter()
            .filter_map(|(key, value)| scalar_preview(value).map(|value| (key, value)))
            .collect::<Vec<_>>();
        scalar_fields.sort_by_key(|(key, _)| *key);
        if let Some((_, value)) = scalar_fields.into_iter().next() {
            return Some(value);
        }
    }
    for field in preferred_tool_input_fields(&call.tool) {
        if let Some(value) = decode_partial_json_string(call.draft_preview.arguments(), field) {
            return Some(value);
        }
    }
    None
}

fn tool_call_display_intent(call: &ToolCallView) -> Option<String> {
    if !call.intent.is_empty() && call.intent != call.tool {
        return Some(call.intent.clone());
    }
    let streamed = decode_partial_json_string(
        call.draft_preview.arguments(),
        atman_runtime::message::TOOL_CALL_INTENT_FIELD,
    )
    .and_then(atman_runtime::message::ToolCallIntent::new)
    .map(|intent| intent.as_str().to_owned());
    streamed.or_else(|| fallback_tool_call_intent(call))
}

fn compact_tool_call_intent(call: &ToolCallView) -> String {
    let authored = (!call.intent.is_empty() && call.intent != call.tool)
        .then(|| call.intent.clone())
        .or_else(|| {
            decode_partial_json_string(
                call.draft_preview.arguments(),
                atman_runtime::message::TOOL_CALL_INTENT_FIELD,
            )
            .and_then(atman_runtime::message::ToolCallIntent::new)
            .map(|intent| intent.as_str().to_owned())
        });
    let intent = authored.unwrap_or_else(|| match call.tool.as_str() {
        "fs.read" => "read file".into(),
        "fs.write" => "write file".into(),
        "fs.edit" => "edit file".into(),
        "fs.list" => "list directory".into(),
        "fs.grep" => "search files".into(),
        tool if tool.starts_with("bash.") => "run command".into(),
        tool if tool.starts_with("term.") || tool == "terminal" => "run terminal".into(),
        "flow.spawn" => "start flow".into(),
        "flow.status" => "inspect flow".into(),
        "flow.interject" => "guide flow".into(),
        "flow.kill" => "stop flow".into(),
        tool => tool
            .rsplit_once('.')
            .map_or(tool, |(_, action)| action)
            .replace('_', " "),
    });
    intent.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn working_group_key(calls: &[ToolCallView]) -> Option<String> {
    calls
        .first()
        .map(|call| format!("{WORKING_GROUP_REGION_PREFIX}{}", call.id))
}

fn working_intent_spans(calls: &[ToolCallView], background: Color) -> Vec<Span<'static>> {
    let t = crate::theme::theme();
    let mut intents = Vec::new();
    for call in calls {
        let intent = compact_tool_call_intent(call);
        if !intent.is_empty() && intents.last().map(String::as_str) != Some(intent.as_str()) {
            intents.push(intent);
        }
    }
    let latest = intents.len().saturating_sub(1);
    let mut spans = Vec::new();
    for (index, intent) in intents.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " → ",
                Style::default().fg(t.work_meta_fg.into()).bg(background),
            ));
        }
        let mut style = Style::default()
            .fg(if index == latest {
                t.work_action_fg.into()
            } else {
                t.work_title_fg.into()
            })
            .bg(background);
        if index == latest {
            style = style.add_modifier(Modifier::BOLD);
        }
        spans.push(Span::styled(intent, style));
    }
    spans
}

fn fallback_tool_call_intent(call: &ToolCallView) -> Option<String> {
    let target = tool_input_summary(call);
    let file_name = target.as_deref().and_then(|value| {
        std::path::Path::new(value)
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .map(str::to_owned)
    });
    let label = match call.tool.as_str() {
        "fs.read" => format!("read {}", file_name.unwrap_or_else(|| "file".into())),
        "fs.write" => format!("write {}", file_name.unwrap_or_else(|| "file".into())),
        "fs.edit" => format!("edit {}", file_name.unwrap_or_else(|| "file".into())),
        "fs.list" => format!("list {}", target.unwrap_or_else(|| "directory".into())),
        "fs.grep" => format!("search {}", target.unwrap_or_else(|| "files".into())),
        tool if tool.starts_with("bash.") => "run command".into(),
        tool if tool.starts_with("term.") || tool == "terminal" => "run terminal".into(),
        "flow.spawn" => "start flow".into(),
        "flow.status" => "inspect flow".into(),
        "flow.interject" => "guide flow".into(),
        "flow.kill" => "stop flow".into(),
        tool => tool
            .rsplit_once('.')
            .map(|(_, action)| action.replace('_', " "))
            .filter(|action| !action.is_empty())?,
    };
    Some(label)
}

fn tool_input_body(call: &ToolCallView) -> Option<String> {
    if let Some(body) = fs_tool_input_body(call) {
        return Some(body);
    }
    let canonical = match &call.input {
        serde_json::Value::Null => None,
        serde_json::Value::Object(object) if object.is_empty() => None,
        value => serde_json::to_string_pretty(value).ok(),
    };
    canonical.or_else(|| {
        let draft = call.draft_preview.arguments().trim();
        (!draft.is_empty()).then(|| draft.to_string())
    })
}

fn tool_input_scalar(call: &ToolCallView, key: &str) -> Option<String> {
    call.input
        .get(key)
        .and_then(scalar_preview)
        .or_else(|| decode_partial_json_string(call.draft_preview.arguments(), key))
}

fn fs_tool_input_body(call: &ToolCallView) -> Option<String> {
    let has_input = call
        .input
        .as_object()
        .is_some_and(|object| !object.is_empty())
        || !call.draft_preview.arguments().trim().is_empty();
    if !has_input {
        return None;
    }
    let path = || tool_input_scalar(call, "path").unwrap_or_else(|| ".".to_string());
    match call.tool.as_str() {
        "fs.read" => {
            let mut lines = vec![format!("file  {}", path())];
            if let Some(anchor) = tool_input_scalar(call, "anchor") {
                lines.push(format!("anchor  {anchor}"));
            } else if let Some(offset) = tool_input_scalar(call, "offset") {
                let range = tool_input_scalar(call, "limit")
                    .map(|limit| format!("{offset} + {limit} lines"))
                    .unwrap_or(offset);
                lines.push(format!("range  {range}"));
            }
            Some(lines.join("\n"))
        }
        "fs.list" => Some(format!("directory  {}", path())),
        "fs.grep" => {
            let mut lines = vec![format!(
                "pattern  {}",
                tool_input_scalar(call, "pattern").unwrap_or_default()
            )];
            lines.push(format!("root  {}", path()));
            let options = [
                ("context", tool_input_scalar(call, "context_lines")),
                ("case-sensitive", tool_input_scalar(call, "case_sensitive")),
                ("limit", tool_input_scalar(call, "limit")),
            ]
            .into_iter()
            .filter_map(|(label, value)| value.map(|value| format!("{label} {value}")))
            .collect::<Vec<_>>();
            if !options.is_empty() {
                lines.push(options.join(" · "));
            }
            Some(lines.join("\n"))
        }
        _ => None,
    }
}

fn tool_input_visual_rows(body: &str, panel_width: u16) -> usize {
    let target = panel_width.max(20) as usize;
    body.lines()
        .map(|line| crate::width::word_wrap(line, target.saturating_sub(4)).len())
        .sum()
}

fn set_detail_expanded(detail: &mut OutputItem, expanded: bool) {
    match detail {
        OutputItem::Terminal {
            expanded: value, ..
        }
        | OutputItem::Bash {
            expanded: value, ..
        }
        | OutputItem::DiffPreview {
            expanded: value, ..
        }
        | OutputItem::FsDetail {
            expanded: value, ..
        }
        | OutputItem::SubAgentActivity {
            expanded: value, ..
        } => *value = expanded,
        OutputItem::CompactionSummary {
            disclosure: value, ..
        }
        | OutputItem::Thinking {
            disclosure: value, ..
        } => {
            *value = if expanded {
                Disclosure::Full
            } else {
                Disclosure::Preview
            };
        }
        _ => {}
    }
}

fn tool_disclosure_depth(call: &ToolCallView, panel_width: u16) -> ToolDisclosureDepth {
    let Some(detail) = call.detail.as_deref() else {
        let Some(body) = tool_input_body(call) else {
            return ToolDisclosureDepth::Summary;
        };
        return if tool_input_visual_rows(&body, panel_width) > TOOL_INPUT_PREVIEW_ROWS {
            ToolDisclosureDepth::Full
        } else {
            ToolDisclosureDepth::Preview
        };
    };

    if let OutputItem::FsDetail { view, .. } = detail {
        return if fs_detail_needs_full(view, panel_width.saturating_sub(4)) {
            ToolDisclosureDepth::Full
        } else {
            ToolDisclosureDepth::Preview
        };
    }

    let mut preview = detail.clone();
    let mut full = detail.clone();
    set_detail_expanded(&mut preview, false);
    set_detail_expanded(&mut full, true);
    let ctx = RenderCtx {
        panel_width: panel_width.saturating_sub(4).max(1),
        ..RenderCtx::empty()
    };
    if render_item(&preview, &ctx) == render_item(&full, &ctx) {
        ToolDisclosureDepth::Preview
    } else {
        ToolDisclosureDepth::Full
    }
}

pub(crate) fn toggle_tool_call_content_disclosure(
    call: &ToolCallView,
    panel_width: u16,
) -> Disclosure {
    match call.disclosure {
        Disclosure::Summary => match tool_disclosure_depth(call, panel_width) {
            ToolDisclosureDepth::Summary => Disclosure::Summary,
            ToolDisclosureDepth::Preview | ToolDisclosureDepth::Full => Disclosure::Preview,
        },
        Disclosure::Preview | Disclosure::Full => Disclosure::Summary,
    }
}

pub(crate) fn toggle_tool_call_detail_disclosure(
    call: &ToolCallView,
    panel_width: u16,
) -> Disclosure {
    match (call.disclosure, tool_disclosure_depth(call, panel_width)) {
        (Disclosure::Preview, ToolDisclosureDepth::Full) => Disclosure::Full,
        (Disclosure::Full, _) => Disclosure::Preview,
        _ => call.disclosure,
    }
}

fn render_tool_input_detail(
    body: &str,
    disclosure: Disclosure,
    width: usize,
    style: Style,
) -> Vec<Line<'static>> {
    let target = width.saturating_sub(4).max(1);
    let rows = body
        .lines()
        .flat_map(|line| crate::width::word_wrap(line, target))
        .collect::<Vec<_>>();
    let visible = if disclosure == Disclosure::Full {
        rows.len()
    } else {
        rows.len().min(TOOL_INPUT_PREVIEW_ROWS)
    };
    let mut lines = vec![document_blank(width, style)];
    for row in rows.iter().take(visible) {
        lines.push(line_with_right_pad(DOCUMENT_PAD, row, width, style, style));
    }
    if visible < rows.len() {
        let hint = format!(
            "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{} more lines — click to expand",
            rows.len() - visible
        );
        lines.push(line_with_right_pad("", &hint, width, style, style));
    } else if disclosure == Disclosure::Full && rows.len() > TOOL_INPUT_PREVIEW_ROWS {
        lines.push(line_with_right_pad(
            "",
            &format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse"),
            width,
            style,
            style,
        ));
    }
    lines.push(document_blank(width, style));
    lines
}

fn output_fullscreen_hovered(ctx: &RenderCtx<'_>) -> bool {
    ctx.hovered_output_node.is_some_and(|(_, key)| {
        key.starts_with(TOOL_DETAIL_FULLSCREEN_REGION_PREFIX)
            || matches!(
                key.as_str(),
                COLLAPSED_CARD_FULLSCREEN_KEY
                    | TERMINAL_FULLSCREEN_KEY
                    | BASH_FULLSCREEN_KEY
                    | MERMAID_FULLSCREEN_KEY
                    | SUB_AGENT_FULLSCREEN_KEY
            )
    })
}

fn document_blank(width: usize, style: Style) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), style))
}

#[cfg(test)]
fn line_is_visually_blank(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|span| span.content.chars().all(char::is_whitespace))
}

fn ensure_external_document_gap(lines: &mut Vec<Line<'static>>) {
    if lines
        .last()
        .is_none_or(|line| crate::width::spans_width(line.spans.iter()) != 0)
    {
        lines.push(Line::from(Span::styled(String::new(), RESET)));
    }
}

fn aligned_document_row(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    target: usize,
    background: Color,
) -> Line<'static> {
    aligned_document_row_with_control(left, right, Vec::new(), target, background)
}

fn aligned_document_row_with_control(
    mut left: Vec<Span<'static>>,
    mut right: Vec<Span<'static>>,
    mut control: Vec<Span<'static>>,
    target: usize,
    background: Color,
) -> Line<'static> {
    let horizontal_pad = DOCUMENT_PAD_X.min(target / 2);
    let inner = target.saturating_sub(horizontal_pad * 2);
    let min_left = 3.min(inner);
    control = crate::width::truncate_spans(control, inner, Some(background));
    let control_width = crate::width::spans_width(control.iter());
    let right_width = crate::width::spans_width(right.iter());
    let right_budget = right_width.min(
        inner
            .saturating_sub(control_width)
            .saturating_sub(min_left.saturating_add(1)),
    );
    right = crate::width::truncate_spans(right, right_budget, Some(background));
    let right_width = crate::width::spans_width(right.iter());
    let left_budget = inner
        .saturating_sub(control_width)
        .saturating_sub(right_width)
        .saturating_sub(usize::from(!right.is_empty()));
    left = crate::width::truncate_spans(left, left_budget, Some(background));
    let left_width = crate::width::spans_width(left.iter());
    let gap = inner.saturating_sub(left_width + right_width + control_width);

    let mut spans = Vec::with_capacity(left.len() + right.len() + control.len() + 3);
    spans.push(Span::styled(
        " ".repeat(horizontal_pad),
        Style::default().bg(background),
    ));
    spans.extend(left);
    if gap > 0 {
        spans.push(Span::styled(
            " ".repeat(gap),
            Style::default().bg(background),
        ));
    }
    spans.extend(right);
    spans.extend(control);
    spans.push(Span::styled(
        " ".repeat(horizontal_pad),
        Style::default().bg(background),
    ));
    Line::from(spans)
}

#[derive(Clone, Copy)]
enum TickerFade {
    Always,
    OverflowOnlySoft,
}

#[allow(clippy::too_many_arguments)]
fn aligned_ticker_document_row_with_control(
    mut fixed: Vec<Span<'static>>,
    ticker: Vec<Span<'static>>,
    separator: &str,
    mut right: Vec<Span<'static>>,
    mut control: Vec<Span<'static>>,
    target: usize,
    background: Color,
    fade: TickerFade,
) -> Line<'static> {
    let horizontal_pad = DOCUMENT_PAD_X.min(target / 2);
    let inner = target.saturating_sub(horizontal_pad * 2);
    control = crate::width::truncate_spans(control, inner, Some(background));
    let control_width = crate::width::spans_width(control.iter());
    let min_fixed = 3.min(inner.saturating_sub(control_width));
    let right_width = crate::width::spans_width(right.iter());
    let right_budget = right_width.min(
        inner
            .saturating_sub(control_width)
            .saturating_sub(min_fixed.saturating_add(1)),
    );
    right = crate::width::truncate_spans(right, right_budget, Some(background));
    let right_width = crate::width::spans_width(right.iter());
    let content_budget = inner
        .saturating_sub(control_width)
        .saturating_sub(right_width)
        .saturating_sub(usize::from(!right.is_empty()));
    let separator_width = usize::from(!ticker.is_empty()) * crate::width::width(separator);
    fixed = crate::width::truncate_spans(fixed, content_budget, Some(background));
    let fixed_width = crate::width::spans_width(fixed.iter());
    let ticker_budget = content_budget.saturating_sub(fixed_width);
    let show_ticker = !ticker.is_empty() && ticker_budget > separator_width;
    let ticker = if show_ticker {
        let viewport_width = ticker_budget.saturating_sub(separator_width);
        let ticker_width = crate::width::spans_width(ticker.iter());
        match fade {
            TickerFade::Always => {
                crate::width::streaming_ticker_spans(ticker, viewport_width, background)
            }
            TickerFade::OverflowOnlySoft if ticker_width > viewport_width => {
                crate::width::streaming_ticker_spans_with_fade_floor(
                    ticker,
                    viewport_width,
                    background,
                    0.68,
                )
            }
            TickerFade::OverflowOnlySoft => {
                crate::width::truncate_spans(ticker, viewport_width, Some(background))
            }
        }
    } else {
        Vec::new()
    };
    let ticker_width = crate::width::spans_width(ticker.iter());
    let used = fixed_width
        .saturating_add(if show_ticker { separator_width } else { 0 })
        .saturating_add(ticker_width)
        .saturating_add(right_width)
        .saturating_add(control_width);
    let gap = inner.saturating_sub(used);

    let mut spans =
        Vec::with_capacity(fixed.len() + ticker.len() + right.len() + control.len() + 4);
    spans.push(Span::styled(
        " ".repeat(horizontal_pad),
        Style::default().bg(background),
    ));
    spans.extend(fixed);
    if show_ticker {
        spans.push(Span::styled(
            separator.to_owned(),
            Style::default()
                .fg(crate::theme::theme().meta_fg.into())
                .bg(background),
        ));
        spans.extend(ticker);
    }
    if gap > 0 {
        spans.push(Span::styled(
            " ".repeat(gap),
            Style::default().bg(background),
        ));
    }
    spans.extend(right);
    spans.extend(control);
    spans.push(Span::styled(
        " ".repeat(horizontal_pad),
        Style::default().bg(background),
    ));
    Line::from(spans)
}

fn edit_metric_spans(
    insertions: usize,
    deletions: usize,
    background: Option<Color>,
) -> Vec<Span<'static>> {
    let t = crate::theme::theme();
    let surface = background.map_or_else(Style::default, |color| Style::default().bg(color));
    vec![
        Span::styled(format!("+{insertions}"), surface.fg(t.success.into())),
        Span::styled(" ", surface),
        Span::styled(format!("−{deletions}"), surface.fg(t.error.into())),
    ]
}

fn paint_running_foreground(line: &mut Line<'static>, animation_frame: u32, accent: Color) {
    const HALF_WIDTH: isize = 10;
    const STEP: usize = 2;

    let target = crate::width::spans_width(line.spans.iter());
    let travel = target.saturating_add(HALF_WIDTH as usize * 2).max(1);
    let head = ((animation_frame as usize * STEP) % travel) as isize - HALF_WIDTH;
    let mut column = 0usize;
    let mut out = Vec::new();
    let mut current_style = None;
    let mut current_text = String::new();

    let flush = |out: &mut Vec<Span<'static>>,
                 current_style: &mut Option<Style>,
                 current_text: &mut String| {
        if let Some(style) = current_style.take()
            && !current_text.is_empty()
        {
            out.push(Span::styled(std::mem::take(current_text), style));
        }
    };

    for span in std::mem::take(&mut line.spans) {
        for (grapheme, grapheme_width) in crate::width::graphemes(span.content.as_ref()) {
            let center = column.saturating_add(grapheme_width / 2) as isize;
            let distance = (center - head).abs();
            let level = if distance >= HALF_WIDTH {
                0
            } else {
                ((HALF_WIDTH - distance + 1) / 2) as u8
            };
            let style = if grapheme.chars().all(char::is_whitespace) {
                span.style
            } else {
                let base = crate::theme::ThemeColor::new(span.style.fg.unwrap_or(accent));
                span.style.fg(base.lerp(accent, f64::from(level) * 0.09))
            };
            if current_style != Some(style) {
                flush(&mut out, &mut current_style, &mut current_text);
                current_style = Some(style);
            }
            current_text.push_str(grapheme);
            column = column.saturating_add(grapheme_width);
        }
    }
    flush(&mut out, &mut current_style, &mut current_text);
    line.spans = out;
}

fn format_tool_elapsed(elapsed: std::time::Duration) -> String {
    if elapsed.as_millis() >= 1000 {
        format!("{:.1}s", elapsed.as_secs_f32())
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

fn render_tool_dispatch(
    calls: &[ToolCallView],
    ctx: &RenderCtx<'_>,
    item_index: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let t = crate::theme::theme();
    let width = ctx.panel_width.max(1) as usize;
    let finished = calls
        .iter()
        .filter(|call| call.status != ToolCallStatus::Running)
        .count();
    let edited_files = calls
        .iter()
        .filter_map(|call| call.applied_edit.as_ref().map(|(path, _)| path))
        .collect::<std::collections::HashSet<_>>()
        .len();
    let (insertions, deletions) = calls
        .iter()
        .filter_map(|call| call.applied_edit.as_ref().map(|(_, metrics)| metrics))
        .fold((0usize, 0usize), |(insertions, deletions), metrics| {
            (
                insertions + metrics.insertions,
                deletions + metrics.deletions,
            )
        });
    let panel_bg: Color = t.work_bg.into();
    let group_key = working_group_key(calls);
    let group_expanded = group_key
        .as_ref()
        .is_some_and(|key| ctx.expanded_tools.contains(key));
    let group_hovered = group_key.as_ref().is_some_and(|key| {
        ctx.hovered_output_node
            .is_some_and(|(_, hovered)| hovered == key)
    });
    let header_bg = if group_hovered {
        t.work_hover_bg.into()
    } else {
        panel_bg
    };
    let header_style = Style::default().fg(t.work_meta_fg.into()).bg(header_bg);
    let header_title_style = Style::default()
        .fg(t.work_title_fg.into())
        .bg(header_bg)
        .add_modifier(Modifier::BOLD);
    let running = finished < calls.len();
    let header_glyph = if running {
        spinner_char(ctx.animation_frame)
    } else {
        "⣿"
    };
    let header_glyph_color = if running {
        t.accent
    } else if calls
        .iter()
        .any(|call| call.status == ToolCallStatus::Error)
    {
        t.error
    } else {
        t.success
    };
    let header_glyph_style = Style::default()
        .fg(header_glyph_color.into())
        .bg(header_bg)
        .add_modifier(Modifier::BOLD);
    let mut header_right = vec![Span::styled(
        format!("{finished}/{}", calls.len()),
        header_style,
    )];
    if edited_files > 0 {
        let noun = if edited_files == 1 { "file" } else { "files" };
        header_right.push(Span::styled(
            format!(" · {edited_files} {noun} · "),
            header_style,
        ));
        header_right.extend(edit_metric_spans(insertions, deletions, Some(header_bg)));
    }
    if let Some(started_at) = calls.iter().map(|call| call.started_at).min() {
        let ended_at = if running {
            Instant::now()
        } else {
            calls
                .iter()
                .filter_map(|call| call.ended_at)
                .max()
                .unwrap_or_else(Instant::now)
        };
        header_right.push(Span::styled(" · ", header_style));
        header_right.push(Span::styled(
            format_tool_elapsed(ended_at.saturating_duration_since(started_at)),
            header_style,
        ));
    }
    let mut lines = vec![document_blank(width, header_style)];
    lines.push(aligned_ticker_document_row_with_control(
        vec![
            Span::styled(format!("{header_glyph}{DOCUMENT_PAD}"), header_glyph_style),
            Span::styled("working", header_title_style),
        ],
        working_intent_spans(calls, header_bg),
        " · ",
        header_right,
        Vec::new(),
        width,
        header_bg,
        if group_expanded {
            TickerFade::Always
        } else {
            TickerFade::OverflowOnlySoft
        },
    ));
    lines.push(document_blank(width, header_style));
    let mut regions = group_key
        .into_iter()
        .map(|path_key| NodeRegion {
            panel_item_index: item_index,
            path_key,
            start_row: 0,
            end_row: 3,
            col_start: 0,
            col_end: ctx.panel_width,
        })
        .collect::<Vec<_>>();
    if !group_expanded {
        return (lines, regions);
    }

    for call in calls {
        let (glyph, color) = match call.status {
            ToolCallStatus::Running => (spinner_char(ctx.animation_frame), t.accent),
            ToolCallStatus::Ok => ("✓", t.success),
            ToolCallStatus::Error => ("✗", t.error),
        };
        let elapsed = call
            .ended_at
            .unwrap_or_else(Instant::now)
            .saturating_duration_since(call.started_at);
        let elapsed = if call.status == ToolCallStatus::Running {
            String::new()
        } else {
            format_tool_elapsed(elapsed)
        };
        let input_tail = if call.applied_edit.is_none() {
            tool_input_summary(call).unwrap_or_default()
        } else {
            String::new()
        };
        let draft_tail = call
            .draft_preview
            .last_line()
            .filter(|line| !input_tail.contains(*line))
            .map(str::to_owned)
            .unwrap_or_default();
        let edit_path = call
            .applied_edit
            .as_ref()
            .map(|(path, _)| path.clone())
            .unwrap_or_default();
        let output_tail = call
            .detail
            .as_deref()
            .and_then(|detail| crate::app::task_detail_tail_lines(detail, 1).pop())
            .unwrap_or_default();
        let has_fullscreen = matches!(
            call.detail.as_deref(),
            Some(
                OutputItem::Terminal { .. }
                    | OutputItem::Bash { .. }
                    | OutputItem::SubAgentActivity { .. }
                    | OutputItem::DiffPreview { .. }
                    | OutputItem::FsDetail { .. }
            )
        );
        let call_key = format!("{TOOL_CALL_REGION_PREFIX}{}", call.id);
        let fullscreen_key = format!("{TOOL_FULLSCREEN_REGION_PREFIX}{}", call.id);
        let hovered_key = ctx.hovered_output_node.map(|(_, key)| key.as_str());
        let fullscreen_hovered = hovered_key == Some(fullscreen_key.as_str());
        let row_hovered = fullscreen_hovered || hovered_key == Some(call_key.as_str());
        let row_bg = if row_hovered {
            t.work_hover_bg.into()
        } else {
            panel_bg
        };
        let glyph_style = Style::default().fg(color.into()).bg(row_bg);
        let action_style = Style::default()
            .fg(t.work_action_fg.into())
            .bg(row_bg)
            .add_modifier(Modifier::BOLD);
        let meta_style = Style::default().fg(t.work_meta_fg.into()).bg(row_bg);
        let stream_style = Style::default().fg(t.work_title_fg.into()).bg(row_bg);
        let mut fixed = vec![
            Span::styled(format!("{glyph}{DOCUMENT_PAD}"), glyph_style),
            Span::styled(call.tool.clone(), meta_style),
        ];
        if let Some(intent) = tool_call_display_intent(call) {
            fixed.push(Span::styled(" · ", meta_style));
            fixed.push(Span::styled(intent, action_style));
        }
        let mut ticker_parts = Vec::new();
        for value in [&input_tail, &draft_tail, &edit_path, &output_tail] {
            if !value.is_empty() && ticker_parts.last() != Some(&value.as_str()) {
                ticker_parts.push(value.as_str());
            }
        }
        let ticker = if ticker_parts.is_empty() {
            Vec::new()
        } else {
            vec![Span::styled(ticker_parts.join(" · "), stream_style)]
        };
        let mut right = Vec::new();
        if let Some((_, metrics)) = call.applied_edit.as_ref() {
            right.extend(edit_metric_spans(
                metrics.insertions,
                metrics.deletions,
                Some(row_bg),
            ));
            right.push(Span::styled(format!(" · {}h", metrics.hunks), meta_style));
        }
        if !elapsed.is_empty() {
            if !right.is_empty() {
                right.push(Span::styled(" · ", meta_style));
            }
            right.push(Span::styled(elapsed, meta_style));
        }
        let control = if has_fullscreen {
            let fullscreen_style = Style::default()
                .fg(if fullscreen_hovered {
                    t.accent.into()
                } else {
                    t.meta_fg.into()
                })
                .bg(row_bg)
                .add_modifier(if fullscreen_hovered {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            vec![Span::styled("  ⤢".to_string(), fullscreen_style)]
        } else {
            vec![Span::styled(
                " ".repeat(TOOL_CONTROL_WIDTH),
                Style::default().bg(row_bg),
            )]
        };
        let summary_line = aligned_ticker_document_row_with_control(
            fixed,
            ticker,
            " · ",
            right,
            control,
            width,
            row_bg,
            TickerFade::Always,
        );
        let row = lines.len() as u32;
        lines.push(document_blank(width, Style::default().bg(row_bg)));
        lines.push(summary_line);
        lines.push(document_blank(width, Style::default().bg(row_bg)));
        regions.push(NodeRegion {
            panel_item_index: item_index,
            path_key: call_key,
            start_row: row,
            end_row: lines.len() as u32,
            col_start: 0,
            col_end: ctx.panel_width,
        });
        if has_fullscreen {
            regions.push(NodeRegion {
                panel_item_index: item_index,
                path_key: fullscreen_key,
                start_row: row,
                end_row: row + 3,
                col_start: ctx
                    .panel_width
                    .saturating_sub((DOCUMENT_PAD_X + TOOL_CONTROL_WIDTH) as u16),
                col_end: ctx.panel_width.saturating_sub(DOCUMENT_PAD_X as u16),
            });
        }

        if call.disclosure == Disclosure::Summary {
            continue;
        }
        let Some(detail) = call.detail.as_deref() else {
            let detail_style = Style::default()
                .fg(t.subtle_fg.into())
                .bg(t.work_detail_bg.into());
            let Some(detail) = tool_input_body(call) else {
                continue;
            };
            let detail_start = lines.len() as u32;
            lines.extend(render_tool_input_detail(
                &detail,
                call.disclosure,
                width,
                detail_style,
            ));
            lines.push(document_blank(width, detail_style));
            regions.push(NodeRegion {
                panel_item_index: item_index,
                path_key: format!("{TOOL_DETAIL_REGION_PREFIX}{}", call.id),
                start_row: detail_start,
                end_row: lines.len() as u32,
                col_start: 0,
                col_end: ctx.panel_width,
            });
            continue;
        };

        let mut detail = detail.clone();
        let expanded = call.disclosure == Disclosure::Full;
        set_detail_expanded(&mut detail, expanded);
        let detail_has_inline_fullscreen = matches!(
            &detail,
            OutputItem::Terminal { .. }
                | OutputItem::Bash { .. }
                | OutputItem::SubAgentActivity { .. }
        );
        let detail_fullscreen_key = format!("{TOOL_DETAIL_FULLSCREEN_REGION_PREFIX}{}", call.id);
        let child_hovered_output_node = ctx
            .hovered_output_node
            .filter(|(_, key)| key == &detail_fullscreen_key);
        let child_ctx = RenderCtx {
            expanded_tools: ctx.expanded_tools,
            messages: ctx.messages,
            animation_frame: ctx.animation_frame,
            panel_width: ctx.panel_width.saturating_sub(4).max(1),
            hovered_thinking_idx: None,
            hovered_output_node: child_hovered_output_node,
        };
        let mut detail_lines = render_item(&detail, &child_ctx);
        while detail_lines.last().is_some_and(|line| {
            line.spans
                .iter()
                .all(|span| span.content.chars().all(char::is_whitespace))
        }) {
            detail_lines.pop();
        }
        let detail_style = Style::default().bg(t.work_detail_bg.into());
        let nested_bg: Color = t.work_output_bg.into();
        let code_bg: Color = t.code_bg.into();
        let detail_start = lines.len() as u32;
        if detail_has_inline_fullscreen && detail_lines.len() > 1 {
            regions.push(NodeRegion {
                panel_item_index: item_index,
                path_key: detail_fullscreen_key,
                start_row: detail_start + 1,
                end_row: detail_start + 2,
                col_start: ctx.panel_width.saturating_sub(6),
                col_end: ctx.panel_width.saturating_sub(2),
            });
        }
        for mut line in detail_lines {
            for span in &mut line.spans {
                if span.style.bg.is_none() || span.style.bg == Some(code_bg) {
                    span.style = span.style.bg(nested_bg);
                }
            }
            line.spans
                .insert(0, Span::styled(DOCUMENT_PAD.to_string(), detail_style));
            line.spans = crate::width::truncate_spans(
                line.spans,
                width.saturating_sub(RIGHT_PAD),
                Some(nested_bg),
            );
            let used = crate::width::spans_width(line.spans.iter());
            if used < width {
                line.spans
                    .push(Span::styled(" ".repeat(width - used), detail_style));
            }
            lines.push(line);
        }
        lines.push(document_blank(width, detail_style));
        regions.push(NodeRegion {
            panel_item_index: item_index,
            path_key: format!("{TOOL_DETAIL_REGION_PREFIX}{}", call.id),
            start_row: detail_start,
            end_row: lines.len() as u32,
            col_start: 0,
            col_end: ctx.panel_width,
        });
    }
    (lines, regions)
}

#[cfg(test)]
fn render_expanded_tool_dispatch(
    calls: &[ToolCallView],
    ctx: &RenderCtx<'_>,
    item_index: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let mut expanded = ctx.expanded_tools.clone();
    if let Some(key) = working_group_key(calls) {
        expanded.insert(key);
    }
    let ctx = RenderCtx {
        expanded_tools: &expanded,
        ..*ctx
    };
    render_tool_dispatch(calls, &ctx, item_index)
}

fn render_activity_summary(
    activity: &crate::app::ActivityTotals,
    panel_width: u16,
) -> Vec<Line<'static>> {
    let width = panel_width.max(1) as usize;
    let text_style = Style::default();
    let file_label = if activity.file_count() == 1 {
        "file"
    } else {
        "files"
    };
    let edit_label = if activity.applied_edits == 1 {
        "edit"
    } else {
        "edits"
    };
    let mut descriptor = format!(
        "  · turn · {} {file_label} · {} {edit_label} · {}h · ",
        activity.file_count(),
        activity.applied_edits,
        activity.hunks
    );
    let metric_width = crate::width::width(&format!(
        "+{} −{} ·  ",
        activity.insertions, activity.deletions
    ));
    if crate::width::width(&descriptor).saturating_add(metric_width) > width {
        descriptor = format!("  · turn · {} {file_label} · ", activity.file_count());
    }
    if crate::width::width(&descriptor).saturating_add(metric_width) > width {
        descriptor = "  · turn · ".into();
    }
    if crate::width::width(&descriptor).saturating_add(metric_width) > width {
        descriptor = "· ".into();
    }

    let mut body = vec![Span::styled(descriptor, text_style)];
    body.extend(edit_metric_spans(
        activity.insertions,
        activity.deletions,
        None,
    ));
    body.push(Span::styled(" ·  ", text_style));
    body = crate::width::truncate_spans(body, width, None);

    let body_width = crate::width::spans_width(body.iter());
    let left_pad = width.saturating_sub(body_width) / 2;
    let right_pad = width.saturating_sub(body_width + left_pad);
    let mut spans = Vec::with_capacity(body.len() + 2);
    spans.push(Span::raw(" ".repeat(left_pad)));
    spans.extend(body);
    spans.push(Span::raw(" ".repeat(right_pad)));
    vec![Line::from(spans)]
}

#[allow(clippy::too_many_arguments)]
fn render_sub_agent_activity(
    handle: &str,
    goal: &str,
    status: &str,
    output: &str,
    iteration: u64,
    done: bool,
    expanded: bool,
    panel_width: u16,
    animation_frame: u32,
    fullscreen_hovered: bool,
) -> Vec<Line<'static>> {
    let glyph = match status {
        "ok" => "✓",
        "err" => "✗",
        "killed" => "⊘",
        "interrupted" => "⚠",
        _ if done => "✓",
        _ => spinner_char(animation_frame),
    };
    let iter_str = if done {
        String::new()
    } else {
        format!(" iter {iteration}")
    };
    let metadata = format!("flow[{handle}]{iter_str}");
    render_output_block(
        goal,
        Some(metadata.as_str()),
        glyph,
        None,
        output,
        expanded,
        panel_width,
        fullscreen_hovered,
    )
}

fn render_mermaid_preview(
    source: &str,
    panel_width: u16,
    _animation_frame: u32,
    fullscreen_hovered: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg: Color = t.code_bg.into();
    let target = panel_width.max(20) as usize;
    let body_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
    let header_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let hint_style = Style::default()
        .fg(t.meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let fs_btn = "⤢";
    let fs_btn_used = crate::width::width(fs_btn);
    let gap = DOCUMENT_PAD_X;

    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_prefix = format!("{DOCUMENT_PAD}◇{DOCUMENT_PAD}mermaid");
    let header_used = crate::width::width(&header_prefix);
    let header_pad = target
        .saturating_sub(header_used)
        .saturating_sub(fs_btn_used)
        .saturating_sub(gap * 2);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    header_spans.push(Span::styled(
        fs_btn.to_string(),
        if fullscreen_hovered {
            Style::default()
                .fg(t.accent.into())
                .bg(t.panel_bg.lerp(t.user_msg_bg, 0.65))
                .add_modifier(Modifier::BOLD)
        } else {
            hint_style
        },
    ));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    let mermaid_lines = crate::mermaid::render_mermaid(source, panel_width.saturating_sub(4));
    let max_preview = 12usize;
    let total = mermaid_lines.len();
    let visible = total.min(max_preview);
    for ml in mermaid_lines.iter().take(visible) {
        let line_w = crate::width::spans_width(&ml.spans);
        let pad = target.saturating_sub(line_w + DOCUMENT_PAD_X);
        let mut spans = vec![Span::styled(DOCUMENT_PAD, body_style)];
        for s in &ml.spans {
            spans.push(Span::styled(
                s.content.clone(),
                s.style.patch(Style::default().bg(bg)),
            ));
        }
        spans.push(Span::styled(" ".repeat(pad), body_style));
        lines.push(Line::from(spans));
    }

    if total > max_preview {
        let hint = format!(
            "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{} more rows — click ⤢ to expand",
            total - max_preview
        );
        lines.push(line_with_right_pad(
            "", &hint, target, hint_style, hint_style,
        ));
    }
    lines.push(blank);
    lines
}

const FS_DETAIL_PREVIEW_ROWS: usize = 10;

fn fs_detail_needs_full(view: &FsDetail, panel_width: u16) -> bool {
    let target = panel_width.max(20) as usize;
    let background: Color = crate::theme::theme().code_bg.into();
    render_fs_detail_body(view, target, background, Some(FS_DETAIL_PREVIEW_ROWS + 1)).len()
        > FS_DETAIL_PREVIEW_ROWS
}

fn wrap_fs_spans(
    spans: Vec<Span<'static>>,
    max_width: usize,
    background: Color,
) -> Vec<Vec<Span<'static>>> {
    if max_width == 0 {
        return vec![Vec::new()];
    }
    let mut rows = vec![Vec::new()];
    let mut used = 0usize;
    for span in spans {
        let mut text = String::new();
        for (grapheme, width) in crate::width::graphemes(span.content.as_ref()) {
            if used + width > max_width && used > 0 {
                push_wrapped_span(&mut rows, &mut text, span.style, background);
                rows.push(Vec::new());
                used = 0;
            }
            text.push_str(grapheme);
            used += width;
        }
        push_wrapped_span(&mut rows, &mut text, span.style, background);
    }
    rows
}

fn render_fs_source_rows(
    path: &str,
    content: &str,
    start_line: usize,
    matched_line: Option<usize>,
    target: usize,
    background: Color,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let source_line_count = content.lines().count();
    let max_line = start_line.saturating_add(source_line_count.saturating_sub(1));
    let digits = max_line.max(1).to_string().len();
    let prefix_width = digits.saturating_add(6);
    let body_width = target
        .saturating_sub(prefix_width)
        .saturating_sub(RIGHT_PAD)
        .max(1);
    let selected = match row_budget {
        Some(limit) => content
            .split_inclusive('\n')
            .take(limit.saturating_add(1))
            .collect::<String>(),
        None => content.to_owned(),
    };
    let lang = language_from_title(path);
    let mut out = Vec::new();
    for (index, highlighted) in crate::highlight::highlight_code(&lang, &selected)
        .into_iter()
        .enumerate()
    {
        let line_number = start_line.saturating_add(index);
        let is_match = matched_line == Some(line_number);
        let marker_style = Style::default()
            .fg(if is_match {
                t.accent.into()
            } else {
                t.meta_fg.into()
            })
            .bg(background);
        let body_rows = wrap_fs_spans(highlighted.spans, body_width, background);
        for (wrapped_index, body) in body_rows.into_iter().enumerate() {
            if row_budget.is_some_and(|budget| out.len() >= budget) {
                return out;
            }
            let marker = if is_match { "›" } else { " " };
            let number = if wrapped_index == 0 {
                format!("{line_number:>digits$}")
            } else {
                " ".repeat(digits)
            };
            let mut spans = vec![
                Span::styled(DOCUMENT_PAD, Style::default().bg(background)),
                Span::styled(marker.to_string(), marker_style),
                Span::styled(number, marker_style),
                Span::styled(" │ ", marker_style),
            ];
            spans.extend(body);
            pad_spans_to_width(&mut spans, target, Style::default().bg(background));
            out.push(Line::from(spans));
        }
    }
    out
}

fn render_fs_list_rows(
    entries: &[String],
    target: usize,
    background: Color,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let style = Style::default().bg(background);
    let mut out = Vec::new();
    for entry in entries {
        for row in wrap_with_prefix(entry, target, DOCUMENT_PAD, DOCUMENT_PAD) {
            if row_budget.is_some_and(|budget| out.len() >= budget) {
                return out;
            }
            out.push(line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                style,
                style,
            ));
        }
    }
    out
}

fn render_fs_grep_rows(
    hits: &[FsSearchHit],
    target: usize,
    background: Color,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let file_style = Style::default()
        .fg(t.accent.into())
        .bg(background)
        .add_modifier(Modifier::BOLD);
    let mut out = Vec::new();
    let mut previous_file: Option<&str> = None;
    for hit in hits {
        if previous_file != Some(hit.file.as_str()) {
            if row_budget.is_some_and(|budget| out.len() >= budget) {
                return out;
            }
            let body = crate::width::middle_truncate(
                &hit.file,
                target.saturating_sub(DOCUMENT_PAD_X + RIGHT_PAD),
            );
            out.push(line_with_right_pad(
                DOCUMENT_PAD,
                &body,
                target,
                file_style,
                file_style,
            ));
            previous_file = Some(&hit.file);
        }
        let start = hit.line.saturating_sub(hit.before.len());
        let snippet = hit
            .before
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(hit.matched.as_str()))
            .chain(hit.after.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        let remaining = row_budget.map(|budget| budget.saturating_sub(out.len()));
        out.extend(render_fs_source_rows(
            &hit.file,
            &snippet,
            start,
            Some(hit.line),
            target,
            background,
            remaining,
        ));
        if row_budget.is_some_and(|budget| out.len() >= budget) {
            return out;
        }
    }
    out
}

fn render_fs_detail_body(
    view: &FsDetail,
    target: usize,
    background: Color,
    row_budget: Option<usize>,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let meta_style = Style::default().fg(t.meta_fg.into()).bg(background);
    let error_style = Style::default().fg(t.error.into()).bg(background);
    match view {
        FsDetail::Read { content, .. } if content.is_empty() => vec![line_with_right_pad(
            DOCUMENT_PAD,
            "(empty file)",
            target,
            meta_style,
            meta_style,
        )],
        FsDetail::Read {
            path,
            content,
            start_line,
            ..
        } => render_fs_source_rows(
            path,
            content,
            *start_line,
            None,
            target,
            background,
            row_budget,
        ),
        FsDetail::List { entries, .. } if entries.is_empty() => vec![line_with_right_pad(
            DOCUMENT_PAD,
            "(empty directory)",
            target,
            meta_style,
            meta_style,
        )],
        FsDetail::List { entries, .. } => {
            render_fs_list_rows(entries, target, background, row_budget)
        }
        FsDetail::Grep { hits, .. } if hits.is_empty() => vec![line_with_right_pad(
            DOCUMENT_PAD,
            "No matches",
            target,
            meta_style,
            meta_style,
        )],
        FsDetail::Grep { hits, .. } => render_fs_grep_rows(hits, target, background, row_budget),
        FsDetail::Raw {
            path,
            content,
            is_error,
            ..
        } => {
            if content.is_empty() {
                vec![line_with_right_pad(
                    DOCUMENT_PAD,
                    "(empty output)",
                    target,
                    meta_style,
                    meta_style,
                )]
            } else if *is_error {
                content
                    .lines()
                    .flat_map(|line| wrap_with_prefix(line, target, DOCUMENT_PAD, DOCUMENT_PAD))
                    .take(row_budget.unwrap_or(usize::MAX))
                    .map(|row| {
                        line_with_right_pad(
                            &row.prefix,
                            &row.body,
                            target,
                            error_style,
                            error_style,
                        )
                    })
                    .collect()
            } else {
                render_fs_source_rows(
                    path.as_deref().unwrap_or(""),
                    content,
                    1,
                    None,
                    target,
                    background,
                    row_budget,
                )
            }
        }
    }
}

fn render_fs_detail(view: &FsDetail, expanded: bool, panel_width: u16) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let background: Color = t.code_bg.into();
    let target = panel_width.max(20) as usize;
    let base = Style::default().bg(background);
    let label_style = Style::default()
        .fg(t.accent.into())
        .bg(background)
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(t.tinted_fg.into()).bg(background);
    let meta_style = Style::default().fg(t.meta_fg.into()).bg(background);
    let blank = document_blank(target, base);
    let (kind, title, metadata) = match view {
        FsDetail::Read {
            path,
            content,
            start_line,
            total_lines,
            truncated,
        } => {
            let shown = content.lines().count();
            let end = start_line.saturating_add(shown.saturating_sub(1));
            let mut metadata = if shown == 0 {
                "0 lines".to_string()
            } else {
                total_lines
                    .map(|total| format!("lines {start_line}–{end} / {total}"))
                    .unwrap_or_else(|| format!("{shown} lines"))
            };
            if *truncated {
                metadata.push_str(" · truncated");
            }
            ("read", path.as_str(), metadata)
        }
        FsDetail::List { path, entries } => {
            let unit = if entries.len() == 1 {
                "entry"
            } else {
                "entries"
            };
            ("list", path.as_str(), format!("{} {unit}", entries.len()))
        }
        FsDetail::Grep {
            path,
            pattern,
            hits,
        } => {
            let unit = if hits.len() == 1 { "match" } else { "matches" };
            (
                "search",
                pattern.as_str(),
                format!("{} {unit} · {path}", hits.len()),
            )
        }
        FsDetail::Raw {
            tool,
            path,
            is_error,
            ..
        } => (
            if *is_error { "error" } else { "output" },
            path.as_deref().unwrap_or(tool),
            tool.clone(),
        ),
    };
    let mut lines = vec![blank.clone()];
    lines.push(aligned_document_row(
        vec![
            Span::styled(format!("◇{DOCUMENT_PAD}{kind}{DOCUMENT_PAD}"), label_style),
            Span::styled(title.to_owned(), value_style),
        ],
        vec![Span::styled(metadata, meta_style)],
        target,
        background,
    ));
    lines.push(blank.clone());

    let needs_full = fs_detail_needs_full(view, panel_width);
    let budget = (!expanded).then_some(FS_DETAIL_PREVIEW_ROWS);
    let mut body = render_fs_detail_body(view, target, background, budget);
    lines.append(&mut body);
    if !expanded && needs_full {
        lines.push(line_with_right_pad(
            DOCUMENT_PAD,
            &format!("▼{DOCUMENT_PAD}more output — click to expand"),
            target,
            meta_style,
            meta_style,
        ));
    } else if expanded && needs_full {
        lines.push(line_with_right_pad(
            DOCUMENT_PAD,
            &format!("▲{DOCUMENT_PAD}click to collapse"),
            target,
            meta_style,
            meta_style,
        ));
    }
    lines.push(blank);
    lines
}

fn render_diff_preview(
    title: &str,
    old_content: Option<&str>,
    new_content: Option<&str>,
    unified_diff: Option<&str>,
    expanded: bool,
    panel_width: u16,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg: Color = t.code_bg.into();
    let target = panel_width.max(20) as usize;
    let base_style = Style::default().bg(bg);
    let header_style = Style::default()
        .fg(t.accent.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let hint_style = Style::default()
        .fg(t.meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let blank = Line::from(Span::styled(" ".repeat(target), base_style));
    let mut lines = vec![blank.clone()];
    let header = format!("{DOCUMENT_PAD}✎{DOCUMENT_PAD}{title}");
    lines.push(line_with_right_pad(
        "",
        &header,
        target,
        header_style,
        base_style,
    ));
    lines.push(blank.clone());
    let layout = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.diff_layout())
        .unwrap_or_default();
    let unified_layout = prefers_unified_diff(layout, panel_width, unified_diff);
    if unified_layout && let Some(diff) = unified_diff {
        let (body, total) = render_unified_diff_rows(diff, expanded, target, bg);
        lines.extend(body);
        push_diff_fold_hint(&mut lines, expanded, total, 15, target, hint_style);
    } else if let (Some(old), Some(new)) = (old_content, new_content) {
        let (body, total) = render_dual_diff_rows(title, old, new, expanded, target, bg);
        lines.extend(body);
        push_diff_fold_hint(&mut lines, expanded, total, 15, target, hint_style);
    } else if let Some(diff) = unified_diff {
        let (cells, lang) = parse_unified_diff_to_dual(diff);
        let total = cells.len();
        let first_change = cells.iter().position(|(l, r)| {
            !matches!(
                l.kind,
                DiffCellKind::Normal | DiffCellKind::Empty | DiffCellKind::Meta
            ) || !matches!(
                r.kind,
                DiffCellKind::Normal | DiffCellKind::Empty | DiffCellKind::Meta
            )
        });
        let (body, _) = render_diff_cell_rows(&cells, &lang, expanded, target, bg, first_change);
        lines.extend(body);
        push_diff_fold_hint(&mut lines, expanded, total, 15, target, hint_style);
    }
    lines.push(blank);
    lines
}

fn prefers_unified_diff(
    layout: atman_runtime::config_hub::DiffLayout,
    panel_width: u16,
    unified_diff: Option<&str>,
) -> bool {
    layout == atman_runtime::config_hub::DiffLayout::Unified
        || panel_width < 72
        || unified_diff.is_some_and(diff_is_addition_only)
}

fn diff_is_addition_only(diff: &str) -> bool {
    let mut has_old_header = false;
    let mut has_new_header = false;
    let mut has_addition = false;
    let mut has_removal = false;
    for line in diff.lines() {
        if line.starts_with("--- ") {
            has_old_header = true;
        } else if line.starts_with("+++ ") {
            has_new_header = true;
        } else if line.starts_with('+') {
            has_addition = true;
        } else if line.starts_with('-') {
            has_removal = true;
        }
    }
    !has_removal && (has_addition || (has_new_header && !has_old_header))
}

fn push_diff_fold_hint(
    lines: &mut Vec<Line<'static>>,
    expanded: bool,
    total: usize,
    folded: usize,
    target: usize,
    style: Style,
) {
    if !expanded && total > folded {
        let hint = format!(
            "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{} more lines — click to expand",
            total - folded
        );
        lines.push(line_with_right_pad("", &hint, target, style, style));
    } else if expanded && total > folded {
        let hint = format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse");
        lines.push(line_with_right_pad("", &hint, target, style, style));
    }
}

fn render_unified_diff_rows(
    diff: &str,
    expanded: bool,
    target: usize,
    bg: Color,
) -> (Vec<Line<'static>>, usize) {
    let t = crate::theme::theme();
    let mut rows = Vec::new();
    let mut change_rows = Vec::new();
    for source in diff.lines() {
        let (marker, text, style, is_change) = if source.starts_with("+++")
            || source.starts_with("---")
            || source.starts_with("@@")
            || source.starts_with("diff --git")
            || source.starts_with("index ")
        {
            (
                "   ",
                source,
                Style::default().fg(t.meta_fg.into()).bg(bg),
                true,
            )
        } else if let Some(text) = source.strip_prefix('+') {
            (
                "+  ",
                text,
                Style::default()
                    .fg(t.diff_add_fg.into())
                    .bg(t.diff_add_bg.into()),
                true,
            )
        } else if let Some(text) = source.strip_prefix('-') {
            (
                "-  ",
                text,
                Style::default()
                    .fg(t.diff_remove_fg.into())
                    .bg(t.diff_remove_bg.into()),
                true,
            )
        } else {
            (
                "   ",
                source.strip_prefix(' ').unwrap_or(source),
                Style::default().bg(bg),
                false,
            )
        };
        let prefix = format!("{DOCUMENT_PAD}{marker}");
        let continuation = " ".repeat(crate::width::width(&prefix));
        let wrapped = crate::width::word_wrap(
            text,
            target
                .saturating_sub(crate::width::width(&prefix))
                .saturating_sub(RIGHT_PAD)
                .max(1),
        );
        for (visual_index, line) in wrapped.into_iter().enumerate() {
            if is_change {
                change_rows.push(rows.len());
            }
            rows.push(line_with_right_pad(
                if visual_index == 0 {
                    &prefix
                } else {
                    &continuation
                },
                &line,
                target,
                style,
                style,
            ));
        }
    }
    let total = rows.len();
    if expanded || total <= 15 {
        return (rows, total);
    }
    let indices = distributed_preview_indices(total, &change_rows, 15);
    let visible = indices
        .into_iter()
        .filter_map(|index| rows.get(index).cloned())
        .collect();
    (visible, total)
}

fn distributed_preview_indices(total: usize, changes: &[usize], max_rows: usize) -> Vec<usize> {
    if total <= max_rows {
        return (0..total).collect();
    }
    if changes.is_empty() {
        return (0..max_rows.min(total)).collect();
    }
    let mut groups = vec![vec![changes[0]]];
    for &index in changes.iter().skip(1) {
        let last = *groups
            .last()
            .and_then(|group| group.last())
            .unwrap_or(&index);
        if index.saturating_sub(last) > 3 {
            groups.push(vec![index]);
        } else if let Some(group) = groups.last_mut() {
            group.push(index);
        }
    }
    let per_group = (max_rows / groups.len()).max(1);
    let mut selected = std::collections::BTreeSet::new();
    for group in groups {
        selected.extend(crate::width::centered_row_range(
            total,
            group[group.len() / 2],
            per_group,
        ));
    }
    selected.into_iter().take(max_rows).collect()
}

#[derive(Clone)]
struct DiffCell {
    line_no: Option<usize>,
    text: String,
    kind: DiffCellKind,
    /// Character-level diff segments: (text, is_changed).
    /// Only set for Delete/Insert cells paired via Replace ops.
    char_diff: Option<CharSegments>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffCellKind {
    Normal,
    Delete,
    Insert,
    Meta,
    Empty,
}

fn diff_preview_indices(rows: &[(DiffCell, DiffCell)], max_rows: usize) -> Vec<usize> {
    if rows.len() <= max_rows {
        return (0..rows.len()).collect();
    }
    let changes = rows
        .iter()
        .enumerate()
        .filter_map(|(index, (left, right))| {
            (!matches!(left.kind, DiffCellKind::Normal | DiffCellKind::Empty)
                || !matches!(right.kind, DiffCellKind::Normal | DiffCellKind::Empty))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    if changes.is_empty() {
        return (0..max_rows.min(rows.len())).collect();
    }
    distributed_preview_indices(rows.len(), &changes, max_rows)
}

fn render_diff_cell_rows(
    rows: &[(DiffCell, DiffCell)],
    lang: &str,
    expanded: bool,
    target: usize,
    bg: Color,
    first_change: Option<usize>,
) -> (Vec<Line<'static>>, usize) {
    let t = crate::theme::theme();
    let total = rows.len();
    // Line number column in the center: " 1234 1234 " (10 chars wide)
    let line_no_w = 10usize;
    let margin_w = DOCUMENT_PAD_X;
    let panes_w = target.saturating_sub(line_no_w + margin_w * 2);
    let left_w = panes_w / 2;
    let right_w = panes_w.saturating_sub(left_w);
    let margin_style = Style::default().bg(bg);
    let line_no_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
    let mut out = Vec::new();
    if expanded || total <= 15 {
        for (left, right) in rows {
            push_diff_visual_rows(
                &mut out,
                DiffVisualSpec {
                    left,
                    right,
                    left_w,
                    right_w,
                    lang,
                    bg,
                    margin_w,
                    margin_style,
                    line_no_style,
                    target,
                },
            );
        }
    } else {
        let _ = first_change;
        for index in diff_preview_indices(rows, 15) {
            let (left, right) = &rows[index];
            push_diff_visual_rows(
                &mut out,
                DiffVisualSpec {
                    left,
                    right,
                    left_w,
                    right_w,
                    lang,
                    bg,
                    margin_w,
                    margin_style,
                    line_no_style,
                    target,
                },
            );
        }
    }
    (out, total)
}

struct DiffVisualSpec<'a> {
    left: &'a DiffCell,
    right: &'a DiffCell,
    left_w: usize,
    right_w: usize,
    lang: &'a str,
    bg: Color,
    margin_w: usize,
    margin_style: Style,
    line_no_style: Style,
    target: usize,
}

fn push_diff_visual_rows(out: &mut Vec<Line<'static>>, spec: DiffVisualSpec<'_>) {
    let left_lines = render_diff_side(spec.left, spec.left_w, spec.lang, spec.bg);
    let right_lines = render_diff_side(spec.right, spec.right_w, spec.lang, spec.bg);
    let left_blank = blank_diff_side(spec.left_w, spec.bg);
    let right_blank = blank_diff_side(spec.right_w, spec.bg);
    let line_count = left_lines.len().max(right_lines.len()).max(1);
    for idx in 0..line_count {
        let mut spans = Vec::new();
        spans.push(Span::styled(" ".repeat(spec.margin_w), spec.margin_style));
        spans.extend(
            left_lines
                .get(idx)
                .cloned()
                .unwrap_or_else(|| left_blank.clone())
                .spans,
        );
        // Center line numbers: " old_no new_no " (new_no left-aligned to hug right pane)
        let line_no_text = if idx == 0 {
            let old_no = spec
                .left
                .line_no
                .map(|n| format!("{n:>4}"))
                .unwrap_or_else(|| "    ".to_string());
            let new_no = spec
                .right
                .line_no
                .map(|n| format!("{n:<4}"))
                .unwrap_or_else(|| "    ".to_string());
            format!(" {old_no} {new_no} ")
        } else {
            " ".repeat(10)
        };
        spans.push(Span::styled(line_no_text, spec.line_no_style));
        spans.extend(
            right_lines
                .get(idx)
                .cloned()
                .unwrap_or_else(|| right_blank.clone())
                .spans,
        );
        spans.push(Span::styled(" ".repeat(spec.margin_w), spec.margin_style));
        pad_spans_to_width(&mut spans, spec.target, spec.margin_style);
        out.push(Line::from(spans));
    }
}

fn render_dual_diff_rows(
    title: &str,
    old: &str,
    new: &str,
    expanded: bool,
    target: usize,
    bg: Color,
) -> (Vec<Line<'static>>, usize) {
    let mut lang = language_from_title(title);
    // Fallback: try to extract language from `// *.ext` header in content.
    if lang.is_empty() {
        if let Some(detected) = detect_lang_from_content(old) {
            lang = detected;
        }
    }
    let old_lines = content_lines(old);
    let new_lines = content_lines(new);
    let diff = similar::TextDiff::from_lines(old, new);
    let mut rows: Vec<(DiffCell, DiffCell)> = Vec::new();
    let mut first_change: Option<usize> = None;
    for op in diff.ops() {
        match *op {
            similar::DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for i in 0..len {
                    rows.push((
                        diff_cell(&old_lines, old_index + i, DiffCellKind::Normal),
                        diff_cell(&new_lines, new_index + i, DiffCellKind::Normal),
                    ));
                }
            }
            similar::DiffOp::Delete {
                old_index, old_len, ..
            } => {
                if first_change.is_none() {
                    first_change = Some(rows.len());
                }
                for i in 0..old_len {
                    rows.push((
                        diff_cell(&old_lines, old_index + i, DiffCellKind::Delete),
                        empty_cell(),
                    ));
                }
            }
            similar::DiffOp::Insert {
                new_index, new_len, ..
            } => {
                if first_change.is_none() {
                    first_change = Some(rows.len());
                }
                for i in 0..new_len {
                    rows.push((
                        empty_cell(),
                        diff_cell(&new_lines, new_index + i, DiffCellKind::Insert),
                    ));
                }
            }
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if first_change.is_none() {
                    first_change = Some(rows.len());
                }
                let len = old_len.max(new_len);
                let paired = old_len.min(new_len);
                for i in 0..len {
                    let (mut left, mut right) = (
                        if i < old_len {
                            diff_cell(&old_lines, old_index + i, DiffCellKind::Delete)
                        } else {
                            empty_cell()
                        },
                        if i < new_len {
                            diff_cell(&new_lines, new_index + i, DiffCellKind::Insert)
                        } else {
                            empty_cell()
                        },
                    );
                    // For paired old/new lines, compute char-level diff
                    if i < paired {
                        let (old_segs, new_segs) = char_diff_segments(&left.text, &right.text);
                        left.char_diff = Some(old_segs);
                        right.char_diff = Some(new_segs);
                    }
                    rows.push((left, right));
                }
            }
        }
    }
    render_diff_cell_rows(&rows, &lang, expanded, target, bg, first_change)
}

/// Parse a unified diff into side-by-side cell pairs and detect language from
/// the `diff --git a/xxx.ext` header line.
fn parse_unified_diff_to_dual(diff: &str) -> (Vec<(DiffCell, DiffCell)>, String) {
    let mut rows = Vec::new();
    let mut lang = String::new();
    let mut old_line = 0usize;
    let mut new_line = 0usize;

    let mut pending_deletes: Vec<DiffCell> = Vec::new();
    let mut pending_inserts: Vec<DiffCell> = Vec::new();

    fn flush_pending(
        rows: &mut Vec<(DiffCell, DiffCell)>,
        deletes: &mut Vec<DiffCell>,
        inserts: &mut Vec<DiffCell>,
    ) {
        if deletes.is_empty() && inserts.is_empty() {
            return;
        }
        let max_len = deletes.len().max(inserts.len());
        for i in 0..max_len {
            let (mut left, mut right) = (
                if i < deletes.len() {
                    deletes[i].clone()
                } else {
                    empty_cell()
                },
                if i < inserts.len() {
                    inserts[i].clone()
                } else {
                    empty_cell()
                },
            );
            // Compute char-level diff for paired delete/insert lines
            if i < deletes.len() && i < inserts.len() {
                let (old_segs, new_segs) = char_diff_segments(&left.text, &right.text);
                left.char_diff = Some(old_segs);
                right.char_diff = Some(new_segs);
            }
            rows.push((left, right));
        }
        deletes.clear();
        inserts.clear();
    }

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            flush_pending(&mut rows, &mut pending_deletes, &mut pending_inserts);
            if lang.is_empty() {
                if let Some(ext) = line
                    .split('.')
                    .next_back()
                    .and_then(|s| s.split_whitespace().next())
                {
                    lang = ext_to_lang(ext).to_string();
                }
            }
            let cell = DiffCell {
                line_no: None,
                text: line.to_string(),
                kind: DiffCellKind::Meta,
                char_diff: None,
            };
            rows.push((cell.clone(), cell));
            continue;
        }
        if line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("\\ ")
        {
            continue;
        }
        if line.starts_with("@@") {
            flush_pending(&mut rows, &mut pending_deletes, &mut pending_inserts);
            if let Some((os, ns)) = parse_hunk_header(line) {
                old_line = os;
                new_line = ns;
            }
            let cell = DiffCell {
                line_no: None,
                text: line.to_string(),
                kind: DiffCellKind::Meta,
                char_diff: None,
            };
            rows.push((cell.clone(), cell));
            continue;
        }
        if line.starts_with(' ') || line.is_empty() {
            flush_pending(&mut rows, &mut pending_deletes, &mut pending_inserts);
            let text = if line.is_empty() {
                ""
            } else {
                line.strip_prefix(' ').unwrap_or(line)
            };
            rows.push((
                DiffCell {
                    line_no: Some(old_line),
                    text: text.to_string(),
                    kind: DiffCellKind::Normal,
                    char_diff: None,
                },
                DiffCell {
                    line_no: Some(new_line),
                    text: text.to_string(),
                    kind: DiffCellKind::Normal,
                    char_diff: None,
                },
            ));
            old_line += 1;
            new_line += 1;
        } else if line.starts_with('-') {
            pending_deletes.push(DiffCell {
                line_no: Some(old_line),
                text: line.strip_prefix('-').unwrap_or(line).to_string(),
                kind: DiffCellKind::Delete,
                char_diff: None,
            });
            old_line += 1;
        } else if line.starts_with('+') {
            pending_inserts.push(DiffCell {
                line_no: Some(new_line),
                text: line.strip_prefix('+').unwrap_or(line).to_string(),
                kind: DiffCellKind::Insert,
                char_diff: None,
            });
            new_line += 1;
        }
    }
    flush_pending(&mut rows, &mut pending_deletes, &mut pending_inserts);
    (rows, lang)
}

/// Parse `@@ -old_start,old_count +new_start,new_count @@` and return
/// `(old_start, new_start)`.
fn parse_hunk_header(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old_part, rest) = rest.split_once('+')?;
    let rest = rest.strip_prefix('+')?;
    let old_start = old_part.split(',').next()?.parse::<usize>().ok()?;
    let new_start = rest
        .split(',')
        .next()?
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()?;
    Some((old_start, new_start))
}

/// Map a file extension to a highlight language name.
fn ext_to_lang(ext: &str) -> &str {
    match ext {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" => "typescript",
        "tsx" => "tsx",
        "jsx" => "jsx",
        "md" => "markdown",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "html" => "html",
        "css" => "css",
        "sh" => "bash",
        other => other,
    }
}

fn detect_lang_from_content(content: &str) -> Option<String> {
    let first_line = content.lines().next()?;
    let header = first_line.strip_prefix("// ")?;
    std::path::Path::new(header)
        .extension()
        .and_then(|s| s.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "py" => "python",
            "js" => "javascript",
            "ts" => "typescript",
            "tsx" => "tsx",
            "jsx" => "jsx",
            "md" => "markdown",
            "toml" => "toml",
            "json" => "json",
            "yaml" | "yml" => "yaml",
            "html" => "html",
            "css" => "css",
            "sh" => "bash",
            other => other,
        })
        .map(String::from)
}

fn content_lines(s: &str) -> Vec<String> {
    s.split_inclusive('\n')
        .map(|line| line.strip_suffix('\n').unwrap_or(line).to_string())
        .collect()
}

fn diff_cell(lines: &[String], idx: usize, kind: DiffCellKind) -> DiffCell {
    DiffCell {
        line_no: Some(idx + 1),
        text: lines.get(idx).cloned().unwrap_or_default(),
        kind,
        char_diff: None,
    }
}

fn empty_cell() -> DiffCell {
    DiffCell {
        line_no: None,
        text: String::new(),
        kind: DiffCellKind::Empty,
        char_diff: None,
    }
}

/// Character-level diff segments: (text, is_changed).
type CharSegments = Vec<(String, bool)>;

/// Compute character-level diff between two lines, returning (old_segs, new_segs).
/// Used to highlight exactly which characters changed within a Replace diff op.
fn char_diff_segments(old: &str, new: &str) -> (CharSegments, CharSegments) {
    let diff = similar::TextDiff::from_chars(old, new);
    let mut old_segs = Vec::new();
    let mut new_segs = Vec::new();
    for op in diff.ops() {
        match *op {
            similar::DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                let old_part: String = old.chars().skip(old_index).take(len).collect();
                let new_part: String = new.chars().skip(new_index).take(len).collect();
                old_segs.push((old_part, false));
                new_segs.push((new_part, false));
            }
            similar::DiffOp::Delete {
                old_index, old_len, ..
            } => {
                let part: String = old.chars().skip(old_index).take(old_len).collect();
                old_segs.push((part, true));
            }
            similar::DiffOp::Insert {
                new_index, new_len, ..
            } => {
                let part: String = new.chars().skip(new_index).take(new_len).collect();
                new_segs.push((part, true));
            }
            similar::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let old_part: String = old.chars().skip(old_index).take(old_len).collect();
                let new_part: String = new.chars().skip(new_index).take(new_len).collect();
                old_segs.push((old_part, true));
                new_segs.push((new_part, true));
            }
        }
    }
    (old_segs, new_segs)
}

fn render_diff_side(cell: &DiffCell, width: usize, lang: &str, bg: Color) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let mark_style = match cell.kind {
        DiffCellKind::Delete => Style::default()
            .fg(t.diff_remove_fg.into())
            .bg(t.diff_remove_bg.into()),
        DiffCellKind::Insert => Style::default()
            .fg(t.diff_add_fg.into())
            .bg(t.diff_add_bg.into()),
        DiffCellKind::Meta => Style::default().fg(t.meta_fg.into()).bg(bg),
        DiffCellKind::Normal | DiffCellKind::Empty => Style::default().bg(bg),
    };
    let body_w = width;

    // If we have char-level diff segments, build spans from them directly
    // — changed chars get extra emphasis (underline for delete, bold for insert).
    let wrapped = if let Some(segs) = &cell.char_diff {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(segs.len());
        for (text, changed) in segs {
            let style = if *changed {
                match cell.kind {
                    DiffCellKind::Delete => mark_style.add_modifier(Modifier::UNDERLINED),
                    DiffCellKind::Insert => mark_style.add_modifier(Modifier::BOLD),
                    _ => mark_style,
                }
            } else {
                mark_style
            };
            spans.push(Span::styled(text.clone(), style));
        }
        wrap_spans_with_bg(spans, body_w, mark_style.bg.unwrap_or(bg))
    } else {
        let highlighted = crate::highlight::highlight_code(lang, &cell.text);
        highlighted
            .into_iter()
            .next()
            .map(|line| wrap_spans_with_bg(line.spans, body_w, mark_style.bg.unwrap_or(bg)))
            .unwrap_or_else(|| vec![Vec::new()])
    };
    let mut lines = Vec::with_capacity(wrapped.len().max(1));
    for body_spans in wrapped.into_iter() {
        let mut spans = Vec::new();
        let mut body = body_spans;
        if cell.char_diff.is_none()
            && !matches!(
                cell.kind,
                DiffCellKind::Normal | DiffCellKind::Empty | DiffCellKind::Meta
            )
        {
            for span in &mut body {
                span.style.fg = mark_style.fg.or(span.style.fg);
            }
        }
        spans.extend(body);
        pad_spans_to_width(&mut spans, width, mark_style);
        lines.push(Line::from(spans));
    }
    if lines.is_empty() {
        vec![blank_diff_side(width, bg)]
    } else {
        lines
    }
}

fn blank_diff_side(width: usize, bg: Color) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), Style::default().bg(bg)))
}

fn language_from_title(title: &str) -> String {
    std::path::Path::new(title)
        .extension()
        .and_then(|s| s.to_str())
        .map(|ext| match ext {
            "rs" => "rust",
            "py" => "python",
            "js" => "javascript",
            "ts" => "typescript",
            "tsx" => "tsx",
            "jsx" => "jsx",
            "md" => "markdown",
            "toml" => "toml",
            "json" => "json",
            "yaml" | "yml" => "yaml",
            "html" => "html",
            "css" => "css",
            "sh" => "bash",
            other => other,
        })
        .unwrap_or("")
        .to_string()
}

fn wrap_spans_with_bg(
    spans: Vec<Span<'static>>,
    max_w: usize,
    bg: Color,
) -> Vec<Vec<Span<'static>>> {
    if max_w == 0 {
        return vec![Vec::new()];
    }
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used = 0usize;
    let max_rows = 3usize;
    let mut truncated = false;
    for span in spans {
        let mut text = String::new();
        for (g, gw) in crate::width::graphemes(span.content.as_ref()) {
            if used + gw > max_w {
                push_wrapped_span(&mut rows, &mut text, span.style, bg);
                if rows.len() >= max_rows {
                    truncated = true;
                    break;
                }
                rows.push(Vec::new());
                used = 0;
            }
            text.push_str(g);
            used += gw;
        }
        push_wrapped_span(&mut rows, &mut text, span.style, bg);
        if truncated {
            break;
        }
    }
    if truncated {
        add_ellipsis_to_row(
            rows.last_mut().expect("wrap rows are never empty"),
            max_w,
            bg,
        );
    }
    rows
}

fn push_wrapped_span(rows: &mut [Vec<Span<'static>>], text: &mut String, style: Style, bg: Color) {
    if text.is_empty() {
        return;
    }
    let mut styled = style;
    styled.bg = styled.bg.or(Some(bg));
    rows.last_mut()
        .expect("wrap rows are never empty")
        .push(Span::styled(std::mem::take(text), styled));
}

fn add_ellipsis_to_row(row: &mut Vec<Span<'static>>, max_w: usize, bg: Color) {
    let style = row
        .last()
        .map(|span| span.style)
        .unwrap_or_else(|| Style::default().bg(bg));
    let trimmed =
        crate::width::truncate_spans(std::mem::take(row), max_w.saturating_sub(1), Some(bg));
    *row = trimmed;
    let mut ellipsis_style = style;
    ellipsis_style.bg = ellipsis_style.bg.or(Some(bg));
    row.push(Span::styled("⋯", ellipsis_style));
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, style: Style) {
    let used: usize = spans
        .iter()
        .map(|s| crate::width::width(s.content.as_ref()))
        .sum();
    if width > used {
        spans.push(Span::styled(" ".repeat(width - used), style));
    }
}

#[derive(Debug, Default)]
struct LlmStatsSummary {
    routes: std::collections::HashMap<LlmStatsRoute, LlmStatsAggregate>,
}

fn aggregate_llm_stats(nodes: &[atman_runtime::workflow::WorkflowNode]) -> Option<LlmStatsSummary> {
    fn visit(nodes: &[atman_runtime::workflow::WorkflowNode], summary: &mut LlmStatsSummary) {
        for node in nodes {
            if let Some(stats) = &node.llm_stats {
                let route = LlmStatsRoute {
                    provider: stats.provider.clone(),
                    model: stats.model.clone(),
                    purpose: stats.context_call_purpose,
                    scope: stats.context_call_scope,
                };
                summary.routes.entry(route).or_default().record(stats);
            }
            visit(&node.children, summary);
        }
    }

    let mut summary = LlmStatsSummary::default();
    visit(nodes, &mut summary);
    (!summary.routes.is_empty()).then_some(summary)
}

fn format_workflow_stats_footer(
    graph: &atman_runtime::workflow::WorkflowGraph,
    summary: Option<&WorkflowSummary>,
    outer_width: u16,
    border_style: Style,
) -> Line<'static> {
    use atman_runtime::humanize::format_count;
    let fallback = if summary.is_none() {
        aggregate_llm_stats(&graph.root)
    } else {
        None
    };
    let routes = summary
        .map(WorkflowSummary::llm_routes)
        .or_else(|| fallback.as_ref().map(|stats| &stats.routes))
        .filter(|routes| !routes.is_empty());
    let bottom_text = if let Some(routes) = routes {
        let primary_routes = routes
            .iter()
            .filter(|(route, _)| route.is_primary())
            .collect::<Vec<_>>();
        let mut primary = LlmStatsAggregate::default();
        for (_, aggregate) in &primary_routes {
            primary.merge(**aggregate);
        }
        let auxiliary_calls = routes
            .iter()
            .filter(|(route, _)| !route.is_primary())
            .map(|(_, aggregate)| aggregate.calls)
            .sum::<usize>();
        let display = if primary_routes.is_empty() {
            let mut all = LlmStatsAggregate::default();
            for aggregate in routes.values() {
                all.merge(*aggregate);
            }
            all
        } else {
            primary
        };
        let speed = display.average_speed();
        let route_count = if primary_routes.is_empty() {
            routes.len()
        } else {
            primary_routes.len()
        };
        let calls_label = if primary_routes.is_empty() {
            format!("{} aux calls", display.calls)
        } else {
            format!("{} calls", display.calls)
        };
        let mut parts = Vec::new();
        parts.push(calls_label);
        if route_count > 1 {
            parts.push(format!("{route_count} routes"));
        }
        parts.push(format!("↑{}", format_count(display.total_in)));
        parts.push(format!("↓{}", format_count(display.total_out)));
        if display.cache_read > 0 {
            let cache = if route_count == 1 && display.total_in > 0 {
                let hit_rate = (display.cache_read as f64 / display.total_in as f64 * 100.0) as u64;
                format!("cache {} ({}%)", format_count(display.cache_read), hit_rate)
            } else {
                format!("cache {}", format_count(display.cache_read))
            };
            parts.push(cache);
        }
        if speed > 0.0 {
            parts.push(format!("{:.0} tok/s", speed));
        }
        if !primary_routes.is_empty() && auxiliary_calls > 0 {
            parts.push(format!("+{auxiliary_calls} aux"));
        }
        let body = parts.join(" · ");
        let body_w = crate::width::width(body.as_str());
        let inner_w = (outer_width as usize).saturating_sub(2);
        let prefix_w = crate::width::width("╰─ ");
        let suffix_w = 1; // ╯
        let dash_w = inner_w
            .saturating_sub(prefix_w)
            .saturating_sub(body_w)
            .saturating_sub(suffix_w);
        format!("╰─ {body}{}╯", "─".repeat(dash_w))
    } else {
        format!("╰{}╯", "─".repeat((outer_width as usize).saturating_sub(2)))
    };
    Line::from(Span::styled(bottom_text, border_style))
}

#[allow(clippy::too_many_arguments)]
fn render_workflow_panel(
    graph: &atman_runtime::projection::workflow::WorkflowProjection,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    _started_at: std::time::Instant,
    _ended_at: Option<std::time::Instant>,
    animation_frame: u32,
    panel_width: u16,
) -> Vec<Line<'static>> {
    render_workflow_projection_with_regions(
        graph,
        expanded_nodes,
        panel_expanded,
        cancelled,
        animation_frame,
        panel_width,
        MAX_COLLAPSED_BODY_ROWS,
    )
    .0
}

pub fn render_workflow_panel_with_regions(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    animation_frame: u32,
    panel_width: u16,
    max_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    render_workflow_panel_impl(
        graph,
        None,
        expanded_nodes,
        panel_expanded,
        cancelled,
        animation_frame,
        panel_width,
        max_body_rows,
        0,
    )
}

pub fn render_workflow_projection_with_regions(
    graph: &atman_runtime::projection::workflow::WorkflowProjection,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    animation_frame: u32,
    panel_width: u16,
    max_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    render_workflow_projection_with_regions_min_body_rows(
        graph,
        expanded_nodes,
        panel_expanded,
        cancelled,
        animation_frame,
        panel_width,
        max_body_rows,
        0,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_workflow_projection_with_regions_min_body_rows(
    graph: &atman_runtime::projection::workflow::WorkflowProjection,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    animation_frame: u32,
    panel_width: u16,
    max_body_rows: usize,
    min_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    render_workflow_panel_impl(
        graph.graph(),
        Some(graph),
        expanded_nodes,
        panel_expanded,
        cancelled,
        animation_frame,
        panel_width,
        max_body_rows,
        min_body_rows,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_workflow_panel_impl(
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    animation_frame: u32,
    panel_width: u16,
    max_body_rows: usize,
    min_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let t = crate::theme::theme();
    let summary = permission_projection.map(WorkflowProjection::summary);
    let count = summary
        .map(|summary| summary.counts().nodes)
        .unwrap_or_else(|| count_workflow_nodes(&graph.root));
    let (mut status_str, mut status_style, running) = if let Some(summary) = summary {
        match summary.status() {
            WorkflowAggregateStatus::Running => {
                ("running…".into(), Style::default().fg(t.warn.into()), true)
            }
            WorkflowAggregateStatus::Error => {
                ("err".into(), Style::default().fg(t.error.into()), false)
            }
            WorkflowAggregateStatus::Empty => (
                "empty".into(),
                Style::default().fg(t.subtle_fg.into()),
                false,
            ),
            WorkflowAggregateStatus::Ok => {
                ("ok".into(), Style::default().fg(t.success.into()), false)
            }
        }
    } else {
        workflow_overall_status(&graph.root)
    };
    if cancelled {
        status_str = "Cancelled".to_string();
        status_style = Style::default().fg(t.warn.into());
    }
    let elapsed = summary
        .map(|summary| summary.elapsed_secs(chrono::Utc::now()))
        .unwrap_or_else(|| compute_elapsed_secs(&graph.root, running));
    let fold_glyph = if panel_expanded { "▼" } else { "▶" };
    let flow_glyph = if running {
        spinner_char(animation_frame)
    } else {
        "⚡"
    };
    let header = Line::from(vec![
        Span::styled(
            format!(" {fold_glyph} "),
            Style::default().fg(t.subtle_fg.into()),
        ),
        Span::styled(
            format!("{flow_glyph} workflow"),
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · {count} nodes · {} · ",
            atman_runtime::humanize::format_secs(elapsed)
        )),
        Span::styled(status_str, status_style),
    ]);
    if !panel_expanded {
        return render_collapsed_workflow_card(
            graph,
            permission_projection,
            animation_frame,
            panel_width,
            running,
            max_body_rows,
            min_body_rows,
        );
    }
    let mut lines = vec![header];
    let mut regions: Vec<NodeRegion> = Vec::new();
    // Register header click region so the expanded panel can be collapsed
    // by clicking the header (path_key="" triggers toggle_workflow_panel_expansion).
    regions.push(NodeRegion {
        panel_item_index: 0,
        path_key: String::new(),
        start_row: 0,
        end_row: 1,
        col_start: 0,
        col_end: panel_width,
    });
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
                    graph,
                    permission_projection,
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
                graph,
                permission_projection,
                node,
                expanded_nodes,
                &[],
                is_last,
                panel_width,
                &path,
                animation_frame,
                running,
                &mut pending_counter,
                None,
                0,
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

fn dynamic_paint_for_item(
    item: &OutputItem,
    lines: &[Line<'static>],
    _ctx: &RenderCtx<'_>,
) -> DynamicPaint {
    if !item.has_dynamic_paint() {
        return DynamicPaint::default();
    }
    match item {
        OutputItem::WorkflowPanel {
            graph,
            panel_expanded,
            ..
        } => workflow_dynamic_paint(graph, *panel_expanded, lines, 0),
        OutputItem::ToolDispatch { .. } => DynamicPaint {
            active: true,
            elapsed: None,
            running_rows: Vec::new(),
        },
        _ => DynamicPaint {
            active: true,
            elapsed: None,
            running_rows: Vec::new(),
        },
    }
}

pub(crate) fn workflow_dynamic_paint(
    graph: &WorkflowProjection,
    expanded: bool,
    lines: &[Line<'static>],
    line_offset: usize,
) -> DynamicPaint {
    let has_spinner = lines.iter().any(|line| {
        line.spans
            .iter()
            .any(|span| span.content.contains(DYNAMIC_SPINNER_MARKER))
    });
    if !has_spinner {
        return DynamicPaint::default();
    }
    let elapsed = expanded
        .then(|| {
            let started_at = graph.summary().started_at()?;
            let line = lines.get(line_offset)?;
            let (span, span_value) = line
                .spans
                .iter()
                .enumerate()
                .find(|(_, span)| span.content.contains(" nodes · "))?;
            let content = span_value.content.as_ref();
            let marker = " nodes · ";
            let duration_start = content.find(marker)?.saturating_add(marker.len());
            let duration_end = content[duration_start..]
                .rfind(" · ")?
                .saturating_add(duration_start);
            Some(ElapsedPaint {
                line: line_offset,
                span,
                prefix: content[..duration_start].to_string(),
                suffix: content[duration_end..].to_string(),
                started_at,
            })
        })
        .flatten();
    DynamicPaint {
        active: true,
        elapsed,
        running_rows: Vec::new(),
    }
}

fn count_workflow_nodes(nodes: &[atman_runtime::workflow::WorkflowNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_workflow_nodes(&n.children))
        .sum()
}

#[derive(Default, Debug, Clone, Copy)]
struct WorkflowStats {
    nodes: usize,
    agents: usize,
    tools: usize,
    edits: usize,
}

fn collect_stats(nodes: &[atman_runtime::workflow::WorkflowNode], acc: &mut WorkflowStats) {
    use atman_runtime::workflow::WorkflowNodeKind;
    for n in nodes {
        acc.nodes += 1;
        if let WorkflowNodeKind::ToolCall { tool, .. } = &n.kind {
            acc.tools += 1;
            if tool == "flow.spawn" {
                acc.agents += 1;
            }
            if matches!(
                tool.as_str(),
                "fs.edit" | "fs.write" | "hunk.apply" | "hunk.plan_edit"
            ) {
                acc.edits += 1;
            }
        }
        collect_stats(&n.children, acc);
    }
}

pub const COLLAPSED_CARD_FULLSCREEN_KEY: &str = "__collapsed_card_fullscreen__";
pub const TERMINAL_FULLSCREEN_KEY: &str = "__terminal_fullscreen__";
pub const BASH_FULLSCREEN_KEY: &str = "__bash_fullscreen__";
pub const MERMAID_FULLSCREEN_KEY: &str = "__mermaid_fullscreen__";
pub const SUB_AGENT_FULLSCREEN_KEY: &str = "__sub_agent_fullscreen__";

fn collect_all_leaves(
    nodes: &[atman_runtime::workflow::WorkflowNode],
    out: &mut Vec<Vec<usize>>,
    path: &mut Vec<usize>,
) {
    use atman_runtime::workflow::WorkflowNodeKind;
    for (i, n) in nodes.iter().enumerate() {
        path.push(i);
        if n.children.is_empty()
            && matches!(
                n.kind,
                WorkflowNodeKind::ToolCall { .. }
                    | WorkflowNodeKind::Stmt { .. }
                    | WorkflowNodeKind::FanoutBranch { .. }
            )
        {
            out.push(path.clone());
        }
        collect_all_leaves(&n.children, out, path);
        path.pop();
    }
}

fn leaf_is_running(nodes: &[atman_runtime::workflow::WorkflowNode], path: &[usize]) -> bool {
    use atman_runtime::workflow::NodeStatus;
    let node = leaf_at_path(nodes, path);
    matches!(
        node.map(|n| n.status),
        Some(NodeStatus::Running | NodeStatus::Pending)
    )
}

fn leaf_at_path<'a>(
    nodes: &'a [atman_runtime::workflow::WorkflowNode],
    path: &[usize],
) -> Option<&'a atman_runtime::workflow::WorkflowNode> {
    let mut cur = nodes;
    let mut node = None;
    for &i in path {
        node = cur.get(i);
        if let Some(n) = node {
            cur = &n.children;
        } else {
            return None;
        }
    }
    node
}

fn render_collapsed_workflow_card(
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    animation_frame: u32,
    panel_width: u16,
    running: bool,
    max_body_rows: usize,
    min_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let t = crate::theme::theme();
    let outer_width = panel_width.clamp(40, MAX_BOX_WIDTH);
    let border_style = Style::default().fg(t.accent.into());
    let summary = permission_projection.map(WorkflowProjection::summary);
    let stats = if let Some(summary) = summary {
        let counts = summary.counts();
        WorkflowStats {
            nodes: counts.nodes,
            agents: counts.agents,
            tools: counts.tools,
            edits: counts.edits,
        }
    } else {
        let mut stats = WorkflowStats::default();
        collect_stats(&graph.root, &mut stats);
        stats
    };
    let flow_glyph = if running {
        spinner_char(animation_frame)
    } else {
        "⚡"
    };
    let title = format!("{flow_glyph} workflow");
    let stats_text = format!(
        "{} nodes · {} agents · {} tools · {} edits",
        stats.nodes, stats.agents, stats.tools, stats.edits
    );
    let button_text = "─[⤢]─";
    let button_w = crate::width::width(button_text) as u16;
    let title_w = crate::width::width(title.as_str());
    let stats_w = crate::width::width(stats_text.as_str());
    let leading = 3usize;
    let trailing = 2usize;
    let separator_w = 3usize;
    let content_w = title_w + separator_w + stats_w;
    let fill_w =
        (outer_width as usize).saturating_sub(leading + content_w + trailing + button_w as usize);
    let mut top_spans: Vec<Span<'static>> = vec![
        Span::styled("╭─ ".to_string(), border_style),
        Span::styled(
            title,
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(stats_text, Style::default().fg(t.tinted_fg.into())),
    ];
    if fill_w > 0 {
        top_spans.push(Span::styled("─".repeat(fill_w), border_style));
    }
    let button_col_end = outer_width;
    let button_col_start = button_col_end.saturating_sub(button_w).saturating_sub(2);
    top_spans.push(Span::styled(
        button_text.to_string(),
        Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::BOLD),
    ));
    top_spans.push(Span::styled("─╮".to_string(), border_style));
    let mut lines: Vec<Line<'static>> = vec![Line::from(top_spans)];

    let root = if permission_projection.is_some() {
        std::borrow::Cow::Borrowed(graph.root.as_slice())
    } else {
        let mut root = graph.root.clone();
        root.sort_by_key(|node| node.started_at);
        std::borrow::Cow::Owned(root)
    };
    let ordered_pool = if let Some(summary) = summary {
        summary.collapsed_leaf_paths(max_body_rows.saturating_mul(4).max(32))
    } else {
        let mut all_leaf_paths = Vec::new();
        collect_all_leaves(&root, &mut all_leaf_paths, &mut Vec::new());
        let mut leaves_with_time = all_leaf_paths
            .into_iter()
            .map(|path| {
                let started_at = leaf_at_path(&root, &path)
                    .and_then(|node| node.started_at)
                    .unwrap_or_else(chrono::Utc::now);
                (path, started_at)
            })
            .collect::<Vec<_>>();
        leaves_with_time.sort_by_key(|(_, started_at)| std::cmp::Reverse(*started_at));
        let (running_paths, completed_paths): (Vec<_>, Vec<_>) = leaves_with_time
            .iter()
            .map(|(path, _)| path.clone())
            .partition(|path| leaf_is_running(&root, path));
        running_paths.into_iter().chain(completed_paths).collect()
    };

    // `estimated_rows` only depends on how many distinct top-level nodes the
    // first `count` paths cover:
    //   top_level_count(count) = |{ path[0] : path ∈ ordered_pool[..count] }|
    // Monotonic non-decreasing in `count` (the visible set only grows), so we
    // precompute in O(N) and binary-search in O(log N), replacing the old
    // O(N²) loop that rebuilt the visible subtree each iteration.
    let total = ordered_pool.len();
    let mut seen_top: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut prefix_top_level_count: Vec<usize> = Vec::with_capacity(total + 1);
    prefix_top_level_count.push(0);
    for path in &ordered_pool {
        if let Some(&top) = path.first() {
            seen_top.insert(top);
        }
        prefix_top_level_count.push(seen_top.len());
    }
    // Smallest `count` with estimated_rows >= max_body_rows; fall back to all
    // paths if none qualify. partition_point returns the first index where the
    // predicate ("< max_body_rows") is false.
    let idx = prefix_top_level_count[1..].partition_point(|&tc| (tc * 4) < max_body_rows);
    let target_count = (idx + 1).min(total);
    let (mut body_lines, mut regions) = render_collapsed_workflow_body(
        graph,
        permission_projection,
        &root,
        &ordered_pool[..target_count],
        outer_width,
        animation_frame,
        running,
    );
    if body_lines.len() < min_body_rows.min(max_body_rows) && target_count < total {
        (body_lines, regions) = render_collapsed_workflow_body(
            graph,
            permission_projection,
            &root,
            &ordered_pool,
            outer_width,
            animation_frame,
            running,
        );
    }
    if body_lines.len() > max_body_rows {
        let drain_count = body_lines.len() - max_body_rows;
        body_lines.drain(..drain_count);
        regions.retain(|r| r.end_row > drain_count as u32);
        for r in regions.iter_mut() {
            r.start_row = r.start_row.saturating_sub(drain_count as u32);
            r.end_row = r.end_row.saturating_sub(drain_count as u32);
        }
    }
    let common_prefix = body_lines
        .iter()
        .filter(|l| !l.spans.is_empty())
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.chars()
                .take_while(|c| matches!(c, ' ' | '┊' | '├' | '└' | '┈'))
                .count()
        })
        .min()
        .unwrap_or(0);
    if common_prefix > 0 {
        for line in body_lines.iter_mut() {
            trim_line_left(line, common_prefix);
        }
        for r in regions.iter_mut() {
            r.col_start = r.col_start.saturating_sub(common_prefix as u16);
            r.col_end = r.col_end.saturating_sub(common_prefix as u16);
        }
    }
    apply_lens_fade(&mut body_lines);
    let pad = min_body_rows
        .min(max_body_rows)
        .saturating_sub(body_lines.len());
    if pad > 0 {
        body_lines.splice(0..0, std::iter::repeat_n(Line::raw(""), pad));
        for region in &mut regions {
            region.start_row = region.start_row.saturating_add(pad as u32);
            region.end_row = region.end_row.saturating_add(pad as u32);
        }
    }
    let card_body_start_row = lines.len() as u32;
    for r in regions.iter_mut() {
        r.start_row = r.start_row.saturating_add(card_body_start_row);
        r.end_row = r.end_row.saturating_add(card_body_start_row);
    }
    lines.extend(body_lines);
    let bottom_line = format_workflow_stats_footer(graph, summary, outer_width, border_style);
    lines.push(bottom_line);
    lines.push(Line::raw(""));
    let card_rows = lines.len() as u32;
    regions.insert(
        0,
        NodeRegion {
            panel_item_index: 0,
            path_key: COLLAPSED_CARD_FULLSCREEN_KEY.to_string(),
            start_row: 0,
            end_row: 1,
            col_start: button_col_start,
            col_end: button_col_end,
        },
    );
    regions.push(NodeRegion {
        panel_item_index: 0,
        path_key: String::new(),
        start_row: 0,
        end_row: card_rows,
        col_start: 0,
        col_end: outer_width,
    });
    (lines, regions)
}

#[allow(clippy::too_many_arguments)]
fn render_collapsed_workflow_body(
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    root: &[atman_runtime::workflow::WorkflowNode],
    selected_paths: &[Vec<usize>],
    outer_width: u16,
    animation_frame: u32,
    running: bool,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let mut visible: std::collections::HashSet<Vec<usize>> = std::collections::HashSet::new();
    for path in selected_paths {
        for i in 1..=path.len() {
            visible.insert(path[..i].to_vec());
        }
    }
    let visible_str: std::collections::HashSet<String> = visible
        .iter()
        .map(|path| {
            path.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    let mut seen_top_level = std::collections::HashSet::new();
    let mut top_level = selected_paths
        .iter()
        .filter_map(|path| path.first().copied())
        .filter(|root_index| seen_top_level.insert(*root_index))
        .collect::<Vec<_>>();
    top_level.reverse();
    let mut body_lines = Vec::new();
    let mut regions = Vec::new();
    let mut pending_counter = 0;
    let child_count = top_level.len();
    for (position, root_index) in top_level.into_iter().enumerate() {
        let Some(node) = root.get(root_index) else {
            continue;
        };
        append_workflow_node_boxed(
            &mut body_lines,
            &mut regions,
            graph,
            permission_projection,
            node,
            &std::collections::HashSet::new(),
            &[],
            position + 1 == child_count,
            outer_width,
            &root_index.to_string(),
            animation_frame,
            running,
            &mut pending_counter,
            Some(&visible_str),
            1,
        );
    }
    (body_lines, regions)
}

fn trim_line_left(line: &mut Line<'static>, n: usize) {
    if n == 0 {
        return;
    }
    let mut remaining = n;
    let mut new_spans = Vec::with_capacity(line.spans.len());
    for span in line.spans.drain(..) {
        if remaining == 0 {
            new_spans.push(span);
            continue;
        }
        let chars: Vec<char> = span.content.chars().collect();
        if chars.len() <= remaining {
            remaining -= chars.len();
        } else {
            let trimmed: String = chars[remaining..].iter().collect();
            remaining = 0;
            new_spans.push(Span::styled(trimmed, span.style));
        }
    }
    line.spans = new_spans;
}

fn apply_lens_fade(body_lines: &mut [Line<'static>]) {
    let n = body_lines.len();
    if n <= 1 {
        return;
    }
    let n_f = (n - 1) as f32;
    for (i, line) in body_lines.iter_mut().enumerate() {
        let bottom_distance = (n - 1 - i) as f32 / n_f;
        if bottom_distance < 0.001 {
            continue;
        }
        let target = (200.0 - bottom_distance * 130.0).round() as u8;
        let shade = Color::Rgb(target, target, target);
        for span in line.spans.iter_mut() {
            if span.style.fg.is_some() {
                span.style.fg = Some(shade);
            }
        }
    }
}

fn workflow_overall_status(
    nodes: &[atman_runtime::workflow::WorkflowNode],
) -> (String, Style, bool) {
    use atman_runtime::workflow::NodeStatus;
    let t = crate::theme::theme();
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
        ("running…".into(), Style::default().fg(t.warn.into()), true)
    } else if has_err {
        ("err".into(), Style::default().fg(t.error.into()), false)
    } else if nodes.is_empty() {
        (
            "empty".into(),
            Style::default().fg(t.subtle_fg.into()),
            false,
        )
    } else {
        ("ok".into(), Style::default().fg(t.success.into()), false)
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
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    branches: &[atman_runtime::workflow::WorkflowNode],
    expanded_nodes: &std::collections::HashSet<String>,
    child_prefix: &str,
    parent_path: &str,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
    panel_width: u16,
) {
    let t = crate::theme::theme();
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
            graph,
            permission_projection,
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
        Style::default().fg(t.subtle_fg.into()),
    )];
    let mut cursor: u32 = 0;
    for i in 0..branch_count {
        let mid = cursor + col_width as u32 / 2;
        while cursor < mid {
            fork_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(t.accent.into()),
            ));
            cursor += 1;
        }
        fork_spans.push(Span::styled(
            "┬".to_string(),
            Style::default().fg(t.accent.into()),
        ));
        cursor += 1;
        let _ = i;
        while cursor < ((i + 1) as u32 * col_width as u32) {
            fork_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(t.accent.into()),
            ));
            cursor += 1;
        }
    }
    out.push(Line::from(fork_spans));
    let body_start_row = out.len() as u32;
    let max_height = per_branch_lines.iter().map(|b| b.len()).max().unwrap_or(0);
    for row_i in 0..max_height {
        let mut spans: Vec<Span<'static>> = vec![Span::raw(child_prefix.to_string())];
        for (b, branch_lines) in per_branch_lines.iter().enumerate() {
            let mut written: u16 = 0;
            let target = col_width;
            if let Some(line) = branch_lines.get(row_i) {
                for span in line.spans.iter() {
                    let mut take = String::new();
                    for (g, gw) in crate::width::graphemes(span.content.as_ref()) {
                        if written + gw as u16 > target {
                            break;
                        }
                        take.push_str(g);
                        written += gw as u16;
                    }
                    if !take.is_empty() {
                        spans.push(Span::styled(take, span.style));
                    }
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
        Style::default().fg(t.subtle_fg.into()),
    )];
    let mut cursor: u32 = 0;
    for i in 0..branch_count {
        let mid = cursor + col_width as u32 / 2;
        while cursor < mid {
            merge_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(t.accent.into()),
            ));
            cursor += 1;
        }
        merge_spans.push(Span::styled(
            "┴".to_string(),
            Style::default().fg(t.accent.into()),
        ));
        cursor += 1;
        while cursor < ((i + 1) as u32 * col_width as u32) {
            merge_spans.push(Span::styled(
                "─".to_string(),
                Style::default().fg(t.accent.into()),
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

const MAX_BOX_WIDTH: u16 = crate::layout::CONTENT_MAX_WIDTH;
const INDENT_PER_DEPTH: u16 = 4;
pub(crate) const MAX_COLLAPSED_BODY_ROWS: usize = 19;
const MAX_COLLAPSED_INDENT: u16 = 12;

fn tree_prefix_spans(ancestor_last: &[bool], is_last: Option<bool>) -> Vec<Span<'static>> {
    let t = crate::theme::theme();
    let style = Style::default().fg(t.subtle_fg.into());
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
    let t = crate::theme::theme();
    let style = Style::default().fg(t.subtle_fg.into());
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
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    node: &atman_runtime::workflow::WorkflowNode,
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_last: &[bool],
    is_last: bool,
    panel_width: u16,
    path: &str,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
    visible_paths: Option<&std::collections::HashSet<String>>,
    depth_offset: u16,
) {
    use atman_runtime::workflow::{ApprovalState, NodeStatus, WorkflowNodeKind};
    #[cfg(test)]
    update_perf_counters(|counters| {
        counters.workflow_node_renders = counters.workflow_node_renders.saturating_add(1);
    });
    let t = crate::theme::theme();
    let depth = ancestor_last.len() as u16;
    let prefix_w = depth.saturating_sub(depth_offset) * INDENT_PER_DEPTH;
    let prefix_w = if depth_offset > 0 {
        prefix_w.min(MAX_COLLAPSED_INDENT)
    } else {
        prefix_w
    };
    let col0 = prefix_w;
    let budget = panel_width.saturating_sub(prefix_w).min(MAX_BOX_WIDTH);
    if budget < 8 {
        return;
    }
    let mut border_style = match node.status {
        NodeStatus::Ok => Style::default().fg(t.success.into()),
        NodeStatus::Err => Style::default().fg(t.error.into()),
        NodeStatus::Cancelled => Style::default().fg(t.subtle_fg.into()),
        NodeStatus::Running | NodeStatus::Pending => Style::default().fg(t.accent.into()),
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
        WorkflowNodeKind::Flow { .. } => ("⚡", t.accent.into()),
        WorkflowNodeKind::Subflow { .. } => ("↳", t.accent.into()),
        WorkflowNodeKind::Stmt { node_kind } => stmt_kind_glyph(node_kind),
        WorkflowNodeKind::ToolCall { .. } => ("🔧", t.accent.into()),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", t.accent.into()),
    };
    let label = match &node.kind {
        WorkflowNodeKind::ToolCall {
            tool,
            args_preview,
            call_intent,
            ..
        } => workflow_tool_label(tool, args_preview, call_intent.as_ref(), 30, false),
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("branch[{branch_index}]  {}", node.label)
        }
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::When { condition_preview },
        } if !condition_preview.is_empty() && condition_preview != "when" => {
            format!(
                "when {} → {}",
                condition_preview,
                node.output_preview.as_deref().unwrap_or("?")
            )
        }
        _ => node.label.clone(),
    };
    let label = if let Some(stats) = &node.llm_stats {
        format!("{label}  · {}", format_llm_stats_brief(stats))
    } else {
        label
    };
    let mut pending_number = None;
    let mut auto_expand = false;
    if matches!(&node.approval, Some(ApprovalState::Pending { .. })) {
        *pending_counter = pending_counter.saturating_add(1);
        pending_number = (*pending_counter <= 9).then_some(*pending_counter);
        border_style = Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::BOLD);
        auto_expand = true;
    } else if matches!(&node.approval, Some(ApprovalState::Denied { .. })) {
        border_style = Style::default().fg(t.error.into());
    }
    let approval = approval_badge(
        node.approval.as_ref(),
        pending_number,
        permission_request_for_node(graph, permission_projection, node)
            .is_some_and(|request| !request.payload.group_ids.is_empty()),
    );
    let is_expanded = auto_expand || expanded_nodes.contains(path);
    let mut inner_lines: Vec<Line<'static>> = Vec::new();
    if is_expanded {
        collect_boxed_details(graph, permission_projection, node, &mut inner_lines);
    }
    let approval_seg = approval
        .as_ref()
        .map_or(0, |(badge, _)| crate::width::width(badge.as_str()) + 2);
    let status_seg = if crate::width::width(status_glyph) > 0 {
        crate::width::width(status_glyph) + 1
    } else {
        0
    };
    let kind_seg = if crate::width::width(kind_glyph) > 0 {
        crate::width::width(kind_glyph) + 1
    } else {
        0
    };
    let compact_content =
        3 + status_seg + kind_seg + crate::width::width(label.as_str()) + approval_seg + 2;
    let compact_w = compact_content.min(budget as usize) as u16;
    let outer_width = if is_expanded { budget } else { compact_w };
    let mut scratch: Vec<Line<'static>> = Vec::new();
    let start_row = out.len() as u32;
    let rect = append_box(
        &mut scratch,
        BoxSpec {
            row0: start_row as u16,
            col0,
            outer_width,
            inner_lines,
            border_style,
            status_glyph,
            kind_glyph,
            label: &label,
            approval_badge: approval,
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
        start_row: rect.row0 as u32,
        end_row: rect.row0.saturating_add(rect.rows) as u32,
        col_start: rect.col0,
        col_end: rect.col_end(),
    });
    let mut child_ancestor_last: Vec<bool> = ancestor_last.to_vec();
    child_ancestor_last.push(is_last);
    let child_count = node.children.len();
    let child_prefix_w = child_ancestor_last.len() as u16;
    let child_prefix_w = child_prefix_w.saturating_sub(depth_offset) * INDENT_PER_DEPTH;
    if is_fanout_group(node)
        && (2..=FANOUT_MAX_BRANCHES).contains(&child_count)
        && panel_width >= FANOUT_MIN_WIDTH
        && panel_width.saturating_sub(child_prefix_w) / child_count as u16 >= FANOUT_MIN_COL_WIDTH
    {
        append_fanout_horizontal_boxed(
            out,
            regions,
            graph,
            permission_projection,
            &node.children,
            expanded_nodes,
            &child_ancestor_last,
            path,
            panel_width,
            animation_frame,
            flow_running,
            pending_counter,
            depth_offset,
        );
        return;
    }
    for (i, child) in node.children.iter().enumerate() {
        let child_path = format!("{path}/{i}");
        if let Some(vp) = visible_paths {
            if !vp.contains(&child_path) {
                continue;
            }
        }
        let child_is_last = i + 1 == child_count;
        append_workflow_node_boxed(
            out,
            regions,
            graph,
            permission_projection,
            child,
            expanded_nodes,
            &child_ancestor_last,
            child_is_last,
            panel_width,
            &child_path,
            animation_frame,
            flow_running,
            pending_counter,
            visible_paths,
            depth_offset,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn append_fanout_horizontal_boxed(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    branches: &[atman_runtime::workflow::WorkflowNode],
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_last: &[bool],
    parent_path: &str,
    panel_width: u16,
    animation_frame: u32,
    flow_running: bool,
    pending_counter: &mut u8,
    depth_offset: u16,
) {
    let branch_count = branches.len();
    let prefix_w = (ancestor_last.len() as u16).saturating_sub(depth_offset) * INDENT_PER_DEPTH;
    let prefix_w = if depth_offset > 0 {
        prefix_w.min(MAX_COLLAPSED_INDENT)
    } else {
        prefix_w
    };
    let col_width = panel_width
        .saturating_sub(prefix_w)
        .saturating_div(branch_count as u16);
    let start_row_before = out.len() as u32;
    let mut per_branch_lines: Vec<Vec<Line<'static>>> = Vec::with_capacity(branch_count);
    let mut per_branch_regions: Vec<Vec<NodeRegion>> = Vec::with_capacity(branch_count);
    for (i, branch) in branches.iter().enumerate() {
        let branch_path = format!("{parent_path}/{i}");
        let is_last = i + 1 == branch_count;
        let mut b_lines: Vec<Line<'static>> = Vec::new();
        let mut b_regions: Vec<NodeRegion> = Vec::new();
        append_workflow_node_boxed(
            &mut b_lines,
            &mut b_regions,
            graph,
            permission_projection,
            branch,
            expanded_nodes,
            &[],
            is_last,
            col_width,
            &branch_path,
            animation_frame,
            flow_running,
            pending_counter,
            None,
            depth_offset,
        );
        per_branch_lines.push(b_lines);
        per_branch_regions.push(b_regions);
    }
    let max_height = per_branch_lines.iter().map(|b| b.len()).max().unwrap_or(0);
    for row_i in 0..max_height {
        let mut spans: Vec<Span<'static>> = tree_continuation_spans(ancestor_last, true);
        for branch_lines in per_branch_lines.iter() {
            let mut written: u16 = 0;
            if let Some(line) = branch_lines.get(row_i) {
                for span in line.spans.iter() {
                    let content = span.content.as_ref();
                    let mut used: u16 = 0;
                    let mut taken = String::new();
                    for (g, gw) in crate::width::graphemes(content) {
                        if used + gw as u16 > col_width.saturating_sub(written) {
                            break;
                        }
                        taken.push_str(g);
                        used += gw as u16;
                    }
                    if !taken.is_empty() {
                        spans.push(Span::styled(taken, span.style));
                        written = written.saturating_add(used);
                    }
                    if written >= col_width {
                        break;
                    }
                }
            }
            while written < col_width {
                spans.push(Span::raw(" ".to_string()));
                written += 1;
            }
        }
        out.push(Line::from(spans));
    }
    for (i, branch_regions) in per_branch_regions.into_iter().enumerate() {
        let col_shift = prefix_w + (i as u16) * col_width;
        for mut r in branch_regions {
            r.start_row = start_row_before.saturating_add(r.start_row);
            r.end_row = start_row_before.saturating_add(r.end_row);
            r.col_start = col_shift.saturating_add(r.col_start);
            r.col_end = col_shift.saturating_add(r.col_end.min(col_width));
            regions.push(r);
        }
    }
}

fn approval_badge(
    approval: Option<&atman_runtime::workflow::ApprovalState>,
    pending_number: Option<u8>,
    grouped: bool,
) -> Option<(String, Style)> {
    use atman_runtime::workflow::ApprovalState;
    let t = crate::theme::theme();
    let (mut badge, style) = match approval {
        Some(ApprovalState::Pending { .. }) => (
            pending_number
                .map(|number| format!("◷{number}"))
                .unwrap_or_else(|| "◷".into()),
            Style::default()
                .fg(t.warn.into())
                .add_modifier(Modifier::BOLD),
        ),
        Some(ApprovalState::Approved) => (
            "✓".into(),
            Style::default()
                .fg(t.success.into())
                .add_modifier(Modifier::BOLD),
        ),
        Some(ApprovalState::Denied { .. }) => (
            "⊘".into(),
            Style::default()
                .fg(t.error.into())
                .add_modifier(Modifier::BOLD),
        ),
        None => return None,
    };
    if grouped {
        badge.push('Ⓖ');
    }
    Some((badge, style))
}

fn permission_request_for_node<'a>(
    graph: &'a atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&'a atman_runtime::projection::workflow::WorkflowProjection>,
    node: &atman_runtime::workflow::WorkflowNode,
) -> Option<&'a atman_runtime::workflow::WorkflowPermissionRequest> {
    if let Some(projection) = permission_projection {
        return projection.permission_request_for_node(&node.id);
    }
    let atman_runtime::workflow::WorkflowNodeKind::ToolCall { tool_use_id, .. } = &node.kind else {
        return None;
    };
    graph.permission_requests.values().find(|request| {
        request.payload.tool_use_id == *tool_use_id
            && node.id
                == format!(
                    "tool:{}:{}",
                    request.payload.requesting_run_id.0, request.payload.tool_use_id
                )
    })
}

fn permission_detail_sections(
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    node: &atman_runtime::workflow::WorkflowNode,
) -> Vec<(&'static str, String)> {
    let Some(request) = permission_request_for_node(graph, permission_projection, node) else {
        return Vec::new();
    };
    let payload = &request.payload;
    let mut sections = vec![(
        "approval",
        format!(
            "{} · {}",
            format!("{:?}", request.state).to_lowercase(),
            format!("{:?}", payload.tier).to_lowercase()
        ),
    )];
    if let Some(reason) = &payload.reason {
        sections.push(("reason", reason.clone()));
    }
    if let Some(call_intent) = &payload.call_intent {
        sections.push(("purpose", call_intent.as_str().into()));
    }
    if let Some(actor) = &payload.actor {
        sections.push(("actor", format!("{actor:?}")));
    }
    if let Some(scope) = &payload.scope {
        sections.push(("scope", format!("{scope:?}")));
    }
    if payload.provenance.risks.contains("ProcessSpawn") {
        sections.push((
            "execution",
            match payload.execution_boundary {
                Some(atman_runtime::permission::ExecutionBoundary::Sandboxed) => "sandboxed".into(),
                Some(atman_runtime::permission::ExecutionBoundary::Direct) => "direct".into(),
                None => "not executed".into(),
            },
        ));
    }
    sections.push((
        "policy",
        format!(
            "{} · {}",
            payload.policy.rule_id, payload.policy.snapshot_id
        ),
    ));
    let provenance = &payload.provenance;
    let mut provenance_parts = Vec::new();
    if let Some(cwd) = &provenance.cwd {
        provenance_parts.push(format!("cwd={cwd}"));
    }
    if let Some(path) = &provenance.path {
        provenance_parts.push(format!("path={path}"));
    }
    if provenance.network {
        provenance_parts.push("network".into());
    }
    if !provenance.risks.is_empty() {
        provenance_parts.push(format!(
            "risks={}",
            provenance
                .risks
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if !provenance.targets.is_empty() {
        provenance_parts.push(format!("targets={}", provenance.targets.join(",")));
    }
    if !provenance_parts.is_empty() {
        sections.push(("provenance", provenance_parts.join(" · ")));
    }
    for group_id in &payload.group_ids {
        if let Some(group) = graph.permission_groups.get(group_id) {
            let (resolved, total) = permission_projection
                .and_then(|projection| projection.permission_group_progress(group_id))
                .unwrap_or_else(|| {
                    let resolved = group
                        .request_ids
                        .iter()
                        .filter(|request_id| {
                            graph.permission_requests.iter().any(|(identity, request)| {
                                matches!(
                                    identity,
                                    atman_runtime::workflow::WorkflowPermissionIdentity::Canonical {
                                        request_id: id
                                    } if id == *request_id
                                ) && !request.state.is_pending()
                            })
                        })
                        .count();
                    (resolved, group.request_ids.len())
                });
            sections.push((
                "group",
                format!("{} · {resolved}/{total} resolved", group.label),
            ));
        }
    }
    if let Some(request_id) = &payload.request_id {
        sections.push(("request", request_id.to_string()));
    }
    sections
}

fn collect_boxed_details(
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    node: &atman_runtime::workflow::WorkflowNode,
    out: &mut Vec<Line<'static>>,
) {
    use atman_runtime::workflow::{ApprovalState, WorkflowNodeKind};
    match &node.kind {
        WorkflowNodeKind::ToolCall {
            args_preview,
            result_preview,
            ..
        } => {
            if !args_preview.is_empty() {
                push_detail_section(out, "args", args_preview);
            }
            if let Some(r) = result_preview {
                push_detail_section(out, "result", r);
            }
        }
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::When { condition_preview },
        } => {
            if !condition_preview.is_empty() {
                push_detail_section(out, "condition", condition_preview);
            }
        }
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::Llm { model: Some(model) },
        } => {
            push_detail_section(out, "model", model);
        }
        WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::Subflow { name },
        } => {
            push_detail_section(out, "subflow", name);
        }
        WorkflowNodeKind::Subflow { flow_name, .. } => {
            push_detail_section(out, "flow", flow_name);
        }
        _ => {}
    }
    if let Some(p) = &node.output_preview {
        push_detail_section(out, "output", p);
    }
    if permission_request_for_node(graph, permission_projection, node).is_none()
        && let Some(ApprovalState::Pending {
            level,
            preview: Some(p),
        }) = &node.approval
    {
        push_detail_section(out, &format!("approval ({level})"), p);
    }
    for (label, body) in permission_detail_sections(graph, permission_projection, node) {
        push_detail_section(out, label, &body);
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
    let t = crate::theme::theme();
    out.push(Line::from(Span::styled(
        format!("{header}:"),
        Style::default().fg(t.subtle_fg.into()),
    )));
    for line in body.lines().take(20) {
        out.push(Line::from(Span::raw(line.to_string())));
    }
}

#[allow(clippy::too_many_arguments)]
fn append_workflow_node(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
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
    let t = crate::theme::theme();
    let start_row = out.len() as u32;
    let effective = node;
    let (branch_glyph, branch_color) = if matches!(node.kind, WorkflowNodeKind::FanoutBranch { .. })
    {
        if is_last {
            ("╚═", t.accent.into())
        } else {
            ("╠═", t.accent.into())
        }
    } else if is_last {
        ("└─", t.subtle_fg.into())
    } else {
        ("├─", t.subtle_fg.into())
    };
    let (status_glyph, status_style) = match effective.status {
        NodeStatus::Ok => ("✓", Style::default().fg(t.success.into())),
        NodeStatus::Err => ("✗", Style::default().fg(t.error.into())),
        NodeStatus::Cancelled => ("⊘", Style::default().fg(t.subtle_fg.into())),
        NodeStatus::Running | NodeStatus::Pending => {
            if flow_running {
                (
                    spinner_char(animation_frame),
                    Style::default().fg(t.warn.into()),
                )
            } else {
                ("○", Style::default().fg(t.subtle_fg.into()))
            }
        }
    };
    let (kind_glyph, kind_color) = match &effective.kind {
        WorkflowNodeKind::Flow { .. } => ("⚡", t.accent.into()),
        WorkflowNodeKind::Subflow { .. } => ("↳", t.accent.into()),
        WorkflowNodeKind::Stmt { node_kind } => stmt_kind_glyph(node_kind),
        WorkflowNodeKind::ToolCall { .. } => ("🔧", t.accent.into()),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", t.accent.into()),
    };
    let base_label = match &effective.kind {
        WorkflowNodeKind::ToolCall {
            tool,
            args_preview,
            call_intent,
            ..
        } => workflow_tool_label(tool, args_preview, call_intent.as_ref(), 60, true),
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
    let pending_number = if matches!(&effective.approval, Some(ApprovalState::Pending { .. })) {
        *pending_counter = pending_counter.saturating_add(1);
        (*pending_counter <= 9).then_some(*pending_counter)
    } else {
        None
    };
    let approval = approval_badge(
        effective.approval.as_ref(),
        pending_number,
        permission_request_for_node(graph, permission_projection, effective)
            .is_some_and(|request| !request.payload.group_ids.is_empty()),
    );
    let label = base_label;
    let mut spans = vec![
        Span::styled(
            format!("{ancestor_prefix}{branch_glyph} "),
            Style::default().fg(branch_color),
        ),
        Span::styled(format!("{status_glyph} "), status_style),
        Span::styled(
            expand_glyph.to_string(),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ];
    spans.push(Span::styled(
        format!("{kind_glyph} "),
        Style::default().fg(kind_color),
    ));
    spans.push(Span::raw(label));
    if let Some((text, style)) = approval {
        spans.push(Span::styled(format!("  {text}"), style));
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
        append_expanded_details(out, graph, permission_projection, effective, &child_prefix);
    }
    let child_count = effective.children.len();
    if child_count > 1
        && is_fanout_group(effective)
        && horizontal_layout_feasible(effective.children.len(), panel_width, &child_prefix)
    {
        append_fanout_horizontal(
            out,
            regions,
            graph,
            permission_projection,
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
            graph,
            permission_projection,
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

fn workflow_tool_label(
    tool: &str,
    args_preview: &str,
    call_intent: Option<&atman_runtime::message::ToolCallIntent>,
    args_width: usize,
    show_empty_args: bool,
) -> String {
    if let Some(call_intent) = call_intent {
        return format!("{} · {tool}", call_intent.as_str());
    }
    let short_args = crate::width::truncate(args_preview, args_width);
    if short_args.is_empty() {
        if show_empty_args {
            format!("{tool}()")
        } else {
            tool.to_string()
        }
    } else {
        format!("{tool}({short_args})")
    }
}

fn append_expanded_details(
    out: &mut Vec<Line<'static>>,
    graph: &atman_runtime::workflow::WorkflowGraph,
    permission_projection: Option<&atman_runtime::projection::workflow::WorkflowProjection>,
    node: &atman_runtime::workflow::WorkflowNode,
    prefix: &str,
) {
    use atman_runtime::workflow::WorkflowNodeKind;
    let t = crate::theme::theme();
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
    sections.extend(permission_detail_sections(
        graph,
        permission_projection,
        node,
    ));
    for (label, body) in sections {
        out.push(Line::from(vec![Span::styled(
            format!("{prefix}  ▪ {label}:"),
            Style::default().fg(t.subtle_fg.into()),
        )]));
        for line in body.lines().take(20) {
            let trimmed: String = line.chars().take(200).collect();
            out.push(Line::from(vec![
                Span::styled(
                    format!("{prefix}    "),
                    Style::default().fg(t.subtle_fg.into()),
                ),
                Span::styled(trimmed, Style::default().fg(t.tinted_fg.into())),
            ]));
        }
    }
}

fn stmt_kind_glyph(kind: &atman_runtime::nodegraph::NodeKind) -> (&'static str, Color) {
    use atman_runtime::nodegraph::NodeKind;
    let t = crate::theme::theme();
    match kind {
        NodeKind::Llm { .. } => ("✦", t.accent.into()),
        NodeKind::ToolCall { .. } => ("🔧", t.accent.into()),
        NodeKind::Fanout { .. } => ("⇉", t.accent.into()),
        NodeKind::UserConfirm => ("?", t.accent.into()),
        NodeKind::Subflow { .. } => ("↳", t.accent.into()),
        NodeKind::Message { .. } => ("✉", t.tinted_fg.into()),
        NodeKind::FixUntilTest => ("↻", t.accent.into()),
        NodeKind::When { .. } => ("⋯", t.subtle_fg.into()),
        NodeKind::Loop => ("↻", t.accent.into()),
        NodeKind::Return => ("←", t.success.into()),
    }
}

fn format_llm_stats_brief(stats: &atman_runtime::workflow::LlmStats) -> String {
    use atman_runtime::humanize::format_count;
    let mut parts = Vec::new();
    if stats.context_call_purpose != atman_runtime::ContextCallPurpose::General
        || stats.context_call_scope != atman_runtime::ContextCallScope::Root
    {
        parts.push(format!(
            "{} {}",
            stats.context_call_scope.as_str(),
            stats.context_call_purpose.as_str()
        ));
    }
    if stats.cache_read > 0 {
        let total_in = stats
            .input_tokens
            .saturating_add(stats.cache_read)
            .saturating_add(stats.cache_write);
        let hit_rate = if total_in > 0 {
            (stats.cache_read as f64 / total_in as f64 * 100.0) as u64
        } else {
            0
        };
        parts.push(format!(
            "cache {} ({}%)",
            format_count(stats.cache_read),
            hit_rate
        ));
    }
    if stats.ttft_ms > 0 {
        parts.push(format!("ttft {}ms", stats.ttft_ms));
    }
    if stats.tokens_per_second > 0.0 {
        parts.push(format!("{:.0} tok/s", stats.tokens_per_second));
    }
    if stats.output_tokens > 0 {
        parts.push(format!("↓{}", format_count(stats.output_tokens)));
    }
    parts.join(" · ")
}

pub fn empty_hint<'a>() -> Paragraph<'a> {
    let t = crate::theme::theme();
    Paragraph::new("plain text → agent · :help for builtins · Ctrl+C to interrupt")
        .style(Style::default().fg(t.subtle_fg.into()))
        .wrap(Wrap { trim: true })
}

fn compact_header_label(title: &str, metadata: Option<&str>, max_width: usize) -> String {
    let title = crate::width::truncate(title, max_width);
    let Some(metadata) = metadata else {
        return title;
    };
    let title_width = crate::width::width(title.as_str());
    let separator = " · ";
    let separator_width = crate::width::width(separator);
    let remaining = max_width.saturating_sub(title_width);
    if remaining <= separator_width + 3 {
        return title;
    }
    let metadata = crate::width::middle_truncate(metadata, remaining - separator_width);
    format!("{title}{separator}{metadata}")
}

fn append_command_lines(
    lines: &mut Vec<Line<'static>>,
    command: Option<&str>,
    expanded: bool,
    target: usize,
    command_style: Style,
    hint_style: Style,
) {
    let Some(command) = command.filter(|command| !command.is_empty()) else {
        return;
    };
    if expanded {
        let prefix = format!("{DOCUMENT_PAD}${DOCUMENT_PAD}");
        let continuation = " ".repeat(crate::width::width(&prefix));
        for row in wrap_with_prefix(command, target, &prefix, &continuation) {
            lines.push(line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                hint_style,
                command_style,
            ));
        }
    } else {
        let first = command.split('\n').next().unwrap_or_default();
        let suffix = if command.contains('\n') {
            " ↩ …"
        } else {
            ""
        };
        let prefix = format!("{DOCUMENT_PAD}${DOCUMENT_PAD}");
        let budget = target
            .saturating_sub(crate::width::width(&prefix))
            .saturating_sub(crate::width::width(suffix))
            .saturating_sub(RIGHT_PAD);
        let preview = crate::width::middle_truncate(first, budget);
        lines.push(line_with_right_pad(
            &prefix,
            &format!("{preview}{suffix}"),
            target,
            hint_style,
            command_style,
        ));
    }
    lines.push(Line::from(Span::styled(" ".repeat(target), command_style)));
}

#[allow(clippy::too_many_arguments)]
fn render_output_block(
    title: &str,
    metadata: Option<&str>,
    glyph: &str,
    command: Option<&str>,
    output: &str,
    expanded: bool,
    panel_width: u16,
    fullscreen_hovered: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg: Color = t.code_bg.into();
    let header_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let body_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);

    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_lead = format!("{DOCUMENT_PAD}{glyph}{DOCUMENT_PAD}");
    let header_trailing = format!("{DOCUMENT_PAD}⤢{DOCUMENT_PAD}");
    let label_budget = target
        .saturating_sub(crate::width::width(header_lead.as_str()))
        .saturating_sub(crate::width::width(&header_trailing));
    let label = compact_header_label(title, metadata, label_budget);
    let header_prefix = format!("{header_lead}{label}");
    let header_used = crate::width::width(header_prefix.as_str());
    let fs_btn = "⤢";
    let fs_btn_used = crate::width::width(fs_btn);
    let gap = DOCUMENT_PAD_X;
    let header_pad = target
        .saturating_sub(header_used)
        .saturating_sub(fs_btn_used)
        .saturating_sub(gap * 2);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    header_spans.push(Span::styled(
        fs_btn.to_string(),
        if fullscreen_hovered {
            Style::default()
                .fg(t.accent.into())
                .bg(t.panel_bg.lerp(t.user_msg_bg, 0.65))
                .add_modifier(Modifier::BOLD)
        } else {
            hint_style
        },
    ));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    append_command_lines(
        &mut lines, command, expanded, target, body_style, hint_style,
    );

    let (total_rows, output_rows) = if expanded {
        let rows = output
            .lines()
            .flat_map(|line| wrap_with_prefix(line, target, DOCUMENT_PAD, DOCUMENT_PAD))
            .collect::<Vec<_>>();
        (rows.len(), rows)
    } else {
        wrap_tail_with_prefix(output, target, DOCUMENT_PAD, 8)
    };
    let hidden_rows = total_rows.saturating_sub(output_rows.len());
    for row in output_rows {
        lines.push(line_with_right_pad(
            &row.prefix,
            &row.body,
            target,
            body_style,
            body_style,
        ));
    }
    if !expanded && hidden_rows > 0 {
        let unit = if hidden_rows == 1 { "line" } else { "lines" };
        let hint =
            format!("{DOCUMENT_PAD}▼{DOCUMENT_PAD}{hidden_rows} more {unit} — click to expand");
        lines.push(line_with_right_pad(
            "", &hint, target, hint_style, hint_style,
        ));
    } else if expanded && total_rows > 8 {
        let hint = format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse");
        lines.push(line_with_right_pad(
            "", &hint, target, hint_style, hint_style,
        ));
    }
    lines.push(blank);
    lines
}

struct BashProjectionInput<'a> {
    handle: &'a str,
    title: Option<&'a str>,
    command: Option<&'a str>,
    output: &'a str,
    generation: u64,
    done: bool,
    expanded: bool,
    panel_width: u16,
    fullscreen_hovered: bool,
}

#[derive(Clone)]
struct BashOutputProjection {
    index: crate::wrapped_text::WrappedRowIndex,
    before_output: Arc<[Line<'static>]>,
    after_output: Arc<[Line<'static>]>,
    output_start: usize,
    output_rows: usize,
    rows: usize,
    target: usize,
    body_style: Style,
    prepared_start: usize,
    prepared_end: usize,
    prepared_lines: Arc<[Line<'static>]>,
}

impl Default for BashOutputProjection {
    fn default() -> Self {
        Self {
            index: crate::wrapped_text::WrappedRowIndex::default(),
            before_output: Arc::from([]),
            after_output: Arc::from([]),
            output_start: 0,
            output_rows: 0,
            rows: 0,
            target: 0,
            body_style: Style::default(),
            prepared_start: 0,
            prepared_end: 0,
            prepared_lines: Arc::from([]),
        }
    }
}

impl BashOutputProjection {
    const COLLAPSED_ROWS: usize = 8;
    const OUTPUT_PREFIX: &'static str = DOCUMENT_PAD;

    fn update(&mut self, input: BashProjectionInput<'_>) -> usize {
        let t = crate::theme::theme();
        let bg: Color = t.code_bg.into();
        self.target = input.panel_width.max(20) as usize;
        self.body_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
        let body_width = self
            .target
            .saturating_sub(crate::width::width(Self::OUTPUT_PREFIX))
            .saturating_sub(RIGHT_PAD)
            .max(1);
        let indexed_bytes = self
            .index
            .update(
                input.output,
                input.generation,
                body_width,
                crate::wrapped_text::WrappedLineMode::Lines,
            )
            .indexed_bytes;

        let glyph = if input.done {
            "✓"
        } else {
            spinner_char(LAYOUT_ANIMATION_FRAME)
        };
        let metadata = format!("bash[{}]", input.handle);
        let mut before_output = render_output_block(
            input.title.unwrap_or("bash"),
            Some(&metadata),
            glyph,
            input.command,
            "",
            input.expanded,
            input.panel_width,
            input.fullscreen_hovered,
        );
        let blank = before_output
            .pop()
            .unwrap_or_else(|| Line::from(Span::styled(" ".repeat(self.target), self.body_style)));

        let total_output_rows = self.index.len();
        self.output_start = if input.expanded {
            0
        } else {
            total_output_rows.saturating_sub(Self::COLLAPSED_ROWS)
        };
        self.output_rows = total_output_rows.saturating_sub(self.output_start);
        let mut after_output = Vec::with_capacity(2);
        let hint = if !input.expanded && self.output_start > 0 {
            let unit = if self.output_start == 1 {
                "line"
            } else {
                "lines"
            };
            Some(format!(
                "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{} more {unit} — click to expand",
                self.output_start
            ))
        } else if input.expanded && total_output_rows > Self::COLLAPSED_ROWS {
            Some(format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse"))
        } else {
            None
        };
        if let Some(hint) = hint {
            let hint_style = Style::default()
                .fg(t.meta_fg.into())
                .bg(bg)
                .add_modifier(Modifier::DIM);
            after_output.push(line_with_right_pad(
                "",
                &hint,
                self.target,
                hint_style,
                hint_style,
            ));
        }
        after_output.push(blank);
        after_output.push(Line::from(Span::styled(String::new(), RESET)));

        self.before_output = Arc::from(before_output);
        self.after_output = Arc::from(after_output);
        self.rows = self
            .before_output
            .len()
            .saturating_add(self.output_rows)
            .saturating_add(self.after_output.len());
        self.prepared_start = 0;
        self.prepared_end = 0;
        self.prepared_lines = Arc::from([]);
        indexed_bytes
    }

    fn rows(&self) -> usize {
        self.rows
    }

    fn prepare_range(&mut self, source: &str, start: usize, end: usize) -> usize {
        let start = start.min(self.rows);
        let end = end.min(self.rows).max(start);
        if start >= self.prepared_start && end <= self.prepared_end {
            return 0;
        }
        let output_offset = self.before_output.len();
        let after_offset = output_offset.saturating_add(self.output_rows);
        let mut lines = Vec::with_capacity(end.saturating_sub(start));
        let mut materialized_output_rows = 0usize;
        for row in start..end {
            if row < output_offset {
                lines.push(self.before_output[row].clone());
            } else if row < after_offset {
                materialized_output_rows = materialized_output_rows.saturating_add(1);
                let source_row = self
                    .output_start
                    .saturating_add(row.saturating_sub(output_offset));
                let body = self.index.row(source, source_row).unwrap_or_default();
                lines.push(line_with_right_pad(
                    Self::OUTPUT_PREFIX,
                    body,
                    self.target,
                    self.body_style,
                    self.body_style,
                ));
            } else {
                lines.push(self.after_output[row.saturating_sub(after_offset)].clone());
            }
        }
        self.prepared_start = start;
        self.prepared_end = end;
        self.prepared_lines = Arc::from(lines);
        materialized_output_rows
    }

    fn append_prepared_range(
        &self,
        start: usize,
        end: usize,
        out: &mut Vec<Line<'static>>,
    ) -> bool {
        if start == end {
            return true;
        }
        if start < self.prepared_start || end > self.prepared_end {
            return false;
        }
        let local_start = start.saturating_sub(self.prepared_start);
        let local_end = end.saturating_sub(self.prepared_start);
        out.extend(self.prepared_lines[local_start..local_end].iter().cloned());
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bash(
    handle: &str,
    title: Option<&str>,
    command: Option<&str>,
    output: &str,
    done: bool,
    expanded: bool,
    animation_frame: u32,
    panel_width: u16,
    fullscreen_hovered: bool,
) -> Vec<Line<'static>> {
    let glyph = if done {
        "✓"
    } else {
        spinner_char(animation_frame)
    };
    let metadata = format!("bash[{handle}]");
    render_output_block(
        title.unwrap_or("bash"),
        Some(&metadata),
        glyph,
        command,
        output,
        expanded,
        panel_width,
        fullscreen_hovered,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_terminal(
    handle: &str,
    title: Option<&str>,
    command: Option<&str>,
    screen: &atman_runtime::tools::term::TerminalScreen,
    accumulated_bytes: &[u8],
    mode: crate::app::TerminalViewMode,
    done: bool,
    expanded: bool,
    animation_frame: u32,
    panel_width: u16,
    fullscreen_hovered: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg: Color = t.code_bg.into();
    let header_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let body_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);

    let glyph = if done {
        "✓"
    } else {
        spinner_char(animation_frame)
    };
    let mode_label = match mode {
        crate::app::TerminalViewMode::Capture => "capture",
        crate::app::TerminalViewMode::Stream => "stream",
    };
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_lead = format!("{DOCUMENT_PAD}{glyph}{DOCUMENT_PAD}");
    let dims = format!("{}×{}", screen.cols, screen.rows);
    let metadata = format!("terminal[{handle}] {mode_label} · {dims}");
    let header_trailing = format!("{DOCUMENT_PAD}⤢{DOCUMENT_PAD}");
    let label_budget = target
        .saturating_sub(crate::width::width(header_lead.as_str()))
        .saturating_sub(crate::width::width(&header_trailing));
    let label = compact_header_label(title.unwrap_or("terminal"), Some(&metadata), label_budget);
    let header_prefix = format!("{header_lead}{label}");
    let header_used = crate::width::width(header_prefix.as_str());
    let fs_btn = "⤢";
    let fs_btn_used = crate::width::width(fs_btn);
    let gap = DOCUMENT_PAD_X;
    let header_pad = target
        .saturating_sub(header_used)
        .saturating_sub(fs_btn_used)
        .saturating_sub(gap * 2);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    header_spans.push(Span::styled(
        fs_btn.to_string(),
        if fullscreen_hovered {
            Style::default()
                .fg(t.accent.into())
                .bg(t.panel_bg.lerp(t.user_msg_bg, 0.65))
                .add_modifier(Modifier::BOLD)
        } else {
            hint_style
        },
    ));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    append_command_lines(
        &mut lines, command, expanded, target, body_style, hint_style,
    );

    match mode {
        crate::app::TerminalViewMode::Capture => {
            let max_rows = if expanded {
                screen.rows as usize
            } else {
                (screen.rows as usize).min(12)
            };
            let cols = screen.cols as usize;
            for row in 0..max_rows.min(screen.rows as usize) {
                let mut body: Vec<Span<'static>> = Vec::new();
                for col in 0..cols {
                    let idx = row * cols + col;
                    if idx >= screen.cells.len() {
                        break;
                    }
                    let cell = &screen.cells[idx];
                    if cell.wide_continuation {
                        continue;
                    }
                    let cs = cell_style_for_viewer(cell, bg);
                    let chars = if cell.chars.is_empty() {
                        " "
                    } else {
                        &cell.chars
                    };
                    body.push(Span::styled(chars.to_string(), cs));
                }
                let body = crate::width::truncate_spans(
                    body,
                    target.saturating_sub(DOCUMENT_PAD_X + RIGHT_PAD),
                    Some(bg),
                );
                let mut spans = vec![Span::styled(DOCUMENT_PAD, body_style)];
                spans.extend(body);
                pad_spans_to_width(&mut spans, target, body_style);
                lines.push(Line::from(spans));
            }
            if !expanded && screen.rows as usize > 12 {
                let hint = format!("{DOCUMENT_PAD}▼{DOCUMENT_PAD}click to expand");
                lines.push(line_with_right_pad(
                    "", &hint, target, hint_style, hint_style,
                ));
            } else if expanded && screen.rows as usize > 12 {
                let hint = format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse");
                lines.push(line_with_right_pad(
                    "", &hint, target, hint_style, hint_style,
                ));
            }
        }
        crate::app::TerminalViewMode::Stream => {
            let text = String::from_utf8_lossy(accumulated_bytes).into_owned();
            let (total_rows, output_rows) = if expanded {
                let rows = text
                    .lines()
                    .flat_map(|line| wrap_with_prefix(line, target, DOCUMENT_PAD, DOCUMENT_PAD))
                    .collect::<Vec<_>>();
                (rows.len(), rows)
            } else {
                wrap_tail_with_prefix(&text, target, DOCUMENT_PAD, 6)
            };
            let hidden_rows = total_rows.saturating_sub(output_rows.len());
            for row in output_rows {
                lines.push(line_with_right_pad(
                    &row.prefix,
                    &row.body,
                    target,
                    body_style,
                    body_style,
                ));
            }
            if !expanded && hidden_rows > 0 {
                let unit = if hidden_rows == 1 { "line" } else { "lines" };
                let hint = format!(
                    "{DOCUMENT_PAD}▼{DOCUMENT_PAD}{hidden_rows} more {unit} — click to expand"
                );
                lines.push(line_with_right_pad(
                    "", &hint, target, hint_style, hint_style,
                ));
            } else if expanded && total_rows > 6 {
                let hint = format!("{DOCUMENT_PAD}▲{DOCUMENT_PAD}click to collapse");
                lines.push(line_with_right_pad(
                    "", &hint, target, hint_style, hint_style,
                ));
            }
        }
    }
    lines.push(blank);
    lines
}

pub fn cell_style_for_viewer(
    cell: &atman_runtime::tools::term::TerminalCell,
    default_bg: Color,
) -> Style {
    let fg = cell_fg(cell);
    let bg = cell_bg(cell, default_bg);
    let mut style = Style::default().fg(fg).bg(bg);
    if cell.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.dim {
        style = style.add_modifier(Modifier::DIM);
    }
    style
}

fn cell_fg(cell: &atman_runtime::tools::term::TerminalCell) -> Color {
    use atman_runtime::tools::term::TerminalColor;
    match cell.fg {
        TerminalColor::Default => crate::theme::theme().subtle_fg.into(),
        TerminalColor::Idx(i) => Color::Indexed(i),
        TerminalColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_bg(cell: &atman_runtime::tools::term::TerminalCell, default_bg: Color) -> Color {
    use atman_runtime::tools::term::TerminalColor;
    match cell.bg {
        TerminalColor::Default => default_bg,
        TerminalColor::Idx(i) => Color::Indexed(i),
        TerminalColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[cfg(test)]
mod terminal_render_tests {
    use super::*;
    use crate::app::TerminalViewMode;
    use atman_runtime::tools::term::{TerminalCell, TerminalScreen};

    fn screen(rows: u16, cols: u16, text: &str) -> TerminalScreen {
        let mut cells = vec![TerminalCell::default(); rows as usize * cols as usize];
        for (i, ch) in text.chars().enumerate() {
            if i < cells.len() {
                cells[i].chars = ch.to_string();
            }
        }
        TerminalScreen {
            rows,
            cols,
            cells,
            cursor: None,
            alt_screen: false,
        }
    }

    fn rendered_text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn render_terminal_capture_produces_header_and_cells() {
        let scr = screen(2, 5, "hello");
        let lines = render_terminal(
            "term_s_0",
            None,
            None,
            &scr,
            &[],
            TerminalViewMode::Capture,
            false,
            false,
            0,
            80,
            false,
        );
        assert!(
            lines.len() >= 3,
            "should have header + blank + at least 1 row"
        );
        let header = lines[1]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        assert!(
            header.contains("term_s_0"),
            "header should contain handle: {header}"
        );
        assert!(
            header.contains("capture"),
            "header should contain mode: {header}"
        );
    }

    #[test]
    fn render_terminal_stream_shows_accumulated_text() {
        let scr = screen(1, 5, "");
        let bytes = b"line1
line2
";
        let lines = render_terminal(
            "term_s_0",
            None,
            None,
            &scr,
            bytes,
            TerminalViewMode::Stream,
            true,
            false,
            0,
            80,
            false,
        );
        let rendered: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref().to_string()))
            .collect::<String>();
        assert!(
            rendered.contains("line1"),
            "stream should show line1: {rendered}"
        );
        assert!(
            rendered.contains("line2"),
            "stream should show line2: {rendered}"
        );
    }

    #[test]
    fn terminal_header_prefers_localized_intent_and_respects_width() {
        let scr = screen(2, 5, "hello");
        let lines = render_terminal(
            "term_session_with_a_long_handle",
            Some("检查终端输出"),
            None,
            &scr,
            &[],
            TerminalViewMode::Capture,
            false,
            false,
            0,
            32,
            false,
        );
        let header = &lines[1];
        let rendered = header
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("检查终端输出"));
        assert!(crate::width::spans_width(&header.spans) <= 32);
    }

    #[test]
    fn expanded_bash_output_shows_the_complete_command() {
        let lines = render_bash(
            "bg_s_0",
            Some("运行测试"),
            Some("cargo test --workspace\nprintf 'done'"),
            "ok",
            true,
            true,
            0,
            80,
            false,
        );
        let rendered = rendered_text(&lines);
        assert!(rendered.contains("cargo test --workspace"));
        assert!(rendered.contains("printf 'done'"));
    }

    #[test]
    fn collapsed_bash_limits_wrapped_visual_rows() {
        let output = format!("{}{}", "A".repeat(68), "B".repeat(272));
        let lines = render_bash("bg_s_0", None, None, &output, true, false, 0, 40, false);
        let rendered = rendered_text(&lines);

        assert_eq!(lines.len(), 13, "header + 8 output rows + hint + padding");
        assert!(rendered.contains("2 more lines — click to expand"));
        assert!(!rendered.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(rendered.contains("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"));
        assert!(
            lines
                .iter()
                .all(|line| crate::width::spans_width(&line.spans) <= 40)
        );
    }

    #[test]
    fn bash_projection_matches_direct_renderer_corpus() {
        let corpus = [
            "",
            "one",
            "one\n",
            "one\n\nthree",
            "one\r\ntwo\r\n",
            "你好世界\nemoji 😀😀\n",
            "e\u{301}e\u{301}e\u{301}\n",
            "a very long physical line that must wrap several times before it ends",
        ];
        for output in corpus {
            for expanded in [false, true] {
                for panel_width in [20, 40, 80] {
                    let mut projection = BashOutputProjection::default();
                    projection.update(BashProjectionInput {
                        handle: "bg_s_0",
                        title: Some("运行测试"),
                        command: Some("printf 'hello'\nprintf 'world'"),
                        output,
                        generation: 1,
                        done: true,
                        expanded,
                        panel_width,
                        fullscreen_hovered: false,
                    });
                    projection.prepare_range(output, 0, projection.rows());
                    let mut projected = Vec::new();
                    assert!(projection.append_prepared_range(0, projection.rows(), &mut projected));
                    let mut expected = render_bash(
                        "bg_s_0",
                        Some("运行测试"),
                        Some("printf 'hello'\nprintf 'world'"),
                        output,
                        true,
                        expanded,
                        0,
                        panel_width,
                        false,
                    );
                    expected.push(Line::from(Span::styled(String::new(), RESET)));
                    assert_eq!(
                        projected, expected,
                        "output={output:?}, expanded={expanded}, panel_width={panel_width}"
                    );
                }
            }
        }
    }

    #[test]
    fn collapsed_terminal_stream_limits_wrapped_visual_rows() {
        let scr = screen(1, 5, "");
        let output = format!("{}{}", "A".repeat(34), "B".repeat(204));
        let lines = render_terminal(
            "term_s_0",
            None,
            None,
            &scr,
            output.as_bytes(),
            TerminalViewMode::Stream,
            true,
            false,
            0,
            40,
            false,
        );
        let rendered = rendered_text(&lines);

        assert_eq!(lines.len(), 11, "header + 6 output rows + hint + padding");
        assert!(rendered.contains("1 more line — click to expand"));
        assert!(!rendered.contains("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(rendered.contains("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"));
        assert!(
            lines
                .iter()
                .all(|line| crate::width::spans_width(&line.spans) <= 40)
        );
    }

    #[test]
    fn collapsed_compaction_summary_limits_wrapped_visual_rows() {
        let summary = "S".repeat(36 * 15);
        let render = |disclosure| {
            render_compaction_summary(CompactionSummaryRender {
                phase: CompactionPhase::Finished,
                range_start: 0,
                range_end: 10,
                summary: &summary,
                before_tokens: 100,
                after_tokens: 50,
                compacted_count: 10,
                disclosure,
                animation_frame: 0,
                panel_width: 40,
                hovered: false,
            })
        };

        let summary_lines = render(Disclosure::Summary);
        let preview_lines = render(Disclosure::Preview);
        let full_lines = render(Disclosure::Full);
        let preview_text = rendered_text(&preview_lines);
        let full_text = rendered_text(&full_lines);

        assert_eq!(
            summary_lines.len(),
            3,
            "padding + single-line summary + padding"
        );
        assert_eq!(
            preview_lines.len(),
            11,
            "header + six rows + hint + padding"
        );
        assert!(preview_text.contains("9 more lines — click to expand"));
        assert!(full_lines.len() > preview_lines.len());
        assert!(full_text.contains("click to collapse"));
        assert!(
            summary_lines
                .iter()
                .chain(preview_lines.iter())
                .chain(full_lines.iter())
                .all(|line| crate::width::spans_width(&line.spans) <= 40)
        );
    }

    #[test]
    fn expanded_terminal_output_shows_the_complete_command() {
        let scr = screen(1, 5, "hello");
        let lines = render_terminal(
            "term_s_0",
            Some("检查进程"),
            Some("ps -axo pid,command | sort -n"),
            &scr,
            &[],
            TerminalViewMode::Capture,
            true,
            true,
            0,
            80,
            false,
        );
        let rendered = rendered_text(&lines);
        assert!(rendered.contains("ps -axo pid,command | sort -n"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup_entry(goal: Option<&str>) -> crate::app::StartupSessionEntry {
        crate::app::StartupSessionEntry {
            session_id: "session-1".into(),
            short_id: "session1".into(),
            goal: goal.map(str::to_owned),
            project: Some("project".into()),
            age_label: "2h ago".into(),
            event_count: 42,
        }
    }

    #[test]
    fn startup_layout_preserves_input_and_limits_four_row_records() {
        let recent = vec![startup_entry(Some("goal")); 5];
        let empty = compute_startup_overlay(ratatui::layout::Rect::new(3, 2, 100, 40), &[]);
        let too_short = compute_startup_overlay(ratatui::layout::Rect::new(3, 2, 100, 31), &recent);
        let one = compute_startup_overlay(ratatui::layout::Rect::new(3, 2, 100, 35), &recent);
        let two = compute_startup_overlay(ratatui::layout::Rect::new(3, 2, 100, 39), &recent);
        let full = compute_startup_overlay(ratatui::layout::Rect::new(3, 2, 100, 51), &recent);
        let narrow = compute_startup_overlay(ratatui::layout::Rect::new(7, 4, 60, 51), &recent);
        let tiny = compute_startup_overlay(ratatui::layout::Rect::new(7, 4, 10, 12), &recent);

        assert_eq!(empty.visible_session_count, 0);
        assert_eq!(too_short.visible_session_count, 0);
        assert_eq!(one.visible_session_count, 1);
        assert_eq!(two.visible_session_count, 2);
        assert_eq!(full.visible_session_count, 5);
        assert_eq!(full.overlay_width, 84);
        assert_eq!(full.input_slot.width, 72);
        assert!(full.all_projects_rect.is_some());
        assert!(empty.all_projects_rect.is_some());
        assert_eq!(full.recent_container.unwrap().width, full.input_slot.width);
        assert_eq!(narrow.overlay_width, 60);
        assert_eq!(narrow.input_slot.width, 60);
        assert_eq!(
            narrow.recent_container.unwrap().width,
            narrow.input_slot.width
        );
        assert_eq!(tiny.visible_session_count, 0);
        assert!(tiny.recent_container.is_none());
        assert!(tiny.all_projects_rect.is_none());

        let container = two.recent_container.unwrap();
        assert_eq!(container.x, two.input_slot.x);
        assert_eq!(
            container.y - two.input_slot.bottom(),
            STARTUP_INPUT_RECENT_GAP_ROWS
        );
        assert_eq!(two.session_rects.len(), 2);
        assert_eq!(two.session_rects[0].height, STARTUP_SESSION_ROWS);
        assert_eq!(two.session_rects[0].bottom(), two.session_rects[1].y);
        assert_eq!(two.session_rects[0].x, container.x + 1);
        assert_eq!(two.session_rects[0].width, container.width - 2);
        assert_eq!(two.all_projects_rect.unwrap().x, two.session_rects[0].x);
        assert_eq!(two.all_projects_rect.unwrap().height, STARTUP_SESSION_ROWS);
        assert_eq!(
            two.all_projects_rect.unwrap().y,
            two.session_rects.last().unwrap().bottom()
        );
        assert_eq!(
            empty.all_projects_rect.unwrap().y,
            empty.recent_container.unwrap().y + 2
        );

        for (bounds, layout) in [
            (ratatui::layout::Rect::new(3, 2, 100, 40), &empty),
            (ratatui::layout::Rect::new(3, 2, 100, 31), &too_short),
            (ratatui::layout::Rect::new(3, 2, 100, 35), &one),
            (ratatui::layout::Rect::new(3, 2, 100, 39), &two),
            (ratatui::layout::Rect::new(3, 2, 100, 51), &full),
            (ratatui::layout::Rect::new(7, 4, 60, 51), &narrow),
            (ratatui::layout::Rect::new(7, 4, 10, 12), &tiny),
        ] {
            for rect in std::iter::once(layout.area)
                .chain(std::iter::once(layout.input_slot))
                .chain(std::iter::once(layout.banner_rect))
                .chain(std::iter::once(layout.help_rect))
                .chain(layout.recent_container)
                .chain(layout.all_projects_rect)
                .chain(layout.session_rects.iter().copied())
            {
                assert!(rect.x >= bounds.x);
                assert!(rect.y >= bounds.y);
                assert!(rect.right() <= bounds.right());
                assert!(rect.bottom() <= bounds.bottom());
            }
        }
    }

    #[test]
    fn startup_session_card_is_four_rows_and_width_safe_for_cjk() {
        let width = 40;
        let entry = startup_entry(Some("修复启动页体验 🚀 with a deliberately long suffix"));
        let normal = render_session_card(1, &entry, width, false, false, false);
        let hovered = render_session_card(1, &entry, width, false, false, true);
        let selected = render_session_card(1, &entry, width, false, true, false);

        for card in [&normal, &hovered, &selected] {
            assert_eq!(card.len(), STARTUP_SESSION_ROWS as usize);
            assert!(
                card.iter()
                    .all(|line| crate::width::spans_width(line.spans.iter()) == width)
            );
            let bg = card[0].spans[0].style.bg;
            assert!(
                card.iter()
                    .flat_map(|line| line.spans.iter())
                    .all(|span| span.style.bg == bg)
            );
            assert!(plain_line(&card[0]).trim().is_empty());
            assert!(plain_line(&card[3]).trim().is_empty());
        }
        let t = crate::theme::theme();
        assert_eq!(normal[0].spans[0].style.bg, Some(Color::Reset));
        assert_eq!(hovered[1].spans[1].style.fg, Some(t.tinted_fg.into()));
        assert_eq!(hovered[1].spans[3].style.fg, Some(t.tinted_fg.into()));
        assert_eq!(selected[1].spans[1].style.fg, Some(t.tinted_fg.into()));
        assert_eq!(selected[1].spans[3].style.fg, Some(t.tinted_fg.into()));
        assert_ne!(normal[0].spans[0].style.bg, hovered[0].spans[0].style.bg);
        assert_ne!(hovered[0].spans[0].style.bg, selected[0].spans[0].style.bg);
        assert!(plain_line(&normal[2]).contains("project"));
    }

    #[test]
    fn all_projects_card_matches_recent_session_spacing_and_hover() {
        let width = 72;
        let normal = render_all_projects_card(width, false, false);
        let hovered = render_all_projects_card(width, false, true);
        let recent_hovered =
            render_session_card(1, &startup_entry(Some("goal")), width, false, false, true);

        for card in [&normal, &hovered] {
            assert_eq!(card.len(), STARTUP_SESSION_ROWS as usize);
            assert!(
                card.iter()
                    .all(|line| crate::width::spans_width(line.spans.iter()) == width)
            );
            assert!(plain_line(&card[0]).trim().is_empty());
            assert!(plain_line(&card[3]).trim().is_empty());
        }
        assert_eq!(
            hovered[0].spans[0].style.bg,
            recent_hovered[0].spans[0].style.bg
        );
        let rendered = hovered
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("All Projects"));
        assert!(rendered.contains("[Ctrl+L]"));
    }

    #[test]
    fn collapsed_work_fold_keeps_five_row_header_and_hides_members() {
        let items = OutputStore::from(vec![
            OutputItem::UserTurn {
                text: "build it".into(),
                presentation: None,
            },
            OutputItem::Thinking {
                text: "hidden reasoning".into(),
                done: true,
                disclosure: Disclosure::Summary,
                retried: false,
            },
            OutputItem::AssistantMd {
                md: "Done.".into(),
                streaming: false,
                retried: false,
            },
        ]);
        let mut cache = LayoutCache::default();
        cache.set_work_folds(vec![WorkFoldProjection {
            key: items.revisions()[1].id,
            start_index: 1,
            end_index: 1,
            visible_members: 0,
            total_members: 1,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 4,
            total_steps: 4,
            expanded: false,
            animating: false,
            hovered: false,
            title: "inspect · edit · verify".into(),
            stats: "2 files · +8 −2".into(),
        }]);
        let metrics = cache.update_dirty(
            LayoutKey {
                width: 100,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 100,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(0, metrics.total_rows, 0);
        let lines = visible.lines;
        let regions = visible.regions;
        let rendered = lines.iter().map(plain_line).collect::<Vec<_>>().join("\n");
        let work_header = lines
            .iter()
            .map(plain_line)
            .find(|line| line.contains("work · 4/4"))
            .unwrap();
        let thinking_header = plain_line(
            &render_thinking("reasoning", true, Disclosure::Summary, false, 0, 100, false)[1],
        );

        assert!(rendered.contains("work · 4/4"));
        assert!(rendered.contains('∴'));
        assert!(!rendered.contains('✓'));
        assert!(rendered.contains("inspect · edit · verify"));
        assert!(!rendered.contains('▶'));
        assert!(!rendered.contains('▼'));
        assert!(!rendered.contains("hidden reasoning"));
        assert!(work_header.starts_with("  ∴  work · 4/4"));
        assert!(work_header.ends_with(DOCUMENT_PAD));
        assert!(thinking_header.starts_with("  ⣿  thinking"));
        assert_eq!(
            crate::width::width(work_header.split_once('∴').unwrap().0),
            crate::width::width(thinking_header.split_once('⣿').unwrap().0),
        );
        let header = regions
            .iter()
            .find(|region| region.path_key.starts_with(WORK_FOLD_REGION_PREFIX))
            .unwrap();
        assert_eq!(header.end_row - header.start_row, 5);
        assert!(
            lines[header.end_row.saturating_sub(1) as usize]
                .spans
                .iter()
                .any(|span| span.style.bg == Some(crate::theme::theme().work_bg.into()))
        );
        assert!(
            lines[header.end_row as usize].spans.is_empty(),
            "work needs an external blank row"
        );
    }

    #[test]
    fn work_fold_header_wraps_the_complete_summary() {
        let summary = "检查布局缓存并修复折叠范围同时验证代码块和终端输出的层级关系";
        let fold = WorkFoldProjection {
            key: 1,
            start_index: 0,
            end_index: 0,
            visible_members: 0,
            total_members: 1,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: false,
            animating: false,
            hovered: false,
            title: summary.into(),
            stats: String::new(),
        };
        let lines = render_work_fold_header(&fold, 24);
        let compact = lines
            .iter()
            .flat_map(|line| plain_line(line).chars().collect::<Vec<_>>())
            .filter(|ch| !ch.is_whitespace())
            .collect::<String>();

        assert!(lines.len() > 5);
        assert!(compact.contains(summary));
        assert!(
            lines
                .iter()
                .all(|line| crate::width::width(&plain_line(line)) == 24)
        );
    }

    #[test]
    fn work_fold_animation_state_does_not_invalidate_all_members() {
        let fold = WorkFoldProjection {
            key: 1,
            start_index: 2,
            end_index: 40,
            visible_members: 0,
            total_members: 39,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: false,
            animating: false,
            hovered: false,
            title: "complete".into(),
            stats: String::new(),
        };
        let mut cache = LayoutCache::default();
        cache.set_work_folds(vec![fold.clone()]);
        cache.pending_layout.clear();
        cache.pending_structure_from = None;
        let mut animating = fold;
        animating.expanded = true;
        animating.animating = true;

        cache.set_work_folds(vec![animating]);

        assert!(cache.pending_layout.is_empty());
        assert!(cache.pending_structure_from.is_none());
    }

    #[test]
    fn expanding_an_old_fold_rehydrates_content_behind_its_retained_header() {
        let items = OutputStore::from(
            (0..70)
                .map(|index| OutputItem::Thinking {
                    text: format!("thought {index}"),
                    done: true,
                    disclosure: Disclosure::Summary,
                    retried: false,
                })
                .collect::<Vec<_>>(),
        );
        let collapsed = WorkFoldProjection {
            key: items.revisions()[0].id,
            start_index: 0,
            end_index: 0,
            visible_members: 0,
            total_members: 1,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: false,
            animating: false,
            hovered: false,
            title: "complete".into(),
            stats: String::new(),
        };
        let mut cache = LayoutCache::default();
        cache.set_work_folds(vec![collapsed.clone()]);
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::ThemeMode::Dark,
        };
        cache.update_dirty(
            key,
            &items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: u32::MAX,
                viewport_rows: 5,
                follow_tail_rows: None,
            },
        );

        let mut expanded = collapsed;
        expanded.visible_members = 1;
        expanded.expanded = true;
        cache.set_work_folds(vec![expanded]);
        let metrics = cache.update_dirty(
            key,
            &items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 20,
                follow_tail_rows: None,
            },
        );
        let lines = cache.visible_slice(0, metrics.total_rows.min(20), 0).lines;
        let rendered = lines.iter().map(plain_line).collect::<Vec<_>>().join("\n");

        assert!(rendered.contains("thought 0"));
    }

    #[test]
    fn expanded_work_fold_frames_member_content_and_closes_the_boundary() {
        let now = Instant::now();
        let items = OutputStore::from(vec![
            OutputItem::Thinking {
                text: "nested work".into(),
                done: true,
                disclosure: Disclosure::Summary,
                retried: false,
            },
            OutputItem::ToolDispatch {
                calls: vec![ToolCallView {
                    id: "read-1".into(),
                    tool: "fs.read".into(),
                    intent: "inspect source".into(),
                    input: serde_json::json!({"path": "src/lib.rs"}),
                    status: ToolCallStatus::Ok,
                    disclosure: Disclosure::Summary,
                    detail: None,
                    draft_index: None,
                    draft_preview: Default::default(),
                    applied_edit: None,
                    started_at: now,
                    ended_at: Some(now),
                }],
            },
            OutputItem::AssistantMd {
                md: "normal output\n\n```rust\nlet nested = true;\n```".into(),
                streaming: false,
                retried: false,
            },
        ]);
        let mut cache = LayoutCache::default();
        cache.set_work_folds(vec![WorkFoldProjection {
            key: items.revisions()[0].id,
            start_index: 0,
            end_index: 2,
            visible_members: 3,
            total_members: 3,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: true,
            animating: false,
            hovered: false,
            title: "inspect".into(),
            stats: String::new(),
        }]);
        let mut ctx = RenderCtx::empty();
        ctx.panel_width = 40;
        let metrics = cache.update_dirty(
            LayoutKey {
                width: 40,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &ctx,
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 100,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(0, metrics.total_rows, 0);
        let lines = visible.lines;
        let regions = visible.regions;
        let rendered_lines = lines.iter().map(plain_line).collect::<Vec<_>>();
        let content_lines = rendered_lines
            .iter()
            .filter(|line| {
                [
                    "nested work",
                    "working",
                    "normal output",
                    "let nested = true",
                ]
                .iter()
                .any(|needle| line.contains(needle))
            })
            .collect::<Vec<_>>();

        assert_eq!(content_lines.len(), 4);
        for content in content_lines {
            assert!(content.starts_with("⠇  "), "{content:?}");
            assert!(content.ends_with("  ⠸"), "{content:?}");
            assert_eq!(crate::width::width(content), 40);
        }
        let first_frame_row = rendered_lines
            .iter()
            .position(|line| line.starts_with("⠇"))
            .expect("work frame top padding");
        assert!(
            rendered_lines[first_frame_row]
                .trim_start_matches('⠇')
                .trim_end_matches('⠸')
                .trim()
                .is_empty()
        );
        let header = regions
            .iter()
            .find(|region| region.path_key.starts_with(WORK_FOLD_REGION_PREFIX))
            .expect("work header region");
        assert_eq!(header.end_row as usize, first_frame_row);
        let footer = rendered_lines
            .iter()
            .position(|line| line.starts_with("⠧") && line.ends_with("⠼"))
            .expect("work boundary footer");
        assert_eq!(
            rendered_lines
                .iter()
                .filter(|line| line.starts_with("⠧"))
                .count(),
            1
        );
        assert_eq!(crate::width::width(&rendered_lines[footer]), 40);
        assert!(rendered_lines[footer + 1].trim().is_empty());
    }

    #[test]
    fn work_fold_frame_preserves_narrow_panel_widths() {
        for outer_width in 3..20 {
            let mut line = Line::from(Span::raw("content"));
            frame_work_fold_content_line(&mut line, outer_width, false);
            let footer = render_work_fold_footer(outer_width, false);

            assert_eq!(
                crate::width::width(&plain_line(&line)),
                outer_width as usize
            );
            assert_eq!(
                crate::width::width(&plain_line(&footer)),
                outer_width as usize
            );
        }
    }

    #[test]
    fn work_fold_visibility_change_invalidates_old_and_new_footer_owners() {
        let fold = WorkFoldProjection {
            key: 1,
            start_index: 4,
            end_index: 8,
            visible_members: 2,
            total_members: 5,
            boundary_member: None,
            boundary_level: 3,
            completed_steps: 1,
            total_steps: 1,
            expanded: true,
            animating: true,
            hovered: false,
            title: "inspect".into(),
            stats: String::new(),
        };
        let mut cache = LayoutCache {
            work_folds: vec![fold.clone()],
            ..Default::default()
        };
        let mut advanced = fold;
        advanced.visible_members = 3;

        cache.set_work_folds(vec![advanced]);

        assert!(cache.pending_layout.contains(&5));
        assert!(cache.pending_layout.contains(&6));
    }

    fn permission_request(
        request_id: atman_runtime::permission::PermissionRequestId,
        run_id: &atman_runtime::event::FlowRunId,
        tool_use_id: String,
    ) -> atman_runtime::permission_audit::PermissionRequestAudit {
        atman_runtime::permission_audit::PermissionRequestAudit {
            request_id: Some(request_id),
            revision: 1,
            session_id: "large-session-baseline".into(),
            requesting_run_id: run_id.clone(),
            parent_run_id: None,
            root_run_id: run_id.clone(),
            tool_use_id,
            tool: "fs.read".into(),
            call_intent: None,
            tier: atman_runtime::Tier::Two,
            execution_boundary: None,
            provenance: Default::default(),
            target: atman_runtime::permission_audit::PermissionAuditTarget::User,
            group_ids: Vec::new(),
            policy: atman_runtime::permission_audit::PermissionPolicyReference {
                snapshot_id: "baseline".into(),
                rule_id: "baseline".into(),
            },
            escalation_path: Vec::new(),
            decision_id: None,
            actor: None,
            scope: None,
            reason: None,
            at: chrono::Utc::now(),
        }
    }

    fn workflow_with_permissions(permission_count: usize) -> OutputItem {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
            WorkflowPermissionIdentity, WorkflowPermissionRequest, WorkflowPermissionState,
        };
        let run_id = atman_runtime::event::FlowRunId::now();
        let mut graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![WorkflowNode {
                id: run_id.to_string(),
                kind: WorkflowNodeKind::Flow {
                    run_id: run_id.to_string(),
                    flow_name: "baseline".into(),
                },
                label: "baseline".into(),
                status: NodeStatus::Running,
                started_at: Some(chrono::Utc::now()),
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        for idx in 0..permission_count {
            let request_id = atman_runtime::permission::PermissionRequestId::now();
            graph.permission_requests.insert(
                WorkflowPermissionIdentity::Canonical {
                    request_id: request_id.clone(),
                },
                WorkflowPermissionRequest {
                    payload: permission_request(request_id, &run_id, format!("tool-{idx}")),
                    state: WorkflowPermissionState::Pending,
                },
            );
        }
        OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: Default::default(),
            panel_expanded: false,
            started_at: std::time::Instant::now(),
            ended_at: None,
            cancelled: false,
        }
    }

    #[test]
    fn permission_details_use_the_indexed_winner() {
        use atman_runtime::workflow::{
            ApprovalState, NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
            WorkflowPermissionIdentity, WorkflowPermissionRequest, WorkflowPermissionState,
        };
        let run_id = atman_runtime::event::FlowRunId::now();
        let run_id_text = run_id.0.to_string();
        let tool_node_id = format!("tool:{run_id_text}:shared");
        let at = chrono::Utc::now();
        let canonical_id = atman_runtime::permission::PermissionRequestId::now();
        let legacy_id = atman_runtime::permission::PermissionRequestId::now();
        let mut canonical = permission_request(canonical_id.clone(), &run_id, "shared".into());
        canonical.at = at + chrono::Duration::seconds(10);
        canonical.reason = Some("canonical".into());
        let mut legacy = permission_request(legacy_id, &run_id, "shared".into());
        legacy.at = at;
        legacy.reason = Some("legacy pending".into());
        let mut requests = std::collections::BTreeMap::new();
        requests.insert(
            WorkflowPermissionIdentity::Canonical {
                request_id: canonical_id,
            },
            WorkflowPermissionRequest {
                payload: canonical,
                state: WorkflowPermissionState::Approved,
            },
        );
        requests.insert(
            WorkflowPermissionIdentity::Legacy {
                seq: 7,
                run_id: run_id_text.clone(),
                tool_use_id: "shared".into(),
            },
            WorkflowPermissionRequest {
                payload: legacy,
                state: WorkflowPermissionState::Pending,
            },
        );
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![WorkflowNode {
                id: run_id_text.clone(),
                kind: WorkflowNodeKind::Flow {
                    run_id: run_id_text,
                    flow_name: "root".into(),
                },
                label: "root".into(),
                status: NodeStatus::Running,
                started_at: Some(at),
                ended_at: None,
                output_preview: None,
                children: vec![WorkflowNode {
                    id: tool_node_id.clone(),
                    kind: WorkflowNodeKind::ToolCall {
                        tool_use_id: "shared".into(),
                        tool: "fs.read".into(),
                        args_preview: "{}".into(),
                        call_intent: None,
                        result_preview: None,
                    },
                    label: "fs.read".into(),
                    status: NodeStatus::Running,
                    started_at: Some(at),
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                    approval: Some(ApprovalState::Pending {
                        level: "two".into(),
                        preview: None,
                    }),
                    llm_stats: None,
                }],
                parallelism: Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: requests,
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let projection = atman_runtime::projection::workflow::WorkflowProjection::from(graph);
        let node = projection.find_node(&tool_node_id).unwrap();
        let sections = permission_detail_sections(projection.graph(), Some(&projection), node);

        assert_eq!(sections[0], ("approval", "pending · two".into()));
        assert!(sections.contains(&("reason", "legacy pending".into())));
        assert!(!sections.contains(&("reason", "canonical".into())));
    }

    fn plain_line(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    #[test]
    fn visible_slice_does_not_reinspect_workflow_permissions() {
        const PERMISSIONS: usize = 4_096;
        let items = OutputStore::from(vec![workflow_with_permissions(PERMISSIONS)]);
        let expanded_tools = std::collections::HashSet::new();
        let ctx = RenderCtx {
            expanded_tools: &expanded_tools,
            messages: &[],
            animation_frame: 0,
            panel_width: 120,
            hovered_thinking_idx: None,
            hovered_output_node: None,
        };
        let key = LayoutKey {
            width: 120,
            theme: crate::theme::current_mode(),
        };
        let mut cache = LayoutCache::default();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: None,
        };
        let metrics = cache.update_dirty(key, &items, &ctx, request);
        reset_perf_counters();
        let first = cache
            .visible_slice(metrics.scroll_offset, request.viewport_rows, 0)
            .lines;
        let mut last = Vec::new();
        for frame in 1..100 {
            last = cache
                .visible_slice(metrics.scroll_offset, request.viewport_rows, frame)
                .lines;
        }
        let counters = perf_counters();
        assert_eq!(counters.semantic_item_visits, 0);
        assert_eq!(counters.item_renders, 0);
        assert_eq!(counters.panel_projection_builds, 0);
        assert_eq!(counters.retention_item_visits, 0);
        assert_eq!(counters.permission_table_entries, 0);
        assert_eq!(counters.animation_item_visits, 100);
        assert_ne!(
            first.iter().map(plain_line).collect::<Vec<_>>(),
            last.iter().map(plain_line).collect::<Vec<_>>()
        );
        assert!(
            !first
                .iter()
                .any(|line| plain_line(line).contains(DYNAMIC_SPINNER_MARKER))
        );
        assert!(
            !last
                .iter()
                .any(|line| plain_line(line).contains(DYNAMIC_SPINNER_MARKER))
        );
    }

    #[test]
    fn offscreen_animation_ticks_do_not_visit_or_render_the_entry() {
        let mut values = (0..10_000)
            .map(|idx| OutputItem::SystemNote {
                text: format!("note-{idx}"),
                level: NoteLevel::Info,
            })
            .collect::<Vec<_>>();
        values.push(OutputItem::Thinking {
            text: "offscreen".into(),
            done: false,
            disclosure: Disclosure::Summary,
            retried: false,
        });
        let items = OutputStore::from(values);
        let mut cache = LayoutCache::default();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 20,
            follow_tail_rows: None,
        };
        cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            &items,
            &RenderCtx::empty(),
            request,
        );

        reset_perf_counters();
        for frame in 0..100 {
            let _ = cache.visible_slice(0, request.viewport_rows, frame);
        }
        let counters = perf_counters();
        assert_eq!(counters.semantic_item_visits, 0);
        assert_eq!(counters.item_renders, 0);
        assert_eq!(counters.animation_item_visits, 0);
        assert_eq!(counters.retention_item_visits, 0);
        assert_eq!(counters.permission_table_entries, 0);
    }

    #[test]
    fn visible_animation_patch_refreshes_workflow_elapsed_time() {
        let mut item = workflow_with_permissions(0);
        let OutputItem::WorkflowPanel {
            graph,
            panel_expanded,
            ..
        } = &mut item
        else {
            unreachable!();
        };
        *panel_expanded = true;
        let mut raw_graph = graph.clone().into_graph();
        raw_graph.root[0].started_at = Some(chrono::Utc::now() - chrono::Duration::seconds(65));
        *graph = raw_graph.into();
        let items = OutputStore::from(vec![item]);
        let mut cache = LayoutCache::default();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 20,
            follow_tail_rows: None,
        };
        let metrics = cache.update_dirty(
            LayoutKey {
                width: 120,
                theme: crate::theme::current_mode(),
            },
            &items,
            &RenderCtx::empty(),
            request,
        );
        let lines = cache
            .visible_slice(metrics.scroll_offset, request.viewport_rows, 0)
            .lines;
        let text = lines.iter().map(plain_line).collect::<String>();
        assert!(text.contains("1m"));
        assert!(!text.contains(DYNAMIC_SPINNER_MARKER));
    }

    #[test]
    fn closed_workflow_ignores_animation_frame_changes() {
        use atman_runtime::workflow::{NodeStatus, WorkflowGraph, WorkflowNode, WorkflowNodeKind};
        use std::collections::HashSet;
        use std::time::Instant;
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![WorkflowNode {
                id: "n0".into(),
                kind: WorkflowNodeKind::Stmt {
                    node_kind: atman_runtime::nodegraph::NodeKind::Return,
                },
                label: "done".into(),
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: atman_runtime::workflow::Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let items = OutputStore::from(vec![OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: HashSet::new(),
            panel_expanded: false,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
            cancelled: false,
        }]);
        let expanded_tools = HashSet::new();
        let mut ctx = RenderCtx {
            expanded_tools: &expanded_tools,
            messages: &[],
            animation_frame: 0,
            panel_width: 120,
            hovered_thinking_idx: None,
            hovered_output_node: None,
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: None,
        };
        let mut cache = LayoutCache::default();
        let key = LayoutKey {
            width: 120,
            theme: crate::theme::current_mode(),
        };
        cache.update_dirty(key, &items, &ctx, request);
        reset_perf_counters();
        ctx.animation_frame = 1;
        cache.update_dirty(key, &items, &ctx, request);
        let _ = cache.visible_slice(0, request.viewport_rows, ctx.animation_frame);
        assert_eq!(perf_counters().item_renders, 0);
    }

    #[test]
    fn unchanged_layout_update_and_slice_do_not_render_items() {
        let items = OutputStore::from(vec![
            OutputItem::SystemNote {
                text: "one".into(),
                level: NoteLevel::Info,
            },
            OutputItem::SystemNote {
                text: "two".into(),
                level: NoteLevel::Info,
            },
        ]);
        let ctx = RenderCtx::empty();
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 20,
            follow_tail_rows: None,
        };
        let mut cache = LayoutCache::default();
        cache.update_dirty(key, &items, &ctx, request);
        reset_perf_counters();
        cache.update_dirty(key, &items, &ctx, request);
        let _ = cache.visible_slice(0, 20, 0);
        assert_eq!(perf_counters().item_renders, 0);
    }

    #[test]
    fn bash_line_appends_index_only_new_source_bytes() {
        const CHUNKS: usize = 2_048;
        let chunk = format!("{}\n", "x".repeat(63));
        let mut app = crate::app::AppState::new("bash-projection".into(), None);
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::BashChunk {
            handle: "bg_s_0".into(),
            kind: "stdout".into(),
            line: chunk.clone(),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        let source_generation = app.items.revisions()[0].source_generation;
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: Some(40),
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        app.layout_cache = cache;

        reset_perf_counters();
        for _ in 1..CHUNKS {
            app.apply_stream_frame(atman_runtime::stream::StreamFrame::BashChunk {
                handle: "bg_s_0".into(),
                kind: "stdout".into(),
                line: chunk.clone(),
                call_intent: None,
                tool_use_id: None,
                run_id: None,
            });
            let mut cache = std::mem::take(&mut app.layout_cache);
            cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
            app.layout_cache = cache;
        }

        assert_eq!(
            app.items.revisions()[0].source_generation,
            source_generation
        );
        let counters = perf_counters();
        assert_eq!(
            counters.bash_source_bytes,
            ((CHUNKS - 1) * chunk.len()) as u64
        );
        assert_eq!(counters.semantic_item_visits, 0);
        assert!(counters.bash_materialized_rows <= ((CHUNKS - 1) * 8) as u64);

        reset_perf_counters();
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::BashExited {
            handle: "bg_s_0".into(),
            exit_code: Some(0),
            error: None,
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        });
        let mut cache = std::mem::take(&mut app.layout_cache);
        let metrics = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        let lines = cache
            .visible_slice(metrics.scroll_offset, request.viewport_rows, 0)
            .lines;
        assert_eq!(perf_counters().bash_source_bytes, 0);
        assert_eq!(
            app.items.revisions()[0].source_generation,
            source_generation
        );
        assert_eq!(lines, render_item(&app.items[0], &RenderCtx::empty()));
    }

    #[test]
    fn expanded_bash_layout_materializes_only_the_visible_rows() {
        let output = (0..4_096)
            .map(|index| format!("line {index:04} 你好 {}\n", "x".repeat(index % 19)))
            .collect::<String>();
        let items = OutputStore::from(vec![OutputItem::Bash {
            handle: "bg_large".into(),
            title: Some("inspect output".into()),
            command: Some("printf 'large output'".into()),
            output,
            done: true,
            expanded: true,
        }]);
        let ctx = RenderCtx {
            panel_width: 48,
            ..RenderCtx::empty()
        };
        let key = LayoutKey {
            width: 48,
            theme: crate::theme::current_mode(),
        };
        let viewport_rows = 17;
        let mut cache = LayoutCache::default();
        let direct = render_item(&items[0], &ctx);

        let first_request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows,
            follow_tail_rows: None,
        };
        let metrics = cache.update_dirty(key, &items, &ctx, first_request);
        assert_eq!(metrics.total_rows as usize, direct.len());
        let first = cache.visible_slice(0, viewport_rows, 0).lines;
        assert_eq!(first, direct[..viewport_rows as usize]);

        let max_scroll = metrics.total_rows.saturating_sub(viewport_rows);
        for scroll_offset in [max_scroll / 2, max_scroll] {
            reset_perf_counters();
            let request = LayoutRequest {
                scroll_offset,
                viewport_rows,
                follow_tail_rows: None,
            };
            cache.update_dirty(key, &items, &ctx, request);
            let projected = cache.visible_slice(scroll_offset, viewport_rows, 0).lines;
            let end = scroll_offset
                .saturating_add(viewport_rows)
                .min(metrics.total_rows) as usize;
            assert_eq!(projected, direct[scroll_offset as usize..end]);
            let counters = perf_counters();
            assert_eq!(counters.bash_source_bytes, 0);
            assert_eq!(counters.item_renders, 0);
            assert!(counters.bash_materialized_rows <= viewport_rows as u64);
        }
    }

    #[test]
    fn evicted_bash_projection_rebuilds_equivalently_when_revisited() {
        let items = OutputStore::from(
            (0..200)
                .map(|index| OutputItem::Bash {
                    handle: format!("bg_{index}"),
                    title: None,
                    command: Some(format!("printf '{index}'")),
                    output: format!("output {index}\n"),
                    done: true,
                    expanded: true,
                })
                .collect::<Vec<_>>(),
        );
        let ctx = RenderCtx::empty();
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let viewport_rows = 20;
        let mut cache = LayoutCache::default();
        cache.update_dirty(
            key,
            &items,
            &ctx,
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows,
                follow_tail_rows: None,
            },
        );
        assert!(cache.entries[0].bash_output.is_some());

        for item_index in (10..=120).step_by(10) {
            let middle_scroll = cache.row_start(item_index);
            cache.update_dirty(
                key,
                &items,
                &ctx,
                LayoutRequest {
                    scroll_offset: middle_scroll,
                    viewport_rows,
                    follow_tail_rows: None,
                },
            );
        }
        assert!(cache.entries[0].bash_output.is_none());

        reset_perf_counters();
        let first_rows = cache.entries[0].rows;
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: first_rows,
            follow_tail_rows: None,
        };
        cache.update_dirty(key, &items, &ctx, request);
        let projected = cache.visible_slice(0, first_rows, 0).lines;
        assert_eq!(projected, render_item(&items[0], &ctx));
        let rebuilt_bytes = items
            .iter()
            .take(1 + LayoutCache::OVERSCAN_ITEMS)
            .filter_map(|item| match item {
                OutputItem::Bash { output, .. } => Some(output.len() as u64),
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(perf_counters().bash_source_bytes, rebuilt_bytes);
    }

    #[test]
    #[ignore = "large release-mode Bash output projection baseline"]
    fn baseline_eight_mib_bash_output_projection() {
        let chunk = format!("{}\n", "x".repeat(63));
        let source = chunk.repeat(8 * 1024 * 1024 / chunk.len());
        assert_eq!(source.len(), 8 * 1024 * 1024);

        let mut projection = BashOutputProjection::default();
        let started = std::time::Instant::now();
        let indexed_bytes = projection.update(BashProjectionInput {
            handle: "bg_baseline",
            title: Some("baseline"),
            command: Some("produce output"),
            output: std::hint::black_box(&source),
            generation: 1,
            done: false,
            expanded: false,
            panel_width: 80,
            fullscreen_hovered: false,
        });
        projection.prepare_range(&source, 0, projection.rows());
        let cold = started.elapsed();
        assert_eq!(indexed_bytes, source.len());

        let started = std::time::Instant::now();
        for _ in 0..16 {
            assert_eq!(
                projection.update(BashProjectionInput {
                    handle: "bg_baseline",
                    title: Some("baseline"),
                    command: Some("produce output"),
                    output: std::hint::black_box(&source),
                    generation: 1,
                    done: false,
                    expanded: false,
                    panel_width: 80,
                    fullscreen_hovered: false,
                }),
                0
            );
            projection.prepare_range(&source, 0, projection.rows());
        }
        let unchanged = started.elapsed();

        let mut appended = String::new();
        let mut append_projection = BashOutputProjection::default();
        let started = std::time::Instant::now();
        let mut append_indexed_bytes = 0usize;
        for _ in 0..2_048 {
            appended.push_str(&chunk);
            append_indexed_bytes = append_indexed_bytes.saturating_add(append_projection.update(
                BashProjectionInput {
                    handle: "bg_append",
                    title: None,
                    command: None,
                    output: std::hint::black_box(&appended),
                    generation: 2,
                    done: false,
                    expanded: false,
                    panel_width: 80,
                    fullscreen_hovered: false,
                },
            ));
            append_projection.prepare_range(&appended, 0, append_projection.rows());
        }
        let appends = started.elapsed();
        assert_eq!(append_indexed_bytes, appended.len());

        projection.update(BashProjectionInput {
            handle: "bg_baseline",
            title: Some("baseline"),
            command: Some("produce output"),
            output: &source,
            generation: 1,
            done: true,
            expanded: true,
            panel_width: 80,
            fullscreen_hovered: false,
        });
        let viewport_rows = 40;
        let starts = [
            0,
            projection.rows() / 2,
            projection.rows().saturating_sub(viewport_rows),
        ];
        let started = std::time::Instant::now();
        let mut materialized_rows = 0usize;
        for start in starts {
            materialized_rows = materialized_rows.saturating_add(projection.prepare_range(
                &source,
                start,
                start.saturating_add(viewport_rows),
            ));
        }
        let viewports = started.elapsed();
        assert!(materialized_rows <= 3 * viewport_rows);

        eprintln!(
            "bash output baseline: bytes={} rows={} cold_ms={} unchanged_16_us={} appends_2048_ms={} viewports_3_us={}",
            source.len(),
            projection.rows(),
            cold.as_millis(),
            unchanged.as_micros(),
            appends.as_millis(),
            viewports.as_micros()
        );
    }

    #[test]
    fn streaming_markdown_defers_layout_but_completion_flushes_latest_source() {
        let mut app = crate::app::AppState::new("stream-projection".into(), None);
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: "one".into(),
            model: "model".into(),
            run_id: None,
        });
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 100,
            follow_tail_rows: None,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert!(cache.entries[0].streaming_markdown.is_some());
        assert!(cache.entries[0].lines.is_none());
        app.layout_cache = cache;

        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: " two".into(),
            model: "model".into(),
            run_id: None,
        });
        let current_revision = app.items.revisions()[0];
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert!(cache.pending_layout.contains(&0));
        assert_ne!(cache.entries[0].revision.layout, current_revision.layout);
        assert!(app.has_active_animation());

        std::thread::sleep(std::time::Duration::from_millis(60));
        let metrics = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert!(!cache.pending_layout.contains(&0));
        assert_eq!(cache.entries[0].revision.layout, current_revision.layout);
        let streaming_lines = cache.visible_slice(0, metrics.total_rows, 0).lines;
        assert_eq!(
            streaming_lines,
            render_item(
                &OutputItem::AssistantMd {
                    md: "one two".into(),
                    streaming: true,
                    retried: false,
                },
                &RenderCtx::empty(),
            )
        );
        app.layout_cache = cache;

        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmDone {
            total_tokens: 2,
            run_id: None,
        });
        let mut cache = std::mem::take(&mut app.layout_cache);
        let metrics = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        let lines = cache.visible_slice(0, metrics.total_rows, 0).lines;
        assert_eq!(
            lines,
            render_item(
                &OutputItem::AssistantMd {
                    md: "one two".into(),
                    streaming: false,
                    retried: false,
                },
                &RenderCtx::empty(),
            )
        );
        assert!(cache.entries[0].streaming_markdown.is_none());
        assert!(!cache.pending_layout.contains(&0));
    }

    #[test]
    fn adaptive_deferred_markdown_completion_flushes_latest_source() {
        let mut initial = "```text\n".to_string();
        while initial.len() <= 32 * 1024 {
            initial.push_str("an unclosed streamed code line\n");
        }
        let mut app = crate::app::AppState::new("adaptive-completion".into(), None);
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: initial.clone(),
            model: "model".into(),
            run_id: None,
        });
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 100,
            follow_tail_rows: None,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        app.layout_cache = cache;

        let suffix = "latest source before completion\n";
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: suffix.into(),
            model: "model".into(),
            run_id: None,
        });
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert!(cache.pending_layout.contains(&0));
        app.layout_cache = cache;

        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmDone {
            total_tokens: 1,
            run_id: None,
        });
        let mut cache = std::mem::take(&mut app.layout_cache);
        let metrics = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        let lines = cache.visible_slice(0, metrics.total_rows, 0).lines;
        initial.push_str(suffix);
        assert_eq!(
            lines,
            render_item(
                &OutputItem::AssistantMd {
                    md: initial,
                    streaming: false,
                    retried: false,
                },
                &RenderCtx::empty(),
            )
        );
        assert!(cache.entries[0].streaming_markdown.is_none());
        assert!(!cache.pending_layout.contains(&0));
    }

    #[test]
    fn streaming_markdown_resets_for_width_theme_and_retry() {
        let source = "# heading\n\nparagraph\n\n```rust\nfn main() {}\n```";
        let mut app = crate::app::AppState::new("stream-reset".into(), None);
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: source.into(),
            model: "model".into(),
            run_id: None,
        });
        let theme = crate::theme::current_mode();
        let key = LayoutKey { width: 80, theme };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 100,
            follow_tail_rows: None,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);

        let narrow_key = LayoutKey { width: 40, theme };
        let narrow_ctx = RenderCtx {
            panel_width: 40,
            ..RenderCtx::empty()
        };
        reset_perf_counters();
        cache.update_dirty(narrow_key, &app.items, &narrow_ctx, request);
        assert_eq!(perf_counters().item_renders, 1);
        assert!(cache.entries[0].streaming_markdown.is_some());

        let alternate_theme = match theme {
            crate::theme::ThemeMode::Dark => crate::theme::ThemeMode::Light,
            crate::theme::ThemeMode::Light => crate::theme::ThemeMode::Dark,
        };
        let alternate_key = LayoutKey {
            width: 40,
            theme: alternate_theme,
        };
        reset_perf_counters();
        cache.update_dirty(alternate_key, &app.items, &narrow_ctx, request);
        assert_eq!(perf_counters().item_renders, 1);
        assert!(cache.entries[0].streaming_markdown.is_some());
        app.layout_cache = cache;

        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmRetry);
        let mut cache = std::mem::take(&mut app.layout_cache);
        let metrics = cache.update_dirty(alternate_key, &app.items, &narrow_ctx, request);
        let lines = cache.visible_slice(0, metrics.total_rows, 0).lines;
        assert_eq!(lines, render_item(&app.items[0], &narrow_ctx));
        assert!(cache.entries[0].streaming_markdown.is_none());
    }

    #[test]
    fn thousands_of_streaming_chunks_do_not_revisit_unrelated_entries() {
        const UNRELATED: usize = 512;
        const CHUNKS: usize = 2_048;
        let mut items = (0..UNRELATED)
            .map(|idx| OutputItem::SystemNote {
                text: format!("note-{idx}"),
                level: NoteLevel::Info,
            })
            .collect::<Vec<_>>();
        items.push(OutputItem::AssistantMd {
            md: "start".into(),
            streaming: true,
            retried: false,
        });
        let mut app =
            crate::app::AppState::new("isolated-stream".into(), None).with_initial_items(items);
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: Some(40),
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        let unrelated_revisions = cache.entries[..UNRELATED]
            .iter()
            .map(|entry| entry.revision)
            .collect::<Vec<_>>();
        app.layout_cache = cache;

        reset_perf_counters();
        for _ in 0..CHUNKS {
            app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
                text: "x".into(),
                model: "model".into(),
                run_id: None,
            });
            let mut cache = std::mem::take(&mut app.layout_cache);
            cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
            app.layout_cache = cache;
        }
        let cache = &app.layout_cache;
        assert_eq!(
            cache.entries[..UNRELATED]
                .iter()
                .map(|entry| entry.revision)
                .collect::<Vec<_>>(),
            unrelated_revisions
        );
        let counters = perf_counters();
        assert_eq!(counters.semantic_item_visits, 0);
        assert_eq!(counters.retention_item_visits, 0);
        assert!(counters.item_renders < UNRELATED as u64);
    }

    #[test]
    fn middle_and_tail_removal_update_total_rows_exactly() {
        let mut app = crate::app::AppState::new("layout-remove".into(), None).with_initial_items(
            (0..5)
                .map(|idx| OutputItem::SystemNote {
                    text: format!("note-{idx}"),
                    level: NoteLevel::Info,
                })
                .collect::<Vec<_>>(),
        );
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 100,
            follow_tail_rows: None,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        let initial = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        app.layout_cache = cache;

        app.remove_item(2);
        let mut cache = std::mem::take(&mut app.layout_cache);
        let middle = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert_eq!(
            middle.total_rows,
            build_lines(&app.items, &RenderCtx::empty()).len() as u32
        );
        assert!(middle.total_rows < initial.total_rows);
        app.layout_cache = cache;

        app.remove_item(app.items.len() - 1);
        let mut cache = std::mem::take(&mut app.layout_cache);
        let tail = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        assert_eq!(
            tail.total_rows,
            build_lines(&app.items, &RenderCtx::empty()).len() as u32
        );
        assert!(tail.total_rows < middle.total_rows);
    }

    #[test]
    fn mutation_followed_by_removal_keeps_the_shifted_entry_dirty() {
        let mut app = crate::app::AppState::new("layout-shift".into(), None);
        app.push_note("prefix", NoteLevel::Info);
        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: "old".into(),
            model: "model".into(),
            run_id: None,
        });
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 100,
            follow_tail_rows: None,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        app.layout_cache = cache;

        app.apply_stream_frame(atman_runtime::stream::StreamFrame::LlmChunk {
            text: "-new".into(),
            model: "model".into(),
            run_id: None,
        });
        app.remove_item(0);

        let mut cache = std::mem::take(&mut app.layout_cache);
        let metrics = cache.update_dirty(key, &app.items, &RenderCtx::empty(), request);
        let lines = cache
            .visible_slice(metrics.scroll_offset, request.viewport_rows, 0)
            .lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("old-new"));
    }

    #[test]
    fn rendered_line_retention_is_bounded() {
        let items = OutputStore::from(
            (0..1_000)
                .map(|idx| OutputItem::SystemNote {
                    text: format!("note-{idx}"),
                    level: NoteLevel::Info,
                })
                .collect::<Vec<_>>(),
        );
        let mut cache = LayoutCache::default();
        cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            &items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 1_000,
                viewport_rows: 20,
                follow_tail_rows: None,
            },
        );
        assert!(cache.retained_item_count() <= LayoutCache::RECENT_ITEM_BUDGET + 16);
    }

    #[test]
    fn width_and_theme_changes_force_full_layout_invalidation() {
        let items = OutputStore::from(vec![
            OutputItem::SystemNote {
                text: "one".into(),
                level: NoteLevel::Info,
            },
            OutputItem::SystemNote {
                text: "two".into(),
                level: NoteLevel::Info,
            },
        ]);
        let ctx = RenderCtx::empty();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 20,
            follow_tail_rows: None,
        };
        let theme = crate::theme::current_mode();
        let mut cache = LayoutCache::default();
        cache.update_dirty(LayoutKey { width: 80, theme }, &items, &ctx, request);

        reset_perf_counters();
        cache.update_dirty(LayoutKey { width: 79, theme }, &items, &ctx, request);
        assert_eq!(perf_counters().item_renders, 2);

        reset_perf_counters();
        cache.update_dirty(
            LayoutKey {
                width: 79,
                theme: match theme {
                    crate::theme::ThemeMode::Dark => crate::theme::ThemeMode::Light,
                    crate::theme::ThemeMode::Light => crate::theme::ThemeMode::Dark,
                },
            },
            &items,
            &ctx,
            request,
        );
        assert_eq!(perf_counters().item_renders, 2);
    }

    #[test]
    fn user_turn_produces_source_atoms_for_wrapped_cjk_text() {
        let text = "第一行内容很长需要换行\n第二行内容";
        let atoms = user_turn_prose_atoms(text, 20);
        assert!(!atoms.is_empty());
        assert!(atoms.iter().any(|atom| atom.row > 0));
        assert!(atoms.iter().all(|atom| atom.cols.start >= 4));
        assert_eq!(
            atoms.iter().map(|atom| atom.event_graphemes.end).max(),
            Some(crate::width::graphemes(text).count())
        );
    }

    #[test]
    fn quoted_user_geometry_tracks_quote_and_prompt_source() {
        let presentation = atman_runtime::user_input::UserInputPresentation {
            prompt: "check".into(),
            quote: Some(atman_runtime::user_input::QuoteSnapshot {
                text: "汉字\nsecond".into(),
            }),
        };
        let source = presentation.model_text();
        let rows = render_user_turn(&source, Some(&presentation), 40, false);
        let atoms = presented_user_prose_atoms(&presentation, 40, false);
        assert!(plain_line(&rows[1]).contains("QUOTED · 2 lines"));
        assert!(plain_line(&rows[2]).contains("汉字"));
        assert!(plain_line(&rows[4]).contains("check"));
        let prompt_start = source.find("check").unwrap();
        let prompt_grapheme = crate::width::graphemes(&source[..prompt_start]).count();
        assert!(
            atoms
                .iter()
                .any(|atom| { atom.row == 4 && atom.event_graphemes.start == prompt_grapheme })
        );
    }

    #[test]
    fn visible_assistant_prose_projection_hits_and_copies() {
        let items = OutputStore::from(vec![OutputItem::AssistantMd {
            md: "ordinary assistant prose".into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = LayoutCache::default();
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::ThemeMode::Dark,
        };
        let metrics = cache.update_dirty(
            key,
            &items,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let surface = visible
            .selection
            .surfaces
            .first()
            .expect("assistant surface");
        let atom = surface.prose_atoms.first().expect("assistant prose atom");
        let rendered_line = plain_line(&visible.lines[atom.screen_row as usize]);
        let rendered = crate::width::graphemes(&rendered_line).collect::<Vec<_>>();
        assert!(!rendered.is_empty());
        assert!(usize::from(atom.atom.cols.start) < rendered.len());
        let start = visible
            .selection
            .prose_point_at(atom.screen_row, atom.atom.cols.start)
            .expect("start point");
        let end_col = atom.atom.cols.end.saturating_sub(1);
        let end = visible
            .selection
            .prose_point_at(atom.screen_row, end_col)
            .expect("end point");
        let state = crate::selection::selection_extend(
            &crate::selection::selection_begin(
                start,
                surface.revision,
                visible.selection.structure_revision,
            ),
            end,
            surface.revision,
            visible.selection.structure_revision,
        )
        .expect("selection in one prose surface");
        let payload = crate::selection::selection_copy_payload(&visible.selection, &state)
            .expect("copy payload");
        let copied = match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        };
        assert!(!copied.is_empty());
        assert!("ordinary assistant prose".contains(&copied));
    }

    #[test]
    fn user_turn_wraps_long_line_to_panel_width() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk";
        let lines = render_user_turn(text, None, 30, false);
        assert!(lines.len() > 3, "should wrap into multiple rows");
        for (i, line) in lines.iter().enumerate() {
            let w = crate::width::width(plain_line(line).as_str());
            assert!(
                w <= 30,
                "line {i} width {w} exceeds panel 30: {:?}",
                plain_line(line)
            );
        }
    }

    #[test]
    fn unified_diff_parser_keeps_multi_file_changes() {
        let diff = "diff --git a/a.rs b/a.rs\nindex 111..222 100644\n--- a/a.rs\n+++ b/a.rs\n@@ -1,1 +1,2 @@\n fn a() {}\n+fn b() {}\ndiff --git a/b.rs b/b.rs\nindex 333..444 100644\n--- a/b.rs\n+++ b/b.rs\n@@ -1,2 +1,1 @@\n keep\n-delete\n";
        let (rows, lang) = parse_unified_diff_to_dual(diff);
        assert_eq!(lang, "rust");
        assert!(rows.iter().any(|(_, r)| r.text == "fn b() {}"));
        assert!(rows.iter().any(|(l, _)| l.text == "delete"));
    }

    #[test]
    fn addition_only_diff_bypasses_the_split_layout() {
        use atman_runtime::config_hub::DiffLayout;

        let addition = "--- new.rs\n+++ new.rs\n@@ -0,0 +1,2 @@\n+first\n+second\n";
        let replacement = "--- file.rs\n+++ file.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let legacy_new_file = "+++ new.rs\nfirst\nsecond\n";

        assert!(prefers_unified_diff(DiffLayout::Split, 120, Some(addition)));
        assert!(prefers_unified_diff(
            DiffLayout::Split,
            120,
            Some(legacy_new_file)
        ));
        assert!(!prefers_unified_diff(
            DiffLayout::Split,
            120,
            Some(replacement)
        ));
        assert!(prefers_unified_diff(
            DiffLayout::Unified,
            120,
            Some(replacement)
        ));
    }

    #[test]
    fn diff_rows_wrap_and_align_long_sides() {
        let t = crate::theme::theme();
        let long = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789中文中文中文";
        let rows = vec![(
            DiffCell {
                line_no: Some(1),
                text: long.into(),
                kind: DiffCellKind::Delete,
                char_diff: None,
            },
            DiffCell {
                line_no: Some(1),
                text: "short".into(),
                kind: DiffCellKind::Insert,
                char_diff: None,
            },
        )];
        let (lines, total) =
            render_diff_cell_rows(&rows, "rust", true, 46, t.code_bg.into(), Some(0));
        assert_eq!(total, 1);
        assert!(lines.len() > 1, "long side should wrap: {lines:?}");
        for line in &lines {
            let text = plain_line(line);
            let width = crate::width::width(text.as_str());
            // Width should match target (may be off by 1 due to wrap rounding)
            assert!(
                (46..=47).contains(&width),
                "line width {width} not in 46..=47: {text:?}"
            );
            assert!(text.starts_with(' '), "left margin missing: {text:?}");
            assert!(text.ends_with(' '), "right margin missing: {text:?}");
            // New layout: line numbers in center, no vertical separator
            assert!(!text.contains('│'), "should have no separator: {text:?}");
        }
    }

    #[test]
    fn diff_layout_has_centered_line_numbers_no_separator() {
        let t = crate::theme::theme();
        let rows = vec![(
            DiffCell {
                line_no: Some(5),
                text: "old line".into(),
                kind: DiffCellKind::Delete,
                char_diff: Some(vec![
                    ("old ".to_string(), false),
                    ("line".to_string(), true),
                ]),
            },
            DiffCell {
                line_no: Some(5),
                text: "new line".into(),
                kind: DiffCellKind::Insert,
                char_diff: Some(vec![
                    ("new ".to_string(), false),
                    ("line".to_string(), true),
                ]),
            },
        )];
        let (lines, _) = render_diff_cell_rows(&rows, "rust", true, 60, t.code_bg.into(), Some(0));
        assert!(!lines.is_empty());
        let text = plain_line(&lines[0]);
        // No vertical separator
        assert!(!text.contains('│'), "should have no │ separator: {text:?}");
        // Both old and new line numbers should be present in the center
        // Format: " ... old content ...  5  5 ... new content ... "
        assert!(
            text.contains(" 5 "),
            "should contain line number 5: {text:?}"
        );
        // Old content on left, new content on right
        assert!(text.contains("old"), "should contain old text: {text:?}");
        assert!(text.contains("new"), "should contain new text: {text:?}");
    }

    #[test]
    fn diff_side_marks_extreme_wrap_with_ellipsis() {
        let t = crate::theme::theme();
        let cell = DiffCell {
            line_no: Some(1),
            text: "x".repeat(200),
            kind: DiffCellKind::Normal,
            char_diff: None,
        };
        let lines = render_diff_side(&cell, 16, "", t.code_bg.into());
        assert_eq!(lines.len(), 3);
        let last = plain_line(lines.last().unwrap());
        assert!(last.contains('⋯'), "ellipsis missing: {last:?}");
    }

    #[test]
    fn char_diff_segments_identifies_changed_chars() {
        let (old_segs, new_segs) = char_diff_segments("hello world", "hello rust");
        // Common prefix "hello " should be unchanged in both
        assert!(!old_segs[0].1, "common prefix should be unchanged");
        assert_eq!(old_segs[0].0, "hello ");
        assert_eq!(new_segs[0].0, "hello ");
        // "world" → "rust": at least one segment in each should be changed
        assert!(
            old_segs.iter().any(|(_, c)| *c),
            "old should have changed chars: {:?}",
            old_segs
        );
        assert!(
            new_segs.iter().any(|(_, c)| *c),
            "new should have changed chars: {:?}",
            new_segs
        );
        // Reconstructed text should match originals
        let old_reconstructed: String = old_segs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(old_reconstructed, "hello world");
        let new_reconstructed: String = new_segs.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(new_reconstructed, "hello rust");
    }

    #[test]
    fn char_diff_segments_identical_lines_all_unchanged() {
        let (old_segs, new_segs) = char_diff_segments("same", "same");
        assert_eq!(old_segs.len(), 1);
        assert!(!old_segs[0].1);
        assert_eq!(new_segs.len(), 1);
        assert!(!new_segs[0].1);
    }

    #[test]
    fn render_diff_side_char_diff_adds_emphasis() {
        use ratatui::style::Modifier;
        let t = crate::theme::theme();
        // Delete cell with char_diff: changed segments should have UNDERLINED
        let cell = DiffCell {
            line_no: Some(1),
            text: "hello world".to_string(),
            kind: DiffCellKind::Delete,
            char_diff: Some(vec![
                ("hello ".to_string(), false),
                ("world".to_string(), true),
            ]),
        };
        let lines = render_diff_side(&cell, 40, "", t.code_bg.into());
        assert_eq!(lines.len(), 1);
        let has_underline = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(has_underline, "changed delete chars should be underlined");
        assert!(lines[0].spans.iter().all(|span| {
            span.style.fg == Some(t.diff_remove_fg.into())
                && span.style.bg == Some(t.diff_remove_bg.into())
        }));

        // Insert cell with char_diff: changed segments should have BOLD
        let cell = DiffCell {
            line_no: Some(1),
            text: "hello rust".to_string(),
            kind: DiffCellKind::Insert,
            char_diff: Some(vec![
                ("hello ".to_string(), false),
                ("rust".to_string(), true),
            ]),
        };
        let lines = render_diff_side(&cell, 40, "", t.code_bg.into());
        let has_bold = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "changed insert chars should be bold");
        assert!(lines[0].spans.iter().all(|span| {
            span.style.fg == Some(t.diff_add_fg.into())
                && span.style.bg == Some(t.diff_add_bg.into())
        }));
    }

    #[test]
    fn unified_diff_pairs_delete_insert_on_same_row() {
        // unified diff with -old/+new should pair them on the same visual row,
        // not split into two separate rows
        let diff = "--- a/test.rs\n+++ b/test.rs\n@@ -1,3 +1,3 @@\n line one\n-old line two\n+new line two\n line three\n";
        let (cells, _lang) = parse_unified_diff_to_dual(diff);
        // Hunk header + normal + paired replacement + normal.
        assert_eq!(cells.len(), 4, "should retain metadata and pair changes");
        let (left, right) = &cells[2];
        assert!(
            matches!(left.kind, DiffCellKind::Delete),
            "left should be Delete"
        );
        assert!(
            matches!(right.kind, DiffCellKind::Insert),
            "right should be Insert"
        );
        assert_eq!(left.text, "old line two");
        assert_eq!(right.text, "new line two");
        // Should have char_diff computed
        assert!(left.char_diff.is_some(), "left should have char_diff");
        assert!(right.char_diff.is_some(), "right should have char_diff");
    }

    #[test]
    fn render_dual_diff_with_replace_has_char_diff() {
        // When old and new lines are Replace'd, the cells should get char_diff
        let old = "line one\nline two\nline three";
        let new = "line one\nline TWO\nline three";
        let t = crate::theme::theme();
        let (lines, total) =
            render_dual_diff_rows("test.txt", old, new, true, 80, t.code_bg.into());
        assert_eq!(total, 3, "should have 3 diff rows");
        assert!(
            lines.len() >= 3,
            "should render diff rows, got {}",
            lines.len()
        );
    }

    #[test]
    fn user_turn_wraps_cjk_long_line() {
        let text =
            "读取文件内容并做分析的一个非常长的中文标题名称这样会超过宽度必须换行才行测试一下";
        let lines = render_user_turn(text, None, 30, false);
        assert!(lines.len() > 3, "CJK long line should wrap");
        for (i, line) in lines.iter().enumerate() {
            let w = crate::width::width(plain_line(line).as_str());
            assert!(w <= 30, "CJK line {i} width {w} exceeds panel 30",);
        }
    }

    #[test]
    fn user_turn_preserves_explicit_newlines() {
        let text = "line one\nline two\nline three";
        let lines = render_user_turn(text, None, 60, false);
        let count = lines
            .iter()
            .map(plain_line)
            .filter(|s| {
                s.contains("line one") || s.contains("line two") || s.contains("line three")
            })
            .count();
        assert_eq!(count, 3, "three explicit lines expected");
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
            approval_badge: approval.map(|n| (format!("◷{n}"), Style::default())),
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
        assert!(top.contains("◷3"), "approval tag missing: {top:?}");
        let idx_approval = top.find("◷3").unwrap();
        let idx_label = top.find("shell.exec").unwrap();
        assert!(
            idx_label < idx_approval,
            "approval must appear after label: {top:?}"
        );
        for (index, line) in out.iter().enumerate() {
            assert_eq!(
                crate::width::width(plain_line(line).as_str()),
                rect.outer_width as usize,
                "box line {index} must match its declared width"
            );
        }
    }

    #[test]
    fn approval_badge_uses_compact_terminal_icons_and_group_marker() {
        use atman_runtime::workflow::ApprovalState;

        let approved =
            approval_badge(Some(&ApprovalState::Approved), None, false).expect("approved badge");
        assert_eq!(approved.0, "✓");

        let denied = approval_badge(
            Some(&ApprovalState::Denied {
                reason: "rejected".into(),
            }),
            None,
            true,
        )
        .expect("denied badge");
        assert_eq!(denied.0, "⊘Ⓖ");
    }

    #[test]
    fn approval_badge_is_not_a_standalone_button_shape() {
        use atman_runtime::workflow::ApprovalState;

        let badge = approval_badge(
            Some(&ApprovalState::Pending {
                level: "high".into(),
                preview: None,
            }),
            Some(2),
            false,
        )
        .expect("pending badge");
        assert_eq!(badge.0, "◷2");
        assert!(!badge.0.contains('['));
        assert!(!badge.0.contains(']'));
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
            crate::width::width(mid.as_str()),
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
        let width = crate::width::width(top.as_str());
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
        let width = crate::width::width(top.as_str());
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
        let width = crate::width::width(top.as_str());
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
        let width = crate::width::width(top.as_str());
        assert_eq!(
            width, 24,
            "emoji width accounting must land on outer_width: {top:?}"
        );
    }

    #[test]
    fn every_variant_ends_with_reset_empty_line() {
        for item in [
            OutputItem::UserTurn {
                text: "hi".into(),
                presentation: None,
            },
            OutputItem::AssistantMd {
                md: "one line".into(),
                streaming: false,
                retried: false,
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
    fn thinking_wraps_long_line() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk lllll";
        let lines = render_thinking(text, true, Disclosure::Full, false, 0, 30, false);
        assert!(
            lines.len() > 6,
            "should wrap into many rows: {}",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = crate::width::width(s.as_str());
            assert_eq!(
                w, 30,
                "thinking line {i} did not fill its background: {s:?}"
            );
        }
    }

    #[test]
    fn thinking_wraps_cjk_long_line() {
        let text =
            "读取文件内容并做分析的一个非常长的中文标题名称这样会超过宽度必须换行才行测试一下看看";
        let lines = render_thinking(text, true, Disclosure::Full, false, 0, 30, false);
        assert!(lines.len() > 6, "CJK thinking should wrap: {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = crate::width::width(s.as_str());
            assert_eq!(w, 30, "CJK thinking line {i} did not fill its background");
        }
    }

    #[test]
    fn thinking_renders_markdown_bold() {
        // **bold** in thinking text should produce a BOLD span, not literal asterisks
        let lines = render_thinking(
            "this is **bold** text",
            true,
            Disclosure::Full,
            false,
            0,
            60,
            false,
        );
        let has_bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier == ratatui::style::Modifier::BOLD);
        assert!(has_bold, "thinking should render **bold** as BOLD style");
    }

    #[test]
    fn thinking_summary_ticker_advances_only_when_content_changes() {
        let text = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
        let lines = render_thinking(text, true, Disclosure::Summary, false, 0, 60, false);
        let middle = plain_line(&lines[1]);
        let later =
            plain_line(&render_thinking(text, true, Disclosure::Summary, false, 40, 60, false)[1]);
        let appended = plain_line(
            &render_thinking(
                &format!("{text}\nline11"),
                true,
                Disclosure::Summary,
                false,
                40,
                60,
                false,
            )[1],
        );
        let t = crate::theme::theme();

        assert_eq!(lines.len(), 3);
        assert!(line_is_visually_blank(&lines[0]));
        assert!(line_is_visually_blank(&lines[2]));
        assert!(middle.starts_with("  ⣿  thinking  "));
        assert!(middle.contains("line10"));
        assert!(!middle.contains("line1 ·"));
        assert_eq!(middle, later);
        assert!(appended.contains("line11"));
        assert_ne!(middle, appended);
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg == Some(t.work_bg.into()))
        );
    }

    #[test]
    fn thinking_summary_rotates_then_stops_on_full_braille() {
        let render = |done, hovered, frame| {
            render_thinking(
                "latest thought",
                done,
                Disclosure::Summary,
                hovered,
                frame,
                60,
                false,
            )
        };
        let running_0 = render(false, false, 0);
        let running_1 = render(false, false, 1);
        let done = render(true, false, 9);
        let hovered = render(true, true, 9);
        let t = crate::theme::theme();

        assert!(plain_line(&running_0[1]).starts_with("  ⠋  thinking…"));
        assert!(plain_line(&running_1[1]).starts_with("  ⠙  thinking…"));
        assert!(plain_line(&done[1]).starts_with("  ⣿  thinking"));
        assert!(
            hovered
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg == Some(t.work_hover_bg.into()))
        );
    }

    #[test]
    fn thinking_disclosure_skips_an_indistinguishable_preview() {
        assert_eq!(
            next_thinking_disclosure("short thought", Disclosure::Summary, 60),
            Disclosure::Full
        );
        assert_eq!(
            next_thinking_disclosure("short thought", Disclosure::Full, 60),
            Disclosure::Summary
        );

        let long = (1..=7)
            .map(|line| format!("thought {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            next_thinking_disclosure(&long, Disclosure::Summary, 60),
            Disclosure::Preview
        );
        assert_eq!(
            next_thinking_disclosure(&long, Disclosure::Preview, 60),
            Disclosure::Full
        );
        assert_eq!(
            next_thinking_disclosure(&long, Disclosure::Full, 60),
            Disclosure::Summary
        );
    }

    #[test]
    fn compaction_disclosure_uses_the_same_three_state_cycle() {
        assert_eq!(
            next_compaction_disclosure("short summary", Disclosure::Summary, 60),
            Disclosure::Full
        );
        assert_eq!(
            next_compaction_disclosure("short summary", Disclosure::Full, 60),
            Disclosure::Summary
        );

        let long = (1..=7)
            .map(|line| format!("summary {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            next_compaction_disclosure(&long, Disclosure::Summary, 60),
            Disclosure::Preview
        );
        assert_eq!(
            next_compaction_disclosure(&long, Disclosure::Preview, 60),
            Disclosure::Full
        );
        assert_eq!(
            next_compaction_disclosure(&long, Disclosure::Full, 60),
            Disclosure::Summary
        );
    }

    #[test]
    fn system_note_wraps_long_line() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk lllll mmmmm";
        let lines = render_system_note(text, NoteLevel::Info, 30);
        assert!(lines.len() > 4, "should wrap: {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = crate::width::width(s.as_str());
            assert!(w <= 30, "note line {i} width {w} > 30: {s:?}");
        }
    }

    #[test]
    fn user_turn_leaves_right_padding() {
        let text = "short";
        let lines = render_user_turn(text, None, 40, false);
        let body_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref().contains("short")))
            .expect("should find body line");
        let s: String = body_line.spans.iter().map(|s| s.content.as_ref()).collect();
        let w = crate::width::width(s.as_str());
        assert_eq!(w, 40, "line should fill to target 40: {s:?}");
        assert!(s.starts_with("  ❯  short"));
        assert!(
            s.ends_with("  "),
            "line should end with >=2 trailing spaces (right pad): {s:?}"
        );
    }

    #[test]
    fn document_surfaces_share_two_column_gutters() {
        let target = 48;
        let rendered = [
            (
                plain_line(&render_system_note("note", NoteLevel::Info, target)[1]),
                "  ·  note",
            ),
            (
                plain_line(
                    &render_output_block("bash", None, "✓", None, "", false, target, false)[1],
                ),
                "  ✓  bash",
            ),
            (
                plain_line(&render_mermaid_preview("graph TD\nA-->B", target, 0, false)[1]),
                "  ◇  mermaid",
            ),
            (
                plain_line(
                    &render_diff_preview("src/lib.rs", None, None, Some("+line"), false, target)[1],
                ),
                "  ✎  src/lib.rs",
            ),
            (
                plain_line(
                    &render_compaction_summary(CompactionSummaryRender {
                        phase: CompactionPhase::Finished,
                        range_start: 0,
                        range_end: 4,
                        summary: "summary",
                        before_tokens: 100,
                        after_tokens: 50,
                        compacted_count: 4,
                        disclosure: Disclosure::Summary,
                        animation_frame: 0,
                        panel_width: target,
                        hovered: false,
                    })[1],
                ),
                "  ⣿  compacted",
            ),
        ];

        for (line, prefix) in rendered {
            assert!(
                line.starts_with(prefix),
                "missing document prefix: {line:?}"
            );
            assert!(
                line.ends_with(DOCUMENT_PAD),
                "missing right gutter: {line:?}"
            );
            assert_eq!(crate::width::width(&line), target as usize);
        }

        let content = line_with_right_pad(
            DOCUMENT_PAD,
            &"x".repeat(80),
            target as usize,
            Style::default(),
            Style::default(),
        );
        let content = plain_line(&content);
        assert!(content.starts_with(DOCUMENT_PAD));
        assert!(content.ends_with(DOCUMENT_PAD));
        assert_eq!(crate::width::width(&content), target as usize);
    }

    #[test]
    fn divider_produces_dashed_line() {
        let lines = render_item(&OutputItem::Divider, &RenderCtx::empty());
        let has_dash = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.as_ref().contains("╌")));
        assert!(has_dash, "no dashed line in {lines:?}");
    }

    #[test]
    fn build_lines_concats_all_items() {
        let items = vec![
            OutputItem::UserTurn {
                text: "hi".into(),
                presentation: None,
            },
            OutputItem::Divider,
        ];
        let out = build_lines(&items, &RenderCtx::empty());
        assert!(out.len() >= 4);
    }

    #[test]
    fn build_lines_with_ranges_gives_one_range_per_item() {
        let items = vec![
            OutputItem::UserTurn {
                text: "hi".into(),
                presentation: None,
            },
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
                    llm_stats: None,
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
                    llm_stats: None,
                },
            ],
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
            cancelled: false,
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
                llm_stats: None,
            }],
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: false,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
            cancelled: false,
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(flat.contains("workflow"));
        assert!(
            flat.contains("⤢"),
            "collapsed card should expose fullscreen button: {flat}"
        );
        assert!(
            flat.contains("hidden-child"),
            "collapsed lens should surface leaf: {flat}"
        );
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
                    llm_stats: None,
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
                llm_stats: None,
            }
        }

        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(subflow_layer(0, 4));
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
            cancelled: false,
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(
            flat.matches("agent_loop").count() >= 5,
            "each iteration must render, got: {flat}"
        );
        assert!(flat.contains("final"));
    }

    fn make_tool_node(
        id: &str,
        label: &str,
        started_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> atman_runtime::workflow::WorkflowNode {
        use atman_runtime::workflow::{NodeStatus, WorkflowNode, WorkflowNodeKind};
        WorkflowNode {
            id: id.into(),
            kind: WorkflowNodeKind::ToolCall {
                tool_use_id: id.into(),
                tool: label.into(),
                args_preview: String::new(),
                call_intent: None,
                result_preview: None,
            },
            label: label.into(),
            status: NodeStatus::Ok,
            started_at,
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: atman_runtime::workflow::Parallelism::Serial,
            approval: None,
            llm_stats: None,
        }
    }

    fn make_llm_stats_node(
        id: &str,
        model: &str,
        purpose: atman_runtime::ContextCallPurpose,
        scope: atman_runtime::ContextCallScope,
        input_tokens: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> atman_runtime::workflow::WorkflowNode {
        let mut node = make_tool_node(id, id, None);
        node.llm_stats = Some(atman_runtime::workflow::LlmStats {
            model: model.into(),
            provider: format!("provider-{model}"),
            context_call_purpose: purpose,
            context_call_scope: scope,
            input_tokens,
            output_tokens: 10,
            cache_read,
            cache_write,
            ttft_ms: 100,
            tokens_per_second: 20.0,
            wallclock_ms: 200,
        });
        node
    }

    #[test]
    fn workflow_footer_separates_auxiliary_calls_and_cache_write() {
        use atman_runtime::workflow::WorkflowGraph;
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![
                make_llm_stats_node(
                    "main",
                    "primary-model",
                    atman_runtime::ContextCallPurpose::General,
                    atman_runtime::ContextCallScope::Root,
                    20,
                    80,
                    50,
                ),
                make_llm_stats_node(
                    "extract",
                    "helper-model",
                    atman_runtime::ContextCallPurpose::Extraction,
                    atman_runtime::ContextCallScope::Detached,
                    1_000,
                    0,
                    0,
                ),
            ],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };

        let line = format_workflow_stats_footer(&graph, None, 120, Style::default());
        let text = plain_line(&line);
        assert!(text.contains("↑150"), "{text}");
        assert!(text.contains("cache 80 (53%)"), "{text}");
        assert!(text.contains("+1 aux"), "{text}");
        assert!(!text.contains("↑1.1k"), "{text}");
    }

    #[test]
    fn workflow_footer_does_not_blend_cache_rates_across_routes() {
        use atman_runtime::workflow::WorkflowGraph;
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![
                make_llm_stats_node(
                    "a",
                    "model-a",
                    atman_runtime::ContextCallPurpose::General,
                    atman_runtime::ContextCallScope::Root,
                    20,
                    80,
                    0,
                ),
                make_llm_stats_node(
                    "b",
                    "model-b",
                    atman_runtime::ContextCallPurpose::General,
                    atman_runtime::ContextCallScope::Root,
                    100,
                    20,
                    0,
                ),
            ],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };

        let line = format_workflow_stats_footer(&graph, None, 120, Style::default());
        let text = plain_line(&line);
        assert!(text.contains("2 routes"), "{text}");
        assert!(text.contains("cache 100"), "{text}");
        assert!(!text.contains("cache 100 ("), "{text}");
    }

    #[test]
    fn workflow_tool_header_prefers_call_intent_over_argument_preview() {
        use atman_runtime::workflow::WorkflowGraph;
        let mut node = make_tool_node("tool-1", "bash.spawn", Some(chrono::Utc::now()));
        if let atman_runtime::workflow::WorkflowNodeKind::ToolCall {
            args_preview,
            call_intent,
            ..
        } = &mut node.kind
        {
            *args_preview = "secret command arguments".into();
            *call_intent = atman_runtime::message::ToolCallIntent::new("Inspect active processes");
        }
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![node],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (lines, _) = render_collapsed_workflow_card(&graph, None, 0, 100, false, 10, 0);
        let rendered = flatten_lines(&lines);
        assert!(rendered.contains("Inspect active processes · bash.spawn"));
        assert!(!rendered.contains("secret command arguments"));
    }

    #[test]
    fn workflow_tool_label_keeps_technical_name_after_localized_intent() {
        let intent = atman_runtime::message::ToolCallIntent::new("检查活动进程").unwrap();
        assert_eq!(
            workflow_tool_label("bash.spawn", "secret args", Some(&intent), 30, false),
            "检查活动进程 · bash.spawn"
        );
    }

    #[test]
    fn sub_agent_block_uses_goal_before_handle() {
        let lines = render_sub_agent_activity(
            "flow_s_1",
            "审计上下文缓存",
            "running",
            "",
            2,
            false,
            false,
            80,
            0,
            false,
        );
        let rendered = flatten_lines(&lines);
        let goal = rendered.find("审计上下文缓存").unwrap();
        let handle = rendered.find("flow[flow_s_1]").unwrap();
        assert!(goal < handle, "{rendered}");
    }

    #[test]
    fn collapsed_card_caps_body_at_max_rows_for_large_workflow() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let root: Vec<_> = (0..20)
            .map(|i| {
                make_tool_node(
                    &format!("n{i}"),
                    &format!("tool_{i}"),
                    Some(now + chrono::Duration::milliseconds(i)),
                )
            })
            .collect();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root,
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, None, 0, 80, false, MAX_COLLAPSED_BODY_ROWS, 0);
        let total = lines.len();
        assert!(total <= MAX_COLLAPSED_BODY_ROWS + 3);
    }

    #[test]
    fn collapsed_card_height_never_falls_below_its_observed_body() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let mut graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: (0..8)
                .map(|index| {
                    make_tool_node(
                        &format!("node-{index}"),
                        &format!("tool_{index}"),
                        Some(now + chrono::Duration::milliseconds(index)),
                    )
                })
                .collect(),
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (before, _) =
            render_collapsed_workflow_card(&graph, None, 0, 80, false, MAX_COLLAPSED_BODY_ROWS, 0);
        let observed_body = before.len() - 3;
        graph.root.truncate(1);
        let (after, regions) = render_collapsed_workflow_card(
            &graph,
            None,
            0,
            80,
            false,
            MAX_COLLAPSED_BODY_ROWS,
            observed_body,
        );
        assert_eq!(after.len(), before.len());
        assert!(after.len() <= MAX_COLLAPSED_BODY_ROWS + 3);
        assert!(
            regions
                .iter()
                .all(|region| region.end_row <= after.len() as u32)
        );
    }

    #[test]
    fn workflow_layout_cache_keeps_collapsed_height_when_nodes_disappear() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let mut graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: (0..8)
                .map(|index| {
                    make_tool_node(
                        &format!("node-{index}"),
                        &format!("tool_{index}"),
                        Some(now + chrono::Duration::milliseconds(index)),
                    )
                })
                .collect(),
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let panel = |graph: WorkflowGraph| OutputItem::WorkflowPanel {
            turn_index: 0,
            graph: graph.into(),
            expanded_nodes: Default::default(),
            panel_expanded: false,
            started_at: Instant::now(),
            ended_at: None,
            cancelled: false,
        };
        let store = OutputStore::from(vec![panel(graph.clone())]);
        let revision = store.revisions()[0];
        let mut cache = LayoutCache::default();
        cache.entries.resize(1, ItemCacheEntry::default());
        let ctx = RenderCtx::empty();
        assert!(cache.render_entry(0, &store[0], revision, &ctx, true, false));
        let observed_rows = cache.entries[0].rows;

        graph.root.truncate(1);
        assert!(cache.render_entry(0, &panel(graph), revision, &ctx, true, false));
        assert_eq!(cache.entries[0].rows, observed_rows);
    }

    #[test]
    fn projected_collapsed_card_renders_a_bounded_recent_slice() {
        use atman_runtime::projection::workflow::WorkflowProjection;
        use atman_runtime::workflow::WorkflowGraph;

        let now = chrono::Utc::now();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: (0..20_000)
                .map(|index| {
                    make_tool_node(
                        &format!("node-{index}"),
                        &format!("tool_{index}"),
                        Some(now + chrono::Duration::milliseconds(index as i64)),
                    )
                })
                .collect(),
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let projection = WorkflowProjection::from(graph);

        reset_perf_counters();
        let (lines, _) = render_workflow_projection_with_regions(
            &projection,
            &Default::default(),
            false,
            false,
            0,
            80,
            MAX_COLLAPSED_BODY_ROWS,
        );
        let counters = perf_counters();
        let rendered = flatten_lines(&lines);

        assert!(rendered.contains("tool_19999"), "{rendered}");
        assert!(!rendered.contains("tool_0"), "{rendered}");
        assert!(
            counters.workflow_node_renders <= 8,
            "collapsed projection rendered {} workflow nodes",
            counters.workflow_node_renders
        );
    }

    #[test]
    fn projected_collapsed_card_keeps_the_newest_root_at_the_bottom() {
        use atman_runtime::projection::workflow::WorkflowProjection;
        use atman_runtime::workflow::WorkflowGraph;

        let now = chrono::Utc::now();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root: vec![
                make_tool_node(
                    "newest",
                    "tool_newest",
                    Some(now + chrono::Duration::seconds(2)),
                ),
                make_tool_node("oldest", "tool_oldest", Some(now)),
                make_tool_node(
                    "middle",
                    "tool_middle",
                    Some(now + chrono::Duration::seconds(1)),
                ),
            ],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let projection = WorkflowProjection::from(graph);

        let (lines, _) = render_workflow_projection_with_regions(
            &projection,
            &Default::default(),
            false,
            false,
            0,
            80,
            MAX_COLLAPSED_BODY_ROWS,
        );
        let rendered = flatten_lines(&lines);

        assert!(rendered.find("tool_oldest").unwrap() < rendered.find("tool_middle").unwrap());
        assert!(rendered.find("tool_middle").unwrap() < rendered.find("tool_newest").unwrap());
    }

    #[test]
    fn collapsed_card_shows_more_than_3_leaves_when_available() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let root: Vec<_> = (0..8)
            .map(|i| {
                make_tool_node(
                    &format!("n{i}"),
                    &format!("tool_{i}"),
                    Some(now + chrono::Duration::milliseconds(i)),
                )
            })
            .collect();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root,
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, None, 0, 80, false, MAX_COLLAPSED_BODY_ROWS, 0);
        let flat = flatten_lines(&lines);
        let tool_count = flat.matches("tool_").count();
        assert!(
            tool_count > 3,
            "should show more than 3 tools, got {tool_count}"
        );
    }

    #[test]
    fn collapsed_card_regions_within_bounds_after_truncation() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let root: Vec<_> = (0..20)
            .map(|i| {
                make_tool_node(
                    &format!("n{i}"),
                    &format!("tool_{i}"),
                    Some(now + chrono::Duration::milliseconds(i)),
                )
            })
            .collect();
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root,
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (lines, regions) =
            render_collapsed_workflow_card(&graph, None, 0, 80, false, MAX_COLLAPSED_BODY_ROWS, 0);
        let total = lines.len() as u32;
        for r in &regions {
            assert!(
                r.end_row <= total,
                "region end_row {} exceeds total lines {}",
                r.end_row,
                total
            );
            assert!(
                r.start_row <= r.end_row,
                "region start_row {} > end_row {}",
                r.start_row,
                r.end_row
            );
        }
    }

    #[test]
    fn collapsed_card_newest_node_at_bottom() {
        use atman_runtime::workflow::WorkflowGraph;
        let now = chrono::Utc::now();
        let root = vec![
            make_tool_node("old", "old_tool", Some(now)),
            make_tool_node("new", "new_tool", Some(now + chrono::Duration::seconds(10))),
        ];
        let graph = WorkflowGraph {
            turn_id: atman_runtime::event::TurnId::now(),
            root,
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, None, 0, 80, false, MAX_COLLAPSED_BODY_ROWS, 0);
        let flat = flatten_lines(&lines);
        let old_pos = flat.find("old_tool").unwrap_or(usize::MAX);
        let new_pos = flat.find("new_tool").unwrap_or(0);
        assert!(new_pos > old_pos, "newest node should be below older node");
    }

    #[test]
    fn collapsed_working_group_shows_only_intents_progress_and_edit_totals() {
        let now = Instant::now();
        let first = ToolCallView {
            id: "edit-1".into(),
            tool: "fs.edit".into(),
            intent: "修复 working 折叠摘要".into(),
            input: serde_json::json!({"path": "/private/project/secret-output.rs"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: Some(Box::new(OutputItem::FsDetail {
                view: FsDetail::Raw {
                    tool: "fs.edit".into(),
                    path: Some("/private/project/secret-output.rs".into()),
                    content: "raw output must stay hidden".into(),
                    is_error: false,
                },
                expanded: false,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: Some((
                "/private/project/secret-output.rs".into(),
                atman_runtime::activity::EditMetrics {
                    hunks: 1,
                    insertions: 4,
                    deletions: 1,
                },
            )),
            started_at: now - std::time::Duration::from_secs(2),
            ended_at: Some(now - std::time::Duration::from_secs(1)),
        };
        let second = ToolCallView {
            id: "bash-1".into(),
            tool: "bash.spawn".into(),
            intent: "运行 TUI 测试".into(),
            input: serde_json::json!({"cmd": "cat /private/project/secret-output.rs"}),
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: now - std::time::Duration::from_secs(1),
            ended_at: None,
        };
        let calls = vec![first, second];
        let ctx = RenderCtx {
            panel_width: 100,
            ..RenderCtx::empty()
        };
        let (lines, regions) = render_tool_dispatch(&calls, &ctx, 3);
        let rendered = flatten_lines(&lines);

        assert_eq!(lines.len(), 3);
        assert!(rendered.contains("working · 修复 working 折叠摘要 → 运行 TUI 测试"));
        assert!(rendered.contains("1/2 · 1 file · +4 −1 ·"));
        assert!(!rendered.contains("working · 2"));
        assert!(!rendered.contains("fs.edit"));
        assert!(!rendered.contains("bash.spawn"));
        assert!(!rendered.contains("secret-output.rs"));
        assert!(!rendered.contains("raw output must stay hidden"));
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].path_key, "__working_group__:edit-1");
        assert_eq!((regions[0].start_row, regions[0].end_row), (0, 3));

        let hovered = (3, regions[0].path_key.clone());
        let hovered_ctx = RenderCtx {
            hovered_output_node: Some(&hovered),
            ..ctx
        };
        assert!(
            render_tool_dispatch(&calls, &hovered_ctx, 3)
                .0
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| span.style.bg == Some(crate::theme::theme().work_hover_bg.into()))
        );
    }

    #[test]
    fn collapsed_working_intents_advance_only_when_an_intent_is_appended() {
        let now = Instant::now();
        let make_call = |id: &str, intent: &str| ToolCallView {
            id: id.into(),
            tool: "fs.read".into(),
            intent: intent.into(),
            input: serde_json::json!({"path": "/private/hidden"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: now - std::time::Duration::from_millis(5),
            ended_at: Some(now),
        };
        let first = make_call(
            "read-1",
            "分析一段很长很长的 working 输出渲染链路以及所有边界条件",
        );
        let ctx = RenderCtx {
            panel_width: 60,
            animation_frame: 0,
            ..RenderCtx::empty()
        };
        let initial = plain_line(&render_tool_dispatch(std::slice::from_ref(&first), &ctx, 0).0[1]);
        let later_ctx = RenderCtx {
            animation_frame: 40,
            ..ctx
        };
        let later =
            plain_line(&render_tool_dispatch(std::slice::from_ref(&first), &later_ctx, 0).0[1]);
        let appended = plain_line(
            &render_tool_dispatch(&[first, make_call("read-2", "运行测试")], &later_ctx, 0).0[1],
        );

        assert_eq!(initial, later);
        assert!(appended.contains("运行测试"), "{appended}");
        assert_ne!(initial, appended);
        assert!(appended.contains("working"));
        assert!(appended.contains("2/2"));
    }

    #[test]
    fn tool_dispatch_uses_document_padding_and_clickable_detail_region() {
        let now = Instant::now();
        let call = ToolCallView {
            id: "edit-1".into(),
            tool: "fs.edit".into(),
            intent: "更新工具调用文档流".into(),
            input: serde_json::json!({"path": "src/output.rs"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Preview,
            detail: Some(Box::new(OutputItem::DiffPreview {
                title: "src/output.rs".into(),
                old_content: Some("old".into()),
                new_content: Some("new".into()),
                unified_diff: None,
                expanded: false,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: Some((
                "src/output.rs".into(),
                atman_runtime::activity::EditMetrics {
                    hunks: 1,
                    insertions: 4,
                    deletions: 1,
                },
            )),
            started_at: now - std::time::Duration::from_millis(42),
            ended_at: Some(now),
        };
        let expanded_groups =
            std::collections::HashSet::from([
                working_group_key(std::slice::from_ref(&call)).unwrap()
            ]);
        let ctx = RenderCtx {
            expanded_tools: &expanded_groups,
            panel_width: 80,
            ..RenderCtx::empty()
        };
        let mut second_call = call.clone();
        second_call.id = "edit-2".into();
        let (_, tool_headers) = build_lines_with_tool_headers(
            &[OutputItem::ToolDispatch {
                calls: vec![call.clone(), second_call],
            }],
            &ctx,
        );
        let mut summary_call = call.clone();
        summary_call.disclosure = Disclosure::Summary;
        let summary_lines = render_expanded_tool_dispatch(&[summary_call.clone()], &ctx, 7).0;
        let (lines, regions) = render_expanded_tool_dispatch(&[call], &ctx, 7);
        let line_text = |line: &Line<'_>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        assert_eq!(summary_lines.len(), 6);
        assert!(line_is_visually_blank(&summary_lines[0]));
        assert!(line_is_visually_blank(summary_lines.last().unwrap()));
        assert!(line_is_visually_blank(&summary_lines[2]));
        assert!(line_is_visually_blank(&summary_lines[3]));
        assert!(line_text(&summary_lines[1]).starts_with("  ⣿  working · "));
        assert!(line_text(&summary_lines[1]).ends_with("1/1 · 1 file · +4 −1 · 42ms  "));
        assert!(line_text(&summary_lines[4]).ends_with("+4 −1 · 1h · 42ms  ⤢  "));
        let tool_row = line_text(&summary_lines[4]);
        assert!(
            !['›', '⌄', '⌃']
                .into_iter()
                .any(|marker| tool_row.contains(marker))
        );

        let call_region = regions
            .iter()
            .find(|region| region.path_key == format!("{TOOL_CALL_REGION_PREFIX}edit-1"))
            .unwrap();
        let fullscreen_region = regions
            .iter()
            .find(|region| region.path_key == format!("{TOOL_FULLSCREEN_REGION_PREFIX}edit-1"))
            .unwrap();
        let detail_region = regions
            .iter()
            .find(|region| region.path_key == format!("{TOOL_DETAIL_REGION_PREFIX}edit-1"))
            .unwrap();
        assert_eq!(call_region.start_row, 3);
        assert_eq!(call_region.end_row, call_region.start_row + 3);
        assert_eq!(fullscreen_region.start_row, call_region.start_row);
        assert_eq!(fullscreen_region.end_row, call_region.end_row);
        assert_eq!(detail_region.start_row, call_region.end_row);
        assert!(detail_region.end_row > detail_region.start_row);
        assert!(fullscreen_region.col_start > call_region.col_start);
        assert!(line_is_visually_blank(lines.last().unwrap()));
        assert!(
            lines[3..6]
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| { span.style.bg == Some(crate::theme::theme().work_bg.into()) })
        );
        assert!(
            lines[6..]
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| { span.style.bg == Some(crate::theme::theme().work_detail_bg.into()) })
        );
        assert!(
            lines
                .iter()
                .rev()
                .find(|line| !line_is_visually_blank(line))
                .unwrap()
                .spans
                .iter()
                .any(|span| { span.style.bg == Some(crate::theme::theme().work_output_bg.into()) })
        );
        assert_eq!(
            tool_headers
                .iter()
                .map(|header| header.tool_id.as_str())
                .collect::<Vec<_>>(),
            vec!["edit-1", "edit-2"]
        );

        let (document, _) = render_item_with_regions(
            &OutputItem::ToolDispatch {
                calls: vec![summary_call],
            },
            &ctx,
            7,
        );
        assert_eq!(
            crate::width::spans_width(document.last().unwrap().spans.iter()),
            0
        );
        assert_eq!(
            crate::width::spans_width(document[document.len() - 2].spans.iter()),
            80
        );
    }

    #[test]
    fn tool_ticker_keeps_identity_and_right_alignment_while_terminal_output_advances() {
        let screen = atman_runtime::tools::term::parse_ansi_to_screen(
            "old output\nterminal live output 你好",
        );
        let call = ToolCallView {
            id: "terminal-1".into(),
            tool: "term.spawn".into(),
            intent: "执行 Python".into(),
            input: serde_json::json!({"cmd": "python -c very_long_command_that_must_not_replace_the_tool_identity"}),
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Summary,
            detail: Some(Box::new(OutputItem::Terminal {
                handle: "term-1".into(),
                title: None,
                command: None,
                screen,
                accumulated_bytes: Vec::new(),
                mode: crate::app::TerminalViewMode::Capture,
                done: false,
                expanded: false,
                scroll_offset: None,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };
        let lines = render_expanded_tool_dispatch(
            &[call],
            &RenderCtx {
                panel_width: 80,
                ..RenderCtx::empty()
            },
            0,
        )
        .0;
        let row = &lines[4];
        let text = plain_line(row);

        assert!(text.contains("term.spawn · 执行 Python"));
        assert!(text.contains("terminal live output 你好"));
        assert!(text.ends_with("⤢  "));
        assert_eq!(crate::width::spans_width(row.spans.iter()), 80);
    }

    #[test]
    fn streaming_tool_draft_displays_intent_before_the_final_tool_use() {
        let mut draft_preview = crate::app::ToolDraftPreview::default();
        draft_preview.push(
            "fs.read",
            r#"{"_atman_intent":"读取项目说明","path":"README.md"}"#,
        );
        let call = ToolCallView {
            id: "draft:root:0".into(),
            tool: "fs.read".into(),
            intent: "fs.read".into(),
            input: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: Some(0),
            draft_preview,
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };

        let row = &render_expanded_tool_dispatch(
            &[call],
            &RenderCtx {
                panel_width: 80,
                ..RenderCtx::empty()
            },
            0,
        )
        .0[4];
        assert!(plain_line(row).contains("fs.read · 读取项目说明"));
        assert!(row.spans.iter().any(|span| {
            span.content == "读取项目说明"
                && span.style.fg == Some(crate::theme::theme().work_action_fg.into())
        }));
    }

    #[test]
    fn missing_tool_intent_uses_a_readable_action_fallback() {
        let call = ToolCallView {
            id: "legacy-read".into(),
            tool: "fs.read".into(),
            intent: "fs.read".into(),
            input: serde_json::json!({
                "limit": 40,
                "path": "/Users/example/project/docs/context-strategy.md"
            }),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };

        let row = &render_expanded_tool_dispatch(
            &[call],
            &RenderCtx {
                panel_width: 100,
                ..RenderCtx::empty()
            },
            0,
        )
        .0[4];
        assert!(plain_line(row).contains("fs.read · read context-strategy.md"));
    }

    #[test]
    fn ticker_row_composition_fills_every_requested_display_width() {
        let background: Color = crate::theme::theme().work_bg.into();
        for width in 1..=120 {
            let row = aligned_ticker_document_row_with_control(
                vec![Span::styled(
                    "⣿  term.spawn · 执行 Python",
                    Style::default(),
                )],
                vec![Span::styled(
                    "新内容 abcdefghijklmnopqrstuvwxyz",
                    Style::default().fg(Color::Cyan),
                )],
                " · ",
                vec![Span::raw("5.1s")],
                vec![Span::raw("  ⤢")],
                width,
                background,
                TickerFade::Always,
            );
            assert_eq!(
                crate::width::spans_width(row.spans.iter()),
                width,
                "row width mismatch at {width} columns"
            );
        }
    }

    #[test]
    fn collapsed_working_ticker_only_fades_when_content_is_hidden() {
        let background: Color = crate::theme::theme().work_bg.into();
        let visible = aligned_ticker_document_row_with_control(
            vec![Span::raw("working")],
            vec![Span::styled(
                "visible intent",
                Style::default().fg(Color::Cyan),
            )],
            " · ",
            Vec::new(),
            Vec::new(),
            80,
            background,
            TickerFade::OverflowOnlySoft,
        );
        assert!(visible.spans.iter().any(|span| {
            span.content.contains("visible intent") && span.style.fg == Some(Color::Cyan)
        }));

        let hidden = aligned_ticker_document_row_with_control(
            vec![Span::raw("working")],
            vec![Span::styled(
                "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
                Style::default().fg(Color::Cyan),
            )],
            " · ",
            Vec::new(),
            Vec::new(),
            24,
            background,
            TickerFade::OverflowOnlySoft,
        );
        assert!(hidden.spans.iter().any(|span| {
            span.content.contains('z') && span.style.fg.is_some_and(|color| color != Color::Cyan)
        }));
    }

    #[test]
    fn running_thinking_spinner_uses_accent_color() {
        let lines = render_thinking("checking", false, Disclosure::Summary, false, 0, 80, false);
        let spinner = spinner_char(0);
        let glyph = lines[1]
            .spans
            .iter()
            .find(|span| span.content.contains(spinner))
            .unwrap();
        assert_eq!(glyph.style.fg, Some(crate::theme::theme().accent.into()));
    }

    #[test]
    fn tool_row_uses_stable_semantic_foregrounds() {
        let make_call = |status| ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取项目文档".into(),
            input: serde_json::json!({"path": "README.md"}),
            status,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };
        let colors = |status, frame| {
            let ctx = RenderCtx {
                panel_width: 60,
                animation_frame: frame,
                ..RenderCtx::empty()
            };
            let line = render_expanded_tool_dispatch(&[make_call(status)], &ctx, 0)
                .0
                .into_iter()
                .find(|line| plain_line(line).contains("fs.read"))
                .unwrap();
            line.spans
                .iter()
                .flat_map(|span| {
                    crate::width::graphemes(span.content.as_ref()).map(move |(grapheme, _)| {
                        (grapheme.to_string(), span.style.fg, span.style.bg)
                    })
                })
                .collect::<Vec<_>>()
        };
        let header = |status, frame| {
            let ctx = RenderCtx {
                panel_width: 60,
                animation_frame: frame,
                ..RenderCtx::empty()
            };
            plain_line(&render_expanded_tool_dispatch(&[make_call(status)], &ctx, 0).0[1])
        };

        let running_0 = colors(ToolCallStatus::Running, 0);
        let running_4 = colors(ToolCallStatus::Running, 4);
        assert!(header(ToolCallStatus::Running, 0).starts_with("  ⠋  working"));
        assert!(header(ToolCallStatus::Running, 1).starts_with("  ⠙  working"));
        assert!(header(ToolCallStatus::Ok, 0).starts_with("  ⣿  working"));
        assert!(header(ToolCallStatus::Error, 0).starts_with("  ⣿  working"));
        assert_eq!(
            running_0.iter().map(|(_, _, bg)| bg).collect::<Vec<_>>(),
            running_4.iter().map(|(_, _, bg)| bg).collect::<Vec<_>>()
        );
        assert_eq!(
            running_0.iter().map(|(_, fg, _)| fg).collect::<Vec<_>>(),
            running_4.iter().map(|(_, fg, _)| fg).collect::<Vec<_>>()
        );
        assert_eq!(colors(ToolCallStatus::Ok, 0), colors(ToolCallStatus::Ok, 4));

        let row = render_expanded_tool_dispatch(
            &[make_call(ToolCallStatus::Running)],
            &RenderCtx {
                panel_width: 80,
                ..RenderCtx::empty()
            },
            0,
        )
        .0
        .into_iter()
        .find(|line| plain_line(line).contains("fs.read"))
        .unwrap();
        let theme = crate::theme::theme();
        assert!(row.spans.iter().any(|span| {
            span.content == "fs.read" && span.style.fg == Some(theme.work_meta_fg.into())
        }));
        assert!(row.spans.iter().any(|span| {
            span.content == "读取项目文档"
                && span.style.fg == Some(theme.work_action_fg.into())
                && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn activity_summary_is_a_centered_transparent_coda() {
        let activity = crate::app::ActivityTotals::from_summary(
            &atman_runtime::activity::ActivitySummary {
                applied_edits: 2,
                hunks: 3,
                insertions: 8,
                deletions: 2,
                ..Default::default()
            },
            ["src/lib.rs".to_string()],
        );
        let lines = render_activity_summary(&activity, 80);
        let line = &lines[0];
        let theme = crate::theme::theme();

        assert_eq!(lines.len(), 1);
        assert_eq!(crate::width::spans_width(line.spans.iter()), 80);
        assert!(line.spans.iter().all(|span| span.style.bg.is_none()));
        let left_pad = crate::width::width(line.spans.first().unwrap().content.as_ref());
        let right_pad = crate::width::width(line.spans.last().unwrap().content.as_ref());
        assert!(left_pad.abs_diff(right_pad) <= 1);
        assert!(
            line.spans.iter().any(|span| {
                span.content == "+8" && span.style.fg == Some(theme.success.into())
            })
        );
        assert!(
            line.spans.iter().any(|span| {
                span.content == "−2" && span.style.fg == Some(theme.error.into())
            })
        );
    }

    #[test]
    fn cached_running_tool_spinner_repaints_without_recoloring_the_row() {
        fn running_row<'a>(lines: &'a [Line<'static>]) -> &'a Line<'static> {
            lines
                .iter()
                .find(|line| plain_line(line).contains("读取项目文档"))
                .unwrap()
        }

        let items = OutputStore::from(vec![OutputItem::ToolDispatch {
            calls: vec![ToolCallView {
                id: "read-1".into(),
                tool: "fs.read".into(),
                intent: "读取项目文档".into(),
                input: serde_json::json!({"path": "README.md"}),
                status: ToolCallStatus::Running,
                disclosure: Disclosure::Summary,
                detail: None,
                draft_index: None,
                draft_preview: Default::default(),
                applied_edit: None,
                started_at: Instant::now(),
                ended_at: None,
            }],
        }]);
        let expanded_groups =
            std::collections::HashSet::from([format!("{WORKING_GROUP_REGION_PREFIX}read-1")]);
        let ctx = RenderCtx {
            expanded_tools: &expanded_groups,
            panel_width: 60,
            ..RenderCtx::empty()
        };
        let mut cache = LayoutCache::default();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 20,
            follow_tail_rows: None,
        };
        cache.update_dirty(
            LayoutKey {
                width: 60,
                theme: crate::theme::current_mode(),
            },
            &items,
            &ctx,
            request,
        );
        reset_perf_counters();
        let frame_0 = cache.visible_slice(0, 20, 0).lines;
        let frame_4 = cache.visible_slice(0, 20, 4).lines;
        let foregrounds = |line: &Line<'static>| {
            line.spans
                .iter()
                .flat_map(|span| {
                    crate::width::graphemes(span.content.as_ref()).map(move |_| span.style.fg)
                })
                .collect::<Vec<_>>()
        };
        let backgrounds = |line: &Line<'static>| {
            line.spans
                .iter()
                .flat_map(|span| {
                    crate::width::graphemes(span.content.as_ref()).map(move |_| span.style.bg)
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            foregrounds(running_row(&frame_0)),
            foregrounds(running_row(&frame_4))
        );
        assert_eq!(
            backgrounds(running_row(&frame_0)),
            backgrounds(running_row(&frame_4))
        );
        let header = |lines: &[Line<'static>]| {
            plain_line(
                lines
                    .iter()
                    .find(|line| plain_line(line).contains("working ·"))
                    .unwrap(),
            )
        };
        assert_ne!(header(&frame_0), header(&frame_4));
        assert_eq!(perf_counters().item_renders, 0);
    }

    #[test]
    fn tool_dispatch_leaves_a_document_gap_before_assistant_output() {
        let call = ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取项目文档".into(),
            input: serde_json::json!({"path": "README.md"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };
        let items = OutputStore::from(vec![
            OutputItem::ToolDispatch { calls: vec![call] },
            OutputItem::AssistantMd {
                md: "继续输出".into(),
                streaming: false,
                retried: false,
            },
        ]);
        let ctx = RenderCtx {
            panel_width: 80,
            ..RenderCtx::empty()
        };
        let mut cache = LayoutCache::default();
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: None,
        };
        let metrics = cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            &items,
            &ctx,
            request,
        );
        let visible = cache.visible_slice(0, metrics.total_rows, 0);
        let lines = visible.lines;
        let ranges = visible.ranges;

        assert_eq!(ranges[1].start_row, ranges[0].end_row);
        let separator = &lines[ranges[0].end_row.saturating_sub(1) as usize];
        assert_eq!(crate::width::spans_width(separator.spans.iter()), 0);
        let internal_padding = &lines[ranges[0].end_row.saturating_sub(2) as usize];
        assert_eq!(crate::width::spans_width(internal_padding.spans.iter()), 80);
        assert!(
            internal_padding
                .spans
                .iter()
                .all(|span| span.style.bg == Some(crate::theme::theme().work_bg.into()))
        );
    }

    #[test]
    fn consecutive_tool_dispatches_remain_separate_documents() {
        let call = ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取项目文档".into(),
            input: serde_json::json!({"path": "README.md"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };
        let items = vec![
            OutputItem::ToolDispatch {
                calls: vec![call.clone()],
            },
            OutputItem::ToolDispatch { calls: vec![call] },
        ];
        let (lines, ranges, _, _) = build_lines_with_ranges(&items, 80, &RenderCtx::empty());

        assert_eq!(ranges[1].start_row, ranges[0].end_row);
        assert_eq!(
            crate::width::spans_width(
                lines[ranges[0].end_row.saturating_sub(1) as usize]
                    .spans
                    .iter()
            ),
            0
        );
    }

    #[test]
    fn tool_rows_reserve_control_space_and_use_semantic_edit_colors() {
        let now = Instant::now();
        let base = ToolCallView {
            id: "edit-1".into(),
            tool: "fs.edit".into(),
            intent: "修改文件".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: Some((
                "src/lib.rs".into(),
                atman_runtime::activity::EditMetrics {
                    hunks: 1,
                    insertions: 3,
                    deletions: 2,
                },
            )),
            started_at: now - std::time::Duration::from_millis(42),
            ended_at: Some(now),
        };
        let mut fullscreen = base.clone();
        fullscreen.id = "edit-2".into();
        fullscreen.detail = Some(Box::new(OutputItem::DiffPreview {
            title: "src/lib.rs".into(),
            old_content: Some("old".into()),
            new_content: Some("new".into()),
            unified_diff: None,
            expanded: false,
        }));
        let lines = render_expanded_tool_dispatch(
            &[base, fullscreen],
            &RenderCtx {
                panel_width: 72,
                ..RenderCtx::empty()
            },
            0,
        )
        .0;
        let rows = lines
            .iter()
            .filter(|line| plain_line(line).contains("fs.edit"))
            .collect::<Vec<_>>();
        let t = crate::theme::theme();

        assert_eq!(rows.len(), 2);
        assert!(plain_line(rows[0]).ends_with("42ms     "));
        assert!(plain_line(rows[1]).ends_with("42ms  ⤢  "));
        for row in rows {
            assert_eq!(crate::width::spans_width(row.spans.iter()), 72);
            assert!(
                row.spans.iter().any(|span| {
                    span.content == "+3" && span.style.fg == Some(t.success.into())
                })
            );
            assert!(
                row.spans.iter().any(|span| {
                    span.content == "−2" && span.style.fg == Some(t.error.into())
                })
            );
        }
    }

    #[test]
    fn expanded_tool_detail_uses_distinct_theme_layers() {
        let call = ToolCallView {
            id: "edit-1".into(),
            tool: "fs.edit".into(),
            intent: "修改文件".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Preview,
            detail: Some(Box::new(OutputItem::DiffPreview {
                title: "src/lib.rs".into(),
                old_content: Some("old".into()),
                new_content: Some("new".into()),
                unified_diff: None,
                expanded: false,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };
        let lines = render_expanded_tool_dispatch(
            &[call],
            &RenderCtx {
                panel_width: 72,
                ..RenderCtx::empty()
            },
            0,
        )
        .0;
        let t = crate::theme::theme();
        let backgrounds = lines
            .iter()
            .flat_map(|line| line.spans.iter().filter_map(|span| span.style.bg))
            .collect::<std::collections::HashSet<_>>();

        assert!(backgrounds.contains(&t.work_bg.into()));
        assert!(backgrounds.contains(&t.work_detail_bg.into()));
        assert!(backgrounds.contains(&t.work_output_bg.into()));
    }

    #[test]
    fn tool_title_toggles_content_and_detail_toggles_full_expansion() {
        let base = ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取项目文档".into(),
            input: serde_json::Value::Null,
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };

        assert_eq!(
            toggle_tool_call_content_disclosure(&base, 80),
            Disclosure::Summary
        );

        let mut short = base.clone();
        short.input = serde_json::json!({"path": "README.md"});
        assert_eq!(
            toggle_tool_call_content_disclosure(&short, 80),
            Disclosure::Preview
        );
        short.disclosure = Disclosure::Preview;
        assert_eq!(
            toggle_tool_call_content_disclosure(&short, 80),
            Disclosure::Summary
        );
        assert_eq!(
            toggle_tool_call_detail_disclosure(&short, 80),
            Disclosure::Preview
        );

        let mut long = base;
        long.detail = Some(Box::new(OutputItem::Bash {
            handle: "bg-1".into(),
            title: None,
            command: Some("printf test".into()),
            output: (0..20)
                .map(|index| format!("line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
            done: true,
            expanded: false,
        }));
        assert_eq!(
            toggle_tool_call_content_disclosure(&long, 80),
            Disclosure::Preview
        );
        long.disclosure = Disclosure::Preview;
        assert_eq!(
            toggle_tool_call_content_disclosure(&long, 80),
            Disclosure::Summary
        );
        assert_eq!(
            toggle_tool_call_detail_disclosure(&long, 80),
            Disclosure::Full
        );
        long.disclosure = Disclosure::Full;
        assert_eq!(
            toggle_tool_call_content_disclosure(&long, 80),
            Disclosure::Summary
        );
        assert_eq!(
            toggle_tool_call_detail_disclosure(&long, 80),
            Disclosure::Preview
        );
    }

    #[test]
    fn tool_input_summary_prefers_auditable_target_fields() {
        let call = ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取项目文档".into(),
            input: serde_json::json!({"limit": 80, "path": "/repo/README.md", "offset": 1}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Summary,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
        };

        assert_eq!(
            tool_input_summary(&call).as_deref(),
            Some("/repo/README.md")
        );
        let rendered =
            flatten_lines(&render_expanded_tool_dispatch(&[call], &RenderCtx::empty(), 0).0);
        assert!(rendered.contains("/repo/README.md"));
    }

    #[test]
    fn filesystem_input_detail_is_human_readable_instead_of_json() {
        let call = ToolCallView {
            id: "grep-1".into(),
            tool: "fs.grep".into(),
            intent: "查找渲染入口".into(),
            input: serde_json::json!({
                "pattern": "render_.*",
                "path": "src",
                "context_lines": 2,
                "case_sensitive": false,
                "limit": 20
            }),
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Preview,
            detail: None,
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };

        let body = tool_input_body(&call).unwrap();
        assert!(body.contains("pattern  render_.*"));
        assert!(body.contains("root  src"));
        assert!(!body.contains('{'));
        assert!(!body.contains('"'));
    }

    #[test]
    fn filesystem_detail_renders_real_content_with_syntax_highlighting() {
        let item = OutputItem::FsDetail {
            view: FsDetail::Read {
                path: "src/lib.rs".into(),
                content: "fn answer() -> usize { 42 }\n".into(),
                start_line: 7,
                total_lines: Some(20),
                truncated: false,
            },
            expanded: false,
        };
        let lines = render_item(
            &item,
            &RenderCtx {
                panel_width: 80,
                ..RenderCtx::empty()
            },
        );
        let text = flatten_lines(&lines);
        assert!(text.contains("src/lib.rs"));
        assert!(text.contains("lines 7–7 / 20"));
        assert!(text.contains("fn answer()"));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| { span.content.contains("fn") && span.style.fg.is_some() })
        );
    }

    #[test]
    fn filesystem_detail_only_adds_full_disclosure_when_content_overflows() {
        let now = Instant::now();
        let mut call = ToolCallView {
            id: "read-1".into(),
            tool: "fs.read".into(),
            intent: "读取配置".into(),
            input: serde_json::json!({"path": "config.toml"}),
            status: ToolCallStatus::Ok,
            disclosure: Disclosure::Preview,
            detail: Some(Box::new(OutputItem::FsDetail {
                view: FsDetail::Read {
                    path: "config.toml".into(),
                    content: "enabled = true\n".into(),
                    start_line: 1,
                    total_lines: Some(1),
                    truncated: false,
                },
                expanded: false,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: now,
            ended_at: Some(now),
        };
        assert_eq!(
            toggle_tool_call_content_disclosure(&call, 80),
            Disclosure::Summary
        );
        assert_eq!(
            toggle_tool_call_detail_disclosure(&call, 80),
            Disclosure::Preview
        );

        call.detail = Some(Box::new(OutputItem::FsDetail {
            view: FsDetail::Read {
                path: "config.toml".into(),
                content: "x".repeat(100),
                start_line: 1,
                total_lines: Some(1),
                truncated: false,
            },
            expanded: false,
        }));
        assert_eq!(
            toggle_tool_call_content_disclosure(&call, 80),
            Disclosure::Summary
        );
        assert_eq!(
            toggle_tool_call_detail_disclosure(&call, 80),
            Disclosure::Preview
        );

        call.detail = Some(Box::new(OutputItem::FsDetail {
            view: FsDetail::List {
                path: "src".into(),
                entries: (0..20)
                    .map(|index| format!("src/file-{index}.rs"))
                    .collect(),
            },
            expanded: false,
        }));
        assert_eq!(
            toggle_tool_call_detail_disclosure(&call, 80),
            Disclosure::Full
        );

        let expanded_groups =
            std::collections::HashSet::from([format!("{WORKING_GROUP_REGION_PREFIX}read-1")]);
        let item = OutputItem::ToolDispatch { calls: vec![call] };
        let (_, regions) = render_item_with_regions(
            &item,
            &RenderCtx {
                expanded_tools: &expanded_groups,
                panel_width: 80,
                ..RenderCtx::empty()
            },
            0,
        );
        assert!(
            regions.iter().any(|region| {
                region.path_key == format!("{TOOL_FULLSCREEN_REGION_PREFIX}read-1")
            })
        );
    }

    #[test]
    fn tool_fullscreen_hover_uses_accent_without_highlight_background() {
        let call = ToolCallView {
            id: "shell-1".into(),
            tool: "bash.spawn".into(),
            intent: "运行检查".into(),
            input: serde_json::json!({"cmd": "true"}),
            status: ToolCallStatus::Running,
            disclosure: Disclosure::Preview,
            detail: Some(Box::new(OutputItem::Bash {
                handle: "bg-1".into(),
                title: None,
                command: Some("true".into()),
                output: String::new(),
                done: false,
                expanded: false,
            })),
            draft_index: None,
            draft_preview: Default::default(),
            applied_edit: None,
            started_at: Instant::now(),
            ended_at: None,
        };
        let mut other_call = call.clone();
        other_call.id = "shell-2".into();
        let hovered = (0, format!("{TOOL_DETAIL_FULLSCREEN_REGION_PREFIX}shell-1"));
        let ctx = RenderCtx {
            panel_width: 80,
            hovered_output_node: Some(&hovered),
            ..RenderCtx::empty()
        };
        let (lines, regions) = render_expanded_tool_dispatch(&[call, other_call], &ctx, 0);
        let fullscreen = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.contains('⤢'))
            .collect::<Vec<_>>();
        let theme = crate::theme::theme();

        assert_eq!(fullscreen.len(), 4);
        assert_eq!(
            fullscreen
                .iter()
                .filter(|span| span.style.fg == Some(theme.accent.into()))
                .count(),
            1
        );
        let hovered = fullscreen
            .into_iter()
            .find(|span| span.style.fg == Some(theme.accent.into()))
            .unwrap();
        assert_ne!(hovered.style.bg, Some(theme.highlight_bg.into()));
        assert!(regions.iter().any(|region| {
            region.path_key == format!("{TOOL_DETAIL_FULLSCREEN_REGION_PREFIX}shell-1")
        }));
    }
}

fn render_compaction_summary(render: CompactionSummaryRender<'_>) -> Vec<Line<'static>> {
    let CompactionSummaryRender {
        phase,
        range_start,
        range_end,
        summary,
        before_tokens,
        after_tokens,
        compacted_count,
        disclosure,
        animation_frame,
        panel_width,
        hovered,
    } = render;
    let t = crate::theme::theme();
    let bg: Color = if hovered {
        t.work_hover_bg.into()
    } else {
        t.work_bg.into()
    };
    let header_style = Style::default().fg(t.work_title_fg.into()).bg(bg);
    let glyph_style = Style::default()
        .fg(match phase {
            CompactionPhase::Running => t.accent.into(),
            CompactionPhase::Finished => t.success.into(),
            CompactionPhase::Failed => t.error.into(),
        })
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(t.work_title_fg.into()).bg(bg);
    let hint_style = Style::default()
        .fg(t.work_meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let (glyph, label, stats) = match phase {
        CompactionPhase::Running => (
            spinner_char(animation_frame),
            "compacting…",
            format!("{range_start}..{range_end}"),
        ),
        CompactionPhase::Finished => (
            "⣿",
            "compacted",
            format!("{before_tokens} → {after_tokens} · {compacted_count} msgs"),
        ),
        CompactionPhase::Failed => (
            "✗",
            "compaction failed",
            format!("{range_start}..{range_end}"),
        ),
    };
    let text = if summary.is_empty() && matches!(phase, CompactionPhase::Running) {
        "summary generation in progress"
    } else {
        summary
    };
    render_markdown_disclosure(MarkdownDisclosureRender {
        text,
        disclosure,
        header_prefix: vec![
            Span::styled(format!("{glyph}{DOCUMENT_PAD}"), glyph_style),
            Span::styled(label, header_style),
        ],
        header_right: vec![Span::styled(stats, hint_style)],
        bg,
        body_style,
        hint_style,
        panel_width,
    })
}

pub fn render_injection_queue(
    pending: &[atman_runtime::injection::Injection],
    width: u16,
) -> Vec<Line<'static>> {
    use atman_runtime::injection::InjectionLevel;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};

    if pending.is_empty() {
        return Vec::new();
    }
    let t = crate::theme::theme();
    let max_w = width.saturating_sub(6) as usize;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let title = format!(" ⚡ next LLM call · {} waiting ", pending.len());
    lines.push(Line::from(Span::styled(
        title,
        Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::BOLD),
    )));

    for inj in pending {
        let level_style = match inj.level {
            InjectionLevel::L1Nudge => Style::default()
                .fg(t.success.into())
                .add_modifier(Modifier::BOLD),
            InjectionLevel::L2CourseCorrect => Style::default()
                .fg(t.warn.into())
                .add_modifier(Modifier::BOLD),
            InjectionLevel::L3Redirect => Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
            InjectionLevel::L4HardStop => Style::default()
                .fg(t.error.into())
                .add_modifier(Modifier::BOLD),
        };
        let level_label = match inj.level {
            InjectionLevel::L1Nudge => "L1",
            InjectionLevel::L2CourseCorrect => "L2",
            InjectionLevel::L3Redirect => "L3",
            InjectionLevel::L4HardStop => "L4",
        };
        let text = if inj.queued_submission_id.is_some() {
            let summary = if let Some(presentation) = &inj.presentation {
                let prompt = presentation.prompt.replace(['\n', '\r'], " ");
                if prompt.trim().is_empty() {
                    presentation
                        .quote
                        .as_ref()
                        .and_then(|quote| quote.text.lines().next())
                        .map_or_else(
                            || "quoted selection".to_owned(),
                            |line| format!("quote: {line}"),
                        )
                } else {
                    prompt
                }
            } else {
                inj.text.replace(['\n', '\r'], " ")
            };
            format!("{summary} · waiting for next LLM call")
        } else {
            inj.text.clone()
        };
        let text = crate::width::truncate_plain(&text, max_w.saturating_sub(6));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(format!("[{level_label}]"), level_style),
            Span::styled(format!(" {text}"), Style::default().fg(t.tinted_fg.into())),
        ]));
    }
    lines
}
