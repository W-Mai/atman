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
    item_cache: &mut Vec<Option<ItemCacheEntry>>,
    animation_frame: Option<u32>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u16) {
    if item_cache.len() < items.len() {
        item_cache.resize(items.len(), None);
    }
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut node_regions: Vec<NodeRegion> = Vec::new();
    let mut cursor: u16 = 0;
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
            render_item_with_regions(item, &item_ctx)
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
        } => {
            1u8.hash(&mut h);
            str_fp(text).hash(&mut h);
            done.hash(&mut h);
            expanded.hash(&mut h);
            hovered.hash(&mut h);
            if !done {
                animation_frame.hash(&mut h);
            }
        }
        OutputItem::AssistantMd { md, streaming } => {
            2u8.hash(&mut h);
            str_fp(md).hash(&mut h);
            streaming.hash(&mut h);
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
        } => {
            5u8.hash(&mut h);
            turn_index.hash(&mut h);
            graph.root.len().hash(&mut h);
            expanded_nodes.len().hash(&mut h);
            panel_expanded.hash(&mut h);
            started_at.hash(&mut h);
            ended_at.hash(&mut h);
            animation_frame.hash(&mut h);
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
        let lines = render_item(item, ctx);
        let regions = if matches!(item, OutputItem::Terminal { .. } | OutputItem::Bash { .. })
            && lines.len() >= 2
        {
            let panel_width = ctx.panel_width as usize;
            vec![NodeRegion {
                panel_item_index: 0,
                path_key: TERMINAL_FULLSCREEN_KEY.to_string(),
                start_row: 1,
                end_row: 2,
                col_start: panel_width.saturating_sub(6) as u16,
                col_end: panel_width as u16,
            }]
        } else {
            Vec::new()
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
    total_rows: u16,
    item_cache: Vec<Option<ItemCacheEntry>>,
    cached_total_rows: u16,
    item_rows: Vec<u16>,
    cached_items_len: usize,
}

#[derive(Clone)]
pub struct ItemCacheEntry {
    content_hash: u64,
    lines: Arc<[Line<'static>]>,
    rows: u16,
    regions: Vec<NodeRegion>,
}

impl LayoutCache {
    pub fn get_or_build(
        &mut self,
        key: LayoutKey,
        items: &[OutputItem],
        ctx: &RenderCtx<'_>,
        scroll_offset: u16,
        viewport_rows: u16,
    ) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u16) {
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
            let (item_lines, item_regions) = render_item_with_regions(item, &item_ctx);
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
        let mut cursor: u16 = 0;
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
            let (item_lines, item_regions) = render_item_with_regions(item, &item_ctx);
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

    pub fn cached_total_rows(&self) -> u16 {
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
    crate::theme::theme().user_msg_bg
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
    use unicode_width::UnicodeWidthChar;
    let prefix_w = unicode_width::UnicodeWidthStr::width(cont_prefix);
    let body_w = target
        .saturating_sub(prefix_w)
        .saturating_sub(RIGHT_PAD)
        .max(1);
    let first_prefix_w = unicode_width::UnicodeWidthStr::width(first_prefix);
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
        for ch in row.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > limit && !cur.is_empty() {
                let prefix = if first_row { first_prefix } else { cont_prefix };
                out.push(PaddedRow {
                    prefix: prefix.to_string(),
                    body: std::mem::take(&mut cur),
                });
                first_row = false;
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
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
    use unicode_width::UnicodeWidthStr;
    let used = UnicodeWidthStr::width(prefix) + UnicodeWidthStr::width(body);
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
    let layout = compute_startup_overlay(transcript_area, recent);
    if progress >= 0.9 {
        return layout;
    }
    let (fg_banner, fg_subtle, fg_bold, extra_mod) = if progress < 0.33 {
        (
            Color::Cyan,
            Color::DarkGray,
            Color::Reset,
            Modifier::empty(),
        )
    } else if progress < 0.66 {
        (Color::Cyan, Color::DarkGray, Color::Reset, Modifier::DIM)
    } else {
        (
            Color::DarkGray,
            Color::DarkGray,
            Color::DarkGray,
            Modifier::DIM,
        )
    };
    let logo_style = Style::default()
        .fg(fg_banner)
        .add_modifier(Modifier::BOLD | extra_mod);
    let subtle = Style::default().fg(fg_subtle).add_modifier(extra_mod);
    let bold_plain = if fg_bold == Color::Reset {
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
            format!("agentic coding in your terminal · v{version}"),
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
    let recent = &recent[..reveal_count.min(recent.len())];
    let layout = compute_startup_overlay(area, recent);
    f.render_widget(ratatui::widgets::Clear, area);
    let inner_area = area;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let logo_style = {
        let mut s = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        if dim {
            s = s.add_modifier(Modifier::DIM);
        }
        s
    };
    let subtle = {
        let mut s = Style::default().fg(Color::DarkGray);
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
            format!("agentic coding in your terminal · v{version}"),
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
    use unicode_width::UnicodeWidthStr;
    let bg = crate::markdown::block_bg();
    let mut extra = Modifier::empty();
    if dim {
        extra |= Modifier::DIM;
    }
    let bg_only = Style::default().bg(bg).add_modifier(extra);
    let index_style = Style::default()
        .fg(Color::Cyan)
        .bg(bg)
        .add_modifier(Modifier::BOLD | extra);
    let title_style = Style::default().bg(bg).add_modifier(Modifier::BOLD | extra);
    let meta_style = Style::default()
        .fg(Color::DarkGray)
        .bg(bg)
        .add_modifier(extra);

    let title_source = entry.goal.as_deref().unwrap_or(&entry.short_id);
    let title = clamp_len(title_source, SESSION_CARD_TITLE_MAX);
    let title_used = 4 + UnicodeWidthStr::width(title.as_str());
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
    let meta_used = 4 + UnicodeWidthStr::width(meta.as_str());
    let meta_pad = width.saturating_sub(meta_used);
    let meta_line = Line::from(vec![
        Span::styled("    ".to_string(), bg_only),
        Span::styled(meta, meta_style),
        Span::styled(" ".repeat(meta_pad), bg_only),
    ]);

    vec![title_line, meta_line]
}

fn clamp_len(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn make_dashed_divider(panel_width: u16) -> Vec<Line<'static>> {
    let side_gap = 4u16;
    let dash_width = panel_width.saturating_sub(side_gap * 2).max(4) as usize;
    let pad = " ".repeat(side_gap as usize);
    let dash_style = Style::default()
        .fg(Color::DarkGray)
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
) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let t = crate::theme::theme();
    let bg = if hovered {
        match t.mode {
            crate::theme::ThemeMode::Dark => Color::Rgb(32, 34, 40),
            crate::theme::ThemeMode::Light => Color::Rgb(232, 232, 236),
        }
    } else {
        t.code_bg
    };
    let header_style = Style::default()
        .fg(t.subtle_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let body_style = Style::default().fg(t.subtle_fg).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let glyph = if done {
        "✓"
    } else {
        spinner_char(animation_frame)
    };
    let label = if done { "thinking" } else { "thinking…" };
    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());
    let header_prefix = format!("  {glyph} {label} ");
    let header_used = UnicodeWidthStr::width(header_prefix.as_str());
    let header_pad = target.saturating_sub(header_used);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
    lines.push(Line::from(header_spans));
    lines.push(blank.clone());

    let all_lines: Vec<&str> = text.lines().collect();
    let max_lines = if expanded {
        all_lines.len()
    } else {
        6.min(all_lines.len())
    };
    for line in all_lines.iter().take(max_lines) {
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
    if !expanded && all_lines.len() > max_lines {
        let hint = format!(
            "    ▼ {} more lines — click to expand",
            all_lines.len() - max_lines
        );
        let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && all_lines.len() > 6 {
        let hint = "    ▲ click to collapse".to_string();
        let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(blank);
    lines
}

fn render_assistant(md: &str, streaming: bool, panel_width: u16) -> Vec<Line<'static>> {
    let mut lines = crate::markdown::render_markdown_with_width(md, panel_width);
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
        NoteLevel::Info => ("·", Color::Cyan, t.note_info_bg),
        NoteLevel::Warn => ("!", Color::Yellow, t.note_warn_bg),
        NoteLevel::Error => ("✗", Color::Red, t.note_error_bg),
    };
    let cleaned = text
        .strip_prefix("[atman] ")
        .or_else(|| text.strip_prefix("[atman]"))
        .unwrap_or(text);
    let body_style = Style::default().fg(t.tinted_fg).bg(bg);
    let glyph_style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
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
    let bg = user_message_bg();
    let prompt_style = Style::default()
        .fg(Color::Cyan)
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
        } => {
            let hovered = ctx.hovered_thinking_idx.is_some();
            render_thinking(
                text,
                *done,
                *expanded,
                hovered,
                ctx.animation_frame,
                ctx.panel_width,
            )
        }
        OutputItem::StartupCard { .. } => Vec::new(),
        OutputItem::AssistantMd { md, streaming } => {
            render_assistant(md, *streaming, ctx.panel_width)
        }
        OutputItem::SystemNote { text, level } => render_system_note(text, *level, ctx.panel_width),
        OutputItem::Divider => make_dashed_divider(ctx.panel_width),
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
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
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
    use unicode_width::UnicodeWidthStr;
    let t = crate::theme::theme();
    let bg = t.code_bg;
    let target = panel_width.max(20) as usize;
    let base_style = Style::default().bg(bg);
    let header_style = Style::default()
        .fg(Color::Cyan)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let hint_style = Style::default()
        .fg(t.meta_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let blank = Line::from(Span::styled(" ".repeat(target), base_style));
    let mut lines = vec![blank.clone()];
    let header = format!("  ✎ {title}");
    let header_w = UnicodeWidthStr::width(header.as_str());
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
    use unicode_width::UnicodeWidthStr;
    if !expanded && total > folded {
        let hint = format!("    ▼ {} more lines — click to expand", total - folded);
        let pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), style));
        }
        lines.push(Line::from(spans));
    } else if expanded && total > folded {
        let hint = "    ▲ click to collapse".to_string();
        let pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
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
    let total = rows.len();
    let sep = " │ ";
    let sep_w = 3usize;
    let left_w = target.saturating_sub(sep_w) / 2;
    let right_w = target.saturating_sub(sep_w).saturating_sub(left_w);
    let sep_style = Style::default().fg(Color::DarkGray).bg(bg);
    let mut out;
    if expanded || total <= 15 {
        out = Vec::with_capacity(total);
        for (left, right) in rows {
            let mut spans = render_diff_side(left, left_w, lang, bg);
            spans.push(Span::styled(sep.to_string(), sep_style));
            spans.extend(render_diff_side(right, right_w, lang, bg));
            out.push(Line::from(spans));
        }
    } else {
        // Collapsed: center window around first change.
        let fc = first_change.unwrap_or(0);
        let radius = 7usize;
        let start = fc.saturating_sub(radius).min(total.saturating_sub(15));
        let end = (start + 15).min(total);
        out = Vec::with_capacity(15);
        for (left, right) in rows[start..end].iter() {
            let mut spans = render_diff_side(left, left_w, lang, bg);
            spans.push(Span::styled(sep.to_string(), sep_style));
            spans.extend(render_diff_side(right, right_w, lang, bg));
            out.push(Line::from(spans));
        }
    }
    (out, total)
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
                for i in 0..len {
                    rows.push((
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
                    ));
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
                },
                DiffCell {
                    line_no: Some(new_line),
                    text: text.to_string(),
                    kind: DiffCellKind::Normal,
                },
            ));
            old_line += 1;
            new_line += 1;
        } else if line.starts_with('-') {
            rows.push((
                DiffCell {
                    line_no: Some(old_line),
                    text: line.strip_prefix('-').unwrap_or(line).to_string(),
                    kind: DiffCellKind::Delete,
                },
                empty_cell(),
            ));
            old_line += 1;
        } else if line.starts_with('+') {
            rows.push((
                empty_cell(),
                DiffCell {
                    line_no: Some(new_line),
                    text: line.strip_prefix('+').unwrap_or(line).to_string(),
                    kind: DiffCellKind::Insert,
                },
            ));
            new_line += 1;
        }
    }
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
    }
}

