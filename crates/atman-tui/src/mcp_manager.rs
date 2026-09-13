use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::theme::theme;
use crate::width;
use crate::wm::WmHitmap;
use std::collections::HashSet;

/// Indent + name column + space = 3 + 22 + 1 = 26 chars before description.
const TOOL_DESC_OFFSET: usize = 26;

pub struct McpBrowserState<'a> {
    pub tab: McpBrowserTab,
    pub content_revision: u64,
    pub resources: &'a std::collections::HashMap<String, Vec<atman_runtime::mcp::McpResource>>,
    pub prompts: &'a std::collections::HashMap<String, Vec<atman_runtime::mcp::McpPrompt>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpProjectionKey {
    content_revision: u64,
    width: u16,
    theme_mode: crate::theme::ThemeMode,
    tab: McpBrowserTab,
    expanded_servers: Vec<String>,
}

#[derive(Debug, Clone)]
enum McpProjectedRow {
    Static(Line<'static>),
    Server(usize),
}

#[derive(Debug, Default)]
pub struct McpPanelProjection {
    key: Option<McpProjectionKey>,
    rows: Vec<McpProjectedRow>,
    #[cfg(test)]
    rebuild_count: u64,
    #[cfg(test)]
    wrap_count: u64,
}

impl McpPanelProjection {
    fn update(
        &mut self,
        width: u16,
        servers: &[atman_runtime::mcp::McpServerStatus],
        expanded: &HashSet<String>,
        browser: &McpBrowserState<'_>,
    ) {
        let theme_mode = crate::theme::current_mode();
        if self.key.as_ref().is_some_and(|key| {
            key.content_revision == browser.content_revision
                && key.width == width
                && key.theme_mode == theme_mode
                && key.tab == browser.tab
                && key.expanded_servers.len() == expanded.len()
                && key
                    .expanded_servers
                    .iter()
                    .all(|name| expanded.contains(name))
        }) {
            return;
        }

        let mut expanded_servers: Vec<String> = expanded.iter().cloned().collect();
        expanded_servers.sort_unstable();
        let mut wrap_count = 0;
        self.rows = build_projected_rows(width, servers, expanded, browser, &mut wrap_count);
        self.key = Some(McpProjectionKey {
            content_revision: browser.content_revision,
            width,
            theme_mode,
            tab: browser.tab,
            expanded_servers,
        });
        #[cfg(test)]
        {
            self.rebuild_count = self.rebuild_count.wrapping_add(1);
            self.wrap_count = self.wrap_count.wrapping_add(wrap_count);
        }
    }

