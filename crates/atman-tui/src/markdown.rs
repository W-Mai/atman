use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const STREAM_REFRESH_MIN: Duration = Duration::from_millis(50);
const STREAM_REFRESH_MAX: Duration = Duration::from_millis(750);
const STREAM_TAIL_SOFT_LIMIT: usize = 32 * 1024;

fn stream_refresh_interval(
    working_source_len: usize,
    exact_full_mode: bool,
    render_cost: Duration,
) -> Duration {
    if working_source_len > STREAM_TAIL_SOFT_LIMIT || exact_full_mode {
        render_cost
            .saturating_mul(8)
            .clamp(STREAM_REFRESH_MIN, STREAM_REFRESH_MAX)
    } else {
        STREAM_REFRESH_MIN
    }
}

pub fn render_markdown(md: &str) -> Vec<Line<'static>> {
    render_markdown_with_width(md, 60)
}

pub fn render_markdown_with_width(md: &str, rule_width: u16) -> Vec<Line<'static>> {
    let mut renderer = Renderer::with_rule_width(rule_width);
    for segment in split_display_math_segments(md).segments {
        match segment {
            MarkdownSegment::Text { text, .. } => {
                for ev in Parser::new_ext(text, markdown_options()) {
                    renderer.consume(ev);
                }
            }
            MarkdownSegment::DisplayMath { tex, .. } => renderer.render_display_math(&tex),
        }
    }
    renderer.finish()
}

fn markdown_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_MATH);
    options
}

enum MarkdownSegment<'a> {
    Text { text: &'a str, offset: usize },
    DisplayMath { tex: String, range: Range<usize> },
}

struct MarkdownSegments<'a> {
    segments: Vec<MarkdownSegment<'a>>,
    unclosed_display_math_start: Option<usize>,
}

fn split_display_math_segments(md: &str) -> MarkdownSegments<'_> {
    let mut offset = 0usize;
    let lines = md
        .split_inclusive('\n')
        .map(|line| {
            let start = offset;
            offset = offset.saturating_add(line.len());
            (start, line)
        })
        .collect::<Vec<_>>();
    let mut segments = Vec::new();
    let mut text_start = 0usize;
    let mut in_code_fence = false;
    let mut index = 0;
    let mut unclosed_display_math_start = None;

    while index < lines.len() {
        let (line_start, line) = lines[index];
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_fence = !in_code_fence;
        }

        if !in_code_fence && trimmed == "$$" {
            let Some(close_offset) = lines[index + 1..]
                .iter()
                .position(|(_, candidate)| candidate.trim() == "$$")
            else {
                unclosed_display_math_start = Some(line_start);
                break;
            };
            let close = index + 1 + close_offset;
            if text_start < line_start {
                segments.push(MarkdownSegment::Text {
                    text: &md[text_start..line_start],
                    offset: text_start,
                });
            }
            let tex = lines[index + 1..close]
                .iter()
                .map(|(_, line)| line.trim())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let close_end = lines[close].0.saturating_add(lines[close].1.len());
            segments.push(MarkdownSegment::DisplayMath {
                tex,
                range: line_start..close_end,
            });
            text_start = close_end;
            index = close + 1;
            continue;
        }

        index += 1;
    }

    if text_start < md.len() {
        segments.push(MarkdownSegment::Text {
            text: &md[text_start..],
            offset: text_start,
        });
    }
    MarkdownSegments {
        segments,
        unclosed_display_math_start,
    }
}

enum ParsedRenderEvent<'a> {
    Common {
        event: Event<'a>,
        range: Range<usize>,
    },
    DisplayMath {
        tex: String,
        range: Range<usize>,
    },
}

impl ParsedRenderEvent<'_> {
    fn start(&self) -> usize {
        match self {
            Self::Common { range, .. } | Self::DisplayMath { range, .. } => range.start,
        }
    }
}

struct ParsedMarkdown<'a> {
    events: Vec<ParsedRenderEvent<'a>>,
    top_level_starts: Vec<usize>,
    has_reference_definitions: bool,
}

fn source_line_start(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .rfind('\n')
        .map_or(0, |newline| newline.saturating_add(1))
}

fn parse_markdown_for_projection(source: &str) -> ParsedMarkdown<'_> {
    let mut events = Vec::new();
    let mut top_level_starts = Vec::new();
    let mut has_reference_definitions = false;
    let split = split_display_math_segments(source);

    for segment in split.segments {
        match segment {
            MarkdownSegment::Text { text, offset } => {
                let parser = Parser::new_ext(text, markdown_options());
                has_reference_definitions |= parser.reference_definitions().iter().next().is_some();
                let mut depth = 0usize;
                for (event, local_range) in parser.into_offset_iter() {
                    let range = local_range.start.saturating_add(offset)
                        ..local_range.end.saturating_add(offset);
                    match &event {
                        Event::Start(_) => {
                            if depth == 0 {
                                top_level_starts.push(source_line_start(source, range.start));
                            }
                            depth = depth.saturating_add(1);
                        }
                        Event::End(_) => {
                            depth = depth.saturating_sub(1);
                        }
                        _ if depth == 0 => {
                            top_level_starts.push(source_line_start(source, range.start));
                        }
                        _ => {}
                    }
                    events.push(ParsedRenderEvent::Common { event, range });
                }
            }
            MarkdownSegment::DisplayMath { tex, range } => {
                top_level_starts.push(range.start);
                events.push(ParsedRenderEvent::DisplayMath { tex, range });
            }
        }
    }

    if let Some(open_math) = split.unclosed_display_math_start {
        top_level_starts.retain(|start| *start < open_math);
        top_level_starts.push(open_math);
    }
    top_level_starts.dedup();
    ParsedMarkdown {
        events,
        top_level_starts,
        has_reference_definitions,
    }
}

struct ProjectionRender {
    stable_lines: Vec<Line<'static>>,
    tail_lines: Vec<Line<'static>>,
    stable_boundary_fresh_line: bool,
    committed_source_bytes: Option<usize>,
}

fn projection_cut(top_level_starts: &[usize]) -> Option<usize> {
    top_level_starts
        .len()
        .checked_sub(2)
        .and_then(|index| top_level_starts.get(index).copied())
}

