use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct StatusInputs<'a> {
    pub session_id: &'a str,
    pub goal: Option<&'a str>,
    pub streaming: bool,
    pub waiting_for_llm: bool,
    pub status_notes: &'a std::collections::HashMap<String, String>,
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
    let t = crate::theme::theme();
    let mut spans = vec![
        Span::styled(
            " atman ",
            Style::default()
                .fg(t.code_bg.into())
                .bg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            &inputs.session_id[..inputs.session_id.len().min(8)],
            Style::default().fg(t.subtle_fg.into()),
        ),
    ];
    if let Some(g) = inputs.goal {
        spans.push(Span::raw("  · goal "));
        spans.push(Span::styled(
            crate::width::truncate(g, 60),
            Style::default().fg(t.warn.into()),
        ));
    }
    if inputs.streaming {
        let label = if inputs.waiting_for_llm {
            "thinking…"
        } else {
            "streaming…"
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            label,
            Style::default()
                .fg(t.success.into())
                .add_modifier(Modifier::BOLD),
        ));
    }
    // Status notes from notify system
    for (_key, text) in inputs.status_notes.iter() {
        spans.push(Span::raw("  · "));
        spans.push(Span::styled(
            crate::width::truncate(text, 60),
            Style::default().fg(t.tinted_fg.into()),
        ));
    }
    Line::from(spans)
}
