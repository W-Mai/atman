use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::floating_panels::FloatingPanelHitmap;
use crate::theme::theme;
use crate::width;
use std::collections::HashSet;

/// Indent + name column + space = 3 + 22 + 1 = 26 chars before description.
const TOOL_DESC_OFFSET: usize = 26;

pub struct McpBrowserState<'a> {
    pub tab: McpBrowserTab,
    pub resources: &'a std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>>,
    pub prompts: &'a std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>>,
}

#[allow(clippy::too_many_arguments)]
pub fn render_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    scroll: &mut u16,
    servers: &[atman_runtime::mcp::McpServerStatus],
    expanded: &HashSet<String>,
    selected: usize,
    hovered: &Option<String>,
    hitmap_out: &mut FloatingPanelHitmap,
    browser: &McpBrowserState<'_>,
) {
    let t = theme();
    let mut lines: Vec<Line> = Vec::new();

    let (ok, total) = atman_runtime::mcp::mcp_counts(servers);

    let desc_width = (area.width as usize)
        .saturating_sub(TOOL_DESC_OFFSET)
        .max(10);
    let total_lines = compute_total_lines(servers, expanded, desc_width);
    let max_scroll = total_lines.saturating_sub(area.height);
    *scroll = (*scroll).min(max_scroll);

    lines.push(Line::from(vec![
        Span::styled(
            " MCP Servers",
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            format!("({ok}/{total} connected)"),
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
                    width::pad_right(&width::truncate(&s.name, 18), 18),
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

            let abs_y = area.y + (lines.len() as u16).saturating_sub(*scroll);
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
                    // Tab switch hint
                    let tab_label = |tab: McpBrowserTab, current: McpBrowserTab| -> String {
                        if tab == current {
                            format!("[●]{}", tab.label())
                        } else {
                            format!("[ ]{}", tab.label())
                        }
                    };
                    let tabs_line = format!(
                        " Tab: {}  {}  {}",
                        tab_label(McpBrowserTab::Tools, browser.tab),
                        tab_label(McpBrowserTab::Resources, browser.tab),
                        tab_label(McpBrowserTab::Prompts, browser.tab),
                    );
                    lines.push(Line::from(Span::styled(
                        tabs_line,
                        Style::default().fg(t.subtle_fg.into()),
                    )));

                    match browser.tab {
                        McpBrowserTab::Tools => {
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

                                for tool in tools {
                                    let name = width::truncate(&tool.name, 22);
                                    let desc = tool.description.as_deref().unwrap_or("");
                                    let desc_wrapped = width::word_wrap(desc, desc_width);
                                    for (di, desc_line) in desc_wrapped.iter().enumerate() {
                                        let name_str = if di == 0 {
                                            width::pad_right(&name, 22)
                                        } else {
                                            " ".repeat(22)
                                        };
                                        lines.push(Line::from(vec![
                                            Span::styled(
                                                "   ",
                                                Style::default().bg(t.code_bg.into()),
                                            ),
                                            Span::styled(
                                                name_str,
                                                Style::default().fg(t.tinted_fg.into()),
                                            ),
                                            Span::styled(
                                                desc_line.clone(),
                                                Style::default().fg(t.subtle_fg.into()),
                                            ),
                                        ]));
                                    }
                                }
                            }
                        }
                        McpBrowserTab::Resources => {
                            let resources = browser.resources.get(&s.name);
                            match resources {
                                None => {
                                    lines.push(Line::from(Span::styled(
                                        "   loading…",
                                        Style::default().fg(t.subtle_fg.into()),
                                    )));
                                }
                                Some(rs) if rs.is_empty() => {
                                    lines.push(Line::from(Span::styled(
                                        "   (no resources)",
                                        Style::default().fg(t.subtle_fg.into()),
                                    )));
                                }
                                Some(rs) => {
                                    for r in rs {
                                        let name = width::truncate(&r.name, 22);
                                        let desc = r.description.as_deref().unwrap_or("");
                                        lines.push(Line::from(vec![
                                            Span::styled(
                                                "   ",
                                                Style::default().bg(t.code_bg.into()),
                                            ),
                                            Span::styled(
                                                width::pad_right(&name, 22),
                                                Style::default().fg(t.tinted_fg.into()),
                                            ),
                                            Span::styled(
                                                desc.to_string(),
                                                Style::default().fg(t.subtle_fg.into()),
                                            ),
                                        ]));
                                    }
                                }
                            }
                        }
                        McpBrowserTab::Prompts => {
                            let prompts = browser.prompts.get(&s.name);
                            match prompts {
                                None => {
                                    lines.push(Line::from(Span::styled(
                                        "   loading…",
                                        Style::default().fg(t.subtle_fg.into()),
                                    )));
                                }
                                Some(ps) if ps.is_empty() => {
                                    lines.push(Line::from(Span::styled(
                                        "   (no prompts)",
                                        Style::default().fg(t.subtle_fg.into()),
                                    )));
                                }
                                Some(ps) => {
                                    for p in ps {
                                        let name = width::truncate(&p.name, 22);
                                        let desc = p.description.as_deref().unwrap_or("");
                                        lines.push(Line::from(vec![
                                            Span::styled(
                                                "   ",
                                                Style::default().bg(t.code_bg.into()),
                                            ),
                                            Span::styled(
                                                width::pad_right(&name, 22),
                                                Style::default().fg(t.tinted_fg.into()),
                                            ),
                                            Span::styled(
                                                desc.to_string(),
                                                Style::default().fg(t.subtle_fg.into()),
                                            ),
                                        ]));
                                    }
                                }
                            }
                        }
                    }
                    lines.push(Line::from(""));
                }
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            " [a]dd  [Enter] expand  [Tab] switch  [↑↓] navigate  [t] test  [r] remove  [d] toggle  [Esc] close",
            Style::default().fg(t.subtle_fg.into()),
        )));
    }

    let visible: Vec<Line> = lines.into_iter().skip(*scroll as usize).collect();
    let p = Paragraph::new(visible);
    f.render_widget(p, area);
}

