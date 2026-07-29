use ratatui::style::{Color, Modifier};

#[test]
fn ansi_code_block_renders_real_colors_end_to_end() {
    // 模拟 cargo build --color=always 输出
    let body = "\x1b[31merror\x1b[0m: missing semicolon\n\x1b[32mok\x1b[0m\n";
    let lines = atman_tui::highlight::highlight_ansi(body);
    assert_eq!(lines.len(), 2);

    // 第一行 "error" 应该是红色
    let line0_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        line0_text.contains("error"),
        "line0 should contain 'error': {:?}",
        line0_text
    );
    let has_red = lines[0].spans.iter().any(|s| {
        matches!(
            s.style.fg,
            Some(Color::Indexed(1)) | Some(Color::Rgb(190, 65, 65))
        )
    });
    assert!(has_red, "line 0 should have red span");

    // 第二行 "ok" 应该是绿色
    let has_green = lines[1].spans.iter().any(|s| {
        matches!(
            s.style.fg,
            Some(Color::Indexed(2)) | Some(Color::Rgb(0, 194, 0))
        )
    });
    assert!(has_green, "line 1 should have green span");
}

#[test]
fn ansi_256_color_and_bold_works() {
    let body = "\x1b[1;38;5;208mWARNING\x1b[0m";
    let lines = atman_tui::highlight::highlight_ansi(body);
    assert_eq!(lines.len(), 1);

    // 所有非空字符的 span 都应该是 256-color 208 + bold
    let colored_spans: Vec<_> = lines[0]
        .spans
        .iter()
        .filter(|s| !s.content.trim().is_empty())
        .collect();
    assert!(!colored_spans.is_empty());

    for span in &colored_spans {
        assert!(
            matches!(span.style.fg, Some(Color::Indexed(208))),
            "span {:?} should be Indexed(208), got {:?}",
            span.content,
            span.style.fg
        );
        assert!(
            span.style.add_modifier.contains(Modifier::BOLD),
            "span {:?} should be bold",
            span.content
        );
    }
}

#[test]
fn ansi_multiline_content_not_lost() {
    // 回归测试：确保多行内容不会因 scroll 丢失
    let body = "line one\nline two\nline three\n";
    let lines = atman_tui::highlight::highlight_ansi(body);
    assert_eq!(lines.len(), 3, "should have 3 lines");

    let l0: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
    let l1: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
    let l2: String = lines[2].spans.iter().map(|s| s.content.as_ref()).collect();

    assert!(l0.contains("line one"), "line 0: {:?}", l0);
    assert!(l1.contains("line two"), "line 1: {:?}", l1);
    assert!(l2.contains("line three"), "line 2: {:?}", l2);
}

#[test]
fn ansi_spans_are_merged_for_same_style() {
    // 相同颜色的连续字符应该合并成一个 span，而不是每字符一个
    let body = "\x1b[1;38;5;208mWARNING\x1b[0m";
    let lines = atman_tui::highlight::highlight_ansi(body);

    let colored_spans: Vec<_> = lines[0]
        .spans
        .iter()
        .filter(|s| !s.content.trim().is_empty() && s.content.len() > 1)
        .collect();
    assert!(
        !colored_spans.is_empty(),
        "should have at least one multi-char span after merge"
    );
    for span in &colored_spans {
        assert!(
            span.content.contains("WARNING") || span.content.len() > 1,
            "merged span should be multi-char: {:?}",
            span.content
        );
    }
}
