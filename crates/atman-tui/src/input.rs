use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

pub fn cursor_display_row(input: &str, cursor: usize) -> u16 {
    let clamped = cursor.min(input.len());
    input[..clamped].matches('\n').count() as u16
}

pub fn cursor_display_col(input: &str, cursor: usize) -> u16 {
    use unicode_width::UnicodeWidthStr;
    let clamped = cursor.min(input.len());
    let head = &input[..clamped];
    let last_line = head.rsplit('\n').next().unwrap_or("");
    UnicodeWidthStr::width(last_line) as u16
}

pub fn input_paragraph<'a>(
    input: &'a str,
    _cursor: usize,
    streaming: bool,
    pending_below: u16,
    scroll_row: u16,
    trust: &'a atman_runtime::trust::TrustConfig,
) -> Paragraph<'a> {
    let prompt_style = if streaming {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let display = trust.display();
    let mode_color = match display.color {
        atman_runtime::trust::ModeColor::Cyan => Color::Cyan,
        atman_runtime::trust::ModeColor::Green => Color::Green,
        atman_runtime::trust::ModeColor::Yellow => Color::Yellow,
        atman_runtime::trust::ModeColor::Orange => Color::Rgb(208, 135, 22),
        atman_runtime::trust::ModeColor::Red => Color::Red,
    };
    let border_style = if streaming {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(mode_color)
    };
    let title = if pending_below > 0 {
        format!(
            " atman · {} {}  ↓ {pending_below} new ",
            display.emoji, display.name
        )
    } else {
        format!(" atman · {} {} ", display.emoji, display.name)
    };
    let title_span = Span::styled(
        title,
        Style::default().fg(mode_color).add_modifier(Modifier::BOLD),
    );
    let hint_right = if trust.mode == atman_runtime::trust::TrustMode::Eager {
        let od = trust.outside_display();
        format!(" outside: {} {} · Tab to cycle ", od.emoji, od.name)
    } else {
        " shift+enter · newline · enter · send ".to_string()
    };
    let hint_line = Line::from(Span::styled(
        hint_right,
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border_style)
        .title(title_span)
        .title_bottom(hint_line)
        .padding(ratatui::widgets::Padding::horizontal(1));

    let raw_lines: Vec<&str> = if input.is_empty() {
        vec![""]
    } else {
        input.split('\n').collect()
    };
    let mut lines: Vec<Line<'a>> = Vec::with_capacity(raw_lines.len());
    for (i, seg) in raw_lines.iter().enumerate() {
        let prefix = if i == 0 {
            Span::styled("❯ ", prompt_style)
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![prefix, Span::raw(*seg)]));
    }

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0))
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

pub fn wrapped_line_count(input: &str, content_width: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    if content_width == 0 {
        return input.split('\n').count().max(1);
    }
    let mut total = 0usize;
    for row in input.split('\n') {
        if row.is_empty() {
            total += 1;
            continue;
        }
        let mut cur_w = 0usize;
        let mut rows_in_line = 1usize;
        for ch in row.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if cur_w + cw > content_width {
                rows_in_line += 1;
                cur_w = cw;
            } else {
                cur_w += cw;
            }
        }
        total += rows_in_line;
    }
    total.max(1)
}

#[derive(Debug, Clone)]
pub struct PastedEntry {
    pub placeholder: String,
    pub content: String,
}

// Paste larger than these gets folded into a placeholder so the editor
// doesn't drown in a hundred-line dump. Numbers match Gemini CLI.
const PASTE_FOLD_LINE_THRESHOLD: usize = 5;
const PASTE_FOLD_CHAR_THRESHOLD: usize = 500;

#[derive(Default)]
pub struct InputEditor {
    buf: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    stashed: Option<String>,
    pending_pastes: Vec<PastedEntry>,
    next_paste_index: u32,
}

impl InputEditor {
    pub fn seed_history(&mut self, entries: Vec<String>) {
        for e in entries {
            if self.history.last().is_none_or(|prev| prev != &e) {
                self.history.push(e);
            }
        }
        self.history_idx = None;
        self.stashed = None;
    }

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