/// Compute total visual line count for scroll clamping.
fn compute_total_lines(
    servers: &[atman_runtime::mcp::McpServerStatus],
    expanded: &HashSet<String>,
    desc_width: usize,
) -> u16 {
    // header + blank
    let mut count: u16 = 2;
    if servers.is_empty() {
        return count + 6;
    }
    // table header + separator
    count += 2;
    for s in servers {
        count += 1; // server row
        if expanded.contains(&s.name) {
            if let atman_runtime::mcp::McpServerState::Connected { tools, .. } = &s.state {
                if tools.is_empty() {
                    count += 1;
                } else {
                    count += 2; // tool header + separator
                    for tool in tools {
                        let desc = tool.description.as_deref().unwrap_or("");
                        count += width::word_wrap(desc, desc_width).len() as u16;
                    }
                }
                count += 1; // blank after tool table
            }
        }
    }
    count += 2; // blank + footer
    count
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
            format!("error: {}", width::truncate(message, 24)),
        ),
        atman_runtime::mcp::McpServerState::Disconnected { message } => (
            "○",
            t.subtle_fg.into(),
            "—".into(),
            width::truncate(message, 24),
        ),
        atman_runtime::mcp::McpServerState::Timeout { message } => {
            ("⏱", t.warn.into(), "—".into(), width::truncate(message, 24))
        }
        atman_runtime::mcp::McpServerState::Pending => {
            ("·", t.subtle_fg.into(), "—".into(), "pending".into())
        }
    }
}

// ── MCP Add Form ──

const MCP_ADD_FIELDS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpBrowserTab {
    #[default]
    Tools,
    Resources,
    Prompts,
}

impl McpBrowserTab {
    pub fn next(self) -> Self {
        match self {
            Self::Tools => Self::Resources,
            Self::Resources => Self::Prompts,
            Self::Prompts => Self::Tools,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tools => "Tools",
            Self::Resources => "Resources",
            Self::Prompts => "Prompts",
        }
    }
}

pub struct McpAddForm {
    pub name: crate::input::InputEditor,
    pub command: crate::input::InputEditor,
    pub url: crate::input::InputEditor,
    pub args: crate::input::InputEditor,
    pub env: crate::input::InputEditor,
    pub transport_idx: usize,
    pub tier_idx: usize,
    pub field: usize,
    pub error: Option<String>,
}

const TRANSPORT_OPTIONS: [&str; 3] = ["stdio", "http", "sse"];
const TIER_OPTIONS: [u8; 3] = [1, 2, 3];

impl Default for McpAddForm {
    fn default() -> Self {
        Self {
            name: crate::input::InputEditor::default(),
            command: crate::input::InputEditor::default(),
            url: crate::input::InputEditor::default(),
            args: crate::input::InputEditor::default(),
            env: crate::input::InputEditor::default(),
            transport_idx: 0,
            tier_idx: 2,
            field: 0,
            error: None,
        }
    }
}

impl McpAddForm {
    pub fn transport(&self) -> atman_runtime::mcp::TransportKind {
        match self.transport_idx {
            1 => atman_runtime::mcp::TransportKind::Http,
            2 => atman_runtime::mcp::TransportKind::Sse,
            _ => atman_runtime::mcp::TransportKind::Stdio,
        }
    }

    pub fn tier(&self) -> atman_runtime::tool::Tier {
        match TIER_OPTIONS[self.tier_idx] {
            1 => atman_runtime::tool::Tier::One,
            2 => atman_runtime::tool::Tier::Two,
            _ => atman_runtime::tool::Tier::Three,
        }
    }

    pub fn next_field(&mut self) {
        self.field = (self.field + 1) % MCP_ADD_FIELDS;
    }

