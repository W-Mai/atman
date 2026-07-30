pub mod grid;
pub mod parser;
pub mod render;
pub mod sequence;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::theme;

pub fn render_mermaid(body: &str, width: u16) -> Vec<Line<'static>> {
    let t = theme();
    let line_style = Style::default().fg(t.subtle_fg.into());

    let source = body.trim();
    let first_line = source.lines().next().unwrap_or("").trim();

    let raw_lines = if first_line.starts_with("graph ") || first_line.starts_with("flowchart ") {
        render::render_flowchart(source, width)
    } else if first_line == "sequenceDiagram" {
        let sd = sequence::parse_sequence(source);
        sequence::render_sequence(&sd)
    } else {
        source.lines().map(String::from).collect()
    };

    raw_lines
        .into_iter()
        .map(|s| {
            if s.is_empty() {
                Line::raw("")
            } else {
                Line::from(Span::styled(s, line_style))
            }
        })
        .collect()
}