    fn total_rows(&self) -> u32 {
        u32::try_from(self.rows.len()).unwrap_or(u32::MAX)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_panel(
    f: &mut ratatui::Frame,
    area: Rect,
    scroll: &mut u32,
    servers: &[atman_runtime::mcp::McpServerStatus],
    expanded: &HashSet<String>,
    selected: usize,
    hovered: &Option<String>,
    hitmap_out: &mut WmHitmap,
    browser: &McpBrowserState<'_>,
    window_id: crate::wm::WindowId,
    projection: &mut McpPanelProjection,
) {
    let t = theme();
    projection.update(area.width, servers, expanded, browser);

    let content_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let help_area = Rect {
        y: content_area.bottom(),
        height: area.height.min(1),
        ..area
    };
    *scroll = clamp_scroll(*scroll, projection.total_rows(), content_area.height);

    let start = usize::try_from(*scroll).unwrap_or(usize::MAX);
    let end = start
        .saturating_add(content_area.height as usize)
        .min(projection.rows.len());
    let mut visible = Vec::with_capacity(end.saturating_sub(start));
    for (visible_y, row) in projection.rows[start.min(end)..end].iter().enumerate() {
        match row {
            McpProjectedRow::Static(line) => visible.push(line.clone()),
            McpProjectedRow::Server(index) => {
                let Some(server) = servers.get(*index) else {
                    continue;
                };
                visible.push(server_line(
                    server,
                    *index == selected,
                    hovered.as_deref() == Some(&server.name),
                    expanded.contains(&server.name),
                    &t,
                ));
                hitmap_out.mcp_row_rects.push((
                    window_id,
                    server.name.clone(),
                    Rect {
                        x: content_area.x,
                        y: content_area.y.saturating_add(visible_y as u16),
                        width: content_area.width,
                        height: 1,
                    },
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(visible), content_area);
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " [a] Add  [e] Edit  [Enter] expand  [Tab] switch  [↑↓] navigate  [t] test  [r] remove  [d] toggle  [Esc] close",
            Style::default().fg(t.subtle_fg.into()),
        ))),
        help_area,
    );
    hitmap_out.mcp_action_rects.push((
        window_id,
        crate::wm::component::McpPanelAction::Add,
        Rect::new(help_area.x + 1, help_area.y, 7.min(help_area.width), 1),
    ));
    if help_area.width > 9 {
        hitmap_out.mcp_action_rects.push((
            window_id,
            crate::wm::component::McpPanelAction::Edit,
            Rect::new(
                help_area.x + 10,
                help_area.y,
                8.min(help_area.width - 10),
                1,
            ),
        ));
    }
}

fn clamp_scroll(scroll: u32, total_rows: u32, viewport_height: u16) -> u32 {
    scroll.min(total_rows.saturating_sub(viewport_height as u32))
}

fn build_projected_rows(
    width: u16,
    servers: &[atman_runtime::mcp::McpServerStatus],
    expanded: &HashSet<String>,
    browser: &McpBrowserState<'_>,
    wrap_count: &mut u64,
) -> Vec<McpProjectedRow> {
    let t = theme();
    let mut rows = Vec::new();
    let (ok, total) = atman_runtime::mcp::mcp_counts(servers);
    rows.push(McpProjectedRow::Static(Line::from(vec![
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
    ])));
    rows.push(McpProjectedRow::Static(Line::from("")));

    if servers.is_empty() {
        for line in [
            Line::from(Span::styled(
                " No MCP servers configured.",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " Add servers via CLI:",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(Span::styled(
                "   atman mcp add",
                Style::default().fg(t.tinted_fg.into()),
            )),
            Line::from(Span::styled(
                "   atman mcp add --template filesystem",
                Style::default().fg(t.tinted_fg.into()),
            )),
            Line::from(""),
            Line::from(Span::styled(
                " Or edit ~/.config/atman/mcp_servers.json",
                Style::default().fg(t.subtle_fg.into()),
            )),
        ] {
            rows.push(McpProjectedRow::Static(line));
        }
        return rows;
    }

    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
        " St  Name              Transport  Tools  Detail",
        Style::default()
            .fg(t.subtle_fg.into())
            .add_modifier(Modifier::BOLD),
    ))));
    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
        " ──  ────────────────  ─────────  ─────  ──────────────",
        Style::default().fg(t.subtle_fg.into()),
    ))));

    let desc_width = (width as usize).saturating_sub(TOOL_DESC_OFFSET).max(10);
    for (index, server) in servers.iter().enumerate() {
        rows.push(McpProjectedRow::Server(index));
        if !expanded.contains(&server.name) {
            continue;
        }
        let atman_runtime::mcp::McpServerState::Connected { tools, .. } = &server.state else {
            continue;
        };

        let tab_label = |tab: McpBrowserTab| {
            if tab == browser.tab {
                format!("[●]{}", tab.label())
            } else {
                format!("[ ]{}", tab.label())
            }
        };
        rows.push(McpProjectedRow::Static(Line::from(Span::styled(
            format!(
                " Tab: {}  {}  {}",
                tab_label(McpBrowserTab::Tools),
                tab_label(McpBrowserTab::Resources),
                tab_label(McpBrowserTab::Prompts),
            ),
            Style::default().fg(t.subtle_fg.into()),
        ))));

        match browser.tab {
            McpBrowserTab::Tools => {
                if tools.is_empty() {
                    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                        "   (no tools exposed)",
                        Style::default().fg(t.subtle_fg.into()),
                    ))));
                } else {
                    rows.push(McpProjectedRow::Static(Line::from(vec![
                        Span::styled("   ", Style::default().bg(t.code_bg.into())),
                        Span::styled(
                            format!("{:<22} Description", "Tool"),
                            Style::default()
                                .fg(t.subtle_fg.into())
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])));
                    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                        "   ────────────────────  ──────────────────────────────",
                        Style::default().fg(t.subtle_fg.into()),
                    ))));
                    for tool in tools {
                        let name = width::truncate(&tool.name, 22);
                        let wrapped =
                            width::word_wrap(tool.description.as_deref().unwrap_or(""), desc_width);
                        *wrap_count = wrap_count.wrapping_add(1);
                        for (line_index, description) in wrapped.into_iter().enumerate() {
                            rows.push(McpProjectedRow::Static(Line::from(vec![
                                Span::styled("   ", Style::default().bg(t.code_bg.into())),
                                Span::styled(
                                    if line_index == 0 {
                                        width::pad_right(&name, 22)
                                    } else {
                                        " ".repeat(22)
                                    },
                                    Style::default().fg(t.tinted_fg.into()),
                                ),
                                Span::styled(description, Style::default().fg(t.subtle_fg.into())),
                            ])));
                        }
                    }
                }
            }
            McpBrowserTab::Resources => match browser.resources.get(&server.name) {
                None => rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                    "   loading…",
                    Style::default().fg(t.subtle_fg.into()),
                )))),
                Some(resources) if resources.is_empty() => {
                    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                        "   (no resources)",
                        Style::default().fg(t.subtle_fg.into()),
                    ))));
                }
                Some(resources) => {
                    for resource in resources {
                        rows.push(McpProjectedRow::Static(named_description_line(
                            &resource.name,
                            resource.description.as_deref().unwrap_or(""),
                            &t,
                        )));
                    }
                }
            },
            McpBrowserTab::Prompts => match browser.prompts.get(&server.name) {
                None => rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                    "   loading…",
                    Style::default().fg(t.subtle_fg.into()),
                )))),
                Some(prompts) if prompts.is_empty() => {
                    rows.push(McpProjectedRow::Static(Line::from(Span::styled(
                        "   (no prompts)",
                        Style::default().fg(t.subtle_fg.into()),
                    ))));
                }
                Some(prompts) => {
                    for prompt in prompts {
                        rows.push(McpProjectedRow::Static(named_description_line(
                            &prompt.name,
                            prompt.description.as_deref().unwrap_or(""),
                            &t,
                        )));
                    }
                }
            },
        }
        rows.push(McpProjectedRow::Static(Line::from("")));
    }
    rows
}

