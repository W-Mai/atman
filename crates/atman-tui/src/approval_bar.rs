use atman_runtime::session::PendingApproval;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

pub fn render(f: &mut ratatui::Frame, area: Rect, pending: &[PendingApproval]) {
    if pending.is_empty() || area.height == 0 {
        return;
    }
    let title = format!(" approvals · {} pending ", pending.len());
    let hint = Line::from(Span::styled(
        " 1..9 accept · a all · d deny · Esc deny all ",
        Style::default().fg(Color::DarkGray),
    ))
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(hint)
        .padding(Padding::horizontal(1));
    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    for (i, p) in pending.iter().take(9).enumerate() {
        let key = format!("[{}] ", i + 1);
        let head_len = key.len() + p.tool_name.len() + 2;
        let args = truncate_line(&p.args_preview, inner_width.saturating_sub(head_len));
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
            format!("(+{} more, only 1..9 have hotkeys)", pending.len() - 9),
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
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
