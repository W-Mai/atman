use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render_bar<'a>(
    session_id: &'a str,
    goal: Option<&'a str>,
    streaming: bool,
) -> Paragraph<'a> {
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
            &session_id[..session_id.len().min(8)],
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(g) = goal {
        spans.push(Span::raw("  · goal "));
        spans.push(Span::styled(
            truncate(g, 60),
            Style::default().fg(Color::Yellow),
        ));
    }
    if streaming {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "streaming…",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }
    Paragraph::new(Line::from(spans))
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
