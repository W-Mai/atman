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
    table_row: Vec<String>,
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
                    self.table_row.push(text.into_string());
                    return;
                }
                self.push_text(&text);
            }
            Event::Code(text) => {
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
            Tag::TableRow => {
                self.table_row.clear();
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
                self.in_table = false;
                self.blank_line();
            }
            TagEnd::TableRow => {
                if self.table_row.is_empty() {
                    return;
                }
                let row = self
                    .table_row
                    .iter()
                    .map(|c| format!(" {c} "))
                    .collect::<Vec<_>>()
                    .join("│");
                self.lines.push(Line::from(vec![
                    Span::styled("│", Style::default().fg(Color::DarkGray)),
                    Span::raw(row),
                    Span::styled("│", Style::default().fg(Color::DarkGray)),
                ]));
                self.fresh_line = true;
                self.table_row.clear();
            }
            _ => {}
        }
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
        for line in body.lines() {
            self.lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(line.to_string(), Style::default().fg(Color::LightGreen)),
            ]));
        }
        self.lines.push(Line::from(Span::styled(
            "└─".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
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
            "{flat:?}"
        );
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