    pub fn move_line_up(&mut self) -> bool {
        use unicode_width::UnicodeWidthChar;
        let head = &self.buf[..self.cursor];
        let Some(cur_line_start_off) = head.rfind('\n') else {
            return false;
        };
        let cur_line_start = cur_line_start_off + 1;
        let cur_col: u16 = self.buf[cur_line_start..self.cursor]
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0) as u16)
            .sum();
        let prev_head = &self.buf[..cur_line_start_off];
        let prev_line_start = prev_head.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prev_line = &self.buf[prev_line_start..cur_line_start_off];
        let mut used: u16 = 0;
        let mut byte_off = prev_line_start;
        for c in prev_line.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0) as u16;
            if used.saturating_add(w) > cur_col {
                break;
            }
            used = used.saturating_add(w);
            byte_off += c.len_utf8();
            if used >= cur_col {
                break;
            }
        }
        self.cursor = byte_off;
        true
    }

    pub fn move_line_down(&mut self) -> bool {
        use unicode_width::UnicodeWidthChar;
        let tail = &self.buf[self.cursor..];
        let Some(rel) = tail.find('\n') else {
            return false;
        };
        let cur_line_start = self.buf[..self.cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let cur_col: u16 = self.buf[cur_line_start..self.cursor]
            .chars()
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0) as u16)
            .sum();
        let next_line_start = self.cursor + rel + 1;
        let next_line_end = self.buf[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.buf.len());
        let next_line = &self.buf[next_line_start..next_line_end];
        let mut used: u16 = 0;
        let mut byte_off = next_line_start;
        for c in next_line.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0) as u16;
            if used.saturating_add(w) > cur_col {
                break;
            }
            used = used.saturating_add(w);
            byte_off += c.len_utf8();
            if used >= cur_col {
                break;
            }
        }
        self.cursor = byte_off;
        true
    }

    // Walks by char width, not char count, so double-wide CJK chars occupy
    // the two display columns they visually take.
    pub fn set_cursor_by_display(&mut self, line: usize, display_col: u16) {
        use unicode_width::UnicodeWidthChar;
        self.consume_history_view();
        let mut line_start = 0usize;
        for _ in 0..line {
            match self.buf[line_start..].find('\n') {
                Some(nl) => line_start += nl + 1,
                None => {
                    self.cursor = self.buf.len();
                    return;
                }
            }
        }
        let line_end = self.buf[line_start..]
            .find('\n')
            .map(|n| line_start + n)
            .unwrap_or(self.buf.len());
        let slice = &self.buf[line_start..line_end];
        let mut used: u16 = 0;
        let mut byte_offset = line_start;
        for c in slice.chars() {
            let w = UnicodeWidthChar::width(c).unwrap_or(0) as u16;
            if used.saturating_add(w) > display_col {
                break;
            }
            used = used.saturating_add(w);
            byte_offset += c.len_utf8();
            if used >= display_col {
                break;
            }
        }
        self.cursor = byte_offset;
    }

    pub fn clear(&mut self) -> String {
        self.consume_history_view();
        self.cursor = 0;
        std::mem::take(&mut self.buf)
    }

    pub fn replace_with(&mut self, text: &str) {
        self.consume_history_view();
        self.buf.clear();
        self.buf.push_str(text);
        self.cursor = self.buf.len();
    }

    pub fn prefill(&mut self, prefix: &str) {
        self.consume_history_view();
        if !self.buf.starts_with(prefix) {
            self.buf.insert_str(0, prefix);
            self.cursor += prefix.len();
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let raw = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.history_idx = None;
        self.stashed = None;
        let pending = std::mem::take(&mut self.pending_pastes);
        self.next_paste_index = 0;
        let mut line = raw;
        for PastedEntry {
            placeholder,
            content,
        } in &pending
        {
            line = line.replacen(placeholder, content, 1);
        }
        if line.trim().is_empty() {
            return None;
        }
        if self.history.last().is_none_or(|prev| prev != &line) {
            self.history.push(line.clone());
        }
        Some(line)
    }

    pub fn expand_paste_at_cursor(&mut self) -> bool {
        let hit = self.pending_pastes.iter().enumerate().find_map(|(i, p)| {
            self.buf.find(&p.placeholder).and_then(|start| {
                let end = start + p.placeholder.len();
                if self.cursor >= start && self.cursor <= end {
                    Some((i, start, end))
                } else {
                    None
                }
            })
        });
        let Some((idx, start, end)) = hit else {
            return false;
        };
        let entry = self.pending_pastes.remove(idx);
        self.buf.replace_range(start..end, &entry.content);
        self.cursor = start + entry.content.len();
        true
    }

    pub fn ingest_paste(&mut self, raw: &str) {
        let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
        let line_count = normalized.matches('\n').count() + 1;
        let char_count = normalized.chars().count();
        if line_count > PASTE_FOLD_LINE_THRESHOLD || char_count > PASTE_FOLD_CHAR_THRESHOLD {
            let placeholder = self.next_paste_placeholder(line_count, char_count);
            self.insert_str(&placeholder);
            self.pending_pastes.push(PastedEntry {
                placeholder,
                content: normalized,
            });
        } else {
            self.insert_str(&normalized);
        }
    }

    fn next_paste_placeholder(&mut self, lines: usize, chars: usize) -> String {
        self.next_paste_index += 1;
        let idx = self.next_paste_index;
        let suffix = if idx == 1 {
            String::new()
        } else {
            format!(" #{idx}")
        };
        format!("[Pasted Text: {lines} lines, {chars} chars{suffix}]")
    }

    pub fn pending_pastes(&self) -> &[PastedEntry] {
        &self.pending_pastes
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
    fn set_cursor_by_display_ascii_first_line() {
        let mut ed = InputEditor::default();
        ed.insert_str("hello world");
        ed.set_cursor_by_display(0, 6);
        assert_eq!(&ed.buf()[..ed.cursor()], "hello ");
    }

    #[test]
    fn set_cursor_by_display_second_line() {
        let mut ed = InputEditor::default();
        ed.insert_str("first\nsecond");
        ed.set_cursor_by_display(1, 3);
        assert_eq!(&ed.buf()[..ed.cursor()], "first\nsec");
    }

    #[test]
    fn set_cursor_by_display_past_end_of_line_clamps_to_line_end() {
        let mut ed = InputEditor::default();
        ed.insert_str("short\nlonger line");
        ed.set_cursor_by_display(0, 99);
        assert_eq!(&ed.buf()[..ed.cursor()], "short");
    }

    #[test]
    fn set_cursor_by_display_past_last_line_clamps_to_buf_end() {
        let mut ed = InputEditor::default();
        ed.insert_str("only\nline");
        ed.set_cursor_by_display(5, 0);
        assert_eq!(ed.cursor(), ed.buf().len());
    }

    #[test]
    fn short_paste_is_inserted_verbatim() {
        let mut ed = InputEditor::default();
        ed.ingest_paste("just one line");
        assert_eq!(ed.buf(), "just one line");
        assert!(ed.pending_pastes().is_empty());
    }

    #[test]
    fn multiline_paste_at_threshold_stays_inline() {
        let mut ed = InputEditor::default();
        ed.ingest_paste("a\nb\nc\nd\ne");
        assert_eq!(ed.buf(), "a\nb\nc\nd\ne");
        assert!(ed.pending_pastes().is_empty());
    }

    #[test]
    fn long_paste_folds_into_placeholder() {
        let mut ed = InputEditor::default();
        let big = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        ed.ingest_paste(&big);
        assert!(ed.buf().starts_with("[Pasted Text: 10 lines"));
        assert_eq!(ed.pending_pastes().len(), 1);
        assert_eq!(ed.pending_pastes()[0].content, big);
    }

    #[test]
    fn wide_paste_over_500_chars_folds() {
        let mut ed = InputEditor::default();
        let big = "x".repeat(600);
        ed.ingest_paste(&big);
        assert!(ed.buf().starts_with("[Pasted Text: 1 lines, 600 chars"));
        assert_eq!(ed.pending_pastes()[0].content, big);
    }

    #[test]
    fn submit_expands_placeholder_back_into_content() {
        let mut ed = InputEditor::default();
        ed.insert_str("before\n");
        ed.ingest_paste("A\nB\nC\nD\nE\nF");
        ed.insert_str("\nafter");
        let out = ed.submit().unwrap();
        assert!(out.starts_with("before\n"));
        assert!(out.contains("A\nB\nC\nD\nE\nF"));
        assert!(out.ends_with("\nafter"));
        assert!(!out.contains("Pasted Text"));
    }

    #[test]
    fn multiple_pastes_get_indexed_placeholders() {
        let mut ed = InputEditor::default();
        ed.ingest_paste("aaa\nbbb\nccc\nddd\neee\nfff");
        ed.ingest_paste("111\n222\n333\n444\n555\n666");
        assert_eq!(ed.pending_pastes().len(), 2);
        assert!(ed.pending_pastes()[0].placeholder.contains("6 lines"));
        assert!(ed.pending_pastes()[1].placeholder.contains("#2"));
    }

    #[test]
    fn paste_normalizes_crlf() {
        let mut ed = InputEditor::default();
        ed.ingest_paste("a\r\nb\r\nc\r\nd\r\ne\r\nf");
        let out = ed.submit().unwrap();
        assert_eq!(out, "a\nb\nc\nd\ne\nf");
    }

    #[test]
    fn set_cursor_by_display_cjk_double_width() {
        let mut ed = InputEditor::default();
        ed.insert_str("你好world");
        // 你(2) 好(2) w(1) → display col 5 lands right before 'o'.
        ed.set_cursor_by_display(0, 5);
        assert_eq!(&ed.buf()[..ed.cursor()], "你好w");
    }

    #[test]
    fn display_width_handles_cjk() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width("你好"), 4);
        assert_eq!(display_width("a\n你好"), 4);
    }

    #[test]
    fn wrapped_line_count_short_fits() {
        assert_eq!(wrapped_line_count("hello", 50), 1);
    }

    #[test]
    fn wrapped_line_count_wraps_long_ascii() {
        let s = "aaaaa bbbbb ccccc ddddd eeeee fffff ggggg hhhhh";
        let n = wrapped_line_count(s, 10);
        assert!(n > 1, "should wrap: {n}");
    }

    #[test]
    fn wrapped_line_count_wraps_cjk() {
        let s = "读取文件内容并做分析的一个非常长的中文标题名称";
        let n = wrapped_line_count(s, 10);
        assert!(n > 1, "CJK should wrap: {n}");
    }

    #[test]
    fn wrapped_line_count_preserves_newlines() {
        let s = "line one\nline two\nline three";
        let n = wrapped_line_count(s, 50);
        assert_eq!(n, 3);
    }

    #[test]
    fn wrapped_line_count_empty_row_counts() {
        assert_eq!(wrapped_line_count("a\n\nb", 50), 3);
        assert_eq!(wrapped_line_count("", 50), 1);
    }
}
