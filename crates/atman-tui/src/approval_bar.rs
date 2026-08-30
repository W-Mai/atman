use crate::app::PendingPermission;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

pub fn render(
    f: &mut ratatui::Frame,
    area: Rect,
    canonical: &[PendingPermission],
    groups: &[crate::app::PendingPermissionGroup],
    selected_group: Option<&atman_runtime::permission::PermissionGroupId>,
) {
    let pending_len = canonical.len();
    let group_count = groups.len();
    if pending_len == 0 || area.height == 0 {
        return;
    }
    let title = if group_count == 0 {
        format!(" approvals · {pending_len} pending ")
    } else {
        format!(" approvals · {pending_len} pending · {group_count} groups ")
    };
    let hint = Line::from(Span::styled(
        " 1..9 accept · s scope · x expand · [/] group · g group · f defer · a all · d deny · Esc deny all ",
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
    let rows: Vec<(String, String)> = canonical
        .iter()
        .filter(|p| {
            p.payload.group_ids.is_empty()
                || !groups.iter().any(|group| {
                    group
                        .payload
                        .request_ids
                        .iter()
                        .any(|request_id| request_id == &p.request_id)
                })
        })
        .map(|p| {
            let execution = match (
                p.payload.provenance.risks.contains("ProcessSpawn"),
                p.payload.execution_boundary,
            ) {
                (true, Some(atman_runtime::permission::ExecutionBoundary::Sandboxed)) => {
                    " · sandboxed"
                }
                (true, Some(atman_runtime::permission::ExecutionBoundary::Direct)) => " · direct",
                _ => "",
            };
            (
                p.payload.tool.clone(),
                format!(
                    "{}{}{execution} · {}",
                    p.payload
                        .call_intent
                        .as_ref()
                        .map(|intent| format!("{} · ", intent.as_str()))
                        .unwrap_or_default(),
                    p.request_id,
                    p.payload.provenance.targets.join(", ")
                ),
            )
        })
        .collect();
    for group in groups {
        let selected = if selected_group == Some(&group.group_id) {
            "● "
        } else {
            "  "
        };
        let marker = if group.expanded { "▾" } else { "▸" };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{selected}{marker}group {} ", group.group_id),
                Style::default()
                    .fg(crate::theme::theme().warn.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} · {} requests · rev {}",
                    group.payload.label,
                    group.payload.request_ids.len(),
                    group.revision
                ),
                Style::default().fg(crate::theme::theme().tinted_fg.into()),
            ),
        ]));
        if group.expanded {
            for request_id in &group.payload.request_ids {
                if let Some(request) = canonical
                    .iter()
                    .find(|request| &request.request_id == request_id)
                {
                    let purpose = request
                        .payload
                        .call_intent
                        .as_ref()
                        .map(|intent| format!(" · {}", intent.as_str()))
                        .unwrap_or_default();
                    lines.push(Line::from(Span::styled(
                        format!(
                            "  └ {}{purpose} · {}",
                            request.payload.tool, request.request_id
                        ),
                        Style::default().fg(crate::theme::theme().subtle_fg.into()),
                    )));
                }
            }
        }
    }
    for (i, (tool_name, args_preview)) in rows.iter().take(9).enumerate() {
        let key = format!("[{}] ", i + 1);
        let head_len = key.len() + tool_name.len() + 2;
        let args_flat = args_preview.replace('\n', " ");
        let args = crate::width::truncate(&args_flat, inner_width.saturating_sub(head_len));
        lines.push(Line::from(vec![
            Span::styled(
                key,
                Style::default()
                    .fg(crate::theme::theme().success.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                tool_name.clone(),
                Style::default().fg(crate::theme::theme().accent.into()),
            ),
            Span::styled(
                format!("  {args}"),
                Style::default().fg(crate::theme::theme().tinted_fg.into()),
            ),
        ]));
    }
    if pending_len > 9 {
        lines.push(Line::from(Span::styled(
            format!("(+{} more, only 1..9 have hotkeys)", pending_len - 9),
            Style::default().fg(crate::theme::theme().subtle_fg.into()),
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}
