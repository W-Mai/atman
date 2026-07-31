use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::floating_panels::FloatingPanelHitmap;
use crate::theme::theme;
use std::collections::HashSet;

#[allow(clippy::too_many_arguments)]
pub fn render_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    scroll: u16,
    servers: &[atman_runtime::mcp::McpServerStatus],
    expanded: &HashSet<String>,
    selected: usize,
    hovered: &Option<String>,
    hitmap_out: &mut FloatingPanelHitmap,
) {
    let t = theme();
    let mut lines: Vec<Line> = Vec::new();

    let (ok, total) = atman_runtime::mcp::mcp_counts(servers);
    lines.push(Line::from(vec![
        Span::styled(
            " MCP Servers",
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({ok}/{total} connected)",),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]));
    lines.push(Line::from(""));

    if servers.is_empty() {
        lines.push(Line::from(Span::styled(
            " No MCP servers configured.",
            Style::default().fg(t.subtle_fg.into()),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Add servers via CLI:",
            Style::default().fg(t.subtle_fg.into()),
        )));
        lines.push(Line::from(Span::styled(
            "   atman mcp add",
            Style::default().fg(t.tinted_fg.into()),
        )));
        lines.push(Line::from(Span::styled(
            "   atman mcp add --template filesystem",
            Style::default().fg(t.tinted_fg.into()),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Or edit ~/.config/atman/mcp_servers.json",
            Style::default().fg(t.subtle_fg.into()),
        )));
    } else {
        lines.push(Line::from(vec![Span::styled(
            " St  Name              Transport  Tools  Detail",
            Style::default()
                .fg(t.subtle_fg.into())
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(Span::styled(
            " ──  ────────────────  ─────────  ─────  ──────────────",
            Style::default().fg(t.subtle_fg.into()),
        )));

        for (i, s) in servers.iter().enumerate() {
            let transport_str = match s.transport {
                atman_runtime::mcp::TransportKind::Stdio => "stdio",
                atman_runtime::mcp::TransportKind::Http => "http",
                atman_runtime::mcp::TransportKind::Sse => "sse",
            };

            let (status_glyph, status_color, tools_str, detail) = server_display(s, &t);

            let is_selected = i == selected;
            let is_hovered = hovered.as_deref() == Some(&s.name);
            let is_expanded = expanded.contains(&s.name);

            let bg = if is_selected {
                t.highlight_bg.into()
            } else if is_hovered {
                t.user_msg_bg.into()
            } else {
                t.code_bg.into()
            };

            let arrow = if is_expanded { "▼" } else { "▶" };
            let detail_str = if is_expanded && s.is_ok() {
                "expanded".to_string()
            } else {
                detail
            };

            let row_line = Line::from(vec![
                Span::styled(
                    format!(" {} ", status_glyph),
                    Style::default().fg(status_color).bg(bg),
                ),
                Span::styled(
                    format!("{:<18}", truncate_name(&s.name, 18)),
                    Style::default().fg(t.tinted_fg.into()).bg(bg),
                ),
                Span::styled(
                    format!("{:<10}", transport_str),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                ),
                Span::styled(
                    format!("{:<6}", tools_str),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                ),
                Span::styled(
                    format!("{} {}", arrow, detail_str),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                ),
            ]);

            let abs_y = area.y + (lines.len() as u16).saturating_sub(scroll);
            if abs_y >= area.y && abs_y < area.y + area.height {
                hitmap_out.mcp_row_rects.push((
                    s.name.clone(),
                    Rect {
                        x: area.x,
                        y: abs_y,
                        width: area.width,
                        height: 1,
                    },
                ));
            }

            lines.push(row_line);

            if is_expanded {
                if let atman_runtime::mcp::McpServerState::Connected { tools, .. } = &s.state {
                    if tools.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "   (no tools exposed)",
                            Style::default().fg(t.subtle_fg.into()),
                        )));
                    } else {
                        lines.push(Line::from(vec![
                            Span::styled("   ", Style::default().bg(t.code_bg.into())),
                            Span::styled(
                                format!("{:<22} Description", "Tool"),
                                Style::default()
                                    .fg(t.subtle_fg.into())
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                        lines.push(Line::from(Span::styled(
                            "   ────────────────────  ──────────────────────────────",
                            Style::default().fg(t.subtle_fg.into()),
                        )));

                        let avail = (area.width as usize).saturating_sub(28);
                        for tool in tools {
                            let desc = tool.description.as_deref().unwrap_or("");
                            let desc_trunc = truncate_msg(desc, avail.max(10));
                            lines.push(Line::from(vec![
                                Span::styled("   ", Style::default().bg(t.code_bg.into())),
                                Span::styled(
                                    format!("{:<22}", truncate_name(&tool.name, 22)),
                                    Style::default().fg(t.tinted_fg.into()),
                                ),
                                Span::styled(desc_trunc, Style::default().fg(t.subtle_fg.into())),
                            ]));
                        }
                    }
                    lines.push(Line::from(""));
                }
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " [Enter] expand  [↑↓] navigate  [t] test  [r] remove  [d] toggle  [Esc] close",
            Style::default().fg(t.subtle_fg.into()),
        )));
    }

    let visible: Vec<Line> = lines.into_iter().skip(scroll as usize).collect();
    let p = Paragraph::new(visible);
    f.render_widget(p, area);
}

fn server_display(
    s: &atman_runtime::mcp::McpServerStatus,
    t: &crate::theme::Theme,
) -> (&'static str, ratatui::style::Color, String, String) {
    match &s.state {
        atman_runtime::mcp::McpServerState::Connected { tool_count, .. } => (
            "●",
            t.success.into(),
            tool_count.to_string(),
            format!("{} tools", tool_count),
        ),
        atman_runtime::mcp::McpServerState::Connecting => {
            ("◐", t.warn.into(), "—".into(), "connecting...".into())
        }
        atman_runtime::mcp::McpServerState::Disabled => {
            ("◌", t.subtle_fg.into(), "—".into(), "disabled".into())
        }
        atman_runtime::mcp::McpServerState::Error { message } => (
            "✗",
            t.error.into(),
            "—".into(),
            format!("error: {}", truncate_msg(message, 24)),
        ),
        atman_runtime::mcp::McpServerState::Disconnected { message } => (
            "○",
            t.subtle_fg.into(),
            "—".into(),
            truncate_msg(message, 24),
        ),
        atman_runtime::mcp::McpServerState::Timeout { message } => {
            ("⏱", t.warn.into(), "—".into(), truncate_msg(message, 24))
        }
        atman_runtime::mcp::McpServerState::Pending => {
            ("·", t.subtle_fg.into(), "—".into(), "pending".into())
        }
    }
}

fn truncate_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

fn truncate_name(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
