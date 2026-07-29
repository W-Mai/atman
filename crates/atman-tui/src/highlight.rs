use std::sync::OnceLock;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

struct Assets {
    syntaxes: SyntaxSet,
    theme: Theme,
}

fn assets() -> &'static Assets {
    static ONCE: OnceLock<Assets> = OnceLock::new();
    ONCE.get_or_init(|| {
        let syntaxes = SyntaxSet::load_defaults_newlines();
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .or_else(|| themes.themes.values().next())
            .expect("bundled theme set is never empty")
            .clone();
        Assets { syntaxes, theme }
    })
}

pub fn highlight_code(lang: &str, body: &str) -> Vec<Line<'static>> {
    let a = assets();
    let syntax = if lang.is_empty() {
        a.syntaxes.find_syntax_plain_text()
    } else {
        a.syntaxes
            .find_syntax_by_token(lang)
            .or_else(|| a.syntaxes.find_syntax_by_name(lang))
            .unwrap_or_else(|| a.syntaxes.find_syntax_plain_text())
    };
    let mut h = HighlightLines::new(syntax, &a.theme);
    let mut out = Vec::new();
    for raw in body.split_inclusive('\n') {
        let stripped = raw.strip_suffix('\n').unwrap_or(raw);
        let regions = h.highlight_line(stripped, &a.syntaxes).unwrap_or_default();
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .map(|(style, text)| Span::styled(text.to_string(), to_ratatui(style)))
            .collect();
        out.push(Line::from(spans));
    }
    out
}

fn to_ratatui(s: SynStyle) -> Style {
    let fg = s.foreground;
    Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b))
}

/// Render a string containing ANSI escape sequences into colored [Line]s,
/// reusing the vt100 parser + terminal cell pipeline.
///
/// Returns the same `Vec<Line<'static>>` shape as [highlight_code] so the
/// markdown renderer can treat both uniformly.
pub fn highlight_ansi(body: &str) -> Vec<Line<'static>> {
    let screen = atman_runtime::tools::term::parse_ansi_to_screen(body);
    let bg: Color = crate::theme::theme().code_bg.into();
    let cols = screen.cols as usize;
    let mut out = Vec::with_capacity(screen.rows as usize);
    for row in 0..screen.rows as usize {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                break;
            }
            let cell = &screen.cells[idx];
            if cell.wide_continuation {
                continue;
            }
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        out.push(Line::from(spans));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_language_produces_multiple_colors() {
        let lines = highlight_code("rust", "fn main() { let x = 1; }\n");
        let colors: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(
            colors.len() >= 2,
            "want distinct token colors, got {colors:?}"
        );
    }

    #[test]
    fn unknown_language_falls_back_without_panic() {
        let lines = highlight_code("no-such-lang", "hello world\n");
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn preserves_line_boundaries() {
        let lines = highlight_code("rust", "let a = 1;\nlet b = 2;\n");
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn empty_body_returns_empty() {
        assert!(highlight_code("rust", "").is_empty());
    }

    #[test]
    fn ansi_produces_colored_spans() {
        let lines = highlight_ansi("\x1b[31mred\x1b[0m");
        assert_eq!(lines.len(), 1);
        let has_red = lines[0].spans.iter().any(|s| {
            matches!(
                s.style.fg,
                Some(Color::Indexed(1)) | Some(Color::Rgb(190, 65, 65))
            )
        });
        assert!(has_red, "want a red fg span, got {:?}", lines[0].spans);
    }

    #[test]
    fn ansi_multiline_body_produces_correct_line_count() {
        let body = "\x1b[31merr\x1b[0m\n\x1b[32mok\x1b[0m\n";
        let lines = highlight_ansi(body);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn ansi_wide_chars_no_trailing_spaces() {
        // CJK wide char should not produce a trailing space after it
        // (wide_continuation cell is skipped, not rendered as space)
        let lines = highlight_ansi("中文");
        assert_eq!(lines.len(), 1);
        // The line should contain the two CJK chars without a phantom space
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.trim(), "中文");
    }

    #[test]
    fn ansi_plain_text_renders_without_codes() {
        // No escape codes — should render as plain text, no literal \x1b
        let lines = highlight_ansi("hello world");
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text.trim(), "hello world");
        assert!(!text.contains('\x1b'));
    }
}