    pub fn prev_field(&mut self) {
        self.field = (self.field + MCP_ADD_FIELDS - 1) % MCP_ADD_FIELDS;
    }

    pub fn build_config(&self) -> Result<atman_runtime::mcp::McpServerConfig, String> {
        let name = self.name.buf().trim().to_string();
        if name.is_empty() {
            return Err("name is required".into());
        }

        let transport = self.transport();
        let tier = self.tier();

        let args: Vec<String> = self
            .args
            .buf()
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        let env: Vec<(String, String)> = self
            .env
            .buf()
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                if l.is_empty() {
                    return None;
                }
                let (k, v) = l.split_once('=')?;
                Some((k.trim().to_string(), v.trim().to_string()))
            })
            .collect();

        let timeout_ms = 30000;

        Ok(match transport {
            atman_runtime::mcp::TransportKind::Stdio => {
                let command = self.command.buf().trim().to_string();
                if command.is_empty() {
                    return Err("command is required for stdio transport".into());
                }
                let mut cfg = atman_runtime::mcp::McpServerConfig::stdio(
                    &name, &command, args, tier, timeout_ms,
                );
                cfg.env = env;
                cfg
            }
            atman_runtime::mcp::TransportKind::Http => {
                let url = self.url.buf().trim().to_string();
                if url.is_empty() {
                    return Err("url is required for http transport".into());
                }
                atman_runtime::mcp::McpServerConfig::http(&name, &url, None, tier, timeout_ms)
            }
            atman_runtime::mcp::TransportKind::Sse => {
                let url = self.url.buf().trim().to_string();
                if url.is_empty() {
                    return Err("url is required for sse transport".into());
                }
                atman_runtime::mcp::McpServerConfig::sse(&name, &url, None, tier, timeout_ms)
            }
        })
    }
}

pub fn render_mcp_add_form(f: &mut ratatui::Frame, area: Rect, form: &McpAddForm) {
    let t = theme();
    use ratatui::widgets::{Block, Clear};

    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(t.modal_bg.into())),
        area,
    );

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        " Add MCP Server",
        Style::default()
            .fg(t.accent.into())
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let label_style = |active: bool| {
        if active {
            Style::default()
                .fg(t.accent.into())
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.tinted_fg.into())
        }
    };

    let val_style = Style::default().fg(t.tinted_fg.into());

    // Field 0: Name
    let active = form.field == 0;
    lines.push(Line::from(Span::styled(" Name:", label_style(active))));
    lines.push(Line::from(Span::styled(
        format!("  {}", form.name.buf()),
        val_style,
    )));

    // Field 1: Transport
    let active = form.field == 1;
    lines.push(Line::from(Span::styled(" Transport:", label_style(active))));
    let transport_str = TRANSPORT_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, opt)| {
            if i == form.transport_idx {
                format!("(●){opt}")
            } else {
                format!("( ){opt}")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(Line::from(Span::styled(
        format!("  {transport_str}"),
        val_style,
    )));

    // Field 2: Command (stdio) or URL (http/sse)
    let is_stdio = form.transport_idx == 0;
    let active = form.field == 2;
    let label = if is_stdio { " Command:" } else { " URL:" };
    lines.push(Line::from(Span::styled(label, label_style(active))));
    let val = if is_stdio {
        form.command.buf()
    } else {
        form.url.buf()
    };
    lines.push(Line::from(Span::styled(format!("  {val}"), val_style)));

    // Field 3: Args
    let active = form.field == 3;
    lines.push(Line::from(Span::styled(" Args:", label_style(active))));
    lines.push(Line::from(Span::styled(
        format!("  {}", form.args.buf()),
        val_style,
    )));

    // Field 4: Env
    let active = form.field == 4;
    lines.push(Line::from(Span::styled(" Env:", label_style(active))));
    lines.push(Line::from(Span::styled(
        format!("  {}", form.env.buf()),
        val_style,
    )));

    // Field 5: Tier
    let active = form.field == 5;
    lines.push(Line::from(Span::styled(" Tier:", label_style(active))));
    let tier_str = TIER_OPTIONS
        .iter()
        .enumerate()
        .map(|(i, &t_val)| {
            if i == form.tier_idx {
                format!("(●){t_val}")
            } else {
                format!("( ){t_val}")
            }
        })
        .collect::<Vec<_>>()
        .join("  ");
    lines.push(Line::from(Span::styled(format!("  {tier_str}"), val_style)));

    lines.push(Line::from(""));

    if let Some(err) = &form.error {
        lines.push(Line::from(Span::styled(
            format!(" ⚠ {err}"),
            Style::default().fg(t.error.into()),
        )));
        lines.push(Line::from(""));
    }

    lines.push(Line::from(Span::styled(
        " Tab cycle · ←→ change select · Enter save · Esc cancel",
        Style::default().fg(t.subtle_fg.into()),
    )));

    let para = Paragraph::new(lines);
    f.render_widget(para, area);
}
