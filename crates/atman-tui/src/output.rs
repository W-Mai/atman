use std::sync::Arc;

use atman_runtime::message::Message;
use atman_runtime::stream::CompactionPhase;
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
    pub hovered_thinking_idx: Option<usize>,
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
    pub start_row: u32,
    pub end_row: u32,
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
    pub approval_hotkey: Option<u8>,
}

struct CompactionSummaryRender<'a> {
    phase: CompactionPhase,
    range_start: usize,
    range_end: usize,
    summary: &'a str,
    before_tokens: u64,
    after_tokens: u64,
    compacted_count: usize,
    expanded: bool,
    animation_frame: u32,
    panel_width: u16,
}

pub fn append_box(out: &mut Vec<Line<'static>>, spec: BoxSpec<'_>) -> BoxRect {
    let t = crate::theme::theme();
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
    let approval_w = approval_text.as_deref().map_or(0, crate::width::width);
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
    if let Some(text) = approval_text {
        top_spans.push(Span::styled(
            text,
            Style::default()
                .fg(t.warn.into())
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
    item_cache: &mut Vec<Option<ItemCacheEntry>>,
    animation_frame: Option<u32>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u32) {
    if item_cache.len() < items.len() {
        item_cache.resize(items.len(), None);
    }
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut node_regions: Vec<NodeRegion> = Vec::new();
    let mut cursor: u32 = 0;
    let mut prev_kind: Option<ItemKind> = None;
    for (idx, item) in items.iter().enumerate() {
        let kind = ItemKind::of(item);
        if let Some(prev) = prev_kind
            && kind.wants_breathing_after(prev)
        {
            all_lines.push(Line::from(""));
            cursor = cursor.saturating_add(1);
        }
        let is_hovered = ctx.hovered_thinking_idx == Some(idx);
        let content_hash = item_content_hash(item, is_hovered, ctx.expanded_tools, animation_frame);
        let cached = item_cache[idx].take();
        let (item_lines, mut item_regions) = if let Some(entry) = cached.as_ref()
            && entry.content_hash == content_hash
        {
            (entry.lines.iter().cloned().collect::<Vec<_>>(), Vec::new())
        } else {
            let item_ctx = RenderCtx {
                expanded_tools: ctx.expanded_tools,
                messages: ctx.messages,
                animation_frame: ctx.animation_frame,
                panel_width: ctx.panel_width,
                hovered_thinking_idx: if is_hovered && matches!(item, OutputItem::Thinking { .. }) {
                    Some(idx)
                } else {
                    None
                },
            };
            render_item_with_regions(item, &item_ctx, idx)
        };
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
        all_lines.extend(item_lines.clone());
        item_cache[idx] = Some(ItemCacheEntry {
            content_hash,
            lines: Arc::from(item_lines),
            rows,
            regions: item_regions,
        });
        if !matches!(kind, ItemKind::StartupCard) {
            prev_kind = Some(kind);
        }
    }
    (all_lines, ranges, node_regions, cursor)
}

fn str_fp(s: &str) -> (usize, [u8; 8], [u8; 8]) {
    let len = s.len();
    let head: [u8; 8] = s
        .as_bytes()
        .get(..8)
        .unwrap_or(&[])
        .try_into()
        .unwrap_or([0; 8]);
    let tail: [u8; 8] = if len > 8 {
        s.as_bytes()[len - 8..].try_into().unwrap_or([0; 8])
    } else {
        [0; 8]
    };
    (len, head, tail)
}

fn item_content_hash(
    item: &OutputItem,
    hovered: bool,
    _expanded_tools: &std::collections::HashSet<String>,
    animation_frame: Option<u32>,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    let _buf = String::new();
    match item {
        OutputItem::UserTurn { text } => {
            0u8.hash(&mut h);
            str_fp(text).hash(&mut h);
        }
        OutputItem::Thinking {
            text,
            done,
            expanded,
            retried,
        } => {
            1u8.hash(&mut h);
            str_fp(text).hash(&mut h);
            done.hash(&mut h);
            expanded.hash(&mut h);
            retried.hash(&mut h);
            hovered.hash(&mut h);
            if !done {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::AssistantMd {
            md,
            streaming,
            retried,
        } => {
            2u8.hash(&mut h);
            str_fp(md).hash(&mut h);
            streaming.hash(&mut h);
            retried.hash(&mut h);
        }
        OutputItem::SystemNote { text, level } => {
            3u8.hash(&mut h);
            str_fp(text).hash(&mut h);
            format!("{:?}", level).hash(&mut h);
        }
        OutputItem::Divider => 4u8.hash(&mut h),
        OutputItem::WorkflowPanel {
            turn_index,
            graph,
            expanded_nodes,
            panel_expanded,
            started_at,
            ended_at,
            ..
        } => {
            5u8.hash(&mut h);
            turn_index.hash(&mut h);
            graph.root.len().hash(&mut h);
            expanded_nodes.len().hash(&mut h);
            panel_expanded.hash(&mut h);
            started_at.hash(&mut h);
            ended_at.hash(&mut h);
            if ended_at.is_none() {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::StartupCard { version, recent } => {
            6u8.hash(&mut h);
            version.hash(&mut h);
            recent.len().hash(&mut h);
        }
        OutputItem::Terminal {
            handle,
            screen,
            accumulated_bytes,
            mode,
            done,
            expanded,
            scroll_offset,
        } => {
            7u8.hash(&mut h);
            handle.hash(&mut h);
            screen.rows.hash(&mut h);
            screen.cols.hash(&mut h);
            screen.alt_screen.hash(&mut h);
            accumulated_bytes.len().hash(&mut h);
            format!("{:?}", mode).hash(&mut h);
            done.hash(&mut h);
            expanded.hash(&mut h);
            scroll_offset.hash(&mut h);
            if !done {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::Bash {
            handle,
            output,
            done,
            expanded,
        } => {
            8u8.hash(&mut h);
            handle.hash(&mut h);
            str_fp(output).hash(&mut h);
            done.hash(&mut h);
            expanded.hash(&mut h);
            if !done {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::CompactionSummary {
            phase,
            range_start,
            range_end,
            summary,
            before_tokens,
            after_tokens,
            compacted_count,
            expanded,
        } => {
            9u8.hash(&mut h);
            phase.hash(&mut h);
            range_start.hash(&mut h);
            range_end.hash(&mut h);
            str_fp(summary).hash(&mut h);
            before_tokens.hash(&mut h);
            after_tokens.hash(&mut h);
            compacted_count.hash(&mut h);
            expanded.hash(&mut h);
            if matches!(phase, CompactionPhase::Running) {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::DiffPreview {
            title,
            old_content,
            new_content,
            unified_diff,
            expanded,
        } => {
            10u8.hash(&mut h);
            title.hash(&mut h);
            old_content.as_deref().map(str_fp).hash(&mut h);
            new_content.as_deref().map(str_fp).hash(&mut h);
            unified_diff.as_deref().map(str_fp).hash(&mut h);
            expanded.hash(&mut h);
        }
        OutputItem::MermaidDiagram { source } => {
            11u8.hash(&mut h);
            str_fp(source).hash(&mut h);
        }
        OutputItem::SubAgentActivity {
            handle,
            goal,
            child_run_id,
            model,
            status,
            output,
            iteration,
            done,
            expanded,
            expanded_nodes,
            workflow_expanded,
            ..
        } => {
            12u8.hash(&mut h);
            str_fp(handle).hash(&mut h);
            str_fp(goal).hash(&mut h);
            str_fp(child_run_id).hash(&mut h);
            str_fp(model).hash(&mut h);
            str_fp(status).hash(&mut h);
            str_fp(output).hash(&mut h);
            iteration.hash(&mut h);
            done.hash(&mut h);
            expanded.hash(&mut h);
            expanded_nodes.len().hash(&mut h);
            workflow_expanded.hash(&mut h);
            if !done {
                animation_frame.hash(&mut h);
            }
        }
    }
    h.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    UserTurn,
    Thinking,
    Assistant,
    SystemNote,
    Divider,
    WorkflowPanel,
    StartupCard,
    Terminal,
    Bash,
    CompactionSummary,
    DiffPreview,
    MermaidDiagram,
    SubAgentActivity,
}

impl ItemKind {
    fn of(item: &OutputItem) -> Self {
        match item {
            OutputItem::UserTurn { .. } => Self::UserTurn,
            OutputItem::Thinking { .. } => Self::Thinking,
            OutputItem::AssistantMd { .. } => Self::Assistant,
            OutputItem::SystemNote { .. } => Self::SystemNote,
            OutputItem::Divider => Self::Divider,
            OutputItem::WorkflowPanel { .. } => Self::WorkflowPanel,
            OutputItem::StartupCard { .. } => Self::StartupCard,
            OutputItem::Terminal { .. } => Self::Terminal,
            OutputItem::Bash { .. } => Self::Bash,
            OutputItem::CompactionSummary { .. } => Self::CompactionSummary,
            OutputItem::DiffPreview { .. } => Self::DiffPreview,
            OutputItem::MermaidDiagram { .. } => Self::MermaidDiagram,
            OutputItem::SubAgentActivity { .. } => Self::SubAgentActivity,
        }
    }

    // Divider self-separates; StartupCard emits no lines; UserTurn brings its own top/bottom padding.
    fn wants_breathing_after(self, prev: Self) -> bool {
        if matches!(prev, Self::Divider | Self::StartupCard | Self::UserTurn)
            || matches!(self, Self::Divider | Self::StartupCard | Self::UserTurn)
        {
            return false;
        }
        prev != self
    }
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
    if let OutputItem::WorkflowPanel {
        graph,
        expanded_nodes,
        panel_expanded,
        cancelled,
        ..
    } = item
    {
        render_workflow_panel_with_regions(
            graph,
            expanded_nodes,
            *panel_expanded,
            *cancelled,
            ctx.animation_frame,
            ctx.panel_width,
            MAX_COLLAPSED_BODY_ROWS,
        )
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
    total_rows: u32,
    item_cache: Vec<Option<ItemCacheEntry>>,
    cached_total_rows: u32,
    item_rows: Vec<u32>,
    cached_items_len: usize,
}

#[derive(Clone)]
pub struct ItemCacheEntry {
    content_hash: u64,
    lines: Arc<[Line<'static>]>,
    rows: u32,
    regions: Vec<NodeRegion>,
}

impl LayoutCache {
    pub fn get_or_build(
        &mut self,
        key: LayoutKey,
        items: &[OutputItem],
        ctx: &RenderCtx<'_>,
        scroll_offset: u32,
        viewport_rows: u32,
    ) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u32) {
        if items.is_empty() {
            self.item_cache.clear();
            self.item_rows.clear();
            self.cached_total_rows = 0;
            self.cached_items_len = 0;
            self.key = Some(key);
            self.lines.clear();
            self.ranges.clear();
            self.node_regions.clear();
            self.total_rows = 0;
            return (Vec::new(), Vec::new(), Vec::new(), 0);
        }

        // Resize caches if item count changed
        if items.len() != self.cached_items_len {
            let old_len = self.item_rows.len();
            self.item_cache.resize(items.len(), None);
            self.item_rows.resize(items.len(), 0);
            // New items have rows=0, will be rendered below
            self.cached_items_len = items.len();
            let _ = old_len;
        }

        // Incremental update: only render items whose content_hash changed.
        // Adjust cached_total_rows by the row delta.
        for (idx, item) in items.iter().enumerate() {
            let is_hovered = ctx.hovered_thinking_idx == Some(idx);
            let content_hash =
                item_content_hash(item, is_hovered, ctx.expanded_tools, key.animation_frame);
            let need_render = self.item_cache[idx]
                .as_ref()
                .map(|e| e.content_hash != content_hash)
                .unwrap_or(true);
            if !need_render {
                continue;
            }
            let old_rows = self.item_rows[idx];
            let item_ctx = RenderCtx {
                expanded_tools: ctx.expanded_tools,
                messages: ctx.messages,
                animation_frame: ctx.animation_frame,
                panel_width: ctx.panel_width,
                hovered_thinking_idx: if is_hovered && matches!(item, OutputItem::Thinking { .. }) {
                    Some(idx)
                } else {
                    None
                },
            };
            let (item_lines, item_regions) = render_item_with_regions(item, &item_ctx, idx);
            let (new_rows, _) = wrap_row_offsets(&item_lines, key.width);
            self.item_cache[idx] = Some(ItemCacheEntry {
                content_hash,
                lines: Arc::from(item_lines),
                rows: new_rows,
                regions: item_regions,
            });
            self.item_rows[idx] = new_rows;
            // Incremental total_rows adjustment
            self.cached_total_rows = self
                .cached_total_rows
                .saturating_sub(old_rows)
                .saturating_add(new_rows);
        }

        let total_rows = self.cached_total_rows;

        // Virtual scroll: absolute coordinates. vis_top from top.
        let vis_top = scroll_offset;
        let vis_bot = scroll_offset.saturating_add(viewport_rows);

        // Two-pass: first pass walks from vis_top backwards to preload
        // PRELOAD_BLOCKS items above viewport. Second pass clones only
        // viewport lines.
        const PRELOAD_BLOCKS: usize = 3;

        // Find the item index where vis_top falls, and preload above it
        let mut cursor: u32 = 0;
        let mut vis_start_idx: usize = 0;
        for (idx, _) in items.iter().enumerate() {
            let rows = self.item_rows[idx];
            let end = cursor.saturating_add(rows);
            if end > vis_top {
                vis_start_idx = idx;
                break;
            }
            cursor = end;
            vis_start_idx = idx + 1;
        }

        // Ensure preloaded items above vis_start_idx are cached
        let preload_start = vis_start_idx.saturating_sub(PRELOAD_BLOCKS);
        for (idx, item) in items
            .iter()
            .enumerate()
            .skip(preload_start)
            .take(vis_start_idx.saturating_sub(preload_start))
        {
            if self.item_cache[idx].is_some() {
                continue;
            }
            let is_hovered = ctx.hovered_thinking_idx == Some(idx);
            let content_hash =
                item_content_hash(item, is_hovered, ctx.expanded_tools, key.animation_frame);
            let item_ctx = RenderCtx {
                expanded_tools: ctx.expanded_tools,
                messages: ctx.messages,
                animation_frame: ctx.animation_frame,
                panel_width: ctx.panel_width,
                hovered_thinking_idx: None,
            };
            let (item_lines, item_regions) = render_item_with_regions(item, &item_ctx, idx);
            let (rows, _) = wrap_row_offsets(&item_lines, key.width);
            self.item_cache[idx] = Some(ItemCacheEntry {
                content_hash,
                lines: Arc::from(item_lines),
                rows,
                regions: item_regions,
            });
            self.item_rows[idx] = rows;
        }

        // Build visible lines: only clone items in [vis_top, vis_bot)
        let mut visible_lines: Vec<Line<'static>> = Vec::new();
        let mut visible_ranges: Vec<ItemRange> = Vec::new();
        let mut visible_regions: Vec<NodeRegion> = Vec::new();
        cursor = 0;
        for (idx, _) in items.iter().enumerate() {
            let entry = match self.item_cache[idx].as_ref() {
                Some(e) => e,
                None => continue,
            };
            let start = cursor;
            let end = cursor.saturating_add(entry.rows);
            cursor = end;
            if end <= vis_top || start >= vis_bot {
                continue;
            }
            let skip = vis_top.saturating_sub(start) as usize;
            let take = end.min(vis_bot).saturating_sub(start.max(vis_top)) as usize;
            let lo = skip.min(entry.lines.len());
            let hi = (skip + take).min(entry.lines.len());
            visible_lines.extend(entry.lines[lo..hi].iter().cloned());
            visible_ranges.push(ItemRange {
                item_index: idx,
                start_row: start,
                end_row: end,
            });
            for r in &entry.regions {
                visible_regions.push(NodeRegion {
                    panel_item_index: idx,
                    path_key: r.path_key.clone(),
                    start_row: r.start_row.saturating_add(start),
                    end_row: r.end_row.saturating_add(start),
                    col_start: r.col_start,
                    col_end: r.col_end,
                });
            }
        }

        self.key = Some(key);
        self.total_rows = total_rows;
        (visible_lines, visible_ranges, visible_regions, total_rows)
    }

    pub fn take_cached(&mut self) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>) {
        (
            std::mem::take(&mut self.lines),
            std::mem::take(&mut self.ranges),
            std::mem::take(&mut self.node_regions),
        )
    }

    pub fn cached_total_rows(&self) -> u32 {
        self.cached_total_rows
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

// Subtle stripe behind user messages so they visually separate from
// assistant markdown without a heavy border or gutter glyph.
fn user_message_bg() -> Color {
    crate::theme::theme().user_msg_bg.into()
}

const RIGHT_PAD: usize = 2;

pub struct PaddedRow {
    pub prefix: String,
    pub body: String,
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

pub fn line_with_right_pad(
    prefix: &str,
    body: &str,
    target: usize,
    prefix_style: Style,
    body_style: Style,
) -> Line<'static> {
    let used = crate::width::width(prefix) + crate::width::width(body);
    let fill = target.saturating_sub(used);
    let mut spans = vec![
        Span::styled(prefix.to_string(), prefix_style),
        Span::styled(body.to_string(), body_style),
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
//   [input slot: 5 rows]
//   1 pad row
//   sessions header + rows
//   1 pad row
//   hint line
const STARTUP_INPUT_SLOT_ROWS: u16 = 8;
const STARTUP_INPUT_SLOT_PAD: u16 = 1;
const STARTUP_INPUT_MAX_WIDTH: u16 = 72;
const STARTUP_BANNER: &[&str] = &[
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
}

const SESSION_CARD_TITLE_MAX: usize = 48;

pub fn compute_startup_overlay(
    area: ratatui::layout::Rect,
    recent: &[crate::app::StartupSessionEntry],
) -> StartupOverlayLayout {
    let banner_h = STARTUP_BANNER.len() as u16 + 2;
    let sessions_h: u16 = if recent.is_empty() {
        3
    } else {
        let n = recent.len() as u16;
        (2 + n * 2 + n.saturating_sub(1)).min(25)
    };
    let hint_h: u16 = 2;
    let total_h = banner_h
        + STARTUP_INPUT_SLOT_PAD
        + STARTUP_INPUT_SLOT_ROWS
        + STARTUP_INPUT_SLOT_PAD
        + sessions_h
        + hint_h;
    // Splash lane is narrower than the docked input; sessions cards line
    // up under this splash input, and the slide animates x/width/height
    // from here to compute_input_rect on dismiss.
    let input_docked = crate::layout::compute_input_rect(area, 1);
    let width = STARTUP_INPUT_MAX_WIDTH.min(input_docked.width);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + area.height.saturating_sub(total_h) / 2;
    let overlay = ratatui::layout::Rect {
        x,
        y,
        width,
        height: total_h.min(area.height),
    };
    let slot_y = overlay.y + banner_h + STARTUP_INPUT_SLOT_PAD;
    let input_slot = ratatui::layout::Rect {
        x: overlay.x,
        y: slot_y,
        width: overlay.width,
        height: STARTUP_INPUT_SLOT_ROWS,
    };
    let banner_rect = ratatui::layout::Rect {
        x: overlay.x,
        y: overlay.y + 1,
        width: overlay.width,
        height: STARTUP_BANNER.len() as u16 + 2,
    };
    StartupOverlayLayout {
        area: overlay,
        input_slot,
        overlay_width: overlay.width,
        banner_rect,
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
    let t = crate::theme::theme();
    let layout = compute_startup_overlay(transcript_area, recent);
    if progress >= 0.9 {
        return layout;
    }
    let (fg_banner, fg_subtle, fg_bold, extra_mod) = if progress < 0.33 {
        (
            t.accent.into(),
            t.subtle_fg.into(),
            t.tinted_fg.into(),
            Modifier::empty(),
        )
    } else if progress < 0.66 {
        (
            t.accent.into(),
            t.subtle_fg.into(),
            t.tinted_fg.into(),
            Modifier::DIM,
        )
    } else {
        (
            t.subtle_fg.into(),
            t.subtle_fg.into(),
            t.subtle_fg.into(),
            Modifier::DIM,
        )
    };
    let logo_style = Style::default()
        .fg(fg_banner)
        .add_modifier(Modifier::BOLD | extra_mod);
    let subtle = Style::default().fg(fg_subtle).add_modifier(extra_mod);
    let bold_plain = if fg_bold == t.tinted_fg.into() {
        Style::default().add_modifier(Modifier::BOLD | extra_mod)
    } else {
        Style::default()
            .fg(fg_bold)
            .add_modifier(Modifier::BOLD | extra_mod)
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(""));
    for row in STARTUP_BANNER {
        lines.push(Line::from(Span::styled((*row).to_string(), logo_style)).centered());
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            format!("atman witnesses; code exists · v{version}"),
            subtle,
        ))
        .centered(),
    );
    for _ in 0..STARTUP_INPUT_SLOT_PAD {
        lines.push(Line::from(""));
    }
    for _ in 0..STARTUP_INPUT_SLOT_ROWS {
        lines.push(Line::from(""));
    }
    for _ in 0..STARTUP_INPUT_SLOT_PAD {
        lines.push(Line::from(""));
    }
    if recent.is_empty() {
        lines.push(
            Line::from(Span::styled(
                "No previous sessions in this project yet.".to_string(),
                subtle,
            ))
            .centered(),
        );
    } else {
        lines.push(
            Line::from(Span::styled(
                "Recent sessions in this project".to_string(),
                bold_plain,
            ))
            .centered(),
        );
        lines.push(Line::from(""));
        let card_width = layout.area.width as usize;
        for (i, entry) in recent.iter().enumerate() {
            lines.extend(render_session_card(i + 1, entry, card_width, true));
            if i + 1 < recent.len() {
                lines.push(Line::from(""));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            "Type 1-9 to resume · start typing to begin a new session".to_string(),
            subtle,
        ))
        .centered(),
    );
    let para =
        ratatui::widgets::Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center);
    f.render_widget(para, layout.area);
    layout
}

pub fn render_startup_overlay(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    version: &str,
    recent: &[crate::app::StartupSessionEntry],
    dim: bool,
    reveal_count: usize,
) -> StartupOverlayLayout {
    let t = crate::theme::theme();
    let recent = &recent[..reveal_count.min(recent.len())];
    let layout = compute_startup_overlay(area, recent);
    f.render_widget(ratatui::widgets::Clear, area);
    let inner_area = area;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let logo_style = {
        let mut s = Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD);
        if dim {
            s = s.add_modifier(Modifier::DIM);
        }
        s
    };
    let subtle = {
        let mut s = Style::default().fg(t.subtle_fg.into());
        if dim {
            s = s.add_modifier(Modifier::DIM);
        }
        s
    };
    let bold_plain = {
        let mut s = Style::default().add_modifier(Modifier::BOLD);
        if dim {
            s = s.add_modifier(Modifier::DIM);
        }
        s
    };
    lines.push(Line::from(""));
    for row in STARTUP_BANNER {
        lines.push(Line::from(Span::styled((*row).to_string(), logo_style)).centered());
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            format!("atman witnesses; code exists · v{version}"),
            subtle,
        ))
        .centered(),
    );

    for _ in 0..STARTUP_INPUT_SLOT_PAD {
        lines.push(Line::from(""));
    }
    for _ in 0..STARTUP_INPUT_SLOT_ROWS {
        lines.push(Line::from(""));
    }
    for _ in 0..STARTUP_INPUT_SLOT_PAD {
        lines.push(Line::from(""));
    }

    if recent.is_empty() {
        lines.push(
            Line::from(Span::styled(
                "No previous sessions in this project yet.".to_string(),
                subtle,
            ))
            .centered(),
        );
    } else {
        lines.push(
            Line::from(Span::styled(
                "Recent sessions in this project".to_string(),
                bold_plain,
            ))
            .centered(),
        );
        lines.push(Line::from(""));
        let card_width = layout.area.width as usize;
        for (i, entry) in recent.iter().enumerate() {
            lines.extend(render_session_card(i + 1, entry, card_width, dim));
            if i + 1 < recent.len() {
                lines.push(Line::from(""));
            }
        }
    }
    lines.push(Line::from(""));
    lines.push(
        Line::from(Span::styled(
            "Type 1-9 to resume · start typing to begin a new session".to_string(),
            subtle,
        ))
        .centered(),
    );

    let para =
        ratatui::widgets::Paragraph::new(lines).alignment(ratatui::layout::Alignment::Center);
    // Paint into inner_area (inside the border of the actual passed-in
    // area), NOT into layout.area — the latter would re-center a fresh
    // rect inside `area`, which for a lerped animation frame means the
    // content stays anchored to the middle of the shrinking rect
    // instead of shrinking with it.
    f.render_widget(para, inner_area);
    layout
}

fn render_session_card(
    n: usize,
    entry: &crate::app::StartupSessionEntry,
    width: usize,
    dim: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = crate::markdown::block_bg();
    let mut extra = Modifier::empty();
    if dim {
        extra |= Modifier::DIM;
    }
    let bg_only = Style::default().bg(bg).add_modifier(extra);
    let index_style = Style::default()
        .fg(t.accent.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let title_style = Style::default().bg(bg).add_modifier(Modifier::BOLD | extra);
    let meta_style = Style::default()
        .fg(t.subtle_fg.into())
        .bg(bg)
        .add_modifier(extra);

    let title_source = entry.goal.as_deref().unwrap_or(&entry.short_id);
    let title = crate::width::truncate(title_source, SESSION_CARD_TITLE_MAX);
    let title_used = 4 + crate::width::width(title.as_str());
    let title_pad = width.saturating_sub(title_used);
    let title_line = Line::from(vec![
        Span::styled(" ".to_string(), bg_only),
        Span::styled(format!("{n} "), index_style),
        Span::styled(" ".to_string(), bg_only),
        Span::styled(title, title_style),
        Span::styled(" ".repeat(title_pad), bg_only),
    ]);

    let project = entry.project.as_deref().unwrap_or("no-project");
    let meta = format!(
        "{}  ·  {}  ·  {} events",
        entry.age_label, project, entry.event_count
    );
    let meta_used = 4 + crate::width::width(meta.as_str());
    let meta_pad = width.saturating_sub(meta_used);
    let meta_line = Line::from(vec![
        Span::styled("    ".to_string(), bg_only),
        Span::styled(meta, meta_style),
        Span::styled(" ".repeat(meta_pad), bg_only),
    ]);

    vec![title_line, meta_line]
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
    expanded: bool,
    hovered: bool,
    animation_frame: u32,
    panel_width: u16,
    retried: bool,
) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let bg = if hovered {
        match t.mode {
            crate::theme::ThemeMode::Dark => Color::Rgb(32, 34, 40),
            crate::theme::ThemeMode::Light => Color::Rgb(232, 232, 236),
        }
    } else {
        t.code_bg.into()
    };
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
        if retried { "↻" } else { "✓" }
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
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());
    let header_prefix = format!("  {glyph} {label} ");
    let header_used = crate::width::width(header_prefix.as_str());
    let header_pad = target.saturating_sub(header_used);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    let all_lines =
        crate::markdown::render_markdown_with_width(text, panel_width.saturating_sub(4));
    let max_lines = if expanded {
        all_lines.len()
    } else {
        6.min(all_lines.len())
    };
    for md_line in all_lines.iter().take(max_lines) {
        let content_w: usize = md_line
            .spans
            .iter()
            .map(|s| crate::width::width(s.content.as_ref()))
            .sum();
        let used = content_w + 4;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(md_line.spans.len() + 2);
        spans.push(Span::styled("    ", body_style));
        for src in &md_line.spans {
            let style = src.style.patch(body_style);
            spans.push(Span::styled(src.content.clone(), style));
        }
        if target > used {
            spans.push(Span::styled(" ".repeat(target - used), body_style));
        }
        lines.push(Line::from(spans));
    }
    if !expanded && all_lines.len() > max_lines {
        let hint = format!(
            "    ▼ {} more lines — click to expand",
            all_lines.len() - max_lines
        );
        let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && all_lines.len() > 6 {
        let hint = "    ▲ click to collapse".to_string();
        let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(blank);
    lines
}

fn render_assistant(
    md: &str,
    streaming: bool,
    retried: bool,
    panel_width: u16,
) -> Vec<Line<'static>> {
    let mut lines = crate::markdown::render_markdown_with_width(md, panel_width);
    if retried {
        let t = crate::theme::theme();
        let retry_style = Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::DIM);
        let mut header = vec![Span::styled(" ↻ retry".to_string(), retry_style)];
        let w = crate::width::width(" ↻ retry");
        let pad = (panel_width as usize).saturating_sub(w);
        if pad > 0 {
            header.push(Span::styled(" ".repeat(pad), retry_style));
        }
        lines.insert(0, Line::from(header));
    }
    if streaming {
        let cursor = Span::styled(
            "▏".to_string(),
            Style::default().add_modifier(Modifier::SLOW_BLINK),
        );
        if let Some(last) = lines.last_mut() {
            last.spans.push(cursor);
        } else {
            lines.push(Line::from(cursor));
        }
    }
    lines
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
    let first = format!(" {glyph} ");
    let rows = wrap_with_prefix(cleaned, target, &first, "   ");
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

fn render_user_turn(text: &str, panel_width: u16) -> Vec<Line<'static>> {
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
    let rows = wrap_with_prefix(text, target, " ❯ ", "   ");
    for row in rows {
        lines.push(line_with_right_pad(
            &row.prefix,
            &row.body,
            target,
            prompt_style,
            body_style,
        ));
    }
    lines.push(blank);
    lines
}

pub fn render_item(item: &OutputItem, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut lines = match item {
        OutputItem::UserTurn { text } => render_user_turn(text, ctx.panel_width),
        OutputItem::Thinking {
            text,
            done,
            expanded,
            retried,
        } => {
            let hovered = ctx.hovered_thinking_idx.is_some();
            render_thinking(
                text,
                *done,
                *expanded,
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
        OutputItem::SystemNote { text, level } => render_system_note(text, *level, ctx.panel_width),
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
            screen,
            accumulated_bytes,
            mode,
            done,
            expanded,
            scroll_offset: _,
        } => render_terminal(
            handle,
            screen,
            accumulated_bytes,
            *mode,
            *done,
            *expanded,
            ctx.animation_frame,
            ctx.panel_width,
        ),
        OutputItem::Bash {
            handle,
            output,
            done,
            expanded,
        } => render_bash(
            handle,
            output,
            *done,
            *expanded,
            ctx.animation_frame,
            ctx.panel_width,
        ),
        OutputItem::CompactionSummary {
            phase,
            range_start,
            range_end,
            summary,
            before_tokens,
            after_tokens,
            compacted_count,
            expanded,
        } => render_compaction_summary(CompactionSummaryRender {
            phase: *phase,
            range_start: *range_start,
            range_end: *range_end,
            summary,
            before_tokens: *before_tokens,
            after_tokens: *after_tokens,
            compacted_count: *compacted_count,
            expanded: *expanded,
            animation_frame: ctx.animation_frame,
            panel_width: ctx.panel_width,
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
        OutputItem::MermaidDiagram { source } => {
            render_mermaid_preview(source, ctx.panel_width, ctx.animation_frame)
        }
        OutputItem::SubAgentActivity {
            handle,
            status,
            output,
            iteration,
            done,
            expanded,
            ..
        } => render_sub_agent_activity(
            handle,
            status,
            output,
            *iteration,
            *done,
            *expanded,
            ctx.panel_width,
            ctx.animation_frame,
        ),
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
}

#[allow(clippy::too_many_arguments)]
fn render_sub_agent_activity(
    handle: &str,
    status: &str,
    output: &str,
    iteration: u64,
    done: bool,
    expanded: bool,
    panel_width: u16,
    animation_frame: u32,
) -> Vec<Line<'static>> {
    let glyph = match status {
        "ok" => "✓",
        "err" => "✗",
        "killed" => "⊘",
        _ if done => "✓",
        _ => spinner_char(animation_frame),
    };
    let iter_str = if done {
        String::new()
    } else {
        format!(" iter {iteration}")
    };
    let label = format!("agent[{handle}]{iter_str}");
    render_output_block(&label, glyph, output, expanded, panel_width)
}

fn render_mermaid_preview(
    source: &str,
    panel_width: u16,
    _animation_frame: u32,
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
    let gap = 1;

    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_prefix = "  ◇ mermaid ";
    let header_used = crate::width::width(header_prefix);
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
        hint_style.add_modifier(Modifier::BOLD),
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
        let pad = target.saturating_sub(line_w + 2);
        let mut spans = vec![Span::styled("  ", body_style)];
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
            "    ▼ {} more rows — click ⤢ to expand",
            total - max_preview
        );
        let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
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
    let header = format!("  ✎ {title}");
    let header_w = crate::width::width(header.as_str());
    let mut header_spans = vec![Span::styled(header, header_style)];
    if target > header_w {
        header_spans.push(Span::styled(" ".repeat(target - header_w), base_style));
    }
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());
    if let (Some(old), Some(new)) = (old_content, new_content) {
        let (body, total) = render_dual_diff_rows(title, old, new, expanded, target, bg);
        lines.extend(body);
        push_diff_fold_hint(&mut lines, expanded, total, 15, target, hint_style);
    } else if let Some(diff) = unified_diff {
        let (cells, lang) = parse_unified_diff_to_dual(diff);
        let total = cells.len();
        let first_change = cells.iter().position(|(l, r)| {
            !matches!(l.kind, DiffCellKind::Normal | DiffCellKind::Empty)
                || !matches!(r.kind, DiffCellKind::Normal | DiffCellKind::Empty)
        });
        let (body, _) = render_diff_cell_rows(&cells, &lang, expanded, target, bg, first_change);
        lines.extend(body);
        push_diff_fold_hint(&mut lines, expanded, total, 15, target, hint_style);
    }
    lines.push(blank);
    lines
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
        let hint = format!("    ▼ {} more lines — click to expand", total - folded);
        let pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), style));
        }
        lines.push(Line::from(spans));
    } else if expanded && total > folded {
        let hint = "    ▲ click to collapse".to_string();
        let pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), style));
        }
        lines.push(Line::from(spans));
    }
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
    Empty,
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
    let margin_w = 1usize;
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
        let fc = first_change.unwrap_or(0);
        let radius = 7usize;
        let start = fc.saturating_sub(radius).min(total.saturating_sub(15));
        let end = (start + 15).min(total);
        for (left, right) in rows[start..end].iter() {
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
            if lang.is_empty() {
                if let Some(ext) = line
                    .split('.')
                    .next_back()
                    .and_then(|s| s.split_whitespace().next())
                {
                    lang = ext_to_lang(ext).to_string();
                }
            }
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
            if let Some((os, ns)) = parse_hunk_header(line) {
                old_line = os;
                new_line = ns;
            }
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
            .fg(t.error.into())
            .bg(t.note_error_bg.into()),
        DiffCellKind::Insert => Style::default()
            .fg(t.success.into())
            .bg(t.note_success_bg.into()),
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
            && !matches!(cell.kind, DiffCellKind::Normal | DiffCellKind::Empty)
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

fn aggregate_llm_stats(
    nodes: &[atman_runtime::workflow::WorkflowNode],
) -> Option<(usize, u64, u64, u64, u64, u64, f64)> {
    let mut calls = 0usize;
    let mut total_in = 0u64;
    let mut total_out = 0u64;
    let mut total_cache_read = 0u64;
    let mut total_cache_write = 0u64;
    let mut total_ttft_ms = 0u64;
    let mut speed_sum = 0.0f64;
    let mut speed_count = 0usize;
    for n in nodes {
        if let Some(s) = &n.llm_stats {
            calls += 1;
            total_in += s.input_tokens + s.cache_read + s.cache_write;
            total_out += s.output_tokens;
            total_cache_read += s.cache_read;
            total_cache_write += s.cache_write;
            total_ttft_ms += s.ttft_ms;
            if s.tokens_per_second > 0.0 {
                speed_sum += s.tokens_per_second;
                speed_count += 1;
            }
        }
        let child = aggregate_llm_stats(&n.children);
        if let Some((c, i, o, cr, cw, ttft, sp)) = child {
            calls += c;
            total_in += i;
            total_out += o;
            total_cache_read += cr;
            total_cache_write += cw;
            total_ttft_ms += ttft;
            speed_sum += sp;
            speed_count += 1;
        }
    }
    if calls == 0 {
        return None;
    }
    let avg_speed = if speed_count > 0 {
        speed_sum / speed_count as f64
    } else {
        0.0
    };
    Some((
        calls,
        total_in,
        total_out,
        total_cache_read,
        total_cache_write,
        total_ttft_ms,
        avg_speed,
    ))
}

fn format_workflow_stats_footer(
    graph: &atman_runtime::workflow::WorkflowGraph,
    outer_width: u16,
    border_style: Style,
) -> Line<'static> {
    use atman_runtime::humanize::format_count;
    let stats = aggregate_llm_stats(&graph.root);
    let inner_w = (outer_width as usize).saturating_sub(2);
    let bottom_text =
        if let Some((calls, total_in, total_out, cache_read, _cache_write, _ttft, speed)) = stats {
            let mut parts = Vec::new();
            parts.push(format!("{calls} calls"));
            parts.push(format!("↑{}", format_count(total_in)));
            parts.push(format!("↓{}", format_count(total_out)));
            if cache_read > 0 {
                let hit_rate = if total_in > 0 {
                    (cache_read as f64 / total_in as f64 * 100.0) as u64
                } else {
                    0
                };
                parts.push(format!(
                    "cache {} ({}%)",
                    format_count(cache_read),
                    hit_rate
                ));
            }
            if speed > 0.0 {
                parts.push(format!("{:.0} tok/s", speed));
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
    let fill = inner_w.saturating_sub(crate::width::width(bottom_text.as_str()));
    let _ = fill;
    Line::from(Span::styled(bottom_text, border_style))
}

#[allow(clippy::too_many_arguments)]
fn render_workflow_panel(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    cancelled: bool,
    _started_at: std::time::Instant,
    _ended_at: Option<std::time::Instant>,
    animation_frame: u32,
    panel_width: u16,
) -> Vec<Line<'static>> {
    render_workflow_panel_with_regions(
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
    let t = crate::theme::theme();
    let count = count_workflow_nodes(&graph.root);
    let (mut status_str, mut status_style, running) = workflow_overall_status(&graph.root);
    if cancelled {
        status_str = "Cancelled".to_string();
        status_style = Style::default().fg(t.warn.into());
    }
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
            animation_frame,
            panel_width,
            running,
            max_body_rows,
        );
    }
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

fn collect_visible_nodes<'a>(
    nodes: &'a [atman_runtime::workflow::WorkflowNode],
    visible: &std::collections::HashSet<Vec<usize>>,
    path: &mut Vec<usize>,
    out: &mut Vec<(&'a atman_runtime::workflow::WorkflowNode, Vec<usize>)>,
) {
    for (i, n) in nodes.iter().enumerate() {
        path.push(i);
        if visible.contains(path) {
            out.push((n, path.clone()));
            collect_visible_nodes(&n.children, visible, path, out);
        }
        path.pop();
    }
}

fn render_collapsed_workflow_card(
    graph: &atman_runtime::workflow::WorkflowGraph,
    animation_frame: u32,
    panel_width: u16,
    running: bool,
    max_body_rows: usize,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let t = crate::theme::theme();
    let outer_width = panel_width.clamp(40, MAX_BOX_WIDTH);
    let border_style = Style::default().fg(t.accent.into());
    let mut stats = WorkflowStats::default();
    collect_stats(&graph.root, &mut stats);
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

    let sorted_root: Vec<atman_runtime::workflow::WorkflowNode> = {
        let mut r = graph.root.clone();
        r.sort_by_key(|n| n.started_at);
        r
    };
    let root = &sorted_root;

    let mut all_leaf_paths: Vec<Vec<usize>> = Vec::new();
    collect_all_leaves(root, &mut all_leaf_paths, &mut Vec::new());

    let mut leaves_with_time: Vec<(Vec<usize>, chrono::DateTime<chrono::Utc>)> = all_leaf_paths
        .iter()
        .map(|p| {
            let ts = leaf_at_path(root, p)
                .and_then(|n| n.started_at)
                .unwrap_or_else(chrono::Utc::now);
            (p.clone(), ts)
        })
        .collect();
    leaves_with_time.sort_by_key(|b| std::cmp::Reverse(b.1));

    let running_paths: Vec<Vec<usize>> = leaves_with_time
        .iter()
        .filter(|(p, _)| leaf_is_running(root, p))
        .map(|(p, _)| p.clone())
        .collect();

    let ordered_pool: Vec<Vec<usize>> = if !running_paths.is_empty() {
        running_paths
    } else {
        leaves_with_time.iter().map(|(p, _)| p.clone()).collect()
    };

    // `estimated_rows` only depends on how many distinct top-level nodes the
    // first `count` paths cover:
    //   top_level_count(count) = |{ path[0] : path ∈ ordered_pool[..count] }|
    // Monotonic non-decreasing in `count` (the visible set only grows), so we
    // precompute in O(N) and binary-search in O(log N), replacing the old
    // O(N²) loop that rebuilt `visible` + re-ran `collect_visible_nodes` each
    // iteration.
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
    let selected_paths: Vec<Vec<usize>> = ordered_pool.iter().take(target_count).cloned().collect();
    let mut visible: std::collections::HashSet<Vec<usize>> = std::collections::HashSet::new();
    for path in &selected_paths {
        for i in 1..=path.len() {
            visible.insert(path[..i].to_vec());
        }
    }
    let visible_str: std::collections::HashSet<String> = visible
        .iter()
        .map(|p| {
            p.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    let mut visible_nodes: Vec<(&atman_runtime::workflow::WorkflowNode, Vec<usize>)> = Vec::new();
    collect_visible_nodes(root, &visible, &mut Vec::new(), &mut visible_nodes);
    let top_level: Vec<&atman_runtime::workflow::WorkflowNode> = visible_nodes
        .iter()
        .filter(|(_, p)| p.len() == 1)
        .map(|(n, _)| *n)
        .collect();
    let mut body_lines: Vec<Line<'static>> = Vec::new();
    let mut regions: Vec<NodeRegion> = Vec::new();
    let mut pending_counter: u8 = 0;
    let child_count = top_level.len();
    for (i, node) in top_level.iter().enumerate() {
        let path = format!("{i}");
        let is_last = i + 1 == child_count;
        append_workflow_node_boxed(
            &mut body_lines,
            &mut regions,
            node,
            &std::collections::HashSet::new(),
            &[],
            is_last,
            outer_width,
            &path,
            animation_frame,
            running,
            &mut pending_counter,
            Some(&visible_str),
            1,
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
    let card_body_start_row = lines.len() as u32;
    for r in regions.iter_mut() {
        r.start_row = r.start_row.saturating_add(card_body_start_row);
        r.end_row = r.end_row.saturating_add(card_body_start_row);
    }
    lines.extend(body_lines);
    let bottom_line = format_workflow_stats_footer(graph, outer_width, border_style);
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
pub(crate) const MAX_COLLAPSED_BODY_ROWS: usize = 27;
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
            tool, args_preview, ..
        } => {
            let short_args = crate::width::truncate(args_preview, 30);
            if short_args.is_empty() {
                tool.to_string()
            } else {
                format!("{tool}({short_args})")
            }
        }
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("branch[{branch_index}]  {}", node.label)
        }
        _ => node.label.clone(),
    };
    let label = if let Some(stats) = &node.llm_stats {
        format!("{label}  · {}", format_llm_stats_brief(stats))
    } else {
        label
    };
    let mut approval_hotkey: Option<u8> = None;
    let mut auto_expand = false;
    if let Some(ApprovalState::Pending { .. }) = &node.approval {
        *pending_counter = pending_counter.saturating_add(1);
        if *pending_counter <= 9 {
            approval_hotkey = Some(*pending_counter);
        }
        border_style = Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::BOLD);
        auto_expand = true;
    } else if matches!(&node.approval, Some(ApprovalState::Denied { .. })) {
        border_style = Style::default().fg(t.error.into());
    }
    let is_expanded = auto_expand || expanded_nodes.contains(path);
    let mut inner_lines: Vec<Line<'static>> = Vec::new();
    if is_expanded {
        collect_boxed_details(node, &mut inner_lines);
    }
    let approval_seg = if approval_hotkey.is_some() { 5 } else { 0 };
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
            tool, args_preview, ..
        } => format!("{tool}({})", crate::width::truncate(args_preview, 60)),
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
                        .fg(t.warn.into())
                        .add_modifier(Modifier::BOLD),
                )),
                Some((
                    format!("  ⏸ waiting approval ({level})"),
                    Style::default().fg(t.warn.into()),
                )),
            )
        }
        Some(ApprovalState::Denied { reason }) => (
            None,
            Some((
                format!("  ⊘ denied: {reason}"),
                Style::default().fg(t.error.into()),
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
            Style::default().fg(t.subtle_fg.into()),
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
        NodeKind::Return => ("←", t.success.into()),
    }
}

fn format_llm_stats_brief(stats: &atman_runtime::workflow::LlmStats) -> String {
    use atman_runtime::humanize::format_count;
    let mut parts = Vec::new();
    if stats.cache_read > 0 {
        let total_in = stats.input_tokens + stats.cache_read + stats.cache_write;
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

fn render_output_block(
    label: &str,
    glyph: &str,
    output: &str,
    expanded: bool,
    panel_width: u16,
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

    let header_prefix = format!("  {glyph} {label} ");
    let header_used = crate::width::width(header_prefix.as_str());
    let fs_btn = "⤢";
    let fs_btn_used = crate::width::width(fs_btn);
    let gap = 1;
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
        hint_style.add_modifier(Modifier::BOLD),
    ));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    let all_lines: Vec<&str> = output.lines().collect();
    let max_lines = if expanded {
        all_lines.len()
    } else {
        all_lines.len().min(8)
    };
    let start = all_lines.len().saturating_sub(max_lines);
    for line in &all_lines[start..] {
        let rows = wrap_with_prefix(line, target, "    ", "    ");
        for row in rows {
            lines.push(line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                body_style,
                body_style,
            ));
        }
    }
    if !expanded && all_lines.len() > 8 {
        let hint = format!("    ▼ {} more lines — click to expand", all_lines.len() - 8);
        let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && all_lines.len() > 8 {
        let hint = "    ▲ click to collapse".to_string();
        let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(blank);
    lines
}

#[allow(clippy::too_many_arguments)]
fn render_bash(
    handle: &str,
    output: &str,
    done: bool,
    expanded: bool,
    animation_frame: u32,
    panel_width: u16,
) -> Vec<Line<'static>> {
    let glyph = if done {
        "✓"
    } else {
        spinner_char(animation_frame)
    };
    let label = if done {
        format!("bash[{handle}]")
    } else {
        format!("bash[{handle}]…")
    };
    render_output_block(&label, glyph, output, expanded, panel_width)
}

#[allow(clippy::too_many_arguments)]
fn render_terminal(
    handle: &str,
    screen: &atman_runtime::tools::term::TerminalScreen,
    accumulated_bytes: &[u8],
    mode: crate::app::TerminalViewMode,
    done: bool,
    expanded: bool,
    animation_frame: u32,
    panel_width: u16,
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
    let label = if done {
        format!("terminal[{handle}] {mode_label}")
    } else {
        format!("terminal[{handle}] {mode_label}…")
    };

    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_prefix = format!("  {glyph} {label} ");
    let header_used = crate::width::width(header_prefix.as_str());
    let dims = format!("{}×{}", screen.cols, screen.rows);
    let dims_used = crate::width::width(dims.as_str());
    let fs_btn = "⤢";
    let fs_btn_used = crate::width::width(fs_btn);
    let gap = 1;
    let header_pad = target
        .saturating_sub(header_used)
        .saturating_sub(dims_used)
        .saturating_sub(fs_btn_used)
        .saturating_sub(gap * 2);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    header_spans.push(Span::styled(dims, hint_style));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    header_spans.push(Span::styled(
        fs_btn.to_string(),
        hint_style.add_modifier(Modifier::BOLD),
    ));
    header_spans.push(Span::styled(" ".repeat(gap), header_style));
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    match mode {
        crate::app::TerminalViewMode::Capture => {
            let max_rows = if expanded {
                screen.rows as usize
            } else {
                (screen.rows as usize).min(12)
            };
            let cols = screen.cols as usize;
            for row in 0..max_rows.min(screen.rows as usize) {
                let mut spans: Vec<Span<'static>> = vec![Span::styled("    ", body_style)];
                let mut row_width = 0usize;
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
                    let cw = crate::width::width(chars);
                    row_width += cw;
                    spans.push(Span::styled(chars.to_string(), cs));
                }
                let pad = target
                    .saturating_sub(4)
                    .saturating_sub(row_width)
                    .saturating_add(RIGHT_PAD);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), body_style));
                }
                lines.push(Line::from(spans));
            }
            if !expanded && screen.rows as usize > 12 {
                let hint = "    ▼ click to expand";
                let hint_pad = target.saturating_sub(crate::width::width(hint));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
            } else if expanded && screen.rows as usize > 12 {
                let hint = "    ▲ click to collapse";
                let hint_pad = target.saturating_sub(crate::width::width(hint));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
            }
        }
        crate::app::TerminalViewMode::Stream => {
            let text = String::from_utf8_lossy(accumulated_bytes).into_owned();
            let all_lines: Vec<&str> = text.lines().collect();
            let max_lines = if expanded {
                all_lines.len()
            } else {
                all_lines.len().min(6)
            };
            let start = all_lines.len().saturating_sub(max_lines);
            for line in &all_lines[start..] {
                let rows = wrap_with_prefix(line, target, "    ", "    ");
                for row in rows {
                    lines.push(line_with_right_pad(
                        &row.prefix,
                        &row.body,
                        target,
                        body_style,
                        body_style,
                    ));
                }
            }
            if !expanded && all_lines.len() > 6 {
                let hint = format!("    ▼ {} more lines — click to expand", all_lines.len() - 6);
                let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
            } else if expanded && all_lines.len() > 6 {
                let hint = "    ▲ click to collapse".to_string();
                let hint_pad = target.saturating_sub(crate::width::width(hint.as_str()));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
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

    #[test]
    fn render_terminal_capture_produces_header_and_cells() {
        let scr = screen(2, 5, "hello");
        let lines = render_terminal(
            "term_s_0",
            &scr,
            &[],
            TerminalViewMode::Capture,
            false,
            false,
            0,
            80,
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
            &scr,
            bytes,
            TerminalViewMode::Stream,
            true,
            false,
            0,
            80,
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

    #[test]
    fn workflow_panel_hash_ignores_animation_frame_when_closed() {
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
        };
        let item = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: HashSet::new(),
            panel_expanded: false,
            started_at: Instant::now(),
            ended_at: Some(Instant::now()),
            cancelled: false,
        };
        let h1 = item_content_hash(&item, false, &HashSet::new(), Some(0));
        let h2 = item_content_hash(&item, false, &HashSet::new(), Some(999));
        assert_eq!(
            h1, h2,
            "animation_frame must not affect hash when panel is closed"
        );
    }

    #[test]
    fn user_turn_wraps_long_line_to_panel_width() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk";
        let lines = render_user_turn(text, 30);
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
        let lines = render_diff_side(&cell, 40, "", t.note_error_bg.into());
        assert_eq!(lines.len(), 1);
        let has_underline = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(has_underline, "changed delete chars should be underlined");

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
        let lines = render_diff_side(&cell, 40, "", t.note_success_bg.into());
        let has_bold = lines[0]
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD));
        assert!(has_bold, "changed insert chars should be bold");
    }

    #[test]
    fn unified_diff_pairs_delete_insert_on_same_row() {
        // unified diff with -old/+new should pair them on the same visual row,
        // not split into two separate rows
        let diff = "--- a/test.rs\n+++ b/test.rs\n@@ -1,3 +1,3 @@\n line one\n-old line two\n+new line two\n line three\n";
        let (cells, _lang) = parse_unified_diff_to_dual(diff);
        // Should be 3 rows: normal, (delete, insert) paired, normal
        assert_eq!(cells.len(), 3, "should have 3 paired rows");
        // Middle row should have both Delete (left) and Insert (right)
        let (left, right) = &cells[1];
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
        let lines = render_user_turn(text, 30);
        assert!(lines.len() > 3, "CJK long line should wrap");
        for (i, line) in lines.iter().enumerate() {
            let w = crate::width::width(plain_line(line).as_str());
            assert!(w <= 30, "CJK line {i} width {w} exceeds panel 30",);
        }
    }

    #[test]
    fn user_turn_preserves_explicit_newlines() {
        let text = "line one\nline two\nline three";
        let lines = render_user_turn(text, 60);
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
            OutputItem::UserTurn { text: "hi".into() },
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
        let lines = render_thinking(text, true, true, false, 0, 30, false);
        assert!(
            lines.len() > 6,
            "should wrap into many rows: {}",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = crate::width::width(s.as_str());
            assert!(w <= 30, "thinking line {i} width {w} > 30: {s:?}");
        }
    }

    #[test]
    fn thinking_wraps_cjk_long_line() {
        let text =
            "读取文件内容并做分析的一个非常长的中文标题名称这样会超过宽度必须换行才行测试一下看看";
        let lines = render_thinking(text, true, true, false, 0, 30, false);
        assert!(lines.len() > 6, "CJK thinking should wrap: {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = crate::width::width(s.as_str());
            assert!(w <= 30, "CJK thinking line {i} width {w} > 30");
        }
    }

    #[test]
    fn thinking_renders_markdown_bold() {
        // **bold** in thinking text should produce a BOLD span, not literal asterisks
        let lines = render_thinking("this is **bold** text", true, true, false, 0, 60, false);
        let has_bold = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.style.add_modifier == ratatui::style::Modifier::BOLD);
        assert!(has_bold, "thinking should render **bold** as BOLD style");
    }

    #[test]
    fn thinking_collapsed_limits_to_six_lines() {
        let text = "line1\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\nline10";
        let lines = render_thinking(text, true, false, false, 0, 60, false);
        // header(3) + 6 body lines + hint(1) + blank(1) = 11
        // (blank + header + blank + 6 body + hint + blank)
        let body_count = lines
            .iter()
            .filter(|l| {
                let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                s.starts_with("    line")
            })
            .count();
        assert_eq!(body_count, 6, "collapsed thinking should show 6 body lines");
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
        let lines = render_user_turn(text, 40);
        let body_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.as_ref().contains("short")))
            .expect("should find body line");
        let s: String = body_line.spans.iter().map(|s| s.content.as_ref()).collect();
        let w = crate::width::width(s.as_str());
        assert_eq!(w, 40, "line should fill to target 40: {s:?}");
        assert!(
            s.ends_with("  "),
            "line should end with >=2 trailing spaces (right pad): {s:?}"
        );
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
            build_lines_with_ranges(&items, 80, &RenderCtx::empty(), &mut Vec::new(), None);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].item_index, 0);
        assert_eq!(ranges[1].item_index, 1);
        assert!(ranges[0].end_row <= ranges[1].start_row);
        assert_eq!(total, ranges[1].end_row);
    }

    #[test]
    fn build_lines_with_ranges_empty_items_returns_empty_vecs() {
        let (lines, ranges, _regions, total) =
            build_lines_with_ranges(&[], 80, &RenderCtx::empty(), &mut Vec::new(), None);
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
            graph,
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
            graph,
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
            graph,
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
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, 0, 80, false, MAX_COLLAPSED_BODY_ROWS);
        let total = lines.len();
        assert!(
            total <= 30,
            "collapsed card should cap at ~30 rows, got {total}"
        );
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
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, 0, 80, false, MAX_COLLAPSED_BODY_ROWS);
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
        };
        let (lines, regions) =
            render_collapsed_workflow_card(&graph, 0, 80, false, MAX_COLLAPSED_BODY_ROWS);
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
        };
        let (lines, _regions) =
            render_collapsed_workflow_card(&graph, 0, 80, false, MAX_COLLAPSED_BODY_ROWS);
        let flat = flatten_lines(&lines);
        let old_pos = flat.find("old_tool").unwrap_or(usize::MAX);
        let new_pos = flat.find("new_tool").unwrap_or(0);
        assert!(new_pos > old_pos, "newest node should be below older node");
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
        expanded,
        animation_frame,
        panel_width,
    } = render;
    let t = crate::theme::theme();
    let bg: Color = t.code_bg.into();
    let header_style = Style::default()
        .fg(t.warn.into())
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(t.subtle_fg.into()).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg.into())
        .bg(bg)
        .add_modifier(Modifier::DIM);

    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let stats = match phase {
        CompactionPhase::Running => format!(
            " {} compacting {range_start}..{range_end}... ",
            spinner_char(animation_frame)
        ),
        CompactionPhase::Finished => {
            format!(
                " ✓ compacted {compacted_count} msgs · {before_tokens} → {after_tokens} tokens "
            )
        }
        CompactionPhase::Failed => {
            format!(" ✗ compaction failed · {summary} ")
        }
    };
    let stats_used = crate::width::width(stats.as_str());
    let stats_pad = target.saturating_sub(stats_used);
    let mut header_spans = vec![Span::styled(stats, header_style)];
    if stats_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(stats_pad), header_style));
    }
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    if matches!(phase, CompactionPhase::Running) {
        lines.push(line_with_right_pad(
            "  ",
            "summary generation in progress",
            target,
            body_style,
            body_style,
        ));
        lines.push(blank);
        return lines;
    }

    let rendered = crate::markdown::render_markdown_with_width(summary, panel_width);
    let total = rendered.len();
    let visible = if expanded { total } else { total.min(12) };
    for line in rendered.into_iter().take(visible) {
        let body = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");
        let rows = wrap_with_prefix(&body, target, "  ", "  ");
        for row in rows {
            lines.push(line_with_right_pad(
                &row.prefix,
                &row.body,
                target,
                body_style,
                body_style,
            ));
        }
    }
    if !expanded && total > visible {
        let hint = format!("  ▼ {} more lines — click to expand", total - visible);
        let pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && total > 12 {
        let hint = "  ▲ click to collapse".to_string();
        let pad = target.saturating_sub(crate::width::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), hint_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(blank);
    lines
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
    let title = format!(" ⚡ interjections · {} pending ", pending.len());
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
        let text = crate::width::truncate_plain(&inj.text, max_w.saturating_sub(6));
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(format!("[{level_label}]"), level_style),
            Span::styled(format!(" {text}"), Style::default().fg(t.tinted_fg.into())),
        ]));
    }
    lines
}
