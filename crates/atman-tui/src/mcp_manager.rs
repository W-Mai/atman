use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::settings_form::{Action, render_action, render_record};
use crate::theme::theme;
use crate::width;
use crate::wm::WmHitmap;
use crate::wm::component::McpPanelAction;
use std::collections::HashSet;

const SERVER_RECORD_HEIGHT: usize = 4;

pub struct McpBrowserState<'a> {
    pub tab: McpBrowserTab,
    pub content_revision: u64,
    pub hovered_action: Option<McpPanelAction>,
    pub remove_armed: Option<&'a str>,
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
    Server { index: usize, line: usize },
}

#[derive(Debug, Default)]
pub struct McpPanelProjection {
    key: Option<McpProjectionKey>,
    rows: Vec<McpProjectedRow>,
    selected_name: Option<String>,
    viewport: Option<(u16, u16)>,
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

    fn follow_selection(
        &mut self,
        selected: usize,
        servers: &[atman_runtime::mcp::McpServerStatus],
        area: Rect,
        scroll: &mut u32,
    ) {
        let name = servers.get(selected).map(|server| server.name.as_str());
        let viewport = (area.width, area.height);
        if self.selected_name.as_deref() == name && self.viewport == Some(viewport) {
            return;
        }
        self.selected_name = name.map(str::to_owned);
        self.viewport = Some(viewport);
        if let Some(start) = self.rows.iter().position(
            |row| matches!(row, McpProjectedRow::Server { index, line: 0 } if *index == selected),
        ) {
            let start = u32::try_from(start).unwrap_or(u32::MAX);
            let visible_height = (SERVER_RECORD_HEIGHT as u32).min(u32::from(area.height));
            if start < *scroll {
                *scroll = start;
            } else if start.saturating_add(visible_height)
                > scroll.saturating_add(area.height.into())
            {
                *scroll = start
                    .saturating_add(visible_height)
                    .saturating_sub(area.height.into());
            }
        }
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
    let width = area.width.saturating_sub(2).min(96);
    let inset_y = u16::from(area.height >= 22);
    let area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + inset_y,
        width,
        area.height.saturating_sub(inset_y * 2),
    );
    let armed = servers
        .get(selected)
        .is_some_and(|server| browser.remove_armed == Some(server.name.as_str()));
    let columns: u16 = if armed {
        2
    } else if width >= 90 {
        6
    } else if width >= 28 {
        3
    } else {
        2
    };
    let action_rows = if armed { 1 } else { 6 / columns };
    let action_height = (action_rows * 4 - 1).min(area.height.saturating_sub(1));
    let help_height = u16::from(area.height > action_height);
    let header_height = 2.min(area.height.saturating_sub(action_height + help_height));
    let gap = u16::from(area.height > action_height + help_height + header_height + 4);
    let content_area = Rect::new(
        area.x,
        area.y + header_height,
        area.width,
        area.height
            .saturating_sub(action_height + help_height + header_height + gap),
    );
    let actions_area = Rect::new(
        area.x,
        content_area.bottom() + gap,
        area.width,
        action_height,
    );
    let help_area = Rect::new(area.x, actions_area.bottom(), area.width, help_height);
    projection.update(content_area.width, servers, expanded, browser);
    *scroll = clamp_scroll(*scroll, projection.total_rows(), content_area.height);
    projection.follow_selection(selected, servers, content_area, scroll);
    *scroll = clamp_scroll(*scroll, projection.total_rows(), content_area.height);