fn render_projection_suffix(
    parsed: ParsedMarkdown<'_>,
    rule_width: u16,
    initial_fresh_line: bool,
) -> ProjectionRender {
    let cut = projection_cut(&parsed.top_level_starts);
    let mut renderer = Renderer::with_boundary(rule_width, initial_fresh_line);
    let mut stable_lines = Vec::new();
    let mut stable_boundary_fresh_line = initial_fresh_line;
    let mut captured = false;
    let mut committed_source_bytes = None;

    for event in parsed.events {
        if !captured && let Some(cut) = cut.filter(|cut| event.start() >= *cut) {
            debug_assert!(renderer.at_top_level_boundary());
            stable_lines = std::mem::take(&mut renderer.lines);
            stable_boundary_fresh_line = renderer.fresh_line;
            captured = true;
            committed_source_bytes = Some(cut);
        }
        match event {
            ParsedRenderEvent::Common { event, .. } => renderer.consume(event),
            ParsedRenderEvent::DisplayMath { tex, .. } => renderer.render_display_math(&tex),
        }
    }

    let mut tail_lines = renderer.finish();
    if captured && tail_lines.is_empty() {
        while stable_lines
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.is_empty()))
        {
            stable_lines.pop();
        }
        tail_lines = stable_lines;
        stable_lines = Vec::new();
        committed_source_bytes = None;
    }

    ProjectionRender {
        stable_lines,
        tail_lines,
        stable_boundary_fresh_line,
        committed_source_bytes,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamingProjectionUpdate {
    Rendered,
    Deferred,
}

#[derive(Clone)]
pub(crate) struct StreamingMarkdownProjection {
    source_generation: u64,
    observed_source_len: usize,
    stable_source_end: usize,
    stable_boundary_fresh_line: bool,
    stable_segments: Vec<Arc<[Line<'static>]>>,
    stable_row_ends: Vec<usize>,
    tail_lines: Arc<[Line<'static>]>,
    exact_full_mode: bool,
    next_refresh_at: Option<Instant>,
    #[cfg(test)]
    parsed_source_bytes: usize,
}

impl StreamingMarkdownProjection {
    pub(crate) fn new(source_generation: u64) -> Self {
        Self {
            source_generation,
            observed_source_len: 0,
            stable_source_end: 0,
            stable_boundary_fresh_line: false,
            stable_segments: Vec::new(),
            stable_row_ends: Vec::new(),
            tail_lines: Arc::from([]),
            exact_full_mode: false,
            next_refresh_at: None,
            #[cfg(test)]
            parsed_source_bytes: 0,
        }
    }

    pub(crate) fn update(
        &mut self,
        source: &str,
        source_generation: u64,
        rule_width: u16,
        now: Instant,
        force: bool,
    ) -> StreamingProjectionUpdate {
        if self.source_generation != source_generation
            || source.len() < self.observed_source_len
            || self.stable_source_end > source.len()
        {
            *self = Self::new(source_generation);
        }
        if !force
            && self.observed_source_len > 0
            && self.next_refresh_at.is_some_and(|deadline| now < deadline)
        {
            return StreamingProjectionUpdate::Deferred;
        }

        let started = Instant::now();
        if self.exact_full_mode {
            self.tail_lines = Arc::from(render_markdown_with_width(source, rule_width));
            #[cfg(test)]
            {
                self.parsed_source_bytes = self.parsed_source_bytes.saturating_add(source.len());
            }
        } else {
            let suffix = &source[self.stable_source_end..];
            let parsed = parse_markdown_for_projection(suffix);
            #[cfg(test)]
            {
                self.parsed_source_bytes = self.parsed_source_bytes.saturating_add(suffix.len());
            }
            if parsed.has_reference_definitions {
                self.stable_source_end = 0;
                self.stable_boundary_fresh_line = false;
                self.stable_segments.clear();
                self.stable_row_ends.clear();
                self.exact_full_mode = true;
                self.tail_lines = Arc::from(render_markdown_with_width(source, rule_width));
                #[cfg(test)]
                {
                    self.parsed_source_bytes =
                        self.parsed_source_bytes.saturating_add(source.len());
                }
            } else {
                let rendered =
                    render_projection_suffix(parsed, rule_width, self.stable_boundary_fresh_line);
                if let Some(committed_source_bytes) = rendered.committed_source_bytes {
                    if !rendered.stable_lines.is_empty() {
                        let rows = rendered.stable_lines.len();
                        self.stable_segments.push(Arc::from(rendered.stable_lines));
                        let previous = self.stable_row_ends.last().copied().unwrap_or(0);
                        self.stable_row_ends.push(previous.saturating_add(rows));
                    }
                    self.stable_source_end = self
                        .stable_source_end
                        .saturating_add(committed_source_bytes);
                    self.stable_boundary_fresh_line = rendered.stable_boundary_fresh_line;
                }
                self.tail_lines = Arc::from(rendered.tail_lines);
            }
        }

        self.observed_source_len = source.len();
        let render_cost = started.elapsed();
        let working_len = if self.exact_full_mode {
            source.len()
        } else {
            source.len().saturating_sub(self.stable_source_end)
        };
        let interval = stream_refresh_interval(working_len, self.exact_full_mode, render_cost);
        self.next_refresh_at = Some(now + interval);
        StreamingProjectionUpdate::Rendered
    }

    pub(crate) fn rows(&self) -> usize {
        self.raw_rows().max(1)
    }

    fn raw_rows(&self) -> usize {
        self.stable_row_ends.last().copied().unwrap_or(0) + self.tail_lines.len()
    }

    pub(crate) fn append_range(&self, start: usize, end: usize, out: &mut Vec<Line<'static>>) {
        let end = end.min(self.rows());
        let start = start.min(end);
        let raw_rows = self.raw_rows();
        if raw_rows == 0 {
            if start == 0 && end > 0 {
                out.push(Line::from(streaming_cursor()));
            }
            return;
        }

        let stable_rows = self.stable_row_ends.last().copied().unwrap_or(0);
        if start < stable_rows {
            let mut segment_index = self.stable_row_ends.partition_point(|row| *row <= start);
            while segment_index < self.stable_segments.len() {
                let segment_start = segment_index
                    .checked_sub(1)
                    .and_then(|index| self.stable_row_ends.get(index).copied())
                    .unwrap_or(0);
                if segment_start >= end {
                    break;
                }
                let segment = &self.stable_segments[segment_index];
                let local_start = start.saturating_sub(segment_start).min(segment.len());
                let local_end = end.saturating_sub(segment_start).min(segment.len());
                out.extend(segment[local_start..local_end].iter().cloned());
                segment_index += 1;
            }
        }

        if end > stable_rows && start < raw_rows {
            let local_start = start.saturating_sub(stable_rows).min(self.tail_lines.len());
            let local_end = end.saturating_sub(stable_rows).min(self.tail_lines.len());
            for (offset, line) in self.tail_lines[local_start..local_end].iter().enumerate() {
                let global_row = stable_rows
                    .saturating_add(local_start)
                    .saturating_add(offset);
                let mut line = line.clone();
                if global_row.saturating_add(1) == raw_rows {
                    line.spans.push(streaming_cursor());
                }
                out.push(line);
            }
        }
    }

    #[cfg(test)]
    fn all_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        self.append_range(0, self.rows(), &mut lines);
        if let Some(last) = lines.last_mut()
            && let Some(span) = last.spans.last()
            && span.content == "▏"
        {
            last.spans.pop();
        }
        if lines.last().is_some_and(|line| line.spans.is_empty()) {
            lines.pop();
        }
        lines
    }
}

fn streaming_cursor() -> Span<'static> {
    Span::styled(
        "▏".to_string(),
        Style::default().add_modifier(Modifier::SLOW_BLINK),
    )
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
    current_width: usize,
    style_stack: Vec<Style>,
    list_stack: Vec<ListKind>,
    in_code_block: Option<String>,
    code_buffer: String,
    heading_level: Option<HeadingLevel>,
    blockquote_depth: u16,
    fresh_line: bool,
    pending_separator: bool,
    in_table: bool,
    in_table_head: bool,
    table_row: Vec<String>,
    table_header: Vec<String>,
    table_body: Vec<Vec<String>>,
    rule_width: u16,
}

impl Renderer {
    fn with_rule_width(w: u16) -> Self {
        Self {
            rule_width: w.max(4),
            ..Default::default()
        }
    }

    fn with_boundary(w: u16, fresh_line: bool) -> Self {
        Self {
            fresh_line,
            ..Self::with_rule_width(w)
        }
    }

    fn at_top_level_boundary(&self) -> bool {
        self.current.is_empty()
            && self.style_stack.is_empty()
            && self.list_stack.is_empty()
            && self.in_code_block.is_none()
            && self.code_buffer.is_empty()
            && self.heading_level.is_none()
            && self.blockquote_depth == 0
            && !self.in_table
            && !self.in_table_head
            && self.table_row.is_empty()
            && self.table_header.is_empty()
            && self.table_body.is_empty()
    }

    fn content_width(&self) -> usize {
        self.rule_width as usize
    }
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered(u64),
}

struct WrapPiece {
    text: String,
    width: usize,
    is_newline: bool,
}

impl WrapPiece {
    fn newline() -> Self {
        Self {
            text: String::new(),
            width: 0,
            is_newline: true,
        }
    }
}

impl Renderer {
    fn active_style(&self) -> Style {
        self.style_stack
            .iter()
            .copied()
            .fold(Style::default(), merge_style)
    }

    fn push_text(&mut self, text: &str) {
        if self.pending_separator && !text.is_empty() && !self.fresh_line {
            self.current.push(Span::raw(" "));
            self.current_width += 1;
        }
        self.pending_separator = false;
        let style = self.active_style();
        let limit = self.content_width();
        let indent = self.indent_prefix();
        let indent_w = crate::width::width(&indent);
        for piece in self.wrap_text(text, limit, indent_w) {
            if piece.is_newline {
                self.end_line();
                if !indent.is_empty() {
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width = indent_w;
                }
                continue;
            }
            self.current.push(Span::styled(piece.text.clone(), style));
            self.current_width += piece.width;
            self.fresh_line = false;
        }
    }

    fn end_line(&mut self) {
        if self.current.is_empty() & self.fresh_line {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
        self.current_width = 0;
        self.fresh_line = true;
    }

    fn blank_line(&mut self) {
        if !self.fresh_line {
            self.end_line();
        }
        self.lines.push(Line::from(""));
        self.current_width = 0;
        self.fresh_line = true;
    }

    fn indent_prefix(&self) -> String {
        let mut out = String::new();
        for _ in 0..self.blockquote_depth {
            out.push_str("│ ");
        }
        for _ in 0..self.list_stack.len().saturating_sub(1) {
            out.push_str("  ");
        }
        out
    }

    fn wrap_text(&self, text: &str, limit: usize, indent_w: usize) -> Vec<WrapPiece> {
        let effective_limit = limit.saturating_sub(indent_w).max(1);
        let mut out = Vec::new();
        let mut buf = String::new();
        let mut buf_w = 0usize;
        let mut line_w = self.current_width;

        fn flush(
            out: &mut Vec<WrapPiece>,
            buf: &mut String,
            buf_w: &mut usize,
            line_w: &mut usize,
        ) {
            if !buf.is_empty() {
                out.push(WrapPiece {
                    text: std::mem::take(buf),
                    width: *buf_w,
                    is_newline: false,
                });
                *line_w += *buf_w;
                *buf_w = 0;
            }
        }
        fn newline(out: &mut Vec<WrapPiece>, line_w: &mut usize) {
            while !out.is_empty()
                && !out.last().unwrap().is_newline
                && out.last().unwrap().text.chars().all(|c| c == ' ')
            {
                let w = out.last().unwrap().width;
                out.pop();
                *line_w -= w;
            }
            out.push(WrapPiece::newline());
            *line_w = 0;
        }

        for (g, gw) in crate::width::graphemes(text) {
            if g == "\n" {
                flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
                newline(&mut out, &mut line_w);
                continue;
            }
            if g == " " {
                flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
                if line_w > 0 && line_w < effective_limit {
                    out.push(WrapPiece {
                        text: " ".into(),
                        width: 1,
                        is_newline: false,
                    });
                    line_w += 1;
                }
                continue;
            }
            let is_word_break = is_cjk(g.chars().next().unwrap_or('\0')) || gw >= 2;
            if is_word_break {
                flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
            }
            if line_w + buf_w + gw > effective_limit {
                if buf_w + gw <= effective_limit {
                    if line_w > 0 {
                        newline(&mut out, &mut line_w);
                    }
                } else {
                    flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
                    if line_w > 0 {
                        newline(&mut out, &mut line_w);
                    }
                }
            }
            buf.push_str(g);
            buf_w += gw;
        }
        flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
        while !out.is_empty()
            && !out.last().unwrap().is_newline
            && out.last().unwrap().text.chars().all(|c| c == ' ')
        {
            out.pop();
        }
        out
    }

    fn consume(&mut self, ev: Event<'_>) {
        let t = crate::theme::theme();
        match ev {
            Event::Start(tag) => self.enter(tag),
            Event::End(end) => self.leave(end),
            Event::Text(text) => {
                if self.in_code_block.is_some() {
                    self.code_buffer.push_str(&text);
                    return;
                }
                if self.in_table {
                    if let Some(cell) = self.table_row.last_mut() {
                        cell.push_str(&text);
                    }
                    return;
                }
                let had_separator = text.ends_with(' ') && !text.ends_with("\n");
                self.push_text(&text);
                self.pending_separator = had_separator;
            }
            Event::Code(text) => {
                if self.in_table {
                    if let Some(cell) = self.table_row.last_mut() {
                        cell.push_str(&text);
                    }
                    return;
                }
                let code_style = Style::default()
                    .fg(t.warn.into())
                    .add_modifier(Modifier::BOLD);
                self.style_stack.push(code_style);
                self.push_text(&text);
                self.style_stack.pop();
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.in_table {
                    if let Some(cell) = self.table_row.last_mut() {
                        cell.push(' ');
                    }
                    return;
                }
                self.end_line();
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width = crate::width::width(&indent);
                }
            }
            Event::Rule => {
                self.blank_line();
                let side = 4usize;
                let dash_w = (self.rule_width as usize).saturating_sub(side * 2).max(4);
                let style = Style::default()
                    .fg(t.subtle_fg.into())
                    .add_modifier(Modifier::DIM);
                self.lines.push(Line::from(vec![
                    Span::raw(" ".repeat(side)),
                    Span::styled("╌".repeat(dash_w), style),
                    Span::raw(" ".repeat(side)),
                ]));
                self.fresh_line = true;
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                let style = Style::default().fg(t.accent.into());
                self.current.push(Span::styled(mark.to_string(), style));
                self.current_width += crate::width::width(mark);
                self.fresh_line = false;
            }
            Event::InlineMath(tex) | Event::DisplayMath(tex) => self.render_display_math(&tex),
            _ => {}
        }
    }

    fn render_display_math(&mut self, tex: &str) {
        let t = crate::theme::theme();
        self.end_line();
        self.blank_line();
        let math_lines = crate::highlight::render_math(tex);
        let target = self.rule_width as usize;
        let math_style = Style::default().fg(t.tinted_fg.into());
        for ml in &math_lines {
            let w = crate::width::spans_width(&ml.spans);
            let pad = target.saturating_sub(w) / 2;
            let mut spans: Vec<Span<'static>> = Vec::new();
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            for s in &ml.spans {
                spans.push(Span::styled(s.content.clone(), s.style.patch(math_style)));
            }
            self.lines.push(Line::from(spans));
        }
        self.blank_line();
        self.fresh_line = true;
    }

    fn enter(&mut self, tag: Tag<'_>) {
        let t = crate::theme::theme();
        match tag {
            Tag::Paragraph => {
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width += crate::width::width(&indent);
                }
            }
            Tag::Heading { level, .. } => {
                self.heading_level = Some(level);
                self.style_stack.push(heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_add(1);
            }
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => l.into_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.in_code_block = Some(lang);
                self.code_buffer.clear();
            }
            Tag::List(start) => {
                if !self.list_stack.is_empty() && !self.fresh_line {
                    self.end_line();
                }
                self.list_stack
                    .push(start.map(ListKind::Ordered).unwrap_or(ListKind::Bullet));
            }
            Tag::Item => {
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width += crate::width::width(&indent);
                }
                let bullet = match self.list_stack.last_mut() {
                    Some(ListKind::Bullet) => "• ".to_string(),
                    Some(ListKind::Ordered(n)) => {
                        let out = format!("{n}. ");
                        *n += 1;
                        out
                    }
                    None => "• ".to_string(),
                };
                let style = Style::default().fg(t.accent.into());
                self.current.push(Span::styled(bullet.clone(), style));
                self.current_width += crate::width::width(&bullet);
                self.fresh_line = false;
            }
            Tag::Emphasis => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::ITALIC));
            }
            Tag::Strong => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link { .. } => {
                self.style_stack.push(
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Table(_) => {
                self.in_table = true;
                self.blank_line();
            }
            Tag::TableHead => {
                self.table_row.clear();
                self.in_table_head = true;
            }
            Tag::TableRow => {
                self.table_row.clear();
            }
            Tag::TableCell => {
                if self.in_table {
                    self.table_row.push(String::new());
                }
            }
            _ => {}
        }
    }

    fn leave(&mut self, end: TagEnd) {
        let t = crate::theme::theme();
        match end {
            TagEnd::Paragraph => {
                self.end_line();
                self.blank_line();
            }
            TagEnd::Heading(level) => {
                self.style_stack.pop();
                self.end_line();
                if matches!(level, HeadingLevel::H1) {
                    let side = 4usize;
                    let dash_w = (self.rule_width as usize).saturating_sub(side * 2).max(4);
                    let style = Style::default()
                        .fg(t.subtle_fg.into())
                        .add_modifier(Modifier::DIM);
                    self.lines.push(Line::from(vec![
                        Span::raw(" ".repeat(side)),
                        Span::styled("╌".repeat(dash_w), style),
                        Span::raw(" ".repeat(side)),
                    ]));
                }
                self.heading_level = None;
                self.blank_line();
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                let lang = self.in_code_block.take().unwrap_or_default();
                let body = std::mem::take(&mut self.code_buffer);
                self.render_code_block(&lang, &body);
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.blank_line();
            }
            TagEnd::Item => {
                self.end_line();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.style_stack.pop();
            }
            TagEnd::Table => {
                self.flush_table();
                self.in_table = false;
                self.blank_line();
            }
            TagEnd::TableHead => {
                if !self.table_row.is_empty() {
                    self.table_header = std::mem::take(&mut self.table_row);
                }
                self.in_table_head = false;
            }
            TagEnd::TableRow => {
                if !self.table_row.is_empty() {
                    self.table_body.push(std::mem::take(&mut self.table_row));
                }
            }
            _ => {}
        }
    }

    fn flush_table(&mut self) {
        let t = crate::theme::theme();
        if self.table_header.is_empty() & self.table_body.is_empty() {
            return;
        }
        let col_count = self
            .table_header
            .len()
            .max(self.table_body.iter().map(|r| r.len()).max().unwrap_or(0));
        if col_count == 0 {
            return;
        }
        let target = self.rule_width as usize;
        let inner_pad = 2usize;
        let available = target.saturating_sub(inner_pad * 2);
        let col_min = 4usize;
        let sep = 3usize;
        let mut widths = vec![col_min; col_count];
        for (i, cell) in self.table_header.iter().enumerate() {
            widths[i] = widths[i].max(crate::width::width(cell));
        }
        for row in &self.table_body {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(crate::width::width(cell));
            }
        }
        let cells_total: usize = widths.iter().sum::<usize>() + sep * col_count.saturating_sub(1);
        if cells_total < available {
            let extra = available - cells_total;
            let per_col = extra / col_count;
            let remainder = extra % col_count;
            for (i, w) in widths.iter_mut().enumerate() {
                *w += per_col + if i < remainder { 1 } else { 0 };
            }
        } else if cells_total > available {
            // Shrink columns proportionally to fit available width.
            // available already excludes inner_pad; cells_total includes sep gaps.
            // If col_min * col_count + seps still exceeds available, reduce col_min
            // and sep further rather than overflowing.
            let seps = sep * col_count.saturating_sub(1);
            let target_sum = available.saturating_sub(seps);
            let effective_min = (target_sum / col_count).max(1);
            loop {
                let total: usize = widths.iter().sum();
                if total <= target_sum {
                    break;
                }
                let excess = total - target_sum;
                let mut shrunk = 0usize;
                for w in widths.iter_mut() {
                    if *w > effective_min {
                        let s = (*w * excess / total.max(1)).min(*w - effective_min);
                        *w -= s;
                        shrunk += s;
                    }
                }
                if shrunk == 0 {
                    if let Some(w) = widths.iter_mut().rev().find(|w| **w > effective_min) {
                        *w -= 1;
                    } else {
                        break;
                    }
                }
            }
        }
        let bg = block_bg();
        let head_style = Style::default()
            .fg(t.accent.into())
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let cell_style = Style::default().bg(bg);
        let rule_style = Style::default().fg(t.subtle_fg.into()).bg(bg);

        self.lines.push(blank_bg_line(target, bg));
        if !self.table_header.is_empty() {
            let wrapped: Vec<Vec<String>> = self
                .table_header
                .iter()
                .enumerate()
                .map(|(i, cell)| crate::width::word_wrap(cell, widths[i]))
                .collect();
            let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
            for line_idx in 0..height {
                let cells: Vec<String> = wrapped
                    .iter()
                    .map(|c| c.get(line_idx).cloned().unwrap_or_default())
                    .collect();
                self.lines.push(table_row(
                    &cells, &widths, inner_pad, target, head_style, bg, sep,
                ));
            }
            let rule: String = (0..col_count)
                .map(|i| "─".repeat(widths[i]))
                .collect::<Vec<_>>()
                .join(&" ".repeat(sep));
            self.lines
                .push(table_line(&rule, inner_pad, target, rule_style, bg));
        }
        let sep_rule: String = (0..col_count)
            .map(|i| "╌".repeat(widths[i]))
            .collect::<Vec<_>>()
            .join(&" ".repeat(sep));
        let sep_style = Style::default()
            .fg(t.subtle_fg.into())
            .bg(bg)
            .add_modifier(Modifier::DIM);
        for (i, row) in self.table_body.iter().enumerate() {
            if i > 0 {
                self.lines
                    .push(table_line(&sep_rule, inner_pad, target, sep_style, bg));
            }
            let wrapped: Vec<Vec<String>> = row
                .iter()
                .enumerate()
                .map(|(col_i, cell)| crate::width::word_wrap(cell, widths[col_i]))
                .collect();
            let height = wrapped.iter().map(|c| c.len()).max().unwrap_or(1);
            for line_idx in 0..height {
                let cells: Vec<String> = wrapped
                    .iter()
                    .map(|c| c.get(line_idx).cloned().unwrap_or_default())
                    .collect();
                self.lines.push(table_row(
                    &cells, &widths, inner_pad, target, cell_style, bg, sep,
                ));
            }
        }
        self.lines.push(blank_bg_line(target, bg));
        self.table_header.clear();
        self.table_body.clear();
        self.fresh_line = true;
    }

    fn render_code_block(&mut self, lang: &str, body: &str) {
        if lang == "mermaid" {
            return;
        }
        let t = crate::theme::theme();
        self.blank_line();
        let bg = block_bg();
        let target = self.rule_width as usize;
        let inner_pad = 2usize;
        let lang_label = if lang.is_empty() {
            "code".to_string()
        } else {
            lang.to_string()
        };
        let gutter = Style::default().fg(t.subtle_fg.into()).bg(bg);
        let bg_only = Style::default().bg(bg);
        let lineno_style = Style::default()
            .fg(t.subtle_fg.into())
            .bg(bg)
            .add_modifier(Modifier::DIM);
        let header = format!("╭─ {lang_label} ─");
        self.lines.push(bg_padded_line(&header, gutter, target, bg));
        self.lines.push(blank_bg_line(target, bg));
        let highlighted = if lang == "ansi" || lang == "terminal" {
            crate::highlight::highlight_ansi(body)
        } else {
            crate::highlight::highlight_code(lang, body)
        };
        let width = digits_for(highlighted.len());
        for (i, hl) in highlighted.into_iter().enumerate() {
            let lineno = format!("{:>width$}  ", i + 1);
            let mut used = inner_pad + crate::width::width(lineno.as_str());
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(hl.spans.len() + 3);
            spans.push(Span::styled(" ".repeat(inner_pad), bg_only));
            spans.push(Span::styled(lineno, lineno_style));
            for src in hl.spans {
                used += crate::width::width(src.content.as_ref());
                let style = if src.style.bg.is_none() {
                    src.style.bg(bg)
                } else {
                    src.style
                };
                spans.push(Span::styled(src.content, style));
            }
            if target > used {
                spans.push(Span::styled(" ".repeat(target - used), bg_only));
            }
            self.lines.push(Line::from(spans));
        }
        self.lines.push(blank_bg_line(target, bg));
        self.fresh_line = true;
        self.blank_line();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current.is_empty() {
            self.end_line();
        }
        while self
            .lines
            .last()
            .map(|l| l.spans.iter().all(|s| s.content.is_empty()))
            .unwrap_or(false)
        {
            self.lines.pop();
        }
        self.lines
    }
}

