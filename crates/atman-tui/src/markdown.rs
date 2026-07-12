use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render_markdown(md: &str) -> Vec<Line<'static>> {
    render_markdown_with_width(md, 60)
}

pub fn render_markdown_with_width(md: &str, rule_width: u16) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut renderer = Renderer::with_rule_width(rule_width);
    for ev in parser {
        renderer.consume(ev);
    }
    renderer.finish()
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
        use unicode_width::UnicodeWidthStr;
        let style = self.active_style();
        let limit = self.content_width();
        let indent = self.indent_prefix();
        let indent_w = indent.width();
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
        if self.current.is_empty() && self.fresh_line {
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
        use unicode_width::UnicodeWidthChar;
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

        for ch in text.chars() {
            if ch == '\n' {
                flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
                newline(&mut out, &mut line_w);
                continue;
            }
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch == ' ' {
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
            let is_word_break = is_cjk(ch) || w >= 2;
            if is_word_break {
                flush(&mut out, &mut buf, &mut buf_w, &mut line_w);
            }
            if line_w + buf_w + w > effective_limit {
                if buf_w + w <= effective_limit {
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
            buf.push(ch);
            buf_w += w;
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
                self.push_text(&text);
            }
            Event::Code(text) => {
                if self.in_table {
                    if let Some(cell) = self.table_row.last_mut() {
                        cell.push('`');
                        cell.push_str(&text);
                        cell.push('`');
                    }
                    return;
                }
                use unicode_width::UnicodeWidthStr;
                let style = merge_style(
                    self.active_style(),
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                );
                let formatted = format!("`{text}`");
                self.current_width += formatted.width();
                self.current.push(Span::styled(formatted, style));
                self.fresh_line = false;
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
                    use unicode_width::UnicodeWidthStr;
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width = indent.width();
                }
            }
            Event::Rule => {
                self.blank_line();
                let side = 4usize;
                let dash_w = (self.rule_width as usize).saturating_sub(side * 2).max(4);
                let style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM);
                self.lines.push(Line::from(vec![
                    Span::raw(" ".repeat(side)),
                    Span::styled("╌".repeat(dash_w), style),
                    Span::raw(" ".repeat(side)),
                ]));
                self.fresh_line = true;
            }
            Event::TaskListMarker(done) => {
                use unicode_width::UnicodeWidthStr;
                let mark = if done { "[x] " } else { "[ ] " };
                let style = Style::default().fg(Color::Cyan);
                self.current.push(Span::styled(mark.to_string(), style));
                self.current_width += mark.width();
                self.fresh_line = false;
            }
            _ => {}
        }
    }

    fn enter(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                use unicode_width::UnicodeWidthStr;
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width += indent.width();
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
                self.list_stack
                    .push(start.map(ListKind::Ordered).unwrap_or(ListKind::Bullet));
            }
            Tag::Item => {
                use unicode_width::UnicodeWidthStr;
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    let style = self.active_style();
                    self.current.push(Span::styled(indent.clone(), style));
                    self.current_width += indent.width();
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
                let style = Style::default().fg(Color::Cyan);
                self.current.push(Span::styled(bullet.clone(), style));
                self.current_width += bullet.width();
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
                        .fg(Color::LightBlue)
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
                        .fg(Color::DarkGray)
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
        use unicode_width::UnicodeWidthStr;
        if self.table_header.is_empty() && self.table_body.is_empty() {
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
            widths[i] = widths[i].max(cell.width());
        }
        for row in &self.table_body {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.width());
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
        }
        let bg = block_bg();
        let head_style = Style::default()
            .fg(Color::LightCyan)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let cell_style = Style::default().bg(bg);
        let rule_style = Style::default().fg(Color::DarkGray).bg(bg);

        self.lines.push(blank_bg_line(target, bg));
        if !self.table_header.is_empty() {
            self.lines.push(table_row(
                &self.table_header,
                &widths,
                inner_pad,
                target,
                head_style,
                bg,
                sep,
            ));
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
            .fg(Color::DarkGray)
            .bg(bg)
            .add_modifier(Modifier::DIM);
        for (i, row) in self.table_body.iter().enumerate() {
            if i > 0 {
                self.lines
                    .push(table_line(&sep_rule, inner_pad, target, sep_style, bg));
            }
            self.lines.push(table_row(
                row, &widths, inner_pad, target, cell_style, bg, sep,
            ));
        }
        self.lines.push(blank_bg_line(target, bg));
        self.table_header.clear();
        self.table_body.clear();
        self.fresh_line = true;
    }

    fn render_code_block(&mut self, lang: &str, body: &str) {
        use unicode_width::UnicodeWidthStr;
        self.blank_line();
        let bg = block_bg();
        let target = self.rule_width as usize;
        let inner_pad = 2usize;
        let lang_label = if lang.is_empty() {
            "code".to_string()
        } else {
            lang.to_string()
        };
        let gutter = Style::default().fg(Color::DarkGray).bg(bg);
        let bg_only = Style::default().bg(bg);
        let lineno_style = Style::default()
            .fg(Color::DarkGray)
            .bg(bg)
            .add_modifier(Modifier::DIM);
        let header = format!("╭─ {lang_label} ─");
        self.lines.push(bg_padded_line(&header, gutter, target, bg));
        self.lines.push(blank_bg_line(target, bg));
        let highlighted = crate::highlight::highlight_code(lang, body);
        let width = digits_for(highlighted.len());
        for (i, hl) in highlighted.into_iter().enumerate() {
            let lineno = format!("{:>width$}  ", i + 1);
            let mut used = inner_pad + UnicodeWidthStr::width(lineno.as_str());
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(hl.spans.len() + 3);
            spans.push(Span::styled(" ".repeat(inner_pad), bg_only));
            spans.push(Span::styled(lineno, lineno_style));
            for src in hl.spans {
                used += UnicodeWidthStr::width(src.content.as_ref());
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
    use unicode_width::UnicodeWidthStr;
    let used = text.width();
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
    use unicode_width::UnicodeWidthStr;
    let bg_only = Style::default().bg(bg);
    let content_w = text.width();
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
    use unicode_width::UnicodeWidthStr;
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
        let pad = w.saturating_sub(cell.width());
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
    crate::theme::theme().code_bg
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H2 => Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H3 => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H4 => Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD),
        HeadingLevel::H5 | HeadingLevel::H6 => Style::default()
            .fg(Color::DarkGray)
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
    fn inline_code_wraps_with_backticks() {
        let lines = render_markdown("call `foo()` please\n");
        let flat = plain(&lines);
        assert!(flat.iter().any(|l| l.contains("`foo()`")), "{flat:?}");
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
            flat.iter().any(|l| l.contains("1") && l.contains("2")),
            "want data row: {flat:?}"
        );
        assert!(
            flat.iter().any(|l| l.contains("a") && l.contains("b")),
            "want header row: {flat:?}"
        );
    }

    #[test]
    fn table_preserves_inline_code_in_cells() {
        let lines = render_markdown("| name | code |\n| - | - |\n| foo | `bar()` |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter()
                .any(|l| l.contains("`bar()`") && l.contains("foo")),
            "inline code lost from table cell: {flat:?}"
        );
    }

    #[test]
    fn table_preserves_bold_and_italic_text_in_cells() {
        let lines = render_markdown("| A | B |\n| - | - |\n| **bold** run | plain |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter()
                .any(|l| l.contains("bold run") && l.contains("plain")),
            "bold text run split across cells: {flat:?}"
        );
    }

    #[test]
    fn table_keeps_empty_cell_columns_aligned() {
        let lines = render_markdown("| A | B | C |\n| - | - | - |\n| x |  | z |\n");
        let flat = plain(&lines);
        let data_row = flat
            .iter()
            .find(|l| l.contains("x") && l.contains("z"))
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
    fn empty_input_gives_empty_output() {
        assert!(render_markdown("").is_empty());
        assert!(render_markdown("\n\n").is_empty());
    }

    fn line_widths(lines: &[Line<'_>]) -> Vec<usize> {
        use unicode_width::UnicodeWidthStr;
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref().width())
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
}
