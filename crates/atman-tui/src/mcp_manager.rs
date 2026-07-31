use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::theme;

pub fn render_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    scroll: u16,
    servers: &[atman_runtime::mcp::McpServerStatus],
) {
    let t = theme();

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            " MCP Servers",
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({} configured)", servers.len()),
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
        for s in servers {
            let transport_str = match s.transport {
                atman_runtime::mcp::TransportKind::Stdio => "stdio",
                atman_runtime::mcp::TransportKind::Http => "http",
                atman_runtime::mcp::TransportKind::Sse => "sse",
            };

            let (status_glyph, status_color, detail) = match &s.state {
                atman_runtime::mcp::McpServerState::Connected { tool_count } => {
                    ("●", t.success, format!("{} tools", tool_count))
                }
                atman_runtime::mcp::McpServerState::Connecting => {
                    ("◐", t.warn, "connecting...".to_string())
                }
                atman_runtime::mcp::McpServerState::Disabled => {
                    ("◌", t.subtle_fg, "disabled".to_string())
                }
                atman_runtime::mcp::McpServerState::Error { message } => (
                    "✗",
                    t.error,
                    format!("error: {}", truncate_msg(message, 30)),
                ),
                atman_runtime::mcp::McpServerState::Disconnected { message } => {
                    ("○", t.subtle_fg, truncate_msg(message, 30))
                }
                atman_runtime::mcp::McpServerState::Timeout { message } => {
                    ("⏱", t.warn, truncate_msg(message, 30))
                }
                atman_runtime::mcp::McpServerState::Pending => {
                    ("·", t.subtle_fg, "pending".to_string())
                }
            };

            let name_style = Style::default().fg(t.tinted_fg.into());
            let transport_style = Style::default().fg(t.subtle_fg.into());
            let status_style = Style::default().fg(status_color.into());
            let detail_style = Style::default().fg(t.subtle_fg.into());

            lines.push(Line::from(vec![
                Span::styled(format!(" {} ", status_glyph), status_style),
                Span::styled(format!("{:<20}", s.name), name_style),
                Span::styled(format!("{:<8}", transport_str), transport_style),
                Span::styled(detail, detail_style),
            ]));
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " Tip: use 'atman mcp list' / 'atman mcp test <name>' in CLI",
            Style::default().fg(t.subtle_fg.into()),
        )));
    }

    let visible: Vec<Line> = lines.into_iter().skip(scroll as usize).collect();
    let p = Paragraph::new(visible);
    f.render_widget(p, area);
}

fn truncate_msg(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}
