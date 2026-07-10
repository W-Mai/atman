use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct StatusInputs<'a> {
    pub session_id: &'a str,
    pub goal: Option<&'a str>,
    pub streaming: bool,
}

pub fn render_bar<'a>(inputs: StatusInputs<'a>) -> Paragraph<'a> {
    let gutter = crate::layout::CONTENT_GUTTER as usize;
    Paragraph::new(with_gutter(top_line(&inputs), gutter))
}

fn with_gutter<'a>(line: Line<'a>, gutter: usize) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::raw(" ".repeat(gutter)));
    spans.extend(line.spans);
    Line::from(spans)
}

fn top_line<'a>(inputs: &StatusInputs<'a>) -> Line<'a> {
    let mut spans = vec![
        Span::styled(
            " atman ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            &inputs.session_id[..inputs.session_id.len().min(8)],
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(g) = inputs.goal {
        spans.push(Span::raw("  · goal "));
        spans.push(Span::styled(
            truncate(g, 60),
            Style::default().fg(Color::Yellow),
        ));
    }
    if inputs.streaming {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "streaming…",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Line::from(spans)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}
