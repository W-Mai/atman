use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

pub fn render_markdown(md: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut renderer = Renderer::default();
    for ev in parser {
        renderer.consume(ev);
    }
    renderer.finish()
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    current: Vec<Span<'static>>,
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
}

#[derive(Clone, Copy)]
enum ListKind {
    Bullet,
    Ordered(u64),
}

impl Renderer {
    fn active_style(&self) -> Style {
        self.style_stack
            .iter()
            .copied()
            .fold(Style::default(), merge_style)
    }

    fn push_text(&mut self, text: &str) {
        let style = self.active_style();
        self.current.push(Span::styled(text.to_string(), style));
        self.fresh_line = false;
    }

    fn end_line(&mut self) {
        if self.current.is_empty() && self.fresh_line {
            return;
        }
        let spans = std::mem::take(&mut self.current);
        self.lines.push(Line::from(spans));
        self.fresh_line = true;
    }

    fn blank_line(&mut self) {
        if !self.fresh_line {
            self.end_line();
        }
        self.lines.push(Line::from(""));
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
                let style = merge_style(
                    self.active_style(),
                    Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                );
                self.current.push(Span::styled(format!("`{text}`"), style));
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
                    self.push_text(&indent);
                }
            }
            Event::Rule => {
                self.blank_line();
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(60),
                    Style::default().fg(Color::DarkGray),
                )));
                self.fresh_line = true;
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "[x] " } else { "[ ] " };
                let style = Style::default().fg(Color::Cyan);
                self.current.push(Span::styled(mark.to_string(), style));
                self.fresh_line = false;
            }
            _ => {}
        }
    }

    fn enter(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    self.push_text(&indent);
                }
            }
            Tag::Heading { level, .. } => {
                self.heading_level = Some(level);
                let hashes = "#".repeat(heading_hashes(level));
                let style = Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD);
                self.current.push(Span::styled(format!("{hashes} "), style));
                self.style_stack.push(style);
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
                let indent = self.indent_prefix();
                if !indent.is_empty() {
                    self.push_text(&indent);
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
                self.current.push(Span::styled(bullet, style));
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
            TagEnd::Heading(_) => {
                self.style_stack.pop();
                self.heading_level = None;
                self.end_line();
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
        let mut widths = vec![0usize; col_count];
        for (i, cell) in self.table_header.iter().enumerate() {
            widths[i] = widths[i].max(cell.width());
        }
        for row in &self.table_body {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.width());
            }
        }
        let border_style = Style::default().fg(Color::DarkGray);
        let head_style = Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD);
        let top = format!("┌{}┐", horizontal_border(&widths, '┬'));
        let mid = format!("├{}┤", horizontal_border(&widths, '┼'));
        let bot = format!("└{}┘", horizontal_border(&widths, '┴'));

        self.lines.push(Line::from(Span::styled(top, border_style)));
        if !self.table_header.is_empty() {
            self.lines.push(row_line(
                &self.table_header,
                &widths,
                head_style,
                border_style,
            ));
            self.lines.push(Line::from(Span::styled(mid, border_style)));
        }
        for row in &self.table_body {
            self.lines
                .push(row_line(row, &widths, Style::default(), border_style));
        }
        self.lines.push(Line::from(Span::styled(bot, border_style)));
        self.table_header.clear();
        self.table_body.clear();
        self.fresh_line = true;
    }

    fn render_code_block(&mut self, lang: &str, body: &str) {
        self.blank_line();
        let lang_label = if lang.is_empty() {
            "code".to_string()
        } else {
            lang.to_string()
        };
        self.lines.push(Line::from(Span::styled(
            format!("┌─ {lang_label} ─"),
            Style::default().fg(Color::DarkGray),
        )));
        let gutter = Style::default().fg(Color::DarkGray);
        for hl in crate::highlight::highlight_code(lang, body) {
            let mut spans = Vec::with_capacity(hl.spans.len() + 1);
            spans.push(Span::styled("│ ", gutter));
            spans.extend(hl.spans);
            self.lines.push(Line::from(spans));
        }
        self.lines
            .push(Line::from(Span::styled("└─".to_string(), gutter)));
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

fn horizontal_border(widths: &[usize], junction: char) -> String {
    let mut out = String::new();
    for (i, w) in widths.iter().enumerate() {
        if i > 0 {
            out.push(junction);
        }
        out.push_str(&"─".repeat(w + 2));
    }
    out
}

fn row_line(
    cells: &[String],
    widths: &[usize],
    cell_style: Style,
    border_style: Style,
) -> Line<'static> {
    use unicode_width::UnicodeWidthStr;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(widths.len() * 2 + 1);
    spans.push(Span::styled("│", border_style));
    for (i, w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        let pad = w.saturating_sub(cell.width());
        spans.push(Span::styled(
            format!(" {cell}{} ", " ".repeat(pad)),
            cell_style,
        ));
        spans.push(Span::styled("│", border_style));
    }
    Line::from(spans)
}

fn heading_hashes(level: HeadingLevel) -> usize {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
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
    fn heading_gets_hash_prefix_and_bold_style() {
        let lines = render_markdown("# Title\n\nbody\n");
        let flat = plain(&lines);
        assert!(flat[0].starts_with("# Title"), "got {:?}", flat);
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
    fn table_renders_row_with_pipe_separators() {
        let lines = render_markdown("| a | b |\n| - | - |\n| 1 | 2 |\n");
        let flat = plain(&lines);
        assert!(
            flat.iter()
                .any(|l| l.contains("│") && l.contains("1") && l.contains("2")),
            "want data row: {flat:?}"
        );
        assert!(
            flat.iter()
                .any(|l| l.contains("│") && l.contains(" a ") && l.contains(" b ")),
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
}