    let (ok, total) = atman_runtime::mcp::mcp_counts(servers);
    let status = if area.width < 58 {
        format!(
            "{} · Tab · {}–{}/{}",
            browser.tab.label(),
            scroll.saturating_add(1),
            scroll
                .saturating_add(content_area.height.into())
                .min(projection.total_rows()),
            projection.total_rows(),
        )
    } else if projection.total_rows() > content_area.height.into() {
        format!(
            "{} · {}–{}/{} · PgUp/PgDn",
            browser.tab.label(),
            scroll.saturating_add(1),
            scroll
                .saturating_add(content_area.height.into())
                .min(projection.total_rows()),
            projection.total_rows(),
        )
    } else {
        format!("{} · Enter expand · Tab section", browser.tab.label())
    };
    let header = if armed {
        let name = servers
            .get(selected)
            .map(|server| server.name.as_str())
            .unwrap_or_default();
        vec![
            Line::from(Span::styled(
                width::truncate(&format!("  Remove {name}?"), area.width as usize),
                Style::default().fg(t.warn.into()),
            )),
            Line::from(Span::styled(
                "  Enter confirm · Esc cancel",
                Style::default().fg(t.subtle_fg.into()),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled(
                if area.width < 58 {
                    format!("  Servers {ok}/{total} · ↑↓ select")
                } else {
                    format!("  Servers · {ok}/{total} connected")
                },
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                width::truncate(&format!("  {status}"), area.width as usize),
                Style::default().fg(t.subtle_fg.into()),
            )),
        ]
    };
    f.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, header_height),
    );

    let start = usize::try_from(*scroll)
        .unwrap_or(usize::MAX)
        .min(projection.rows.len());
    let end = start
        .saturating_add(content_area.height as usize)
        .min(projection.rows.len());
    let mut cursor = start;
    while cursor < end {
        let y = content_area.y + (cursor - start) as u16;
        match &projection.rows[cursor] {
            McpProjectedRow::Static(line) => {
                f.render_widget(
                    Paragraph::new(line.clone()),
                    Rect::new(content_area.x, y, content_area.width, 1),
                );
                cursor += 1;
            }
            McpProjectedRow::Server { index, line } => {
                let index = *index;
                let offset = *line;
                let height = (SERVER_RECORD_HEIGHT - offset).min(end - cursor);
                let rect = Rect::new(content_area.x, y, content_area.width, height as u16);
                if let Some(server) = servers.get(index) {
                    let bg = render_record(
                        f,
                        rect,
                        index == selected,
                        hovered.as_deref() == Some(server.name.as_str()),
                        &t,
                    );
                    let lines =
                        server_lines(server, expanded.contains(&server.name), rect.width, &t);
                    let visible = if content_area.height == 1 {
                        vec![lines[1].clone()]
                    } else {
                        lines.into_iter().skip(offset).take(height).collect()
                    };
                    // The record owns its selection gutter; text never paints over it.
                    f.render_widget(
                        Paragraph::new(visible).style(Style::default().bg(bg)),
                        Rect::new(
                            rect.x + 1.min(rect.width),
                            rect.y,
                            rect.width.saturating_sub(1),
                            rect.height,
                        ),
                    );
                    hitmap_out
                        .mcp_row_rects
                        .push((window_id, server.name.clone(), rect));
                }
                cursor += height;
            }
        }
    }

    let has_selection = servers.get(selected).is_some();
    let actions: &[(McpPanelAction, &str, &str)] = if armed {
        &[
            (McpPanelAction::Remove, "Confirm", "Enter"),
            (McpPanelAction::CancelRemove, "Cancel", "Esc"),
        ]
    } else {
        &[
            (McpPanelAction::Add, "Add", "a"),
            (McpPanelAction::Edit, "Edit", "e"),
            (McpPanelAction::Test, "Test", "t"),
            (McpPanelAction::Toggle, "Toggle", "d"),
            (McpPanelAction::Remove, "Remove", "r"),
            (McpPanelAction::NextTab, "Section", "Tab"),
        ]
    };
    for (index, &(action, label, shortcut)) in actions.iter().enumerate() {
        let col = index as u16 % columns;
        let row = index as u16 / columns;
        let left = actions_area.x + actions_area.width * col / columns;
        let right = actions_area.x + actions_area.width * (col + 1) / columns;
        let rect = Rect::new(
            left,
            actions_area.y + row * 4,
            right.saturating_sub(left + u16::from(col + 1 < columns)),
            3.min(actions_area.height.saturating_sub(row * 4)),
        );
        if rect.height == 0 {
            continue;
        }
        let disabled = action != McpPanelAction::Add && !has_selection;
        let (label, shortcut) = match action {
            McpPanelAction::Remove if armed => {
                ("Confirm", if rect.width >= 16 { "Enter" } else { "↵" })
            }
            McpPanelAction::Toggle if rect.width < 11 => ("Mode", "d"),
            McpPanelAction::Remove if rect.width < 11 => ("Del", "r"),
            McpPanelAction::NextTab if rect.width < 14 => ("View", "⇥"),
            _ => (label, shortcut),
        };
        render_action(
            f,
            rect,
            &Action {
                label,
                shortcut,
                hovered: browser.hovered_action == Some(action),
                disabled,
                ..Default::default()
            },
            &t,
        );
        if !disabled {
            hitmap_out.mcp_action_rects.push((window_id, action, rect));
        }
    }
    let help = if armed {
        "  Enter confirm · Esc cancel"
    } else if area.width < 58 {
        "  Enter expand · Esc close"
    } else {
        "  ↑↓ select · Enter expand · Tab section · Wheel / PgUp / PgDn scroll · Esc close"
    };
    f.render_widget(
        Paragraph::new(width::truncate(help, help_area.width as usize))
            .style(Style::default().fg(t.subtle_fg.into())),
        help_area,
    );
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
    use atman_runtime::mcp::McpServerState;
    let t = theme();
    let mut rows = Vec::new();
    if servers.is_empty() {
        push_wrapped(
            &mut rows,
            "No MCP servers configured.\n\nUse Add to connect a server.",
            width,
            t.subtle_fg.into(),
        );
        return rows;
    }
    for (index, server) in servers.iter().enumerate() {
        rows.extend((0..SERVER_RECORD_HEIGHT).map(|line| McpProjectedRow::Server { index, line }));
        if !expanded.contains(&server.name) {
            continue;
        }
        match &server.state {
            McpServerState::Connected { tools, .. } => {
                push_wrapped(&mut rows, browser.tab.label(), width, t.subtle_fg.into());
                rows.push(McpProjectedRow::Static(Line::default()));
                match browser.tab {
                    McpBrowserTab::Tools => {
                        if tools.is_empty() {
                            push_wrapped(&mut rows, "No tools exposed.", width, t.subtle_fg.into());
                        }
                        for tool in tools {
                            push_named_description(
                                &mut rows,
                                &tool.name,
                                tool.description.as_deref(),
                                width,
                                &t,
                                wrap_count,
                            );
                        }
                    }
                    McpBrowserTab::Resources => match browser.resources.get(&server.name) {
                        None => {
                            push_wrapped(&mut rows, "Loading resources…", width, t.subtle_fg.into())
                        }
                        Some(resources) if resources.is_empty() => {
                            push_wrapped(&mut rows, "No resources.", width, t.subtle_fg.into())
                        }
                        Some(resources) => {
                            for resource in resources {
                                push_named_description(
                                    &mut rows,
                                    &resource.name,
                                    resource.description.as_deref(),
                                    width,
                                    &t,
                                    wrap_count,
                                );
                            }
                        }
                    },
                    McpBrowserTab::Prompts => match browser.prompts.get(&server.name) {
                        None => {
                            push_wrapped(&mut rows, "Loading prompts…", width, t.subtle_fg.into())
                        }
                        Some(prompts) if prompts.is_empty() => {
                            push_wrapped(&mut rows, "No prompts.", width, t.subtle_fg.into())
                        }
                        Some(prompts) => {
                            for prompt in prompts {
                                push_named_description(
                                    &mut rows,
                                    &prompt.name,
                                    prompt.description.as_deref(),
                                    width,
                                    &t,
                                    wrap_count,
                                );
                            }
                        }
                    },
                }
            }
            McpServerState::Error { message }
            | McpServerState::Timeout { message }
            | McpServerState::Disconnected { message } => {
                let (_, color, _) = server_display(server, &t);
                push_wrapped(&mut rows, message, width, color);
                *wrap_count = wrap_count.wrapping_add(1);
            }
            McpServerState::Disabled => push_wrapped(
                &mut rows,
                "Disabled. Toggle to enable this server.",
                width,
                t.subtle_fg.into(),
            ),
            McpServerState::Pending => {
                push_wrapped(&mut rows, "Waiting to connect.", width, t.subtle_fg.into())
            }
            McpServerState::Connecting => push_wrapped(
                &mut rows,
                "Connection in progress.",
                width,
                t.subtle_fg.into(),
            ),
        }
        rows.push(McpProjectedRow::Static(Line::default()));
    }
    rows
}