fn empty_cell() -> DiffCell {
    DiffCell {
        line_no: None,
        text: String::new(),
        kind: DiffCellKind::Empty,
    }
}

fn render_diff_side(cell: &DiffCell, width: usize, lang: &str, bg: Color) -> Vec<Span<'static>> {
    let mark_style = match cell.kind {
        DiffCellKind::Delete => Style::default().fg(Color::Red).bg(Color::Rgb(62, 30, 34)),
        DiffCellKind::Insert => Style::default().fg(Color::Green).bg(Color::Rgb(28, 56, 36)),
        DiffCellKind::Normal | DiffCellKind::Empty => Style::default().bg(bg),
    };
    let prefix = match cell.line_no {
        Some(n) => format!("{n:>4} "),
        None => "     ".to_string(),
    };
    let mut spans = vec![Span::styled(prefix, mark_style)];
    let body_w = width.saturating_sub(5);
    let highlighted = crate::highlight::highlight_code(lang, &cell.text);
    if let Some(line) = highlighted.into_iter().next() {
        let mut body = truncate_spans_with_bg(line.spans, body_w, mark_style.bg.unwrap_or(bg));
        if !matches!(cell.kind, DiffCellKind::Normal | DiffCellKind::Empty) {
            for span in &mut body {
                span.style.fg = mark_style.fg.or(span.style.fg);
            }
        }
        spans.extend(body);
    }
    pad_spans_to_width(&mut spans, width, mark_style);
    spans
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

