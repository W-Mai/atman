use crate::app::PendingPermission;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalClick {
    ApproveRequest(usize),
    DenyRequest(usize),
    ToggleGroup(atman_runtime::permission::PermissionGroupId),
    ApproveGroup(atman_runtime::permission::PermissionGroupId),
    DeferGroup(atman_runtime::permission::PermissionGroupId),
    ApproveAll,
    DenyAll,
}

#[derive(Debug, Clone, Default)]
pub struct ApprovalHitMap(pub Vec<(Rect, ApprovalClick)>);

impl ApprovalHitMap {
    pub fn at(&self, x: u16, y: u16) -> Option<ApprovalClick> {
        self.0.iter().find_map(|(rect, action)| {
            (x >= rect.x
                && x < rect.x.saturating_add(rect.width)
                && y >= rect.y
                && y < rect.y.saturating_add(rect.height))
            .then(|| action.clone())
        })
    }
}

pub fn render(
    f: &mut ratatui::Frame,
    area: Rect,
    canonical: &std::collections::BTreeMap<
        atman_runtime::permission::PermissionRequestId,
        PendingPermission,
    >,
    groups: &std::collections::BTreeMap<
        atman_runtime::permission::PermissionGroupId,
        crate::app::PendingPermissionGroup,
    >,
    grouped_request_ids: &std::collections::BTreeSet<
        atman_runtime::permission::PermissionRequestId,
    >,
    selected_group: Option<&atman_runtime::permission::PermissionGroupId>,
) -> ApprovalHitMap {
    let pending_len = canonical.len();
    let group_count = groups.len();
    if pending_len == 0 || area.height == 0 {
        return ApprovalHitMap::default();
    }
    let title = if group_count == 0 {
        format!(" ACTION REQUIRED · {pending_len} pending ")
    } else {
        format!(" ACTION REQUIRED · {pending_len} pending · {group_count} groups ")
    };
    const HINT: &str =
        " 1..9 allow · s scope · [/] group · g allow group · f defer · [allow all] [deny all] ";
    let hint = Line::from(Span::styled(
        HINT,
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
    let mut actions: Vec<(usize, usize, ApprovalClick)> = Vec::new();
    let rows: Vec<(String, Option<String>, String)> = canonical
        .values()
        .filter(|p| !grouped_request_ids.contains(&p.request_id))
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
            let risks = if p.payload.provenance.risks.is_empty() {
                String::new()
            } else {
                format!(
                    " · {}",
                    p.payload
                        .provenance
                        .risks
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            (
                title,
                technical,
                format!(
                    "{}{execution}{risks}",
                    p.payload.provenance.targets.join(", ")
                ),
            )
        })
        .collect();
    for group in groups.values() {
        let selected = if selected_group == Some(&group.group_id) {
            "● "
        } else {
            "  "
        };
        let marker = if group.expanded { "▾" } else { "▸" };
        let group_head = format!(
            "{selected}{marker}group · {} · {} requests",
            group.payload.label,
            group.payload.request_ids.len()
        );
        let action_base = crate::width::width(&group_head);
        let line_index = lines.len();
        lines.push(Line::from(vec![
            Span::styled(
                format!("{selected}{marker}group · "),
                Style::default()
                    .fg(crate::theme::theme().warn.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "{} · {} requests",
                    group.payload.label,
                    group.payload.request_ids.len()
                ),
                Style::default().fg(crate::theme::theme().tinted_fg.into()),
            ),
            Span::styled(
                "  details  allow group  defer ",
                Style::default().fg(crate::theme::theme().subtle_fg.into()),
            ),
        ]));
        actions.push((
            line_index,
            action_base + 2,
            ApprovalClick::ToggleGroup(group.group_id.clone()),
        ));
        actions.push((
            line_index,
            action_base + 11,
            ApprovalClick::ApproveGroup(group.group_id.clone()),
        ));
        actions.push((
            line_index,
            action_base + 24,
            ApprovalClick::DeferGroup(group.group_id.clone()),
        ));
        if group.expanded {
            for request_id in &group.payload.request_ids {
                if let Some(request) = canonical.get(request_id) {
                    let (title, technical) = approval_labels(
                        &request.payload.tool,
                        request.payload.call_intent.as_ref(),
                    );
                    let technical = technical
                        .map(|tool| format!(" · {tool}"))
                        .unwrap_or_default();
                    lines.push(Line::from(Span::styled(
                        format!("  └ {title}{technical}"),
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
            inner_width.saturating_sub(head_width + 14),
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
        let detail_text = format!("  {detail}");
        spans.push(Span::styled(
            detail_text.clone(),
            Style::default().fg(crate::theme::theme().tinted_fg.into()),
        ));
        let action_base = head_width + crate::width::width(&detail_text) - 2;
        spans.push(Span::styled(
            "  allow  deny ",
            Style::default().fg(crate::theme::theme().subtle_fg.into()),
        ));
        let line_index = lines.len();
        lines.push(Line::from(spans));
        actions.push((
            line_index,
            action_base + 2,
            ApprovalClick::ApproveRequest(i),
        ));
        actions.push((line_index, action_base + 9, ApprovalClick::DenyRequest(i)));
    }
    if pending_len > 9 {
        lines.push(Line::from(Span::styled(
            format!("(+{} more, only 1..9 have hotkeys)", pending_len - 9),
            Style::default().fg(crate::theme::theme().subtle_fg.into()),
        )));
    }
    let inner = block.inner(area);
    let mut hitmap = ApprovalHitMap::default();
    for (line, x_offset, action) in actions {
        if line >= inner.height as usize {
            continue;
        }
        let label_width = match action {
            ApprovalClick::ApproveRequest(_) => 5,
            ApprovalClick::DenyRequest(_) => 4,
            ApprovalClick::ToggleGroup(_) => 7,
            ApprovalClick::ApproveGroup(_) => 11,
            ApprovalClick::DeferGroup(_) => 5,
            ApprovalClick::ApproveAll | ApprovalClick::DenyAll => 0,
        };
        hitmap.0.push((
            Rect::new(
                inner
                    .x
                    .saturating_add(x_offset.min(u16::MAX as usize) as u16),
                inner.y + line as u16,
                label_width,
                1,
            ),
            action,
        ));
    }
    let hint_x = area
        .x
        .saturating_add(area.width.saturating_sub(1))
        .saturating_sub(crate::width::width(HINT) as u16);
    if let Some(offset) = HINT.find("[allow all]") {
        hitmap.0.push((
            Rect::new(
                hint_x + crate::width::width(&HINT[..offset]) as u16,
                area.y + area.height.saturating_sub(1),
                11,
                1,
            ),
            ApprovalClick::ApproveAll,
        ));
    }
    if let Some(offset) = HINT.find("[deny all]") {
        hitmap.0.push((
            Rect::new(
                hint_x + crate::width::width(&HINT[..offset]) as u16,
                area.y + area.height.saturating_sub(1),
                10,
                1,
            ),
            ApprovalClick::DenyAll,
        ));
    }
    f.render_widget(Paragraph::new(lines).block(block), area);
    hitmap
}

fn approval_labels(
    tool: &str,
    call_intent: Option<&atman_runtime::message::ToolCallIntent>,
) -> (String, Option<String>) {
    match call_intent {
        Some(intent) => (intent.as_str().to_owned(), Some(tool.to_owned())),
        None => (
            fallback_approval_label(tool).to_owned(),
            Some(tool.to_owned()),
        ),
    }
}

fn fallback_approval_label(tool: &str) -> &'static str {
    if tool.starts_with("bash.") || tool.starts_with("terminal.") {
        "Run command"
    } else if tool.starts_with("fs.write") || tool.starts_with("fs.edit") {
        "Modify files"
    } else if tool.starts_with("fs.") || tool == "image.read" {
        "Read files"
    } else if tool.starts_with("web.") || tool.starts_with("http.") {
        "Access network"
    } else if tool.starts_with("flow.") {
        "Start flow"
    } else {
        "Allow tool call"
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
        assert_eq!(title, "Run command");
        assert_eq!(technical.as_deref(), Some("bash.spawn"));
    }
}