fn push_wrapped(
    rows: &mut Vec<McpProjectedRow>,
    text: &str,
    width: u16,
    color: ratatui::style::Color,
) {
    let inset = 4.min(width.saturating_sub(1));
    let available = width.saturating_sub(inset + 2).max(1) as usize;
    rows.extend(width::word_wrap(text, available).into_iter().map(|line| {
        McpProjectedRow::Static(Line::from(Span::styled(
            format!("{}{line}", " ".repeat(inset as usize)),
            Style::default().fg(color),
        )))
    }));
}

fn push_named_description(
    rows: &mut Vec<McpProjectedRow>,
    name: &str,
    description: Option<&str>,
    width: u16,
    t: &crate::theme::Theme,
    wrap_count: &mut u64,
) {
    push_wrapped(rows, name, width, t.tinted_fg.into());
    if let Some(description) = description.filter(|value| !value.is_empty()) {
        push_wrapped(rows, description, width, t.subtle_fg.into());
    }
    rows.push(McpProjectedRow::Static(Line::default()));
    *wrap_count = wrap_count.wrapping_add(1);
}

fn server_lines(
    server: &atman_runtime::mcp::McpServerStatus,
    expanded: bool,
    width: u16,
    t: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let transport = match server.transport {
        atman_runtime::mcp::TransportKind::Stdio => "stdio",
        atman_runtime::mcp::TransportKind::Http => "http",
        atman_runtime::mcp::TransportKind::Sse => "sse",
    };
    let (glyph, color, detail) = server_display(server, t);
    let name_width = width.saturating_sub(9) as usize;
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(format!(" {glyph} "), Style::default().fg(color)),
            Span::styled(
                width::pad_right(&width::truncate(&server.name, name_width), name_width),
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if expanded { "  ▾" } else { "  ▸" },
                Style::default().fg(t.subtle_fg.into()),
            ),
        ]),
        Line::from(Span::styled(
            width::truncate(
                &format!("   {transport} · {detail}"),
                width.saturating_sub(3) as usize,
            ),
            Style::default().fg(t.subtle_fg.into()),
        )),
        Line::default(),
    ]
}

