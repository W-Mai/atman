use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn graphemes(s: &str) -> impl Iterator<Item = (&str, usize)> {
    s.graphemes(true).map(|g| {
        let w = UnicodeWidthStr::width(g);
        (g, w)
    })
}

pub fn grapheme_indices(s: &str) -> impl Iterator<Item = (usize, &str, usize)> {
    s.grapheme_indices(true).map(|(i, g)| {
        let w = UnicodeWidthStr::width(g);
        (i, g, w)
    })
}

pub fn truncate(s: &str, max_w: usize) -> String {
    if width(s) <= max_w {
        return s.to_string();
    }
    if max_w == 0 {
        return String::new();
    }
    let budget = max_w.saturating_sub(1);
    format!("{}…", grapheme_prefix(s, budget))
}

pub fn truncate_plain(s: &str, max_w: usize) -> String {
    grapheme_prefix(s, max_w)
}

pub fn trim_display_offset(s: &str, offset: usize, max_w: usize) -> String {
    let mut skipped = 0usize;
    let mut result = String::new();
    let mut in_partial = false;
    for (g, gw) in graphemes(s) {
        if skipped < offset {
            let take = (offset - skipped).min(gw);
            if take < gw {
                in_partial = true;
            }
            skipped += gw;
            if skipped >= offset && in_partial {
                let pad = skipped - offset;
                result.push_str(&" ".repeat(pad));
            }
            continue;
        }
        if result.width() + gw > max_w {
            break;
        }
        result.push_str(g);
    }
    let dw = result.width();
    if dw < max_w {
        result.push_str(&" ".repeat(max_w - dw));
    }
    result
}

pub fn middle_truncate(s: &str, max_w: usize) -> String {
    if width(s) <= max_w {
        return s.to_string();
    }
    if max_w <= 1 {
        return "…".into();
    }
    let ell = 1;
    let side_budget = max_w.saturating_sub(ell) / 2;
    let prefix = grapheme_prefix(s, side_budget);
    let prefix_w = width(&prefix);
    let remaining = max_w.saturating_sub(prefix_w + ell);
    let suffix = grapheme_suffix(s, remaining);
    format!("{prefix}…{suffix}")
}

pub fn pad_right(s: &str, target_w: usize) -> String {
    let w = width(s);
    if w >= target_w {
        return truncate_plain(s, target_w);
    }
    format!("{s}{}", " ".repeat(target_w - w))
}

pub fn spans_width<'a>(spans: impl IntoIterator<Item = &'a Span<'a>>) -> usize {
    spans.into_iter().map(|s| width(s.content.as_ref())).sum()
}

pub fn truncate_spans(
    spans: Vec<Span<'static>>,
    max_w: usize,
    bg: Option<ratatui::style::Color>,
) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let mut text = String::new();
        for g in span.content.graphemes(true) {
            let gw = width(g);
            if used + gw > max_w {
                break;
            }
            text.push_str(g);
            used += gw;
        }
        if !text.is_empty() {
            let mut style = span.style;
            if let Some(bg) = bg {
                style.bg = style.bg.or(Some(bg));
            }
            out.push(Span::styled(text, style));
        }
        if used >= max_w {
            break;
        }
    }
    out
}

fn grapheme_prefix(s: &str, max_w: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for g in s.graphemes(true) {
        let gw = width(g);
        if used + gw > max_w {
            break;
        }
        out.push_str(g);
        used += gw;
    }
    out
}

fn grapheme_suffix(s: &str, max_w: usize) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for g in s.graphemes(true).rev() {
        let gw = width(g);
        if used + gw > max_w {
            break;
        }
        out.push(g);
        used += gw;
    }
    out.reverse();
    out.concat()
}

