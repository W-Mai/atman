use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
use std::collections::VecDeque;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::ModeColorExt;

pub fn input_paragraph<'a>(
    input: &'a str,
    _cursor: usize,
    border_color: ratatui::style::Color,
    pending_below: u16,
    scroll_row: u16,
    trust: &'a atman_runtime::trust::TrustConfig,
) -> Paragraph<'a> {
    let display = trust.display();
    let mode_color = display.color.ratatui();
    let border_style = Style::default().fg(border_color);
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
    let t = crate::theme::theme();
    let hint_line = Line::from(Span::styled(
        hint_right,
        Style::default().fg(t.subtle_fg.into()),
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
    for seg in &raw_lines {
        lines.push(Line::from(vec![Span::raw(*seg)]));
    }

    Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll_row, 0))
}

#[derive(Debug, Clone)]
struct WrappedLine {
    byte_range: (usize, usize),
}

const NBSP: &str = "\u{00a0}";
const ZWSP: &str = "\u{200b}";

fn cell_width(g: &str) -> u16 {
    if g.len() == 1 {
        if g.as_bytes()[0].is_ascii_control() {
            return 0;
        }
        return 1;
    }
    let w = UnicodeWidthStr::width(g) as u16;
    w + g
        .chars()
        .filter(|c| matches!(*c, '\u{FF9E}' | '\u{FF9F}'))
        .count() as u16
}

fn is_ws(g: &str) -> bool {
    g == ZWSP || (g.chars().all(char::is_whitespace) && g != NBSP)
}

fn compute_wrapped_lines(input: &str, content_width: usize) -> Vec<WrappedLine> {
    let max_w = content_width.max(1) as u16;
    let mut lines: Vec<WrappedLine> = Vec::new();

    if input.is_empty() {
        lines.push(WrappedLine { byte_range: (0, 0) });
        return lines;
    }

    let mut byte_offset = 0usize;

    for logical in input.split('\n') {
        let logical_start = byte_offset;
        let logical_end = logical_start + logical.len();
        let lines_before = lines.len();

        // State mirrors ratatui WordWrapper::process_input (trim=false)
        let mut line_start: Option<usize> = None;
        let mut line_end = logical_start;
        let mut line_width: u16 = 0;
        let mut word_start: Option<usize> = None;
        let mut word_end = logical_start;
        let mut word_width: u16 = 0;
        let mut ws_q: VecDeque<(usize, usize, u16)> = VecDeque::new();
        let mut ws_width: u16 = 0;
        let mut non_ws_prev = false;

        for (g_off, g) in logical.grapheme_indices(true) {
            let g_bs = logical_start + g_off;
            let g_be = g_bs + g.len();

            if g.contains(char::is_control) {
                continue;
            }

            let gw = cell_width(g);
            let g_ws = is_ws(g);

            if gw > max_w {
                continue;
            }

            let word_found = non_ws_prev && g_ws;
            let untrimmed_overflow = line_start.is_none() && word_width + ws_width + gw > max_w;

            if word_found || untrimmed_overflow {
                if line_start.is_none() {
                    line_start = ws_q.front().map(|&(s, _, _)| s).or(word_start);
                }
                if !ws_q.is_empty() {
                    line_end = ws_q.back().unwrap().1;
                    line_width += ws_width;
                    ws_q.clear();
                    ws_width = 0;
                }
                if word_start.is_some() {
                    line_end = word_end;
                    line_width += word_width;
                    word_start = None;
                    word_width = 0;
                }
            }

            let line_full = line_width >= max_w;
            let pending_word_overflow = gw > 0 && line_width + ws_width + word_width >= max_w;

            if line_full || pending_word_overflow {
                let start = line_start.unwrap_or(line_end);
                lines.push(WrappedLine {
                    byte_range: (start, line_end),
                });

                let mut remaining = max_w.saturating_sub(line_width);
                line_width = 0;
                line_start = None;
                line_end = g_bs;

                while let Some(&(_, _, w)) = ws_q.front() {
                    if w > remaining {
                        break;
                    }
                    remaining -= w;
                    ws_width -= w;
                    ws_q.pop_front();
                }

                if g_ws && ws_q.is_empty() {
                    non_ws_prev = false;
                    continue;
                }
            }

            if g_ws {
                ws_q.push_back((g_bs, g_be, gw));
                ws_width += gw;
            } else {
                if word_start.is_none() {
                    word_start = Some(g_bs);
                }
                word_end = g_be;
                word_width += gw;
            }

            non_ws_prev = !g_ws;
        }

        // Flush remaining for this logical line
        if line_start.is_none() {
            line_start = ws_q.front().map(|&(s, _, _)| s).or(word_start);
        }
        if !ws_q.is_empty() {
            line_end = ws_q.back().unwrap().1;
        }
        if word_start.is_some() {
            line_end = word_end;
        }

        if let Some(ls) = line_start {
            lines.push(WrappedLine {
                byte_range: (ls, line_end),
            });
        } else if lines.len() == lines_before {
            lines.push(WrappedLine {
                byte_range: (logical_start, logical_start),
            });
        }

        byte_offset = logical_end + 1; // +1 for '\n'
    }

    if lines.is_empty() {
        lines.push(WrappedLine { byte_range: (0, 0) });
    }

    lines
}