fn blank_bg_line(width: usize, bg: Color) -> Line<'static> {
    Line::from(Span::styled(" ".repeat(width), Style::default().bg(bg)))
}

fn bg_padded_line(text: &str, style: Style, target: usize, bg: Color) -> Line<'static> {
    let used = crate::width::width(text);
    let mut spans = Vec::with_capacity(2);
    spans.push(Span::styled(text.to_string(), style));
    if target > used {
        spans.push(Span::styled(
            " ".repeat(target - used),
            Style::default().bg(bg),
        ));
    }
    Line::from(spans)
}

fn table_line(
    text: &str,
    inner_pad: usize,
    target: usize,
    style: Style,
    bg: Color,
) -> Line<'static> {
    let bg_only = Style::default().bg(bg);
    let content_w = crate::width::width(text);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
    spans.push(Span::styled(" ".repeat(inner_pad), bg_only));
    spans.push(Span::styled(text.to_string(), style));
    let right = target.saturating_sub(inner_pad + content_w);
    if right > 0 {
        spans.push(Span::styled(" ".repeat(right), bg_only));
    }
    Line::from(spans)
}

fn table_row(
    cells: &[String],
    widths: &[usize],
    inner_pad: usize,
    target: usize,
    style: Style,
    bg: Color,
    sep: usize,
) -> Line<'static> {
    let bg_only = Style::default().bg(bg);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(widths.len() * 2 + 3);
    spans.push(Span::styled(" ".repeat(inner_pad), bg_only));
    let mut used = inner_pad;
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" ".repeat(sep), bg_only));
            used += sep;
        }
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        let pad = w.saturating_sub(crate::width::width(cell));
        spans.push(Span::styled(cell.to_string(), style));
        spans.push(Span::styled(" ".repeat(pad), bg_only));
        used += w;
    }
    let right = target.saturating_sub(used);
    if right > 0 {
        spans.push(Span::styled(" ".repeat(right), bg_only));
    }
    Line::from(spans)
}