fn named_description_line(name: &str, description: &str, t: &crate::theme::Theme) -> Line<'static> {
    let name = width::truncate(name, 22);
    Line::from(vec![
        Span::styled("   ", Style::default().bg(t.code_bg.into())),
        Span::styled(
            width::pad_right(&name, 22),
            Style::default().fg(t.tinted_fg.into()),
        ),
        Span::styled(
            description.to_string(),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ])
}

fn server_line(
    server: &atman_runtime::mcp::McpServerStatus,
    selected: bool,
    hovered: bool,
    expanded: bool,
    t: &crate::theme::Theme,
) -> Line<'static> {
    let transport = match server.transport {
        atman_runtime::mcp::TransportKind::Stdio => "stdio",
        atman_runtime::mcp::TransportKind::Http => "http",
        atman_runtime::mcp::TransportKind::Sse => "sse",
    };
    let (status_glyph, status_color, tools, detail) = server_display(server, t);
    let bg = if selected {
        t.highlight_bg.into()
    } else if hovered {
        t.user_msg_bg.into()
    } else {
        t.code_bg.into()
    };
    let detail = if expanded && server.is_ok() {
        "expanded".to_string()
    } else {
        detail
    };
    Line::from(vec![
        Span::styled(
            format!(" {status_glyph} "),
            Style::default().fg(status_color).bg(bg),
        ),
        Span::styled(
            width::pad_right(&width::truncate(&server.name, 18), 18),
            Style::default().fg(t.tinted_fg.into()).bg(bg),
        ),
        Span::styled(
            format!("{transport:<10}"),
            Style::default().fg(t.subtle_fg.into()).bg(bg),
        ),
        Span::styled(
            format!("{tools:<6}"),
            Style::default().fg(t.subtle_fg.into()).bg(bg),
        ),
        Span::styled(
            format!("{} {detail}", if expanded { "▼" } else { "▶" }),
            Style::default().fg(t.subtle_fg.into()).bg(bg),
        ),
    ])
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

#[derive(Clone)]
pub enum McpEditorMode {
    Add,
    Edit {
        original_name: String,
        original: Box<atman_runtime::mcp::McpServerConfig>,
    },
}

pub struct McpEditor {
    pub open: bool,
    pub name: crate::input::InputEditor,
    pub command: crate::input::InputEditor,
    pub url: crate::input::InputEditor,
    pub args: crate::input::InputEditor,
    pub env: crate::input::InputEditor,
    pub transport_idx: usize,
    pub tier_idx: usize,
    pub field: usize,
    pub error: Option<String>,
    mode: McpEditorMode,
    cursor_position: Option<(u16, u16)>,
}

