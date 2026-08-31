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
    let rows: Vec<(String, Option<String>, String)> = canonical
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
            let (title, technical) =
                approval_labels(&p.payload.tool, p.payload.call_intent.as_ref());
            (
                title,
                technical,
                format!(
                    "{}{execution} · {}",
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
                    let (title, technical) = approval_labels(
                        &request.payload.tool,
                        request.payload.call_intent.as_ref(),
                    );
                    let technical = technical
                        .map(|tool| format!(" · {tool}"))
                        .unwrap_or_default();
                    lines.push(Line::from(Span::styled(
                        format!("  └ {title}{technical} · {}", request.request_id),
                        Style::default().fg(crate::theme::theme().subtle_fg.into()),
                    )));
                }
            }
        }
    }
    for (i, (title, technical, detail)) in rows.iter().take(9).enumerate() {
        let key = format!("[{}] ", i + 1);
        let technical_text = technical
            .as_ref()
            .map(|tool| format!(" · {tool}"))
            .unwrap_or_default();
        let head_width = crate::width::width(&key)
            + crate::width::width(title)
            + crate::width::width(&technical_text)
            + 2;
        let detail = crate::width::truncate(
            &detail.replace('\n', " "),
            inner_width.saturating_sub(head_width),
        );
        let mut spans = vec![
            Span::styled(
                key,
                Style::default()
                    .fg(crate::theme::theme().success.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title.clone(),
                Style::default().fg(crate::theme::theme().accent.into()),
            ),
        ];
        if !technical_text.is_empty() {
            spans.push(Span::styled(
                technical_text,
                Style::default().fg(crate::theme::theme().subtle_fg.into()),
            ));
        }
        spans.push(Span::styled(
            format!("  {detail}"),
            Style::default().fg(crate::theme::theme().tinted_fg.into()),
        ));
        lines.push(Line::from(spans));
    }
    if pending_len > 9 {
        lines.push(Line::from(Span::styled(
            format!("(+{} more, only 1..9 have hotkeys)", pending_len - 9),
            Style::default().fg(crate::theme::theme().subtle_fg.into()),
        )));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn approval_labels(
    tool: &str,
    call_intent: Option<&atman_runtime::message::ToolCallIntent>,
) -> (String, Option<String>) {
    match call_intent {
        Some(intent) => (intent.as_str().to_owned(), Some(tool.to_owned())),
        None => (tool.to_owned(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_labels_put_intent_before_technical_tool_name() {
        let intent = atman_runtime::message::ToolCallIntent::new("检查活动进程").unwrap();
        let (title, technical) = approval_labels("bash.spawn", Some(&intent));
        assert_eq!(title, "检查活动进程");
        assert_eq!(technical.as_deref(), Some("bash.spawn"));
    }

    #[test]
    fn approval_labels_keep_tool_as_legacy_fallback() {
        let (title, technical) = approval_labels("bash.spawn", None);
        assert_eq!(title, "bash.spawn");
        assert_eq!(technical, None);
    }
}
