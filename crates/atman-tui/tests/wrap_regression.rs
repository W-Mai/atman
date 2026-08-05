use atman_tui::input::{visual_line_count, wrapped_cursor_col, wrapped_cursor_row};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

fn ratatui_line_count(text: &str, cw: u16) -> usize {
    let lines: Vec<Line> = text.split('\n').map(Line::raw).collect();
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    para.line_count(cw)
}

fn ratatui_render_lines(text: &str, cw: u16) -> Vec<String> {
    let lines: Vec<Line> = text.split('\n').map(Line::raw).collect();
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    let height = para.line_count(cw).max(1) as u16;
    let mut buf = Buffer::filled(
        Rect::new(0, 0, cw, height),
        ratatui::buffer::Cell::new("\0"),
    );
    para.render(Rect::new(0, 0, cw, height), &mut buf);

    let mut result = Vec::new();
    for row in 0..height {
        let mut line = String::new();
        for col in 0..cw {
            let sym = buf[(col, row)].symbol();
            if sym == "\0" {
                let prev_w = if col > 0 {
                    unicode_width::UnicodeWidthStr::width(buf[(col - 1, row)].symbol())
                } else {
                    0
                };
                if prev_w == 2 {
                    continue;
                }
                break;
            }
            line.push_str(sym);
        }
        result.push(line);
    }
    result
}

fn assert_line_count_match(text: &str, cw: usize) {
    let custom = visual_line_count(text, cw);
    let ratatui = ratatui_line_count(text, cw as u16);
    assert_eq!(
        custom, ratatui,
        "line_count mismatch for {:?} cw={}: custom={} ratatui={}",
        text, cw, custom, ratatui
    );
}

fn assert_render_match(text: &str, cw: usize) {
    let ratatui_lines = ratatui_render_lines(text, cw as u16);
    let custom_count = visual_line_count(text, cw);
    assert_eq!(
        custom_count,
        ratatui_lines.len(),
        "line count mismatch for {:?} cw={}: custom={} ratatui={}\nratatui lines: {:?}",
        text,
        cw,
        custom_count,
        ratatui_lines.len(),
        ratatui_lines
    );
}

#[test]
fn basic_no_wrap() {
    assert_render_match("hello", 50);
    assert_render_match("hello world", 50);
}

#[test]
fn basic_wrap() {
    assert_render_match("hello world foo", 10);
    assert_render_match("hello world foo bar", 10);
}

#[test]
fn trailing_whitespace_stripped() {
    assert_render_match("hello world ", 10);
    assert_render_match("hello     world", 10);
    assert_render_match("a   b   c", 5);
}

#[test]
fn word_crossing() {
    assert_render_match("hello world!", 11);
}

#[test]
fn cjk_mix() {
    assert_render_match("你好world你好", 6);
    assert_render_match("读取文件内容并做分析的一个非常长的中文标题名称", 10);
}

#[test]
fn tab_filtered() {
    assert_render_match("hello\tworld", 10);
    assert_render_match("a\tb\tc", 4);
}

#[test]
fn control_char_filtered() {
    assert_render_match("a\u{1}b", 5);
    assert_render_match("a\rb", 5);
}

#[test]
fn crlf_as_newline() {
    assert_render_match("a\r\nb", 5);
}

#[test]
fn zwsp_as_whitespace() {
    assert_render_match("foo\u{200b}bar", 4);
}

#[test]
fn nbsp_not_whitespace() {
    assert_render_match("AAAA\u{a0}AAA", 5);
}

#[test]
fn empty_input() {
    assert_render_match("", 10);
    assert_line_count_match("", 10);
}

#[test]
fn empty_lines() {
    assert_render_match("a\n\nb", 10);
    assert_render_match("abc\n\n\nxyz", 10);
    assert_render_match("\n", 10);
    assert_render_match("\n\n", 10);
}

#[test]
fn trailing_newline() {
    assert_render_match("a\n", 10);
    assert_render_match("a\n\n", 10);
}

#[test]
fn zero_content_width() {
    assert_line_count_match("ab", 1);
    assert_line_count_match("hello", 1);
}

#[test]
fn halfwidth_katakana() {
    assert_render_match("ｶﾞﾋﾟ", 4);
    assert_render_match("ｶﾞﾋﾟ", 3);
}

#[test]
fn fuzz_random_ascii() {
    use std::collections::HashSet;
    let mut rng_state: u64 = 42;
    let mut rng = || {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };
    let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789      \n";
    let mut tested = HashSet::new();
    for _ in 0..500 {
        let len = 1 + (rng() % 60) as usize;
        let text: String = (0..len)
            .map(|_| chars[(rng() % chars.len() as u64) as usize] as char)
            .collect();
        if tested.insert(text.clone()) {
            for &cw in &[3usize, 5, 8, 10, 15, 20, 30] {
                assert_line_count_match(&text, cw);
            }
        }
    }
}

#[test]
fn fuzz_random_cjk() {
    use std::collections::HashSet;
    let mut rng_state: u64 = 12345;
    let mut rng = || {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        rng_state
    };
    let chars = [
        'a', 'b', 'c', 'd', ' ', ' ', ' ', '你', '好', '世', '界', 's', 't', '\n',
    ];
    let mut tested = HashSet::new();
    for _ in 0..500 {
        let len = 1 + (rng() % 40) as usize;
        let text: String = (0..len)
            .map(|_| chars[(rng() % chars.len() as u64) as usize])
            .collect();
        if tested.insert(text.clone()) {
            for &cw in &[3usize, 5, 7, 10, 15] {
                assert_line_count_match(&text, cw);
            }
        }
    }
}

#[test]
fn cursor_col_matches_render() {
    let text = "hello world foo bar";
    let cw = 10;
    let ratatui_lines = ratatui_render_lines(text, cw as u16);

    for cursor in 0..=text.len() {
        let row = wrapped_cursor_row(text, cursor, cw);
        let col = wrapped_cursor_col(text, cursor, cw);
        assert!(
            row < ratatui_lines.len(),
            "cursor={} row={} out of bounds (ratatui={})",
            cursor,
            row,
            ratatui_lines.len()
        );
        assert!(
            col <= ratatui_lines[row].chars().count(),
            "cursor={} row={} col={} exceeds rendered line len {} ({:?})",
            cursor,
            row,
            col,
            ratatui_lines[row].chars().count(),
            ratatui_lines[row]
        );
    }
}

#[test]
fn cursor_row_col_round_trip() {
    let inputs = [
        ("the quick brown fox jumps over the lazy dog", 10),
        ("你好world你好", 6),
        ("hello world foo", 10),
        ("a\nb\nc", 50),
    ];
    for (input, cw) in inputs {
        let mut cursor = 0;
        loop {
            let row = wrapped_cursor_row(input, cursor, cw);
            let col = wrapped_cursor_col(input, cursor, cw);
            let back = atman_tui::input::cursor_from_wrapped(input, row, col, cw);
            assert_eq!(
                back, cursor,
                "round-trip failed for {:?} cw={} at cursor={}: ({}, {}) → {}",
                input, cw, cursor, row, col, back
            );
            if cursor >= input.len() {
                break;
            }
            cursor += input[cursor..].chars().next().unwrap().len_utf8();
        }
    }
}
