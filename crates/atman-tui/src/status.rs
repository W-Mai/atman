use atman_runtime::ContextSnapshot;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct StatusInputs<'a> {
    pub session_id: &'a str,
    pub goal: Option<&'a str>,
    pub streaming: bool,
    pub context: &'a ContextSnapshot,
    pub attach_count: usize,
    pub include_compact_line: bool,
}

pub fn render_bar<'a>(inputs: StatusInputs<'a>) -> Paragraph<'a> {
    let mut lines: Vec<Line<'a>> = Vec::with_capacity(2);
    lines.push(top_line(&inputs));
    if inputs.include_compact_line {
        lines.push(compact_context_line(inputs.context, inputs.attach_count));
    }
    Paragraph::new(lines)
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

fn compact_context_line<'a>(ctx: &ContextSnapshot, attach_count: usize) -> Line<'a> {
    let model = if ctx.model.is_empty() {
        "(none)".to_string()
    } else {
        ctx.model.clone()
    };
    use crate::humanize::format_count;
    let window = if ctx.window_budget == 0 {
        format_count(ctx.window_tokens)
    } else {
        format!(
            "{}/{}",
            format_count(ctx.window_tokens),
            format_count(ctx.window_budget)
        )
    };
    let text = format!(
        " {model} · ctx {window} · spent {}/{} · attach {attach_count} · mcp {}/{}",
        format_count(ctx.tokens_in),
        format_count(ctx.tokens_out),
        ctx.mcp_ok,
        ctx.mcp_total,
    );
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
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