/// Given a visual (row, col) from a mouse click, compute the byte offset
/// into the input string. Clamps to valid positions.
pub fn cursor_from_wrapped(
    input: &str,
    visual_row: usize,
    visual_col: usize,
    content_width: usize,
) -> usize {
    let lines = compute_wrapped_lines(input, content_width);
    if lines.is_empty() {
        return 0;
    }
    if visual_row >= lines.len() {
        return input.len();
    }
    let idx = visual_row.min(lines.len() - 1);
    let line = &lines[idx];
    let slice = &input[line.byte_range.0..line.byte_range.1];
    let mut byte_offset = line.byte_range.0;
    let mut used: usize = 0;
    for g in slice.graphemes(true) {
        let gw = cell_width(g) as usize;
        if used + gw > visual_col {
            break;
        }
        used += gw;
        byte_offset += g.len();
        if used >= visual_col {
            break;
        }
    }
    byte_offset
}

/// Which visual (wrapped) row the cursor is on, 0-based.
pub fn wrapped_cursor_row(input: &str, cursor: usize, content_width: usize) -> usize {
    let cursor = cursor.min(input.len());
    let lines = compute_wrapped_lines(input, content_width);
    if lines.is_empty() {
        return 0;
    }
    for (i, line) in lines.iter().enumerate() {
        if cursor >= line.byte_range.0 && cursor < line.byte_range.1 {
            return i;
        }
    }
    // Cursor in a gap (at a `\n`) — assign to the preceding line.
    for (i, line) in lines.iter().enumerate().rev() {
        if cursor >= line.byte_range.1 {
            return i;
        }
    }
    0
}

/// Visual column within the current wrapped row, 0-based.
pub fn wrapped_cursor_col(input: &str, cursor: usize, content_width: usize) -> usize {
    let cursor = cursor.min(input.len());
    let lines = compute_wrapped_lines(input, content_width);
    for line in &lines {
        if cursor >= line.byte_range.0 && cursor < line.byte_range.1 {
            let slice = &input[line.byte_range.0..cursor];
            return slice.graphemes(true).map(cell_width).sum::<u16>() as usize;
        }
    }
    for line in lines.iter().rev() {
        if cursor >= line.byte_range.1 {
            return input[line.byte_range.0..line.byte_range.1]
                .graphemes(true)
                .map(cell_width)
                .sum::<u16>() as usize;
        }
    }
    0
}

