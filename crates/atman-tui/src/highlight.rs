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
        let mut cur_text = String::new();
        let mut cur_style: Option<Style> = None;
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
                " "
            } else {
                &cell.chars
            };
            if cur_style == Some(style) {
                cur_text.push_str(text);
            } else {
                if !cur_text.is_empty() {
                    if let Some(s) = cur_style.take() {
                        spans.push(Span::styled(std::mem::take(&mut cur_text), s));
                    }
                }
                cur_style = Some(style);
                cur_text.push_str(text);
            }
        }
        if !cur_text.is_empty() {
            if let Some(s) = cur_style {
                spans.push(Span::styled(cur_text, s));
            }
        }
        out.push(Line::from(spans));
    }
    out
}

fn normalize_tex_compat(tex: &str) -> String {
    tex.replace(r"\begin{vmatrix}", r"|\begin{matrix}")
        .replace(r"\end{vmatrix}", r"\end{matrix}|")
        .replace(r"\begin{aligned}", r"\begin{matrix}")
        .replace(r"\end{aligned}", r"\end{matrix}")
        .replace(r"\operatorname{Re}", "Re")
        .replace(r"\qquad", " ")
        .replace(r"\quad", " ")
        .replace(r"\,", "")
        .replace(r"\;", "")
        .replace(r"\!", "")
}

pub fn render_math(tex: &str) -> Vec<Line<'static>> {
    let normalized = normalize_tex_compat(tex);
    let render = |source: &str| {
        atman_runtime::capture_blocking_panic(|| txm::render(source)).and_then(Result::ok)
    };
    let rendered =
        render(&normalized).or_else(|| if normalized == tex { None } else { render(tex) });
    match rendered {
        Some(ansi_string) => {
            let lines = highlight_ansi(&ansi_string);
            if lines.is_empty() {
                vec![Line::raw(tex.to_string())]
            } else {
                lines
            }
        }
        None => vec![Line::raw(tex.to_string())],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamma_integral_normalizes_unregistered_text_commands() {
        let source =
            r"\Gamma(z) = \int_{0}^{\infty} t^{z-1}e^{-t}\,dt, \qquad \operatorname{Re}(z)>0";
        let text: String = render_math(source)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("operatorname"));
        assert!(!text.contains("qquad"));
        assert!(!text.contains('\\'));
        assert!(text.contains("Re"));
    }

    #[test]
    fn common_matrix_environments_render_without_raw_tex_fallback() {
        for source in [
            r"\begin{vmatrix} a & b \\ c & d \end{vmatrix}",
            r"\begin{aligned} x &= y \\ y &= z \end{aligned}",
        ] {
            let text: String = render_math(source)
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect();
            assert!(!text.contains(r"\begin"), "raw TeX leaked for {source:?}");
            assert!(text.contains('a') || text.contains('x'));
        }
    }

    #[test]
    fn incomplete_math_falls_back_without_unwinding() {
        for source in [r"\frac{1}", "x^", r"\sqrt"] {
            let lines = render_math(source);
            assert_eq!(lines.len(), 1);
            assert_eq!(lines[0].spans[0].content, source);
        }
    }

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