fn truncate_spans_with_bg(
    spans: Vec<Span<'static>>,
    max_w: usize,
    bg: Color,
) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > max_w {
                break;
            }
            text.push(ch);
            used += w;
        }
        if !text.is_empty() {
            let mut style = span.style;
            style.bg = style.bg.or(Some(bg));
            out.push(Span::styled(text, style));
        }
        if used >= max_w {
            break;
        }
    }
    out
}

fn pad_spans_to_width(spans: &mut Vec<Span<'static>>, width: usize, style: Style) {
    use unicode_width::UnicodeWidthStr;
    let used: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
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
            let body_w = unicode_width::UnicodeWidthStr::width(body.as_str());
            let inner_w = (outer_width as usize).saturating_sub(2);
            let prefix_w = unicode_width::UnicodeWidthStr::width("╰─ ");
            let suffix_w = 1; // ╯
            let dash_w = inner_w
                .saturating_sub(prefix_w)
                .saturating_sub(body_w)
                .saturating_sub(suffix_w);
            format!("╰─ {body}{}╯", "─".repeat(dash_w))
        } else {
            format!("╰{}╯", "─".repeat((outer_width as usize).saturating_sub(2)))
        };
    let fill = inner_w.saturating_sub(unicode_width::UnicodeWidthStr::width(bottom_text.as_str()));
    let _ = fill;
    Line::from(Span::styled(bottom_text, border_style))
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

