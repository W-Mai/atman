use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub struct StatusInputs<'a> {
    pub session_id: &'a str,
    pub flow_id: Option<String>,
    pub session_name: Option<&'a str>,
    pub goal: Option<&'a str>,
    pub streaming: bool,
    pub waiting_for_llm: bool,
    pub status_notes: &'a std::collections::HashMap<String, String>,
    pub activity: Option<&'a crate::app::ActivityTotals>,
    pub pending_permissions: usize,
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
            " ∴ atman ",
            Style::default()
                .fg(t.code_bg.into())
                .bg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    for key in ["controls", "clipboard", "view-read"] {
        if let Some(pending) = inputs.status_notes.get(key) {
            spans.push(Span::styled(
                pending,
                Style::default().fg(t.tinted_fg.into()),
            ));
            spans.push(Span::raw("  · "));
        }
    }
    spans.push(Span::styled(
        &inputs.session_id[..inputs.session_id.len().min(8)],
        Style::default().fg(t.subtle_fg.into()),
    ));
    if let Some(flow) = &inputs.flow_id {
        spans.push(Span::styled(
            format!(" / Flow {}", crate::width::middle_truncate(flow, 17)),
            Style::default().fg(t.heading.into()),
        ));
    }
    if let Some(name) = inputs.session_name {
        spans.push(Span::raw("  · "));
        spans.push(Span::styled(
            crate::width::truncate(name, 36),
            Style::default()
                .fg(t.heading.into())
                .add_modifier(Modifier::BOLD),
        ));
    }
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
    if inputs.pending_permissions > 0 {
        spans.push(Span::raw("  · "));
        spans.push(Span::styled(
            format!(
                "authorization required · {} pending",
                inputs.pending_permissions
            ),
            Style::default()
                .fg(t.warn.into())
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(activity) = inputs.activity
        && (activity.attempted_calls > 0 || activity.applied_edits > 0)
    {
        spans.push(Span::raw("  · "));
        spans.push(Span::styled(
            activity.compact_label(),
            Style::default().fg(t.meta_fg.into()),
        ));
    }
    // Status notes from notify system
    for (key, text) in inputs.status_notes.iter() {
        if matches!(key.as_str(), "controls" | "clipboard" | "view-read") {
            continue;
        }
        spans.push(Span::raw("  · "));
        spans.push(Span::styled(
            crate::width::truncate(text, 60),
            Style::default().fg(t.tinted_fg.into()),
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_exit_progress_remains_visible_before_long_view_metadata() {
        let notes = [
            ("controls".into(), "⠧ Finishing".into()),
            (
                "execution".into(),
                "Interrupted · 0 execution pauses · 0 pending".into(),
            ),
        ]
        .into();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(
                    render_bar(StatusInputs {
                        session_id: "0123456789abcdef",
                        flow_id: Some("0123456789abcdef0123456789abcdef".into()),
                        session_name: Some("A long retained conversation title"),
                        goal: None,
                        streaming: false,
                        waiting_for_llm: false,
                        status_notes: &notes,
                        activity: None,
                        pending_permissions: 0,
                    }),
                    frame.area(),
                )
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Finishing"), "{text}");
    }
}