const TRANSPORT_OPTIONS: [&str; 3] = ["stdio", "http", "sse"];
const TIER_OPTIONS: [u8; 3] = [1, 2, 3];

impl Default for McpEditor {
    fn default() -> Self {
        Self {
            open: false,
            name: crate::input::InputEditor::default(),
            command: crate::input::InputEditor::default(),
            url: crate::input::InputEditor::default(),
            args: crate::input::InputEditor::default(),
            env: crate::input::InputEditor::default(),
            transport_idx: 0,
            tier_idx: 2,
            field: 0,
            error: None,
            mode: McpEditorMode::Add,
            cursor_position: None,
        }
    }
}

impl McpEditor {
    pub fn open_add(&mut self) {
        *self = Self::default();
        self.open = true;
    }

    pub fn open_edit(&mut self, config: atman_runtime::mcp::McpServerConfig) {
        let original_name = config.name.clone();
        *self = Self::default();
        self.open = true;
        self.name.replace_with(&config.name);
        self.command.replace_with(&config.command);
        self.url
            .replace_with(config.url.as_deref().unwrap_or_default());
        self.args.replace_with(&config.args.join(" "));
        self.env.replace_with(
            &config
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        self.transport_idx = match config.transport {
            atman_runtime::mcp::TransportKind::Stdio => 0,
            atman_runtime::mcp::TransportKind::Http => 1,
            atman_runtime::mcp::TransportKind::Sse => 2,
        };
        self.tier_idx = match config.tier {
            atman_runtime::tool::Tier::One => 0,
            atman_runtime::tool::Tier::Two => 1,
            _ => 2,
        };
        self.mode = McpEditorMode::Edit {
            original_name,
            original: Box::new(config),
        };
    }

    fn transport(&self) -> atman_runtime::mcp::TransportKind {
        match self.transport_idx {
            1 => atman_runtime::mcp::TransportKind::Http,
            2 => atman_runtime::mcp::TransportKind::Sse,
            _ => atman_runtime::mcp::TransportKind::Stdio,
        }
    }

    fn tier(&self) -> atman_runtime::tool::Tier {
        match TIER_OPTIONS[self.tier_idx] {
            1 => atman_runtime::tool::Tier::One,
            2 => atman_runtime::tool::Tier::Two,
            _ => atman_runtime::tool::Tier::Three,
        }
    }

    fn next_field(&mut self) {
        self.field = (self.field + 1) % MCP_ADD_FIELDS;
    }

    fn prev_field(&mut self) {
        self.field = (self.field + MCP_ADD_FIELDS - 1) % MCP_ADD_FIELDS;
    }

    fn current_editor(&mut self) -> Option<&mut crate::input::InputEditor> {
        match self.field {
            0 => Some(&mut self.name),
            2 if self.transport_idx == 0 => Some(&mut self.command),
            2 => Some(&mut self.url),
            3 => Some(&mut self.args),
            4 => Some(&mut self.env),
            _ => None,
        }
    }

    fn build_config(&self) -> Result<atman_runtime::mcp::McpServerConfig, String> {
        let name = self.name.buf().trim().to_string();
        if name.is_empty() {
            return Err("name is required".into());
        }
        let transport = self.transport();
        let command = self.command.buf().trim().to_string();
        let url = self.url.buf().trim().to_string();
        if transport == atman_runtime::mcp::TransportKind::Stdio && command.is_empty() {
            return Err("command is required for stdio transport".into());
        }
        if transport != atman_runtime::mcp::TransportKind::Stdio && url.is_empty() {
            return Err("url is required for http and sse transports".into());
        }
        let env = self
            .env
            .buf()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let (key, value) = line
                    .split_once('=')
                    .ok_or_else(|| format!("invalid environment entry {line:?}"))?;
                let key = key.trim();
                if key.is_empty() {
                    return Err("environment variable name is required".into());
                }
                Ok((key.to_string(), value.trim().to_string()))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut config = match &self.mode {
            McpEditorMode::Add => {
                atman_runtime::mcp::McpServerConfig::stdio("", "", Vec::new(), self.tier(), 30_000)
            }
            McpEditorMode::Edit { original, .. } => original.as_ref().clone(),
        };
        config.name = name;
        config.transport = transport;
        config.command = command;
        config.url = (transport != atman_runtime::mcp::TransportKind::Stdio).then_some(url);
        config.args = self
            .args
            .buf()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        config.env = env;
        config.tier = self.tier();
        Ok(config)
    }

    fn save(&mut self) -> Result<&'static str, String> {
        let config = self.build_config()?;
        let hub = atman_runtime::config_hub::ConfigHub::global().map_err(|e| e.to_string())?;
        match &self.mode {
            McpEditorMode::Add => hub.upsert_mcp(config).map_err(|e| e.to_string())?,
            McpEditorMode::Edit { original_name, .. } => hub
                .replace_mcp(original_name, config)
                .map_err(|e| e.to_string())?,
        }
        Ok(if matches!(self.mode, McpEditorMode::Add) {
            "MCP server added"
        } else {
            "MCP server updated"
        })
    }