pub fn render_workflow_panel_with_regions(
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
            atman_runtime::humanize::format_secs(elapsed)
        )),
        Span::styled(status_str, status_style),
    ]);
    if !panel_expanded {
        return render_collapsed_workflow_card(graph, animation_frame, panel_width, running);
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
            if tool == "agent.spawn" {
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
    let mut cur = nodes;
    let mut node = None;
    for &i in path {
        node = cur.get(i);
        if let Some(n) = node {
            cur = &n.children;
        } else {
            return false;
        }
    }
    matches!(
        node.map(|n| n.status),
        Some(NodeStatus::Running | NodeStatus::Pending)
    )
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
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    use unicode_width::UnicodeWidthStr;
    let outer_width = panel_width.clamp(40, MAX_BOX_WIDTH);
    let border_style = Style::default().fg(Color::Cyan);
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
    let button_w = UnicodeWidthStr::width(button_text) as u16;
    let title_w = UnicodeWidthStr::width(title.as_str());
    let stats_w = UnicodeWidthStr::width(stats_text.as_str());
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
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" · "),
        Span::styled(stats_text, Style::default().fg(Color::Gray)),
    ];
    if fill_w > 0 {
        top_spans.push(Span::styled("─".repeat(fill_w), border_style));
    }
    let button_col_end = outer_width;
    let button_col_start = button_col_end.saturating_sub(button_w).saturating_sub(2);
    top_spans.push(Span::styled(
        button_text.to_string(),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ));
    top_spans.push(Span::styled("─╮".to_string(), border_style));
    let mut lines: Vec<Line<'static>> = vec![Line::from(top_spans)];

    let mut all_leaf_paths: Vec<Vec<usize>> = Vec::new();
    collect_all_leaves(&graph.root, &mut all_leaf_paths, &mut Vec::new());
    let running_paths: Vec<Vec<usize>> = all_leaf_paths
        .iter()
        .filter(|p| leaf_is_running(&graph.root, p))
        .cloned()
        .collect();
    let selected_paths: Vec<Vec<usize>> = if running_paths.is_empty() {
        all_leaf_paths.iter().rev().take(3).rev().cloned().collect()
    } else if running_paths.len() > 3 {
        running_paths.iter().rev().take(3).rev().cloned().collect()
    } else {
        running_paths
    };
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
    collect_visible_nodes(&graph.root, &visible, &mut Vec::new(), &mut visible_nodes);
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
        );
    }
    apply_lens_fade(&mut body_lines);
    let card_body_start_row = lines.len() as u16;
    for r in regions.iter_mut() {
        r.start_row = r.start_row.saturating_add(card_body_start_row);
        r.end_row = r.end_row.saturating_add(card_body_start_row);
    }
    lines.extend(body_lines);
    let bottom_line = format_workflow_stats_footer(graph, outer_width, border_style);
    lines.push(bottom_line);
    lines.push(Line::raw(""));
    let card_rows = lines.len() as u16;
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

