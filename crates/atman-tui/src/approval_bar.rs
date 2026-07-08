use atman_runtime::session::PendingApproval;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub fn render(f: &mut ratatui::Frame, area: Rect, pending: &[PendingApproval]) {
    if pending.is_empty() || area.height == 0 {
        return;
    }
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    let header = format!(
        " {} pending — 1..{} approve · [a]ll · [d]eny first · [Esc] deny all",
        pending.len(),
        pending.len().min(9)
    );
    lines.push(Line::from(Span::styled(
        header,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )));
    for (i, p) in pending.iter().take(9).enumerate() {
        let key = format!(" [{}] ", i + 1);
        let args = truncate_line(&p.args_preview, area.width.saturating_sub(24) as usize);
        lines.push(Line::from(vec![
            Span::styled(
                key,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(p.tool_name.clone(), Style::default().fg(Color::Cyan)),
            Span::styled(format!("  {args}"), Style::default().fg(Color::Gray)),
        ]));
    }
    if pending.len() > 9 {
        lines.push(Line::from(Span::styled(
            format!(" (+{} more, only 1..9 have hotkeys)", pending.len() - 9),
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn truncate_line(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out = String::with_capacity(max);
    for (i, c) in s.chars().enumerate() {
        if i + 1 >= max {
            out.push('…');
            break;
        }
        if c == '\n' {
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}
