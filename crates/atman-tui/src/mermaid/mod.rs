pub mod charts;
pub mod diagrams;
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
    } else if first_line == "pie" {
        let chart = charts::parse_pie(source);
        charts::render_pie(&chart)
    } else if first_line == "gantt" {
        let chart = charts::parse_gantt(source);
        charts::render_gantt(&chart)
    } else if first_line == "timeline" {
        let tl = charts::parse_timeline(source);
        charts::render_timeline(&tl)
    } else if first_line == "erDiagram" {
        let er = diagrams::parse_er(source);
        diagrams::render_er(&er)
    } else if first_line == "classDiagram" {
        let cd = diagrams::parse_class(source);
        diagrams::render_class(&cd)
    } else if first_line == "mindmap" {
        let mm = diagrams::parse_mindmap(source);
        diagrams::render_mindmap(&mm)
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