pub fn visual_line_count(input: &str, content_width: usize) -> usize {
    compute_wrapped_lines(input, content_width).len().max(1)
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

    /// Set cursor to a specific byte position (clamped).
    pub fn set_cursor(&mut self, pos: usize) {
        self.consume_history_view();
        self.cursor = pos.min(self.buf.len());
    }

    pub fn move_line_up(&mut self) -> bool {
        let effective =
            if self.cursor > 0 && self.buf.as_bytes().get(self.cursor).copied() == Some(b'\n') {
                self.cursor - 1
            } else {
                self.cursor
            };
        let head = &self.buf[..effective];
        let Some(cur_line_start_off) = head.rfind('\n') else {
            return false;
        };
        let cur_line_start = cur_line_start_off + 1;
        let cur_col: u16 = crate::width::width(&self.buf[cur_line_start..self.cursor]) as u16;
        let prev_head = &self.buf[..cur_line_start_off];
        let prev_line_start = prev_head.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prev_line = &self.buf[prev_line_start..cur_line_start_off];
        let mut used: u16 = 0;
        let mut byte_off = prev_line_start;
        for (g, gw) in crate::width::graphemes(prev_line) {
            if used.saturating_add(gw as u16) > cur_col {
                break;
            }
            used = used.saturating_add(gw as u16);
            byte_off += g.len();
            if used >= cur_col {
                break;
            }
        }
        self.cursor = byte_off;
        true
    }

    pub fn move_line_down(&mut self) -> bool {
        let tail = &self.buf[self.cursor..];
        let Some(rel) = tail.find('\n') else {
            return false;
        };
        let cur_line_start = self.buf[..self.cursor]
            .rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);
        let cur_col: u16 = crate::width::width(&self.buf[cur_line_start..self.cursor]) as u16;
        let next_line_start = self.cursor + rel + 1;
        let next_line_end = self.buf[next_line_start..]
            .find('\n')
            .map(|p| next_line_start + p)
            .unwrap_or(self.buf.len());
        let next_line = &self.buf[next_line_start..next_line_end];
        let mut used: u16 = 0;
        let mut byte_off = next_line_start;
        for (g, gw) in crate::width::graphemes(next_line) {
            if used.saturating_add(gw as u16) > cur_col {
                break;
            }
            used = used.saturating_add(gw as u16);
            byte_off += g.len();
            if used >= cur_col {
                break;
            }
        }
        self.cursor = byte_off;
        true
    }

    /// Move cursor up one visual (wrapped) line, keeping the same column.
    /// Returns false when already on the first visual row.
    pub fn move_line_up_visual(&mut self, cw: usize) -> bool {
        let cur_row = wrapped_cursor_row(&self.buf, self.cursor, cw);
        if cur_row == 0 {
            return false;
        }
        let cur_col = wrapped_cursor_col(&self.buf, self.cursor, cw);
        let target_row = cur_row - 1;
        self.cursor = cursor_from_wrapped(&self.buf, target_row, cur_col, cw);
        true
    }

    /// Move cursor down one visual (wrapped) line, keeping the same column.
    /// Returns false when already on the last visual row.
    pub fn move_line_down_visual(&mut self, cw: usize) -> bool {
        let total = visual_line_count(&self.buf, cw);
        let cur_row = wrapped_cursor_row(&self.buf, self.cursor, cw);
        if cur_row + 1 >= total {
            return false;
        }
        let cur_col = wrapped_cursor_col(&self.buf, self.cursor, cw);
        let target_row = cur_row + 1;
        self.cursor = cursor_from_wrapped(&self.buf, target_row, cur_col, cw);
        true
    }

    // Walks by char width, not char count, so double-wide CJK chars occupy
    // the two display columns they visually take.
    pub fn set_cursor_by_display(&mut self, line: usize, display_col: u16) {
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
        for (g, gw) in crate::width::graphemes(slice) {
            if used.saturating_add(gw as u16) > display_col {
                break;
            }
            used = used.saturating_add(gw as u16);
            byte_offset += g.len();
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
        assert_eq!(ed.buf().split('\n').count(), 3);
    }

    #[test]
    fn newline_inserts_and_keeps_editing() {
        let mut ed = InputEditor::default();
        ed.insert_str("hi");
        ed.insert_newline();
        ed.insert_str("bye");
        assert_eq!(ed.buf(), "hi\nbye");
        assert_eq!(ed.buf().split('\n').count(), 2);
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
    fn cursor_from_wrapped_single_line_col_zero() {
        assert_eq!(cursor_from_wrapped("hello", 0, 0, 50), 0);
    }

    #[test]
    fn cursor_from_wrapped_single_line_mid() {
        // "hello" → click at col 2 should land at byte 2 (after "he")
        assert_eq!(cursor_from_wrapped("hello", 0, 2, 50), 2);
    }

    #[test]
    fn cursor_from_wrapped_single_line_end() {
        assert_eq!(cursor_from_wrapped("hello", 0, 5, 50), 5);
    }

    #[test]
    fn cursor_from_wrapped_past_end_of_line_clamps_to_line_end() {
        // click at col 99 on a 5-char line → clamp to end of line
        assert_eq!(cursor_from_wrapped("hello", 0, 99, 50), 5);
    }

    #[test]
    fn cursor_from_wrapped_past_last_visual_row_clamps_to_buf_end() {
        assert_eq!(cursor_from_wrapped("hello", 10, 0, 50), 5);
    }

    #[test]
    fn cursor_from_wrapped_wraps_to_second_visual_row() {
        assert_eq!(cursor_from_wrapped("hello world", 1, 0, 10), 6);
    }

    #[test]
    fn cursor_from_wrapped_wraps_to_mid_of_second_visual_row() {
        // "hello world" at width 10 → row 0: "hello ", row 1: "world"
        // Click row 1 col 2 → "wo" → cursor at byte 6 + 2 = 8
        assert_eq!(cursor_from_wrapped("hello world", 1, 2, 10), 8);
    }

    #[test]
    fn cursor_from_wrapped_cjk_double_width() {
        // "你好world" at width 50 → one visual line
        // Click at col 4 (after "你好") → cursor at byte 6 (你=3, 好=3)
        assert_eq!(cursor_from_wrapped("你好world", 0, 4, 50), 6);
    }

    #[test]
    fn cursor_from_wrapped_multiple_newlines() {
        // "a\nb\nc" → 3 logical lines, 3 visual lines at any width
        assert_eq!(cursor_from_wrapped("a\nb\nc", 0, 1, 50), 1); // after "a\n" → 'b'
        assert_eq!(cursor_from_wrapped("a\nb\nc", 2, 1, 50), 5); // end of buf
    }

    #[test]
    fn cursor_from_wrapped_empty_input() {
        assert_eq!(cursor_from_wrapped("", 0, 0, 50), 0);
        assert_eq!(cursor_from_wrapped("", 0, 5, 50), 0);
        assert_eq!(cursor_from_wrapped("", 3, 0, 50), 0);
    }

    #[test]
    fn cursor_from_wrapped_zero_content_width_graceful() {
        // content_width == 0 → treated as 1, each char on its own line
        assert_eq!(cursor_from_wrapped("ab", 0, 0, 0), 0); // 'a' on row 0
        assert_eq!(cursor_from_wrapped("ab", 1, 0, 0), 1); // 'b' on row 1
    }

    #[test]
    fn cursor_from_wrapped_round_trip() {
        let input = "the quick brown fox jumps over the lazy dog";
        let cw = 10;
        let pos = cursor_from_wrapped(input, 0, 16, cw);
        assert!(pos > 0 && pos <= input.len());
    }

    #[test]
    fn cursor_from_wrapped_word_boundary_wrapping() {
        let pos = cursor_from_wrapped("hello world", 0, 4, 6);
        assert_eq!(&"hello world"[..pos], "hell");
    }

    #[test]
    fn wrapped_cursor_row_single_unwrapped_line() {
        // "hello" at width 50 → all on row 0
        assert_eq!(wrapped_cursor_row("hello", 0, 50), 0);
        assert_eq!(wrapped_cursor_row("hello", 2, 50), 0);
        assert_eq!(wrapped_cursor_row("hello", 5, 50), 0);
    }

    #[test]
    fn wrapped_cursor_row_wraps_to_second_row() {
        // "hello world" at width 10 → row 0: "hello " (6 cols), row 1: "world"
        // cursor at byte 6 ('w') → row 1
        assert_eq!(wrapped_cursor_row("hello world", 6, 10), 1);
        // cursor at byte 8 ('r') → still row 1
        assert_eq!(wrapped_cursor_row("hello world", 8, 10), 1);
        // cursor at byte 3 ('l') → row 0
        assert_eq!(wrapped_cursor_row("hello world", 3, 10), 0);
    }

    #[test]
    fn wrapped_cursor_row_newline_advances() {
        // "a\nb\nc" → 3 visual rows
        assert_eq!(wrapped_cursor_row("a\nb\nc", 0, 50), 0); // 'a'
        assert_eq!(wrapped_cursor_row("a\nb\nc", 2, 50), 1); // 'b'
        assert_eq!(wrapped_cursor_row("a\nb\nc", 4, 50), 2); // 'c'
    }

    #[test]
    fn wrapped_cursor_row_cursor_at_newline() {
        // cursor at position 1 (right after "a\n") → on row 1 (the empty-ish row? or row 1 of 'b'?)
        // Actually cursor at byte 1 (the '\n' itself) — should be row 0 (end of "a" line)
        // But after the newline, the cursor is effectively at start of next line.
        // Let's define: cursor AT the '\n' byte → row 0. Cursor AFTER '\n' → row 1.
        assert_eq!(wrapped_cursor_row("a\nb", 1, 50), 0); // at '\n'
        assert_eq!(wrapped_cursor_row("a\nb", 2, 50), 1); // at 'b'
    }

    #[test]
    fn wrapped_cursor_row_empty_input() {
        assert_eq!(wrapped_cursor_row("", 0, 50), 0);
    }

    #[test]
    fn wrapped_cursor_row_zero_width() {
        assert_eq!(wrapped_cursor_row("ab", 0, 0), 0);
        assert_eq!(wrapped_cursor_row("ab", 1, 0), 1);
    }

    #[test]
    fn wrapped_cursor_col_single_unwrapped_line() {
        assert_eq!(wrapped_cursor_col("hello", 0, 50), 0);
        assert_eq!(wrapped_cursor_col("hello", 2, 50), 2);
        assert_eq!(wrapped_cursor_col("hello", 5, 50), 5);
    }

    #[test]
    fn wrapped_cursor_col_wraps_resets_on_new_visual_row() {
        // "hello world" at width 10 → row 0: "hello " (6 cols), row 1: "world" (5 cols)
        // cursor at byte 0 ('h') → col 0 on row 0
        assert_eq!(wrapped_cursor_col("hello world", 0, 10), 0);
        // cursor at byte 5 (' ') → col 5 on row 0
        assert_eq!(wrapped_cursor_col("hello world", 5, 10), 5);
        // cursor at byte 6 ('w') → col 0 on row 1
        assert_eq!(wrapped_cursor_col("hello world", 6, 10), 0);
        // cursor at byte 8 ('r') → col 2 on row 1
        assert_eq!(wrapped_cursor_col("hello world", 8, 10), 2);
    }

    #[test]
    fn wrapped_cursor_col_newline_resets() {
        assert_eq!(wrapped_cursor_col("a\nb", 0, 50), 0); // 'a'
        assert_eq!(wrapped_cursor_col("a\nb", 2, 50), 0); // 'b' — new line
        assert_eq!(wrapped_cursor_col("a\nb", 3, 50), 1); // end of 'b'
    }

    #[test]
    fn wrapped_cursor_col_cjk() {
        // "你好w" — 你(2) + 好(2) + w(1)
        assert_eq!(wrapped_cursor_col("你好w", 0, 50), 0);
        assert_eq!(wrapped_cursor_col("你好w", 3, 50), 2); // after "你"
        assert_eq!(wrapped_cursor_col("你好w", 6, 50), 4); // after "你好"
    }

    #[test]
    fn wrapped_cursor_row_col_round_trip() {
        let input = "the quick brown fox jumps over the lazy dog";
        let cw = 10;
        for cursor in 0..=input.len() {
            let row = wrapped_cursor_row(input, cursor, cw);
            let col = wrapped_cursor_col(input, cursor, cw);
            let back = cursor_from_wrapped(input, row, col, cw);
            assert_eq!(
                back, cursor,
                "round-trip failed at cursor={cursor}: (row={row}, col={col}) → {back}"
            );
        }
    }

    #[test]
    fn wrapped_cursor_col_empty_input() {
        assert_eq!(wrapped_cursor_col("", 0, 50), 0);
    }

    #[test]
    fn wrapped_cursor_col_zero_width() {
        assert_eq!(wrapped_cursor_col("ab", 0, 0), 0);
        assert_eq!(wrapped_cursor_col("ab", 1, 0), 0);
    }
}