fn digits_for(n: usize) -> usize {
    if n == 0 {
        1
    } else {
        (n as f32).log10().floor() as usize + 1
    }
}

pub fn block_bg() -> Color {
    crate::theme::theme().code_bg.into()
}

fn heading_style(level: HeadingLevel) -> Style {
    let t = crate::theme::theme();
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H2 => Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default()
            .fg(t.tinted_fg.into())
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H5 | HeadingLevel::H6 => Style::default()
            .fg(t.subtle_fg.into())
            .add_modifier(Modifier::BOLD | Modifier::DIM),
    }
}

fn merge_style(base: Style, layer: Style) -> Style {
    let mut out = base;
    if layer.fg.is_some() {
        out.fg = layer.fg;
    }
    if layer.bg.is_some() {
        out.bg = layer.bg;
    }
    out.add_modifier |= layer.add_modifier;
    out.sub_modifier |= layer.sub_modifier;
    out
}

fn is_cjk(ch: char) -> bool {
    let u = ch as u32;
    matches!(u,
        0x3000..=0x303F
        | 0x3040..=0x309F
        | 0x30A0..=0x30FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0xFF00..=0xFFEF
        | 0x11000..=0x11FFF
        | 0x1F300..=0x1FAFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn heading_renders_bold_without_hash_prefix() {
        let lines = render_markdown("# Title\n\nbody\n");
        let flat = plain(&lines);
        assert!(flat[0].starts_with("Title"), "got {:?}", flat);
        let bold = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "Title")
            .expect("title span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn bullet_list_uses_bullet_glyph() {
        let lines = render_markdown("- one\n- two\n");
        let flat = plain(&lines);
        assert!(flat.iter().any(|l| l.contains("• one")), "{flat:?}");
        assert!(flat.iter().any(|l| l.contains("• two")), "{flat:?}");
    }

    #[test]
    fn ordered_list_numbers_items() {
        let lines = render_markdown("1. alpha\n2. beta\n");
        let flat = plain(&lines);
        assert!(flat.iter().any(|l| l.contains("1. alpha")), "{flat:?}");
        assert!(flat.iter().any(|l| l.contains("2. beta")), "{flat:?}");
    }

    #[test]
    fn code_block_gets_frame_and_language_label() {
        let lines = render_markdown("```rust\nfn main() {}\n```\n");
        let flat = plain(&lines);
        assert!(flat.iter().any(|l| l.contains("rust")), "{flat:?}");
        assert!(flat.iter().any(|l| l.contains("fn main()")), "{flat:?}");
    }

    #[test]
    fn strong_emphasis_stacks_bold_modifier() {
        let lines = render_markdown("**bold** normal *italic*\n");
        let bold_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "bold")
            .expect("bold span");
        assert!(
            bold_span.style.add_modifier.contains(Modifier::BOLD),
            "want bold: {:?}",
            bold_span.style
        );
        let italic_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "italic")
            .expect("italic span");
        assert!(
            italic_span.style.add_modifier.contains(Modifier::ITALIC),
            "want italic: {:?}",
            italic_span.style
        );
    }

    #[test]
    fn inline_code_renders_without_backticks() {
        let lines = render_markdown("call `foo()` please\n");
        let flat = plain(&lines);
        let joined = flat.join("");
        assert!(joined.contains("foo()"), "{flat:?}");
        assert!(
            !joined.contains('`'),
            "backtick should be stripped: {flat:?}"
        );
    }

    #[test]
    fn inline_code_wraps_long_text() {
        let long = "x".repeat(80);
        let md = format!("call `{long}` please\n");
        let lines = render_markdown_with_width(&md, 30);
        let flat = plain(&lines);
        assert!(flat.len() > 1, "long inline code should wrap: {flat:?}");
        for l in &flat {
            assert!(!l.contains('`'), "no backtick after wrap: {l:?}");
        }
    }

    #[test]
    fn blockquote_prepends_bar_glyph() {
        let lines = render_markdown("> hint\n");
        let flat = plain(&lines);
        assert!(flat.iter().any(|l| l.contains("│")), "{flat:?}");
    }

    #[test]
    fn table_renders_header_and_body_rows() {
        let lines = render_markdown("| a | b |\n| - | - |\n| 1 | 2 |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter().any(|l| l.contains("1") & l.contains("2")),
            "want data row: {flat:?}"
        );
        assert!(
            flat.iter().any(|l| l.contains("a") & l.contains("b")),
            "want header row: {flat:?}"
        );
    }

    #[test]
    fn table_preserves_inline_code_in_cells() {
        let lines = render_markdown("| name | code |\n| - | - |\n| foo | `bar()` |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter().any(|l| l.contains("bar()") & l.contains("foo")),
            "inline code lost from table cell: {flat:?}"
        );
        let joined = flat.join("");
        assert!(
            !joined.contains('`'),
            "backtick stripped in table cells: {joined:?}"
        );
    }

    #[test]
    fn table_preserves_bold_and_italic_text_in_cells() {
        let lines = render_markdown("| A | B |\n| - | - |\n| **bold** run | plain |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter()
                .any(|l| l.contains("bold run") & l.contains("plain")),
            "bold text run split across cells: {flat:?}"
        );
    }

    #[test]
    fn table_keeps_empty_cell_columns_aligned() {
        let lines = render_markdown("| A | B | C |\n| - | - | - |\n| x |  | z |\n");
        let flat = plain(&lines);
        let data_row = flat
            .iter()
            .find(|l| l.contains("x") & l.contains("z"))
            .unwrap_or_else(|| panic!("no data row: {flat:?}"));
        let x_pos = data_row.find("x").unwrap();
        let z_pos = data_row.find("z").unwrap();
        assert!(z_pos - x_pos > 2, "empty column collapsed: {data_row:?}");
    }

    #[test]
    fn strikethrough_toggles_crossed_out() {
        let lines = render_markdown("~~old~~ new\n");
        let old_span = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == "old")
            .expect("strikethrough span");
        assert!(
            old_span.style.add_modifier.contains(Modifier::CROSSED_OUT),
            "want crossed_out: {:?}",
            old_span.style
        );
    }

    #[test]
    fn multiline_display_math_is_split_before_markdown_parsing() {
        let split = split_display_math_segments("before\n$$\nx = y\n=\nz\n$$\nafter\n");
        let segments = split.segments;
        assert_eq!(split.unclosed_display_math_start, None);
        assert!(
            matches!(segments[0], MarkdownSegment::Text { text, offset: 0 } if text == "before\n")
        );
        assert!(
            matches!(segments[1], MarkdownSegment::DisplayMath { ref tex, range: Range { start: 7, end: 23 } } if tex == "x = y = z")
        );
        assert!(
            matches!(segments[2], MarkdownSegment::Text { text, offset: 23 } if text == "after\n")
        );
    }

    #[test]
    fn multiline_display_math_keeps_code_fence_text_in_markdown_segment() {
        let split = split_display_math_segments("```text\n$$\nx = y\n$$\n```\n");
        let segments = split.segments;
        assert_eq!(split.unclosed_display_math_start, None);
        assert!(
            matches!(segments.as_slice(), [MarkdownSegment::Text { text, offset: 0 }] if *text == "```text\n$$\nx = y\n$$\n```\n")
        );
    }

    #[test]
    fn multiline_display_math_renders_through_document_lines() {
        let lines = render_markdown_with_width("before\n$$\nx = y\n=\nz\n$$\nafter", 80);
        let text = plain(&lines).join("\n");
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        assert!(!text.contains("$$"));
        assert!(!text.contains('╌'));
        assert!(!text.contains("\\frac"));
    }

    #[test]
    fn multiline_display_math_preserves_list_items_around_it() {
        let lines = render_markdown_with_width("- before\n\n$$\nx = y\n=\nz\n$$\n\n- after", 80);
        let text = plain(&lines).join("\n");
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        assert!(!text.contains("$$"));
        assert!(!text.contains('╌'));
    }

    #[test]
    fn nested_list_starts_on_its_own_indented_line() {
        let lines = plain(&render_markdown("- parent\n  - child\n"));
        assert_eq!(lines[0], "• parent");
        assert_eq!(lines[1], "  • child");
    }

    #[test]
    fn nested_ordered_list_starts_on_its_own_indented_line() {
        let lines = plain(&render_markdown("1. parent\n   1. child\n"));
        assert_eq!(lines[0], "1. parent");
        assert_eq!(lines[1], "  1. child");
    }

    #[test]
    fn inline_code_preserves_left_separator_space() {
        let lines = render_markdown("before `code` after");
        assert_eq!(plain(&lines), vec!["before code after"]);
    }

    #[test]
    fn inline_code_without_separator_stays_adjacent() {
        let lines = render_markdown("before`code` after");
        assert_eq!(plain(&lines), vec!["beforecode after"]);
    }

    #[test]
    fn empty_input_gives_empty_output() {
        assert!(render_markdown("").is_empty());
        assert!(render_markdown("\n\n").is_empty());
    }

    fn line_widths(lines: &[Line<'_>]) -> Vec<usize> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| crate::width::width(s.content.as_ref()))
                    .sum::<usize>()
            })
            .collect()
    }

    #[test]
    fn long_english_text_wraps_at_content_width() {
        let md = "this is a very long line of english text that should wrap when it exceeds the configured content width limit";
        let lines = render_markdown_with_width(md, 20);
        let widths = line_widths(&lines);
        assert!(
            lines.len() > 1,
            "expected wrapping, got {} lines: {:?}",
            lines.len(),
            plain(&lines)
        );
        for (i, w) in widths.iter().enumerate() {
            assert!(*w <= 20, "line {i} width {w} > 20: {:?}", plain(&lines));
        }
    }

    #[test]
    fn long_chinese_text_wraps_correctly() {
        let md = "这是一段非常长的中文文本它没有任何空格也没有任何换行符就是一整个连续的字符串应该按终端宽度自动换行";
        let lines = render_markdown_with_width(md, 20);
        let widths = line_widths(&lines);
        assert!(
            lines.len() > 1,
            "expected wrapping, got {} lines",
            lines.len()
        );
        for (i, w) in widths.iter().enumerate() {
            assert!(*w <= 20, "line {i} width {w} > 20: {:?}", plain(&lines));
        }
    }

    #[test]
    fn emoji_takes_double_width() {
        let md = "😀😁😂🤣😃😄😅😆😉😊😋😎😍😘😗😙😚☺🙂🤗🤩🤔🤨😐😑😶🙄😏😣😥😮🤐😯😪😫😴😌😛😜😝🤤😒😓😔😕🙃🤑😲☹🙁😖😞😟😤😢😭😦😧😨😩🤯😬😰😱😳🤪😵😡😠🤬😷🤒🤕🤢🤮🤧😇🤠🤡🤥🤫🤭🧐🤓😈👿";
        let lines = render_markdown_with_width(md, 20);
        let widths = line_widths(&lines);
        assert!(
            lines.len() > 1,
            "expected wrapping for emoji, got {} lines",
            lines.len()
        );
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= 20,
                "emoji line {i} width {w} > 20: {:?}",
                plain(&lines)
            );
        }
    }

    #[test]
    fn kaomoji_preserved_as_single_unit_when_possible() {
        let md = "(｡◕‿◕｡) ᕕ(ᐛ)ᕗ (ノಠ益ಠ)ノ彡┻━┻ ╰(▽)╯";
        let lines = render_markdown_with_width(md, 40);
        let flat = plain(&lines);
        let joined = flat.join("");
        assert!(joined.contains("(｡◕‿◕｡)"), "kaomoji broken: {joined:?}");
        assert!(joined.contains("ᕕ(ᐛ)ᕗ"), "kaomoji broken: {joined:?}");
        assert!(joined.contains("┻━┻"), "kaomoji broken: {joined:?}");
        let widths = line_widths(&lines);
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= 40,
                "kaomoji line {i} width {w} > 40: {:?}",
                plain(&lines)
            );
        }
    }

    #[test]
    fn mixed_cjk_emoji_english_wraps_correctly() {
        let md = "Hello 世界 🌍 this is a mixed 文本 with emoji 🎉 and English words and 中文 characters and more emoji 🚀✨💡 all mixed together in one long paragraph that should wrap properly";
        let lines = render_markdown_with_width(md, 24);
        let widths = line_widths(&lines);
        assert!(lines.len() > 1, "expected wrapping for mixed text");
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= 24,
                "mixed line {i} width {w} > 24: {:?}",
                plain(&lines)
            );
        }
        let joined: String = plain(&lines).join("");
        assert!(joined.contains("Hello"), "lost Hello: {joined:?}");
        assert!(joined.contains("世界"), "lost 世界: {joined:?}");
        assert!(joined.contains("🌍"), "lost emoji: {joined:?}");
        assert!(joined.contains("🎉"), "lost emoji: {joined:?}");
    }

    #[test]
    fn wrapping_preserves_word_boundaries_for_english() {
        let md = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let lines = render_markdown_with_width(md, 15);
        let flat = plain(&lines);
        assert!(lines.len() > 1, "expected wrapping");
        for line in &flat {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let words: Vec<&str> = trimmed.split_whitespace().collect();
            for word in &words {
                let original_words = [
                    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
                    "india", "juliet", "kilo", "lima", "mike", "november", "oscar", "papa",
                    "quebec", "romeo", "sierra", "tango", "uniform", "victor", "whiskey", "xray",
                    "yankee", "zulu",
                ];
                if word.len() <= 10 {
                    assert!(
                        original_words.contains(word),
                        "word broken: {word:?} in line {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn narrow_width_does_not_panic() {
        let md = "some text here";
        let lines = render_markdown_with_width(md, 4);
        assert!(!lines.is_empty());
        let widths = line_widths(&lines);
        for (i, w) in widths.iter().enumerate() {
            assert!(*w <= 8, "narrow line {i} width {w}: {:?}", plain(&lines));
        }
    }

    #[test]
    fn list_item_text_wraps_with_indent() {
        let md = "- this is a very long list item that should wrap to the next line with proper indentation aligned under the text after the bullet point";
        let lines = render_markdown_with_width(md, 30);
        let widths = line_widths(&lines);
        assert!(lines.len() > 1, "expected list wrapping");
        for (i, w) in widths.iter().enumerate() {
            assert!(
                *w <= 30,
                "list line {i} width {w} > 30: {:?}",
                plain(&lines)
            );
        }
        let flat = plain(&lines);
        assert!(flat[0].contains("• "), "missing bullet: {:?}", flat[0]);
    }

    #[test]
    fn zero_width_combining_chars_dont_break_width() {
        let md =
            "e\u{0301} means é, and a\u{0308} means ä — combining chars should not inflate width";
        let lines = render_markdown(md);
        let flat = plain(&lines);
        let joined = flat.join("");
        assert!(joined.contains("é"), "combining char lost: {joined:?}");
        assert!(joined.contains("ä"), "combining char lost: {joined:?}");
    }

    #[test]
    fn streaming_projection_matches_full_renderer_at_arbitrary_boundaries() {
        let fixtures = [
            "# 标题\n\n正文包含 **粗体**、`code` 和 [link](https://example.com)。\n\n- one\n- two\n\n> quote\n\nend",
            "Setext heading\n===\n\n- [ ] pending\n- [x] done\n  - nested **item**\n    1. ordered child\n\nend",
            "before\n\n    let x = 1;\n    let y = 2;\n\n<section>\n<div>html block</div>\n</section>\n\nafter",
            "before\n\n---\n\nafter\n\n$$\nx = y\n=\nz\n$$\n\nend",
            "| name | value |\n| --- | ---: |\n| alpha | 123 |\n\n```rust\nfn main() {}\n```\n\nend",
            "table | header\n--- | ---\nleft | right\n\n```mermaid\ngraph TD\n  A-->B\n```\n\n~~~text\ntilde fence\n~~~",
            "[target]: https://example.com\n\n[resolved][target]\n\nend",
            "[earlier][target]\n\nbody\n\n[target]: https://example.com\n\nend",
        ];
        for source in fixtures {
            let mut projection = StreamingMarkdownProjection::new(1);
            let now = Instant::now();
            for end in source
                .char_indices()
                .map(|(index, ch)| index + ch.len_utf8())
            {
                let prefix = &source[..end];
                assert_eq!(
                    projection.update(prefix, 1, 40, now, true),
                    StreamingProjectionUpdate::Rendered
                );
                assert_eq!(
                    projection.all_lines(),
                    render_markdown_with_width(prefix, 40),
                    "projection diverged at byte {end} for {source:?}"
                );
            }
        }
    }

    #[test]
    fn streaming_projection_coalesces_refreshes_without_losing_source() {
        let mut projection = StreamingMarkdownProjection::new(1);
        let now = Instant::now();
        assert_eq!(
            projection.update("one", 1, 40, now, false),
            StreamingProjectionUpdate::Rendered
        );
        assert_eq!(
            projection.update("one two", 1, 40, now + Duration::from_millis(10), false,),
            StreamingProjectionUpdate::Deferred
        );
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width("one", 40)
        );
        assert_eq!(
            projection.update("one two", 1, 40, now + Duration::from_millis(60), false,),
            StreamingProjectionUpdate::Rendered
        );
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width("one two", 40)
        );
    }

    #[test]
    fn streaming_projection_resets_after_replacement_and_source_shrink() {
        let mut projection = StreamingMarkdownProjection::new(1);
        let now = Instant::now();
        projection.update("one\n\ntwo\n\nthree", 1, 40, now, true);
        let replacement = "replacement that is longer than the original source";
        projection.update(replacement, 2, 40, now, true);
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width(replacement, 40)
        );
        assert_eq!(projection.source_generation, 2);
        assert_eq!(projection.stable_source_end, 0);

        projection.update("short", 2, 40, now, true);
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width("short", 40)
        );
        assert_eq!(projection.stable_source_end, 0);
    }

    #[test]
    fn appended_reference_definition_discards_stable_projection() {
        let mut projection = StreamingMarkdownProjection::new(1);
        let now = Instant::now();
        let unresolved = "before\n\n[earlier][target]\n\nbody\n\nend";
        projection.update(unresolved, 1, 40, now, true);
        assert!(!projection.stable_segments.is_empty());
        assert!(!projection.exact_full_mode);

        let resolved =
            "before\n\n[earlier][target]\n\nbody\n\nend\n\n[target]: https://example.com";
        projection.update(resolved, 1, 40, now, true);
        assert!(projection.exact_full_mode);
        assert!(projection.stable_segments.is_empty());
        assert_eq!(projection.stable_source_end, 0);
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width(resolved, 40)
        );
    }

    #[test]
    fn streaming_projection_parses_block_rich_source_linearly() {
        const BLOCK: &str = "## Section\n\nA paragraph with **bold**, `code`, and a [link](https://example.com).\n\n- one\n- two\n\n```rust\nfn example() {}\n```\n\n| a | b |\n| - | - |\n| 1 | 2 |\n\n";
        let mut source = String::new();
        while source.len() < 128 * 1024 {
            source.push_str(BLOCK);
        }
        let mut projection = StreamingMarkdownProjection::new(1);
        let now = Instant::now();
        let mut end = 64usize;
        while end < source.len() {
            projection.update(&source[..end], 1, 80, now, true);
            end += 64;
        }
        projection.update(&source, 1, 80, now, true);
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width(&source, 80)
        );
        assert!(
            projection.parsed_source_bytes <= source.len() * 12,
            "parsed {} bytes for {} bytes of source",
            projection.parsed_source_bytes,
            source.len()
        );
    }

    #[test]
    fn streaming_projection_slices_segmented_lines_without_flattening() {
        let source = "one\n\ntwo\n\nthree\n\nfour\n\nfive";
        let mut projection = StreamingMarkdownProjection::new(1);
        projection.update(source, 1, 40, Instant::now(), true);
        assert!(!projection.stable_segments.is_empty());

        let mut expected = render_markdown_with_width(source, 40);
        expected
            .last_mut()
            .expect("rendered markdown")
            .spans
            .push(streaming_cursor());
        for start in 0..expected.len() {
            for end in start..=expected.len() {
                let mut actual = Vec::new();
                projection.append_range(start, end, &mut actual);
                assert_eq!(actual, expected[start..end], "slice {start}..{end}");
            }
        }
    }

    #[test]
    fn oversized_stream_refresh_interval_obeys_cpu_budget_bounds() {
        assert_eq!(
            stream_refresh_interval(1024, false, Duration::from_millis(100)),
            STREAM_REFRESH_MIN
        );
        assert_eq!(
            stream_refresh_interval(STREAM_TAIL_SOFT_LIMIT + 1, false, Duration::from_millis(25),),
            Duration::from_millis(200)
        );
        assert_eq!(
            stream_refresh_interval(
                STREAM_TAIL_SOFT_LIMIT + 1,
                false,
                Duration::from_millis(200),
            ),
            STREAM_REFRESH_MAX
        );
        assert_eq!(
            stream_refresh_interval(1024, true, Duration::from_millis(1)),
            STREAM_REFRESH_MIN
        );
    }

    #[test]
    fn unclosed_128_kib_fence_obeys_adaptive_fake_clock_schedule() {
        const CHUNKS: usize = 256;
        let mut source = "```text\n".to_string();
        while source.len() < 128 * 1024 {
            source.push_str("a long unclosed code line that remains in the mutable tail\n");
        }
        let mut projection = StreamingMarkdownProjection::new(1);
        let mut now = Instant::now();
        assert_eq!(
            projection.update(&source, 1, 80, now, true),
            StreamingProjectionUpdate::Rendered
        );
        let first_interval = projection
            .next_refresh_at
            .and_then(|deadline| deadline.checked_duration_since(now))
            .expect("refresh deadline");
        assert!((STREAM_REFRESH_MIN..=STREAM_REFRESH_MAX).contains(&first_interval));

        let chunk = "another streamed line inside the still-open fence\n";
        let mut rendered = 1usize;
        for _ in 0..CHUNKS {
            source.push_str(chunk);
            now += Duration::from_millis(5);
            rendered += usize::from(matches!(
                projection.update(&source, 1, 80, now, false),
                StreamingProjectionUpdate::Rendered
            ));
        }

        assert!(
            rendered <= 27,
            "adaptive schedule rendered {rendered} times for {CHUNKS} chunks"
        );
        projection.update(&source, 1, 80, now, true);
        assert_eq!(
            projection.all_lines(),
            render_markdown_with_width(&source, 80)
        );
        assert!(
            projection.parsed_source_bytes <= source.len() * 32,
            "parsed {} bytes for {} bytes of unclosed source",
            projection.parsed_source_bytes,
            source.len()
        );
    }
}