const MAX_BOX_WIDTH: u16 = crate::layout::CONTENT_MAX_WIDTH;
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
    visible_paths: Option<&std::collections::HashSet<String>>,
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
        } => {
            let short_args = truncate_preview(args_preview, 30);
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
    let child_prefix_w = child_ancestor_last.len() as u16 * INDENT_PER_DEPTH;
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
) {
    let branch_count = branches.len();
    let prefix_w = ancestor_last.len() as u16 * INDENT_PER_DEPTH;
    let col_width = panel_width
        .saturating_sub(prefix_w)
        .saturating_div(branch_count as u16);
    let start_row_before = out.len() as u16;
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
                    for ch in content.chars() {
                        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                        if used + w > col_width.saturating_sub(written) {
                            break;
                        }
                        taken.push(ch);
                        used += w;
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
    Paragraph::new("plain text → agent · :help for builtins · Ctrl+C to interrupt")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
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
    use unicode_width::UnicodeWidthStr;
    let t = crate::theme::theme();
    let bg = t.code_bg;
    let header_style = Style::default()
        .fg(t.subtle_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let body_style = Style::default().fg(t.subtle_fg).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);

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

    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let header_prefix = format!("  {glyph} {label} ");
    let header_used = UnicodeWidthStr::width(header_prefix.as_str());
    let header_pad = target.saturating_sub(header_used);
    let mut header_spans = vec![Span::styled(header_prefix, header_style)];
    if header_pad > 0 {
        header_spans.push(Span::styled(" ".repeat(header_pad), header_style));
    }
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
        let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if hint_pad > 0 {
            spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && all_lines.len() > 8 {
        let hint = "    ▲ click to collapse".to_string();
        let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
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
    use unicode_width::UnicodeWidthStr;
    let t = crate::theme::theme();
    let bg = t.code_bg;
    let header_style = Style::default()
        .fg(t.subtle_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);
    let body_style = Style::default().fg(t.subtle_fg).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg)
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
    let header_used = UnicodeWidthStr::width(header_prefix.as_str());
    let dims = format!("{}×{}", screen.cols, screen.rows);
    let dims_used = UnicodeWidthStr::width(dims.as_str());
    let fs_btn = "⤢";
    let fs_btn_used = UnicodeWidthStr::width(fs_btn);
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
                    let cs = cell_style_for_viewer(cell, bg);
                    let chars = if cell.chars.is_empty() {
                        " "
                    } else {
                        &cell.chars
                    };
                    let cw = UnicodeWidthStr::width(chars);
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
                let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
            } else if expanded && screen.rows as usize > 12 {
                let hint = "    ▲ click to collapse";
                let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint));
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
                let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
                let mut spans = vec![Span::styled(hint, hint_style)];
                if hint_pad > 0 {
                    spans.push(Span::styled(" ".repeat(hint_pad), hint_style));
                }
                lines.push(Line::from(spans));
            } else if expanded && all_lines.len() > 6 {
                let hint = "    ▲ click to collapse".to_string();
                let hint_pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
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
        TerminalColor::Default => crate::theme::theme().subtle_fg,
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
    fn user_turn_wraps_long_line_to_panel_width() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk";
        let lines = render_user_turn(text, 30);
        assert!(lines.len() > 3, "should wrap into multiple rows");
        for (i, line) in lines.iter().enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(plain_line(line).as_str());
            assert!(
                w <= 30,
                "line {i} width {w} exceeds panel 30: {:?}",
                plain_line(line)
            );
        }
    }

    #[test]
    fn user_turn_wraps_cjk_long_line() {
        let text =
            "读取文件内容并做分析的一个非常长的中文标题名称这样会超过宽度必须换行才行测试一下";
        let lines = render_user_turn(text, 30);
        assert!(lines.len() > 3, "CJK long line should wrap");
        for (i, line) in lines.iter().enumerate() {
            let w = unicode_width::UnicodeWidthStr::width(plain_line(line).as_str());
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
    fn thinking_wraps_long_line() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk lllll";
        let lines = render_thinking(text, true, true, false, 0, 30);
        assert!(
            lines.len() > 6,
            "should wrap into many rows: {}",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = unicode_width::UnicodeWidthStr::width(s.as_str());
            assert!(w <= 30, "thinking line {i} width {w} > 30: {s:?}");
        }
    }

    #[test]
    fn thinking_wraps_cjk_long_line() {
        let text =
            "读取文件内容并做分析的一个非常长的中文标题名称这样会超过宽度必须换行才行测试一下看看";
        let lines = render_thinking(text, true, true, false, 0, 30);
        assert!(lines.len() > 6, "CJK thinking should wrap: {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = unicode_width::UnicodeWidthStr::width(s.as_str());
            assert!(w <= 30, "CJK thinking line {i} width {w} > 30");
        }
    }

    #[test]
    fn system_note_wraps_long_line() {
        let text = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh iiiii jjjjj kkkkk lllll mmmmm";
        let lines = render_system_note(text, NoteLevel::Info, 30);
        assert!(lines.len() > 4, "should wrap: {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let s: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            let w = unicode_width::UnicodeWidthStr::width(s.as_str());
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
        let w = unicode_width::UnicodeWidthStr::width(s.as_str());
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
    use unicode_width::UnicodeWidthStr;
    let t = crate::theme::theme();
    let bg = t.code_bg;
    let header_style = Style::default()
        .fg(Color::Yellow)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(t.subtle_fg).bg(bg);
    let hint_style = Style::default()
        .fg(t.meta_fg)
        .bg(bg)
        .add_modifier(Modifier::DIM);

    let target = panel_width.max(20) as usize;
    let blank = Line::from(Span::styled(" ".repeat(target), body_style));
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(blank.clone());

    let stats = match phase {
        CompactionPhase::Running => format!(
            " {} 正在压缩 {range_start}..{range_end} 条消息... ",
            spinner_char(animation_frame)
        ),
        CompactionPhase::Finished => {
            format!(" ✓ 已压缩 {compacted_count} 条消息 · {before_tokens} → {after_tokens} tokens ")
        }
    };
    let stats_used = UnicodeWidthStr::width(stats.as_str());
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
        let pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), hint_style));
        }
        lines.push(Line::from(spans));
    } else if expanded && total > 12 {
        let hint = "  ▲ click to collapse".to_string();
        let pad = target.saturating_sub(UnicodeWidthStr::width(hint.as_str()));
        let mut spans = vec![Span::styled(hint, hint_style)];
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), hint_style));
        }
        lines.push(Line::from(spans));
    }
    lines.push(blank);
    lines
}
