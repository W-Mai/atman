use ratatui::text::Line;

pub fn render_markdown(md: &str) -> Vec<Line<'static>> {
    md.lines().map(|l| Line::from(l.to_string())).collect()
}