/// Word-wrap `text` to fit `max_w` display columns.
///
/// - ASCII: breaks on spaces (word-boundary), collapses leading spaces on new line
/// - CJK/emoji: breaks anywhere (no spaces in CJK)
/// - Existing `\n` preserved as hard line breaks
/// - Single grapheme wider than `max_w` stays on its own line (overflow allowed)
/// - Empty input returns `vec![String::new()]` (one empty line)
pub fn word_wrap(text: &str, max_w: usize) -> Vec<String> {
    if max_w == 0 {
        return vec![text.to_string()];
    }
    if text.is_empty() {
        return vec![String::new()];
    }

    let mut lines = Vec::new();

    for hard_line in text.split('\n') {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let mut pending_space = false;
        let mut flushed_overflow = false;

        for (g, gw) in graphemes(hard_line) {
            if g == " " {
                if !cur.is_empty() {
                    pending_space = true;
                }
                continue;
            }

            // A single grapheme wider than max_w overflows on its own line.
            if gw > max_w {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                pending_space = false;
                lines.push(g.to_string());
                flushed_overflow = true;
                continue;
            }

            flushed_overflow = false;

            // Determine if we can fit the space (if pending) + this grapheme.
            let space_w = if pending_space { 1 } else { 0 };
            if cur_w + space_w + gw > max_w && !cur.is_empty() {
                // Wrap here; drop the pending space on the new line.
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
                pending_space = false;
            }

            // Consume pending space if we didn't wrap (or just wrapped to empty).
            if pending_space && !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
                pending_space = false;
            }

            cur.push_str(g);
            cur_w += gw;
        }

        if !flushed_overflow || !cur.is_empty() {
            lines.push(cur);
        }
    }

    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_cjk() {
        assert_eq!(width("你好"), 4);
        assert_eq!(width("a中"), 3);
    }

    #[test]
    fn width_zwj_sequence() {
        assert_eq!(width("👨‍👩‍👧‍👦"), 2);
        assert_eq!(width("🧑‍🌾"), 2);
        assert_eq!(width("🐻‍❄️"), 2);
        assert_eq!(width("🏳️‍🌈"), 2);
        assert_eq!(width("👍🏽"), 2);
        assert_eq!(width("🇨🇳"), 2);
    }

    #[test]
    fn width_mixed() {
        assert_eq!(width("aβ中🎉👨‍👩‍👧‍👦"), 8);
    }

    #[test]
    fn truncate_cjk() {
        assert_eq!(truncate("你好世界", 10), "你好世界");
        assert_eq!(truncate("你好世界", 5), "你好…");
        assert_eq!(truncate("你好ab", 6), "你好ab");
        assert_eq!(truncate("你好ab", 5), "你好…");
    }

    #[test]
    fn truncate_zwj_does_not_split_cluster() {
        assert_eq!(truncate("👨‍👩‍👧‍👦test", 5), "👨‍👩‍👧‍👦te…");
        assert_eq!(truncate("👨‍👩‍👧‍👦test", 3), "👨‍👩‍👧‍👦…");
        assert_eq!(truncate("ab👨‍👩‍👧‍👦", 3), "ab…");
    }

    #[test]
    fn truncate_zero_max() {
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_plain_cjk() {
        assert_eq!(truncate_plain("你好世界", 5), "你好");
        assert_eq!(truncate_plain("你好世界", 4), "你好");
    }

    #[test]
    fn middle_truncate_path() {
        let result = middle_truncate("a/b/c/d/e/f/g", 11);
        assert!(result.contains('…'));
        assert_eq!(width(&result), 11);
    }

    #[test]
    fn middle_truncate_cjk() {
        let result = middle_truncate("你好世界测试代码", 8);
        assert!(result.contains('…'));
        assert!(width(&result) <= 8);
    }

    #[test]
    fn pad_right_cjk() {
        assert_eq!(pad_right("你好", 6), "你好  ");
        assert_eq!(pad_right("你好", 4), "你好");
        assert_eq!(width(&pad_right("你好世界", 2)), 2);
        assert_eq!(width(&pad_right("你好", 6)), 6);
    }

    #[test]
    fn pad_right_zwj() {
        let result = pad_right("👨‍👩‍👧‍👦", 5);
        assert_eq!(width(&result), 5);
        assert!(result.starts_with('👨'));
    }

    #[test]
    fn grapheme_prefix_never_splits_wide_char() {
        assert_eq!(grapheme_prefix("你好", 3), "你");
        assert_eq!(grapheme_prefix("你好", 1), "");
    }

    #[test]
    fn grapheme_prefix_zwj() {
        assert_eq!(grapheme_prefix("👨‍👩‍👧‍👦ab", 4), "👨‍👩‍👧‍👦ab");
        assert_eq!(grapheme_prefix("👨‍👩‍👧‍👦ab", 3), "👨‍👩‍👧‍👦a");
        assert_eq!(grapheme_prefix("👨‍👩‍👧‍👦ab", 2), "👨‍👩‍👧‍👦");
    }

    #[test]
    fn word_wrap_empty() {
        assert_eq!(word_wrap("", 10), vec![String::new()]);
    }

    #[test]
    fn word_wrap_short() {
        assert_eq!(word_wrap("hello", 10), vec!["hello".to_string()]);
    }

    #[test]
    fn word_wrap_ascii_word_boundary() {
        // width=5: each word is exactly 5, spaces force wrap
        assert_eq!(
            word_wrap("hello world foo", 5),
            vec!["hello".to_string(), "world".to_string(), "foo".to_string()]
        );
        // width=8: "hello wo" (8) then "rld foo" (7)
        assert_eq!(
            word_wrap("hello world foo", 8),
            vec!["hello wo".to_string(), "rld foo".to_string()]
        );
    }

    #[test]
    fn word_wrap_cjk() {
        assert_eq!(
            word_wrap("你好世界测试", 4),
            vec!["你好".to_string(), "世界".to_string(), "测试".to_string()]
        );
    }

    #[test]
    fn word_wrap_preserves_newlines() {
        assert_eq!(
            word_wrap("a\nb", 10),
            vec!["a".to_string(), "b".to_string()]
        );
    }

    #[test]
    fn word_wrap_mixed() {
        // width=5: "hello" (5), "你好" (4), "world" (5) — each fits exactly
        assert_eq!(
            word_wrap("hello 你好 world", 5),
            vec!["hello".to_string(), "你好".to_string(), "world".to_string()]
        );
    }

    #[test]
    fn word_wrap_overflow() {
        // emoji width=2, max_w=1: overflow on its own line, no trailing empty
        let emoji = "🙂";
        assert_eq!(word_wrap(emoji, 1), vec![emoji.to_string()]);
        // text after overflow continues on new line
        assert_eq!(word_wrap("🙂x", 1), vec!["🙂".to_string(), "x".to_string()]);
    }
}