#[cfg(test)]
mod table_wrap_tests {
    use super::*;

    fn table_widths(md: &str, rule_width: u16) -> usize {
        let lines = render_markdown_with_width(md, rule_width);
        let bg = block_bg();
        let mut max_w = 0usize;
        for line in &lines {
            let w = crate::width::spans_width(&line.spans);
            if w > max_w {
                max_w = w;
            }
        }
        let _ = bg;
        max_w
    }

    #[test]
    fn table_long_cell_does_not_exceed_rule_width() {
        let md = "| name | description |\n|---|---|\n| A | this is a very long description that should wrap instead of overflowing the table width |\n";
        let w = table_widths(md, 40);
        assert!(w <= 40, "table width {w} exceeds rule_width 40");
    }

    #[test]
    fn table_cjk_cell_does_not_exceed_rule_width() {
        let md = "| 名字 | 描述 |\n|---|---|\n| 测试 | 这是一段很长的中文描述内容用于测试表格换行是否正常工作不会超出宽度限制 |\n";
        let w = table_widths(md, 30);
        assert!(w <= 30, "table width {w} exceeds rule_width 30");
    }

    #[test]
    fn table_many_columns_shrink_to_fit() {
        let md = "| a | b | c | d | e |\n|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 |\n";
        let w = table_widths(md, 20);
        assert!(w <= 20, "table width {w} exceeds rule_width 20");
    }
}