    fn mode_label(&self) -> &'static str {
        if matches!(self.mode, McpEditorMode::Add) {
            "Add MCP Server"
        } else {
            "Edit MCP Server"
        }
    }
}

fn render_mcp_editor(f: &mut ratatui::Frame, area: Rect, form: &mut McpEditor) {
    let t = theme();
    use ratatui::widgets::{Block, Clear};

    f.render_widget(Clear, area);
    f.render_widget(
        Block::default().style(Style::default().bg(t.modal_bg.into())),
        area,
    );

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!(" {}", form.mode_label()),
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
    let value_width = area.width.saturating_sub(2) as usize;
    let mut cursor_offset = 0;
    let display_value = |editor: &crate::input::InputEditor, active: bool, env: bool| {
        let value = if env {
            editor.buf().replace('\n', " · ")
        } else {
            editor.buf().to_owned()
        };
        let prefix = if env {
            editor.buf()[..editor.cursor()].replace('\n', " · ")
        } else {
            editor.buf()[..editor.cursor()].to_owned()
        };
        let offset = if active {
            width::width(&prefix).saturating_sub(value_width.saturating_sub(1))
        } else {
            0
        };
        (
            width::trim_display_offset(&value, offset, value_width),
            offset,
        )
    };

    let active = form.field == 0;
    lines.push(Line::from(Span::styled(" Name:", label_style(active))));
    let (shown, offset) = display_value(&form.name, active, false);
    if active {
        cursor_offset = offset;
    }
    lines.push(Line::from(Span::styled(format!("  {shown}"), val_style)));

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

    let is_stdio = form.transport_idx == 0;
    let active = form.field == 2;
    let label = if is_stdio { " Command:" } else { " URL:" };
    lines.push(Line::from(Span::styled(label, label_style(active))));
    let val = if is_stdio { &form.command } else { &form.url };
    let (shown, offset) = display_value(val, active, false);
    if active {
        cursor_offset = offset;
    }
    lines.push(Line::from(Span::styled(format!("  {shown}"), val_style)));

    let active = form.field == 3;
    lines.push(Line::from(Span::styled(" Args:", label_style(active))));
    let (shown, offset) = display_value(&form.args, active, false);
    if active {
        cursor_offset = offset;
    }
    lines.push(Line::from(Span::styled(format!("  {shown}"), val_style)));

    let active = form.field == 4;
    lines.push(Line::from(Span::styled(" Env:", label_style(active))));
    let (shown, offset) = display_value(&form.env, active, true);
    if active {
        cursor_offset = offset;
    }
    lines.push(Line::from(Span::styled(format!("  {shown}"), val_style)));

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
        " ←→ edit/select · ↑↓/Tab fields · Enter/Esc save",
        Style::default().fg(t.subtle_fg.into()),
    )));

    let para = Paragraph::new(lines);
    f.render_widget(para, area);

    let field = form.field;
    let cursor_y = area.y.saturating_add(match field {
        0 => 3,
        2 => 7,
        3 => 9,
        4 => 11,
        _ => 0,
    });
    form.cursor_position = form.current_editor().and_then(|editor| {
        let prefix = editor.buf()[..editor.cursor()].replace('\n', " · ");
        let col = width::width(&prefix).saturating_sub(cursor_offset) as u16;
        let x = area.x.saturating_add(2).saturating_add(col);
        (x < area.right() && cursor_y < area.bottom()).then_some((x, cursor_y))
    });
}

