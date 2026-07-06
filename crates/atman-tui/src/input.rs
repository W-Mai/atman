use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

pub fn input_paragraph<'a>(
    input: &'a str,
    cursor: usize,
    streaming: bool,
    pending_below: u16,
) -> Paragraph<'a> {
    let prompt_style = if streaming {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let border_style = if streaming {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };
    let title = if pending_below > 0 {
        format!(" atman  ↓ {pending_below} new ")
    } else {
        " atman ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);

    let (before, after) = split_at_cursor(input, cursor);
    let mut lines: Vec<Line<'a>> = Vec::new();
    let before_lines: Vec<&str> = if before.is_empty() {
        vec![""]
    } else {
        before.split('\n').collect()
    };
    let after_lines: Vec<&str> = if after.is_empty() {
        vec![""]
    } else {
        after.split('\n').collect()
    };
    let last_before = before_lines.len() - 1;

    for (i, seg) in before_lines.iter().enumerate() {
        let mut spans: Vec<Span<'a>> = Vec::new();
        if i == 0 {
            spans.push(Span::styled("❯ ", prompt_style));
        } else {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::raw(*seg));
        if i == last_before {
            spans.push(Span::styled(
                "▏",
                Style::default().add_modifier(Modifier::SLOW_BLINK),
            ));
            spans.push(Span::raw(after_lines[0]));
            lines.push(Line::from(spans));
            for tail in &after_lines[1..] {
                lines.push(Line::from(vec![Span::raw("  "), Span::raw(*tail)]));
            }
        } else {
            lines.push(Line::from(spans));
        }
    }

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
}

fn split_at_cursor(s: &str, cursor: usize) -> (&str, &str) {
    let clamped = cursor.min(s.len());
    let mut safe = clamped;
    while safe > 0 && !s.is_char_boundary(safe) {
        safe -= 1;
    }
    s.split_at(safe)
}

pub fn display_line_count(input: &str) -> usize {
    input.split('\n').count()
}

pub fn display_width(input: &str) -> usize {
    input
        .split('\n')
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
}

#[derive(Default)]
pub struct InputEditor {
    buf: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    stashed: Option<String>,
}

impl InputEditor {
    pub fn buf(&self) -> &str {
        &self.buf
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        self.consume_history_view();
        self.buf.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    pub fn insert_str(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.consume_history_view();
        self.buf.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    pub fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    pub fn backspace(&mut self) {
        self.consume_history_view();
        if self.cursor == 0 {
            return;
        }
        let mut prev = self.cursor - 1;
        while prev > 0 && !self.buf.is_char_boundary(prev) {
            prev -= 1;
        }
        self.buf.drain(prev..self.cursor);
        self.cursor = prev;
    }

    pub fn delete_word_backward(&mut self) {
        self.consume_history_view();
        if self.cursor == 0 {
            return;
        }
        let target = word_boundary_backward(&self.buf, self.cursor);
        self.buf.drain(target..self.cursor);
        self.cursor = target;
    }

    pub fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut prev = self.cursor - 1;
        while prev > 0 && !self.buf.is_char_boundary(prev) {
            prev -= 1;
        }
        self.cursor = prev;
    }

    pub fn move_right(&mut self) {
        if self.cursor >= self.buf.len() {
            return;
        }
        let mut next = self.cursor + 1;
        while next < self.buf.len() && !self.buf.is_char_boundary(next) {
            next += 1;
        }
        self.cursor = next;
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.buf.len();
    }

    pub fn clear(&mut self) -> String {
        self.consume_history_view();
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn prefill(&mut self, prefix: &str) {
        self.consume_history_view();
        if !self.buf.starts_with(prefix) {
            self.buf.insert_str(0, prefix);
            self.cursor += prefix.len();
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.history_idx = None;
        self.stashed = None;
        if line.trim().is_empty() {
            return None;
        }
        if self.history.last().is_none_or(|prev| prev != &line) {
            self.history.push(line.clone());
        }
        Some(line)
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => {
                if self.stashed.is_none() {
                    self.stashed = Some(self.buf.clone());
                }
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(new_idx);
        self.buf = self.history[new_idx].clone();
        self.cursor = self.buf.len();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_idx else {
            return;
        };
        if i + 1 >= self.history.len() {
            self.history_idx = None;
            self.buf = self.stashed.take().unwrap_or_default();
        } else {
            self.history_idx = Some(i + 1);
            self.buf = self.history[i + 1].clone();
        }
        self.cursor = self.buf.len();
    }

    fn consume_history_view(&mut self) {
        self.history_idx = None;
    }
}

fn word_boundary_backward(s: &str, cursor: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = cursor;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_backspace_maintain_cursor() {
        let mut ed = InputEditor::default();
        ed.insert_char('h');
        ed.insert_char('i');
        assert_eq!(ed.buf(), "hi");
        assert_eq!(ed.cursor(), 2);
        ed.backspace();
        assert_eq!(ed.buf(), "h");
        assert_eq!(ed.cursor(), 1);
    }

    #[test]
    fn move_left_right_over_ascii() {
        let mut ed = InputEditor::default();
        "hello".chars().for_each(|c| ed.insert_char(c));
        assert_eq!(ed.cursor(), 5);
        ed.move_left();
        assert_eq!(ed.cursor(), 4);
        ed.move_home();
        assert_eq!(ed.cursor(), 0);
        ed.move_right();
        assert_eq!(ed.cursor(), 1);
        ed.move_end();
        assert_eq!(ed.cursor(), 5);
    }

    #[test]
    fn move_left_over_multibyte() {
        let mut ed = InputEditor::default();
        "你好".chars().for_each(|c| ed.insert_char(c));
        assert_eq!(ed.cursor(), 6);
        ed.move_left();
        assert_eq!(ed.cursor(), 3);
        ed.move_left();
        assert_eq!(ed.cursor(), 0);
        ed.move_right();
        assert_eq!(ed.cursor(), 3);
    }

    #[test]
    fn insert_at_middle_shifts_cursor() {
        let mut ed = InputEditor::default();
        "ac".chars().for_each(|c| ed.insert_char(c));
        ed.move_left();
        ed.insert_char('b');
        assert_eq!(ed.buf(), "abc");
        assert_eq!(ed.cursor(), 2);
    }

    #[test]
    fn insert_str_paste_multiline() {
        let mut ed = InputEditor::default();
        ed.insert_str("line1\nline2\nline3");
        assert_eq!(ed.buf(), "line1\nline2\nline3");
        assert_eq!(display_line_count(ed.buf()), 3);
    }

    #[test]
    fn newline_inserts_and_keeps_editing() {
        let mut ed = InputEditor::default();
        ed.insert_str("hi");
        ed.insert_newline();
        ed.insert_str("bye");
        assert_eq!(ed.buf(), "hi\nbye");
        assert_eq!(display_line_count(ed.buf()), 2);
    }

    #[test]
    fn delete_word_backward_removes_ascii_word() {
        let mut ed = InputEditor::default();
        ed.insert_str("hello world");
        ed.delete_word_backward();
        assert_eq!(ed.buf(), "hello ");
        ed.delete_word_backward();
        assert_eq!(ed.buf(), "");
    }

    #[test]
    fn history_up_stashes_wip_and_restores_on_down() {
        let mut ed = InputEditor::default();
        "one".chars().for_each(|c| ed.insert_char(c));
        ed.submit();
        "wip text".chars().for_each(|c| ed.insert_char(c));
        ed.history_up();
        assert_eq!(ed.buf(), "one");
        ed.history_down();
        assert_eq!(ed.buf(), "wip text");
    }

    #[test]
    fn typing_while_browsing_history_keeps_stashed_intact() {
        let mut ed = InputEditor::default();
        "one".chars().for_each(|c| ed.insert_char(c));
        ed.submit();
        "wip".chars().for_each(|c| ed.insert_char(c));
        ed.history_up();
        assert_eq!(ed.buf(), "one");
        ed.insert_char('X');
        ed.history_up();
        assert_eq!(ed.buf(), "one");
        ed.history_down();
        assert_eq!(ed.buf(), "wip");
    }

    #[test]
    fn display_width_handles_cjk() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("a\n你好"), 4);
    }
}
