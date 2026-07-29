pub mod grid;
pub mod parser;
pub mod render;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::theme;

pub fn render_mermaid(body: &str, width: u16) -> Vec<Line<'static>> {
    let t = theme();
    let line_style = Style::default().fg(t.subtle_fg.into());

    let source = body.trim();
    let first_line = source.lines().next().unwrap_or("").trim();

    if first_line.starts_with("graph ") || first_line.starts_with("flowchart ") {
        let lines = render::render_flowchart(source, width);
        lines
            .into_iter()
            .map(|s| {
                if s.is_empty() {
                    Line::raw("")
                } else {
                    Line::from(Span::styled(s, line_style))
                }
            })
            .collect()
    } else {
        source.lines().map(|l| Line::raw(l.to_string())).collect()
    }
}