impl crate::wm::modal::ModalOverlay for McpEditor {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        _t: &crate::theme::Theme,
    ) {
        render_mcp_editor(f, area, self);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<crate::wm::modal::ModalAction> {
        use crate::keys::KeyAction;
        self.error = None;
        match action {
            KeyAction::Submit | KeyAction::Escape => match self.save() {
                Ok(message) => {
                    self.open = false;
                    if let Some(tx) = tx {
                        let _ = tx.send(crate::TuiControl::McpReload);
                    }
                    app.push_toast(
                        message.to_string(),
                        crate::app::NoteLevel::Success,
                        std::time::Duration::from_secs(3),
                        crate::app::ToastPosition::TopRight,
                    );
                }
                Err(error) => self.error = Some(error),
            },
            KeyAction::Tab | KeyAction::HistoryDown => self.next_field(),
            KeyAction::BackTab | KeyAction::HistoryUp => self.prev_field(),
            KeyAction::CursorLeft if self.field == 1 => {
                self.transport_idx = self.transport_idx.saturating_sub(1);
            }
            KeyAction::CursorRight if self.field == 1 => {
                self.transport_idx = (self.transport_idx + 1).min(TRANSPORT_OPTIONS.len() - 1);
            }
            KeyAction::CursorLeft if self.field == 5 => {
                self.tier_idx = self.tier_idx.saturating_sub(1);
            }
            KeyAction::CursorRight if self.field == 5 => {
                self.tier_idx = (self.tier_idx + 1).min(TIER_OPTIONS.len() - 1);
            }
            KeyAction::Newline if self.field != 4 => {}
            _ => {
                if let Some(editor) = self.current_editor() {
                    editor.handle_key(action);
                }
            }
        }
        Some(crate::wm::modal::ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        self.cursor_position
    }

    fn title(&self) -> Line<'static> {
        Line::from(self.mode_label())
    }

    fn icon(&self) -> &str {
        "⚙"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }

    fn handle_paste(&mut self, text: &str) {
        let field = self.field;
        if let Some(editor) = self.current_editor() {
            if field == 4 {
                editor.paste_multiline(text);
            } else {
                editor.paste_single_line(text);
            }
        }
        self.error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::mcp::{
        McpPrompt, McpResource, McpServerState, McpServerStatus, McpToolInfo, TransportKind,
    };
    use ratatui::backend::TestBackend;
    use std::collections::HashMap;

    #[test]
    fn editor_env_horizontal_keys_edit_and_vertical_keys_change_fields() {
        let mut editor = McpEditor::default();
        editor.open_add();
        editor.field = 4;
        editor.env.replace_with("FIRST=one\nSECOND=two");
        let original_cursor = editor.env.cursor();
        let mut app = crate::app::AppState::new("session".into(), None);

        crate::wm::modal::ModalOverlay::handle_key(
            &mut editor,
            &crate::keys::KeyAction::CursorLeft,
            &mut app,
            None,
        );
        assert_eq!(editor.field, 4);
        assert!(editor.env.cursor() < original_cursor);

        crate::wm::modal::ModalOverlay::handle_key(
            &mut editor,
            &crate::keys::KeyAction::HistoryUp,
            &mut app,
            None,
        );
        assert_eq!(editor.field, 3);
        crate::wm::modal::ModalOverlay::handle_key(
            &mut editor,
            &crate::keys::KeyAction::HistoryDown,
            &mut app,
            None,
        );
        assert_eq!(editor.field, 4);

        editor.env.move_end();
        crate::wm::modal::ModalOverlay::handle_paste(&mut editor, "\r\nTHIRD=three");
        assert_eq!(editor.env.buf(), "FIRST=one\nSECOND=two\nTHIRD=three");
    }

    #[test]
    fn editor_long_values_keep_cursor_inside_the_visible_row() {
        let mut editor = McpEditor::default();
        editor.open_add();
        editor.field = 4;
        editor
            .env
            .replace_with("FIRST=one\nSECOND=a-very-long-environment-value");
        let mut terminal = ratatui::Terminal::new(TestBackend::new(30, 20)).unwrap();
        let area = Rect::new(2, 1, 20, 18);
        terminal
            .draw(|frame| render_mcp_editor(frame, area, &mut editor))
            .unwrap();
        assert_eq!(editor.cursor_position, Some((21, 12)));

        editor.field = 0;
        editor.name.replace_with("a-very-long-server-name");
        terminal
            .draw(|frame| render_mcp_editor(frame, area, &mut editor))
            .unwrap();
        assert_eq!(editor.cursor_position, Some((21, 4)));
    }

    #[test]
    fn editor_edit_preserves_fields_not_exposed_by_the_form() {
        let mut original = atman_runtime::mcp::McpServerConfig::http(
            "server",
            "https://old.example",
            Some("secret".into()),
            atman_runtime::tool::Tier::Three,
            45_000,
        );
        original.headers = vec![("X-Test".into(), "value".into())];
        original.disabled = true;
        let mut editor = McpEditor::default();
        editor.open_edit(original);
        editor.name.replace_with("renamed");
        editor.url.replace_with("https://new.example");

        let updated = editor.build_config().unwrap();
        assert_eq!(updated.name, "renamed");
        assert_eq!(updated.url.as_deref(), Some("https://new.example"));
        assert_eq!(updated.auth_token.as_deref(), Some("secret"));
        assert_eq!(updated.headers, [("X-Test".into(), "value".into())]);
        assert_eq!(updated.timeout_ms, 45_000);
        assert!(updated.disabled);
    }

    #[test]
    fn panel_exposes_add_and_edit_mouse_actions() {
        let servers = vec![connected_server("server", Vec::new())];
        let expanded = HashSet::new();
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let mut hitmap = WmHitmap::default();
        let mut projection = McpPanelProjection::default();
        let mut scroll = 0;
        let backend = TestBackend::new(90, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_panel(
                    frame,
                    frame.area(),
                    &mut scroll,
                    &servers,
                    &expanded,
                    0,
                    &None,
                    &mut hitmap,
                    &McpBrowserState {
                        tab: McpBrowserTab::Tools,
                        content_revision: 0,
                        resources: &resources,
                        prompts: &prompts,
                    },
                    crate::wm::WindowId(1),
                    &mut projection,
                );
            })
            .unwrap();

        assert_eq!(hitmap.mcp_action_rects.len(), 2);
        assert_eq!(
            hitmap.mcp_action_rects[0].1,
            crate::wm::component::McpPanelAction::Add
        );
        assert_eq!(
            hitmap.mcp_action_rects[1].1,
            crate::wm::component::McpPanelAction::Edit
        );
    }

    fn connected_server(name: &str, tools: Vec<McpToolInfo>) -> McpServerStatus {
        McpServerStatus {
            name: name.into(),
            transport: TransportKind::Stdio,
            state: McpServerState::Connected {
                tool_count: tools.len(),
                tools,
            },
        }
    }

    #[test]
    fn active_tab_rows_are_the_single_scroll_source() {
        let servers = vec![connected_server("server", Vec::new())];
        let expanded = HashSet::from(["server".to_string()]);
        let resources = HashMap::from([(
            "server".to_string(),
            vec![
                McpResource {
                    uri: "resource://one".into(),
                    name: "one".into(),
                    description: None,
                    mime_type: None,
                },
                McpResource {
                    uri: "resource://two".into(),
                    name: "two".into(),
                    description: None,
                    mime_type: None,
                },
            ],
        )]);
        let prompts = HashMap::from([(
            "server".to_string(),
            vec![
                McpPrompt {
                    name: "one".into(),
                    description: None,
                    arguments: Vec::new(),
                },
                McpPrompt {
                    name: "two".into(),
                    description: None,
                    arguments: Vec::new(),
                },
                McpPrompt {
                    name: "three".into(),
                    description: None,
                    arguments: Vec::new(),
                },
            ],
        )]);

        let mut projection = McpPanelProjection::default();
        for (tab, expected_rows) in [
            (McpBrowserTab::Tools, 8),
            (McpBrowserTab::Resources, 9),
            (McpBrowserTab::Prompts, 10),
        ] {
            projection.update(
                80,
                &servers,
                &expanded,
                &McpBrowserState {
                    tab,
                    content_revision: 1,
                    resources: &resources,
                    prompts: &prompts,
                },
            );
            assert_eq!(projection.total_rows(), expected_rows);
        }
    }

    #[test]
    fn stable_and_paint_only_frames_reuse_wrapped_projection() {
        let servers = vec![connected_server(
            "server",
            vec![
                McpToolInfo {
                    name: "first".into(),
                    description: Some("a description that wraps across several cells".into()),
                },
                McpToolInfo {
                    name: "second".into(),
                    description: Some("another description".into()),
                },
            ],
        )];
        let expanded = HashSet::from(["server".to_string()]);
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let browser = McpBrowserState {
            tab: McpBrowserTab::Tools,
            content_revision: 7,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();

        projection.update(52, &servers, &expanded, &browser);
        assert_eq!(projection.rebuild_count, 1);
        assert_eq!(projection.wrap_count, 2);
        projection.update(52, &servers, &expanded, &browser);
        assert_eq!(projection.rebuild_count, 1);
        assert_eq!(projection.wrap_count, 2);

        let backend = TestBackend::new(52, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut hitmap = WmHitmap::default();
        let mut scroll = 3;
        terminal
            .draw(|frame| {
                render_panel(
                    frame,
                    frame.area(),
                    &mut scroll,
                    &servers,
                    &expanded,
                    0,
                    &Some("server".into()),
                    &mut hitmap,
                    &browser,
                    crate::wm::WindowId(1),
                    &mut projection,
                );
            })
            .unwrap();
        assert_eq!(projection.rebuild_count, 1);
        assert_eq!(projection.wrap_count, 2);
    }

    #[test]
    fn viewport_clamp_and_hitmap_use_projected_rows() {
        let servers: Vec<McpServerStatus> = (0..12)
            .map(|index| McpServerStatus {
                name: format!("server-{index}"),
                transport: TransportKind::Http,
                state: McpServerState::Disconnected {
                    message: "offline".into(),
                },
            })
            .collect();
        let expanded = HashSet::new();
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let browser = McpBrowserState {
            tab: McpBrowserTab::Tools,
            content_revision: 1,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();
        let mut hitmap = WmHitmap::default();
        let mut scroll = u32::MAX;
        let backend = TestBackend::new(60, 6);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                render_panel(
                    frame,
                    frame.area(),
                    &mut scroll,
                    &servers,
                    &expanded,
                    11,
                    &None,
                    &mut hitmap,
                    &browser,
                    crate::wm::WindowId(2),
                    &mut projection,
                );
            })
            .unwrap();

        assert_eq!(projection.total_rows(), 16);
        assert_eq!(scroll, 11);
        assert_eq!(hitmap.mcp_row_rects.len(), 5);
        assert_eq!(hitmap.mcp_row_rects.first().unwrap().1, "server-7");
        assert_eq!(hitmap.mcp_row_rects.last().unwrap().1, "server-11");
    }

    #[test]
    fn scroll_clamp_preserves_offsets_above_u16() {
        assert_eq!(clamp_scroll(u32::MAX, 70_000, 20), 69_980);
        assert!(clamp_scroll(u32::MAX, 70_000, 20) > u16::MAX as u32);
    }

    #[test]
    #[ignore = "large release-mode MCP projection baseline"]
    fn baseline_large_mcp_projection_rebuild_and_stable_frames() {
        const STABLE_FRAMES: u32 = 1_000;
        const REBUILD_FRAMES: u32 = 16;

        let servers = (0..8)
            .map(|server| {
                connected_server(
                    &format!("server-{server}"),
                    (0..250)
                        .map(|tool| McpToolInfo {
                            name: format!("tool-{server}-{tool}"),
                            description: Some(format!(
                                "Inspect projected data for server {server}, tool {tool}, with enough text to exercise wrapping"
                            )),
                        })
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        let expanded = (0..8)
            .map(|server| format!("server-{server}"))
            .collect::<HashSet<_>>();
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let browser = McpBrowserState {
            tab: McpBrowserTab::Tools,
            content_revision: 1,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();

        let started = std::time::Instant::now();
        projection.update(100, &servers, &expanded, &browser);
        let cold = started.elapsed();
        assert!(projection.total_rows() > 2_000);

        let started = std::time::Instant::now();
        for _ in 0..STABLE_FRAMES {
            std::hint::black_box(&mut projection).update(
                100,
                std::hint::black_box(&servers),
                &expanded,
                &browser,
            );
        }
        let stable = started.elapsed();

        let started = std::time::Instant::now();
        for content_revision in 2..REBUILD_FRAMES + 2 {
            projection.update(
                100,
                std::hint::black_box(&servers),
                &expanded,
                &McpBrowserState {
                    content_revision: u64::from(content_revision),
                    ..browser
                },
            );
        }
        let rebuild = started.elapsed();

        assert_eq!(projection.rebuild_count, u64::from(REBUILD_FRAMES) + 1);
        eprintln!(
            "MCP projection baseline: servers=8 tools=2000 rows={} cold_ms={:.3} stable_us_per_frame={:.3} rebuild_ms_per_frame={:.3}",
            projection.total_rows(),
            cold.as_secs_f64() * 1_000.0,
            stable.as_secs_f64() * 1_000_000.0 / f64::from(STABLE_FRAMES),
            rebuild.as_secs_f64() * 1_000.0 / f64::from(REBUILD_FRAMES),
        );
    }
}
