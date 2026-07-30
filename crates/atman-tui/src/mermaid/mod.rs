pub mod charts;
pub mod diagrams;
pub mod extras;
pub mod grid;
pub mod parser;
pub mod render;
pub mod sequence;
pub mod state;

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
    } else if first_line == "stateDiagram-v2" || first_line == "stateDiagram" {
        state::render_state_diagram(source, width)
    } else if first_line == "pie" || first_line.starts_with("pie ") {
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
    } else if first_line == "gitGraph" {
        let gg = extras::parse_git_graph(source);
        extras::render_git_graph(&gg)
    } else if first_line == "journey" {
        let j = extras::parse_journey(source);
        extras::render_journey(&j)
    } else if first_line == "quadrantChart" {
        let qc = extras::parse_quadrant(source);
        extras::render_quadrant(&qc)
    } else if first_line.starts_with("xychart") {
        let chart = extras::parse_xychart(source);
        extras::render_xychart(&chart)
    } else if first_line.starts_with("block") {
        let bd = extras::parse_block(source);
        extras::render_block(&bd)
    } else if first_line.starts_with("packet") {
        let pkt = extras::parse_packet(source);
        extras::render_packet(&pkt)
    } else if first_line.starts_with("sankey") {
        let sk = extras::parse_sankey(source);
        extras::render_sankey(&sk)
    } else if first_line == "requirementDiagram" {
        let rd = extras::parse_requirement(source);
        extras::render_requirement(&rd)
    } else if first_line.starts_with("architecture") {
        let arch = extras::parse_architecture(source);
        extras::render_architecture(&arch)
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
