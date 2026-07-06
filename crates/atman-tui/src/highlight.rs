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
}