fn server_display(
    s: &atman_runtime::mcp::McpServerStatus,
    t: &crate::theme::Theme,
) -> (&'static str, ratatui::style::Color, String) {
    use atman_runtime::mcp::McpServerState;
    match &s.state {
        McpServerState::Connected { tool_count, .. } => {
            ("●", t.success.into(), format!("{tool_count} tools"))
        }
        McpServerState::Connecting => ("◐", t.warn.into(), "connecting…".into()),
        McpServerState::Disabled => ("◌", t.subtle_fg.into(), "disabled".into()),
        McpServerState::Error { .. } => ("✗", t.error.into(), "error · expand for details".into()),
        McpServerState::Disconnected { .. } => (
            "○",
            t.subtle_fg.into(),
            "disconnected · expand for details".into(),
        ),
        McpServerState::Timeout { .. } => {
            ("⏱", t.warn.into(), "timed out · expand for details".into())
        }
        McpServerState::Pending => ("·", t.subtle_fg.into(), "pending".into()),
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
    field_hits: Vec<(usize, crate::settings_form::FieldHit)>,
    hovered: Option<usize>,
    cancel_rect: Option<Rect>,
    save_rect: Option<Rect>,
    info_rect: Option<Rect>,
    info_scroll: usize,
    info_max_scroll: usize,
}

const TRANSPORT_OPTIONS: [&str; 3] = ["stdio", "http", "sse"];
const TIER_OPTIONS: [u8; 3] = [1, 2, 3];
const MCP_CANCEL: usize = MCP_ADD_FIELDS;
const MCP_SAVE: usize = MCP_ADD_FIELDS + 1;

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
            field_hits: Vec::new(),
            hovered: None,
            cancel_rect: None,
            save_rect: None,
            info_rect: None,
            info_scroll: 0,
            info_max_scroll: 0,
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

    fn clear_error(&mut self) {
        self.error = None;
        self.info_scroll = 0;
    }

    pub fn handle_mouse(
        &mut self,
        event: &crossterm::event::MouseEvent,
        app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        use crate::keys::KeyAction;
        use crate::wm::modal::ModalOverlay;
        use crossterm::event::{MouseButton, MouseEventKind};

        let point = ratatui::layout::Position::new(event.column, event.row);
        let hit = |rect: Option<Rect>| rect.is_some_and(|rect| rect.contains(point));
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = if hit(self.cancel_rect) {
                    Some(MCP_CANCEL)
                } else if hit(self.save_rect) {
                    Some(MCP_SAVE)
                } else {
                    self.field_hits
                        .iter()
                        .find(|(_, field)| field.record.contains(point))
                        .map(|(index, _)| *index)
                };
            }
            MouseEventKind::Down(MouseButton::Left) if hit(self.cancel_rect) => {
                self.handle_key(&KeyAction::Escape, app, tx);
            }
            MouseEventKind::Down(MouseButton::Left) if hit(self.save_rect) => {
                self.handle_key(&KeyAction::Submit, app, tx);
            }
            MouseEventKind::ScrollUp if hit(self.info_rect) => {
                self.info_scroll = self.info_scroll.saturating_sub(1);
            }
            MouseEventKind::ScrollDown if hit(self.info_rect) => {
                self.info_scroll = self.info_scroll.saturating_add(1).min(self.info_max_scroll);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self
                    .field_hits
                    .iter()
                    .any(|(_, field)| field.record.contains(point)) =>
            {
                self.handle_key(
                    &if matches!(event.kind, MouseEventKind::ScrollUp) {
                        KeyAction::HistoryUp
                    } else {
                        KeyAction::HistoryDown
                    },
                    app,
                    tx,
                );
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some((index, field_hit)) = self
                    .field_hits
                    .iter()
                    .find(|(_, field)| field.record.contains(point))
                    .copied()
                else {
                    return;
                };
                self.field = index;
                if let Some((left, right)) = field_hit.arrows
                    && (left.contains(point) || right.contains(point))
                {
                    self.handle_key(
                        &if left.contains(point) {
                            KeyAction::CursorLeft
                        } else {
                            KeyAction::CursorRight
                        },
                        app,
                        tx,
                    );
                } else if field_hit.value.contains(point) {
                    if matches!(index, 1 | 5) {
                        self.handle_key(
                            &if event.column < field_hit.value.x + field_hit.value.width / 2 {
                                KeyAction::CursorLeft
                            } else {
                                KeyAction::CursorRight
                            },
                            app,
                            tx,
                        );
                    } else if let Some(editor) = self.current_editor() {
                        let column =
                            usize::from(event.column - field_hit.value.x) + field_hit.offset;
                        let mut display_column = 0;
                        let mut byte = 0;
                        for (grapheme, columns) in width::graphemes(editor.buf()) {
                            let columns = if grapheme == "\n" { 3 } else { columns };
                            if display_column + columns > column {
                                break;
                            }
                            display_column += columns;
                            byte += grapheme.len();
                        }
                        editor.set_cursor(byte);
                    }
                }
            }
            _ => {}
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
    use crate::settings_form::{Action, Field, FormLayout, render_action, render_field};

    let t = theme();
    let surface = crate::wm::shell::surface_bg(&t);
    let layout = FormLayout::new(area, MCP_ADD_FIELDS, form.field);
    form.field_hits.clear();
    form.cursor_position = None;
    for &(index, rect) in &layout.fields {
        let (label, value, placeholder, cursor) = match index {
            1 => (
                "Transport",
                TRANSPORT_OPTIONS[form.transport_idx].to_owned(),
                "",
                None,
            ),
            5 => (
                "Approval tier",
                TIER_OPTIONS[form.tier_idx].to_string(),
                "",
                None,
            ),
            _ => {
                let (label, editor, placeholder) = match index {
                    0 => ("Name", &form.name, "server name"),
                    2 if form.transport_idx == 0 => ("Command", &form.command, "executable path"),
                    2 => ("URL", &form.url, "https://server.example/mcp"),
                    3 => ("Arguments", &form.args, "optional arguments"),
                    4 => ("Environment", &form.env, "KEY=value"),
                    _ => continue,
                };
                let value = editor.buf().replace('\n', " · ");
                let cursor = width::width(&editor.buf()[..editor.cursor()].replace('\n', " · "));
                (label, value, placeholder, Some(cursor))
            }
        };
        let hit = render_field(
            f,
            rect,
            &Field {
                label,
                value: &value,
                placeholder,
                focused: index == form.field,
                hovered: form.hovered == Some(index),
                cursor,
                selector: matches!(index, 1 | 5),
                locked: false,
            },
            &t,
        );
        if index == form.field && hit.value.width > 0 && hit.value.height > 0 {
            form.cursor_position = cursor.map(|cursor| {
                (
                    hit.value.x
                        + cursor
                            .saturating_sub(hit.offset)
                            .min(usize::from(hit.value.width - 1)) as u16,
                    hit.value.y,
                )
            });
        }
        form.field_hits.push((index, hit));
    }
    if let (Some((first, _)), Some((last, _))) = (layout.fields.first(), layout.fields.last())
        && layout.fields.len() < MCP_ADD_FIELDS
    {
        f.render_widget(
            Paragraph::new(format!(
                " Fields {}–{} / {} · Tab or scroll",
                first + 1,
                last + 1,
                MCP_ADD_FIELDS
            ))
            .style(Style::default().fg(t.meta_fg.into()).bg(surface)),
            layout.scroll,
        );
    }

    let info = form.error.as_deref().unwrap_or(if form.field == 4 {
        "One KEY=value per line · Shift+Enter: newline"
    } else {
        "Changes take effect after saving."
    });
    let lines = width::word_wrap(info, usize::from(layout.info.width).max(1));
    form.info_rect = (layout.info.height > 0).then_some(layout.info);
    form.info_max_scroll = if layout.info.height > 0 {
        lines.len().saturating_sub(usize::from(layout.info.height))
    } else {
        0
    };
    form.info_scroll = form.info_scroll.min(form.info_max_scroll);
    f.render_widget(
        Paragraph::new(
            lines
                .iter()
                .skip(form.info_scroll)
                .take(usize::from(layout.info.height))
                .cloned()
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .style(
            Style::default()
                .fg(if form.error.is_some() {
                    t.error.into()
                } else {
                    t.meta_fg.into()
                })
                .bg(surface),
        ),
        layout.info,
    );
    form.cancel_rect = (layout.test.width > 0 && layout.test.height > 0).then_some(layout.test);
    form.save_rect = (layout.save.width > 0 && layout.save.height > 0).then_some(layout.save);
    render_action(
        f,
        layout.test,
        &Action {
            label: "Cancel",
            shortcut: "Esc",
            hovered: form.hovered == Some(MCP_CANCEL),
            ..Action::default()
        },
        &t,
    );
    render_action(
        f,
        layout.save,
        &Action {
            label: "Save",
            shortcut: "Enter",
            hovered: form.hovered == Some(MCP_SAVE),
            ..Action::default()
        },
        &t,
    );
    let help = if form.info_max_scroll > 0 {
        format!(
            "{}–{}/{} · PgUp/PgDn: details",
            form.info_scroll + 1,
            (form.info_scroll + usize::from(layout.info.height)).min(lines.len()),
            lines.len()
        )
    } else if layout.help.width < 56 {
        "Tab field · ←→ select".to_owned()
    } else {
        "Tab / Shift+Tab: field · ← / →: select".to_owned()
    };
    f.render_widget(
        Paragraph::new(help).style(Style::default().fg(t.meta_fg.into()).bg(surface)),
        layout.help,
    );
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
        match action {
            KeyAction::Escape => self.open = false,
            KeyAction::Submit => match self.save() {
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
                Err(error) => {
                    self.error = Some(error);
                    self.info_scroll = 0;
                }
            },
            KeyAction::Tab | KeyAction::HistoryDown => self.next_field(),
            KeyAction::BackTab | KeyAction::HistoryUp => self.prev_field(),
            KeyAction::PageUp => {
                self.info_scroll = self.info_scroll.saturating_sub(
                    self.info_rect
                        .map_or(1, |rect| usize::from(rect.height).max(1)),
                );
            }
            KeyAction::PageDown => {
                self.info_scroll = self
                    .info_scroll
                    .saturating_add(
                        self.info_rect
                            .map_or(1, |rect| usize::from(rect.height).max(1)),
                    )
                    .min(self.info_max_scroll);
            }
            KeyAction::CursorLeft if self.field == 1 => {
                self.transport_idx = self.transport_idx.saturating_sub(1);
                self.clear_error();
            }
            KeyAction::CursorRight if self.field == 1 => {
                self.transport_idx = (self.transport_idx + 1).min(TRANSPORT_OPTIONS.len() - 1);
                self.clear_error();
            }
            KeyAction::CursorLeft if self.field == 5 => {
                self.tier_idx = self.tier_idx.saturating_sub(1);
                self.clear_error();
            }
            KeyAction::CursorRight if self.field == 5 => {
                self.tier_idx = (self.tier_idx + 1).min(TIER_OPTIONS.len() - 1);
                self.clear_error();
            }
            KeyAction::Newline if self.field != 4 => {}
            _ => {
                if let Some(editor) = self.current_editor()
                    && editor.handle_key(action)
                {
                    self.clear_error();
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
        self.clear_error();
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
        let value = editor
            .field_hits
            .iter()
            .find(|(field, _)| *field == 4)
            .unwrap()
            .1
            .value;
        assert_eq!(editor.cursor_position, Some((value.right() - 1, value.y)));

        editor.field = 0;
        editor.name.replace_with("a-very-long-server-name");
        terminal
            .draw(|frame| render_mcp_editor(frame, area, &mut editor))
            .unwrap();
        let value = editor
            .field_hits
            .iter()
            .find(|(field, _)| *field == 0)
            .unwrap()
            .1
            .value;
        assert_eq!(editor.cursor_position, Some((value.right() - 1, value.y)));
    }

    #[test]
    fn editor_mouse_and_cursor_share_rendered_field_geometry() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mouse = |kind, x, y| MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        };
        for width in [40, 96] {
            let mut editor = McpEditor::default();
            editor.open_add();
            editor.field = 4;
            editor.env.replace_with("A=界\nB=🙂value");
            let mut app = crate::app::AppState::new("session".into(), None);
            let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 32)).unwrap();
            terminal
                .draw(|frame| render_mcp_editor(frame, frame.area(), &mut editor))
                .unwrap();
            let hit = editor
                .field_hits
                .iter()
                .find(|(field, _)| *field == 4)
                .unwrap()
                .1;
            editor.handle_mouse(
                &mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.value.x + 7,
                    hit.value.y,
                ),
                &mut app,
                None,
            );
            assert_eq!(editor.env.cursor(), "A=界\n".len());
            let cursor = editor.env.cursor();
            editor.handle_mouse(
                &mouse(
                    MouseEventKind::Down(MouseButton::Left),
                    hit.record.x + 1,
                    hit.record.y,
                ),
                &mut app,
                None,
            );
            assert_eq!(editor.env.cursor(), cursor);
            editor.handle_mouse(
                &mouse(
                    MouseEventKind::Moved,
                    hit.record.right() - 1,
                    hit.record.bottom() - 1,
                ),
                &mut app,
                None,
            );
            assert_eq!(editor.hovered, Some(4));
            editor.field = 1;
            terminal
                .draw(|frame| render_mcp_editor(frame, frame.area(), &mut editor))
                .unwrap();
            let hit = editor
                .field_hits
                .iter()
                .find(|(field, _)| *field == 1)
                .unwrap()
                .1;
            let (_, right) = hit.arrows.unwrap();
            editor.handle_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), right.x, right.y),
                &mut app,
                None,
            );
            assert_eq!(editor.transport_idx, 1);
            assert_eq!(editor.field, 1);
            assert!(
                !editor
                    .cancel_rect
                    .unwrap()
                    .intersects(editor.save_rect.unwrap())
            );
        }
    }

    #[test]
    fn editor_cancel_does_not_save_or_reload_and_errors_remain_scrollable() {
        use crate::keys::KeyAction;
        use crate::wm::modal::ModalOverlay;
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut editor = McpEditor::default();
        editor.open_add();
        let mut app = crate::app::AppState::new("session".into(), None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        editor.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        assert!(editor.open);
        assert_eq!(editor.error.as_deref(), Some("name is required"));
        editor.handle_key(&KeyAction::Escape, &mut app, Some(&tx));
        assert!(!editor.open);
        assert!(rx.try_recv().is_err());

        editor.open_add();
        editor.error = Some(
            "Required configuration contains a very long invalid environment entry. ".repeat(10),
        );
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 18)).unwrap();
        terminal
            .draw(|frame| render_mcp_editor(frame, frame.area(), &mut editor))
            .unwrap();
        assert!(editor.info_max_scroll > 0);
        editor.handle_key(&KeyAction::PageDown, &mut app, None);
        assert!(editor.info_scroll > 0);
        assert!(editor.error.is_some());
        let info = editor.info_rect.unwrap();
        let before = editor.info_scroll;
        editor.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: info.x,
                row: info.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            None,
        );
        assert_eq!(editor.info_scroll, before - 1);
        let cancel = editor.cancel_rect.unwrap();
        editor.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: cancel.x,
                row: cancel.y,
                modifiers: KeyModifiers::NONE,
            },
            &mut app,
            Some(&tx),
        );
        assert!(!editor.open);
        assert!(rx.try_recv().is_err());
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
    fn panel_records_and_actions_share_padded_hit_geometry() {
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
                        hovered_action: None,
                        remove_armed: None,
                        resources: &resources,
                        prompts: &prompts,
                    },
                    crate::wm::WindowId(1),
                    &mut projection,
                );
            })
            .unwrap();

        assert_eq!(hitmap.mcp_action_rects.len(), 6);
        for (index, (_, _, rect)) in hitmap.mcp_action_rects.iter().enumerate() {
            assert_eq!(rect.height, 3);
            assert!(
                hitmap.mcp_action_rects[index + 1..]
                    .iter()
                    .all(|(_, _, other)| !rect.intersects(*other))
            );
        }
        let record = hitmap.mcp_row_rects[0].2;
        assert_eq!(record.height, SERVER_RECORD_HEIGHT as u16);
        for row in record.y..record.bottom() {
            assert_eq!(terminal.backend().buffer()[(record.x, row)].symbol(), "▌");
            assert_eq!(
                terminal.backend().buffer()[(record.right() - 1, row)].bg,
                terminal.backend().buffer()[(record.x + 1, record.y)].bg
            );
        }
        hitmap = WmHitmap::default();
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
                        hovered_action: None,
                        remove_armed: Some("server"),
                        resources: &resources,
                        prompts: &prompts,
                    },
                    crate::wm::WindowId(1),
                    &mut projection,
                );
            })
            .unwrap();
        assert_eq!(
            hitmap
                .mcp_action_rects
                .iter()
                .map(|(_, action, _)| *action)
                .collect::<Vec<_>>(),
            [McpPanelAction::Remove, McpPanelAction::CancelRemove]
        );
        assert!(
            hitmap
                .mcp_action_rects
                .iter()
                .all(|(_, _, rect)| rect.height == 3)
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
    fn narrow_panel_keeps_a_complete_record_and_readable_action_shortcuts() {
        let servers = vec![connected_server("server", Vec::new())];
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let mut projection = McpPanelProjection::default();
        let mut scroll = 0;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(40, 18)).unwrap();
        for armed in [None, Some("server")] {
            let mut hitmap = WmHitmap::default();
            terminal
                .draw(|frame| {
                    render_panel(
                        frame,
                        Rect::new(4, 2, 32, 14),
                        &mut scroll,
                        &servers,
                        &HashSet::new(),
                        0,
                        &None,
                        &mut hitmap,
                        &McpBrowserState {
                            tab: McpBrowserTab::Tools,
                            content_revision: 0,
                            hovered_action: None,
                            remove_armed: armed,
                            resources: &resources,
                            prompts: &prompts,
                        },
                        crate::wm::WindowId(1),
                        &mut projection,
                    );
                })
                .unwrap();
            assert_eq!(
                hitmap.mcp_row_rects[0].2.height,
                SERVER_RECORD_HEIGHT as u16
            );
            let labels = hitmap
                .mcp_action_rects
                .iter()
                .map(|(_, _, rect)| {
                    assert_eq!(rect.height, 3);
                    (rect.x..rect.right())
                        .map(|x| terminal.backend().buffer()[(x, rect.y + 1)].symbol())
                        .collect::<String>()
                        .trim()
                        .to_owned()
                })
                .collect::<Vec<_>>();
            if armed.is_some() {
                assert_eq!(labels, ["Confirm  ↵", "Cancel  Esc"]);
                let help = (0..40)
                    .map(|x| terminal.backend().buffer()[(x, 15)].symbol())
                    .collect::<String>();
                assert!(help.contains("Enter confirm · Esc cancel"));
                assert!(!help.contains("expand"));
            } else {
                assert_eq!(
                    labels,
                    [
                        "Add  a",
                        "Edit  e",
                        "Test  t",
                        "Mode  d",
                        "Del  r",
                        "View  ⇥"
                    ]
                );
                assert_eq!(
                    hitmap.mcp_action_rects[0].2.bottom() + 1,
                    hitmap.mcp_action_rects[3].2.y
                );
            }
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
            (McpBrowserTab::Resources, 11),
            (McpBrowserTab::Prompts, 13),
        ] {
            projection.update(
                80,
                &servers,
                &expanded,
                &McpBrowserState {
                    tab,
                    content_revision: 1,
                    hovered_action: None,
                    remove_armed: None,
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
            hovered_action: None,
            remove_armed: None,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();

        projection.update(50, &servers, &expanded, &browser);
        assert_eq!(projection.rebuild_count, 1);
        assert_eq!(projection.wrap_count, 2);
        projection.update(50, &servers, &expanded, &browser);
        assert_eq!(projection.rebuild_count, 1);
        assert_eq!(projection.wrap_count, 2);

        let backend = TestBackend::new(52, 18);
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
            hovered_action: None,
            remove_armed: None,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();
        let mut hitmap = WmHitmap::default();
        let mut scroll = u32::MAX;
        let backend = TestBackend::new(60, 18);
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

        assert_eq!(projection.total_rows(), 48);
        assert_eq!(scroll, 41);
        assert_eq!(hitmap.mcp_row_rects.len(), 2);
        assert_eq!(hitmap.mcp_row_rects.first().unwrap().1, "server-10");
        assert_eq!(hitmap.mcp_row_rects.last().unwrap().1, "server-11");
    }

    #[test]
    fn selection_follows_navigation_and_resize_without_overriding_manual_scroll() {
        let servers = (0..12)
            .map(|index| connected_server(&format!("server-{index}"), Vec::new()))
            .collect::<Vec<_>>();
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let mut projection = McpPanelProjection::default();
        projection.update(
            40,
            &servers,
            &HashSet::new(),
            &McpBrowserState {
                tab: McpBrowserTab::Tools,
                content_revision: 0,
                hovered_action: None,
                remove_armed: None,
                resources: &resources,
                prompts: &prompts,
            },
        );
        let area = Rect::new(0, 0, 40, 8);
        let mut scroll = 0;
        projection.follow_selection(9, &servers, area, &mut scroll);
        assert_eq!(scroll, 32);
        scroll = 12;
        projection.follow_selection(9, &servers, area, &mut scroll);
        assert_eq!(scroll, 12);
        projection.follow_selection(10, &servers, area, &mut scroll);
        assert_eq!(scroll, 36);
        projection.follow_selection(10, &servers, Rect::new(0, 0, 40, 4), &mut scroll);
        assert_eq!(scroll, 40);
    }

    #[test]
    fn expanded_diagnostics_and_descriptions_wrap_without_losing_content() {
        let servers = vec![McpServerStatus {
            name: "failed".into(),
            transport: TransportKind::Http,
            state: McpServerState::Error { message: "Cannot connect to configured endpoint. 检查服务器配置和网络连接。 Final detail remains readable.".into() },
        }];
        let resources = HashMap::new();
        let prompts = HashMap::new();
        let browser = McpBrowserState {
            tab: McpBrowserTab::Tools,
            content_revision: 0,
            hovered_action: None,
            remove_armed: None,
            resources: &resources,
            prompts: &prompts,
        };
        let mut projection = McpPanelProjection::default();
        projection.update(34, &servers, &HashSet::from(["failed".into()]), &browser);
        let text = projection
            .rows
            .iter()
            .filter_map(|row| match row {
                McpProjectedRow::Static(line) => {
                    assert!(line.width() <= 34);
                    Some(line.to_string())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let McpServerState::Error { message } = &servers[0].state else {
            unreachable!();
        };
        assert_eq!(
            text.split_whitespace().collect::<String>(),
            message.split_whitespace().collect::<String>()
        );
        assert!(projection.total_rows() > SERVER_RECORD_HEIGHT as u32);
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
            hovered_action: None,
            remove_armed: None,
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
