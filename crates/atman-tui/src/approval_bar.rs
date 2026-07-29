use atman_runtime::session::PendingApproval;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

pub fn render(f: &mut ratatui::Frame, area: Rect, pending: &[PendingApproval]) {
    if pending.is_empty() || area.height == 0 {
        return;
    }
    let title = format!(" approvals · {} pending ", pending.len());
    let hint = Line::from(Span::styled(
        " 1..9 accept · a all · d deny · Esc deny all ",
        Style::default().fg(crate::theme::theme().subtle_fg.into()),
    ))
    .right_aligned();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(crate::theme::theme().warn.into()))
        .title(Span::styled(
            title,
            Style::default()
                .fg(crate::theme::theme().warn.into())
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(hint)
        .padding(Padding::horizontal(1));
    let inner_width = area.width.saturating_sub(4) as usize;
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(area.height as usize);
    for (i, p) in pending.iter().take(9).enumerate() {
        let key = format!("[{}] ", i + 1);
        let head_len = key.len() + p.tool_name.len() + 2;
        let args_flat = p.args_preview.replace('\n', " ");
        let args = crate::width::truncate(&args_flat, inner_width.saturating_sub(head_len));
        lines.push(Line::from(vec![
            Span::styled(
                key,
                Style::default()
                    .fg(crate::theme::theme().success.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                p.tool_name.clone(),
                Style::default().fg(crate::theme::theme().accent.into()),
            ),
            Span::styled(
                format!("  {args}"),
                Style::default().fg(crate::theme::theme().tinted_fg.into()),
            ),
        ]));
    }
    if pending.len() > 9 {
        lines.push(Line::from(Span::styled(
            format!("(+{} more, only 1..9 have hotkeys)", pending.len() - 9),
            Style::default().fg(crate::theme::theme().subtle_fg.into()),
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}
