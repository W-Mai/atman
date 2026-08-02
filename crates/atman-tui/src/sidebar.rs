use atman_runtime::ContextSnapshot;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

pub const GOAL_MAX_LINES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidebarPopupKind {
    Plan(usize),
    Todo(usize),
}

pub struct SidebarInputs<'a> {
    pub goal: Option<&'a str>,
    pub context: &'a ContextSnapshot,
    pub attach_count: usize,
    pub session_id: &'a str,
    pub session_dir: &'a str,
    pub project_root: Option<&'a str>,
    pub app_version: &'a str,
    pub latest_release: Option<&'a str>,
    pub streaming: bool,
    pub todos: &'a [atman_runtime::memory::todo::Todo],
    pub plans: &'a [atman_runtime::memory::plan::Plan],
    pub goal_scroll: u16,
    pub plans_scroll: u16,
    pub todos_scroll: u16,
    pub goal_collapsed: bool,
    pub plan_collapsed: bool,
    pub todo_collapsed: bool,
    pub context_collapsed: bool,
    pub meta_collapsed: bool,
    pub mcp_collapsed: bool,
    pub sidebar_collapsed: bool,
    pub upper_collapsed: bool,
    pub lower_collapsed: bool,
    pub animation_frame: u32,
    pub hovered_row: Option<&'a str>,
    pub hovered_hamburger: bool,
    pub hovered_lower: bool,
    pub hovered_more: bool,
    pub on_goal_scroll: &'a dyn Fn(u16),
    pub on_plans_scroll: &'a dyn Fn(u16),
    pub on_todos_scroll: &'a dyn Fn(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarMode {
    #[default]
    Open,
    Closed,
}

impl SidebarMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Open => Self::Closed,
            Self::Closed => Self::Open,
        }
    }

    // The old `resolve(wide_enough)` API let width auto-hide the sidebar;
    // now the mode is the single source of truth and the layout code
    // separately guards against tiny terminals.
    pub fn resolve(self, _wide_enough: bool) -> bool {
        matches!(self, Self::Open)
    }
}

pub struct SidebarRenderResult {
    pub goal_rect: Option<Rect>,
    pub plan_rect: Option<Rect>,
    pub todo_rect: Option<Rect>,
    pub goal_hdr_rect: Option<Rect>,
    pub plan_hdr_rect: Option<Rect>,
    pub todo_hdr_rect: Option<Rect>,
    pub ctx_hdr_rect: Option<Rect>,
    pub meta_hdr_rect: Option<Rect>,
    pub mcp_hdr_rect: Option<Rect>,
    pub collapse_btn_rect: Option<Rect>,
    pub expand_btn_rect: Option<Rect>,
    pub upper_title_rect: Option<Rect>,
    pub lower_title_rect: Option<Rect>,
    pub mcp_more_rect: Option<Rect>,
    pub strip_rects: std::collections::HashMap<String, Rect>,
}

impl SidebarRenderResult {
    fn empty() -> Self {
        Self {
            goal_rect: None,
            plan_rect: None,
            todo_rect: None,
            goal_hdr_rect: None,
            plan_hdr_rect: None,
            todo_hdr_rect: None,
            ctx_hdr_rect: None,
            meta_hdr_rect: None,
            mcp_hdr_rect: None,
            collapse_btn_rect: None,
            expand_btn_rect: None,
            upper_title_rect: None,
            lower_title_rect: None,
            mcp_more_rect: None,
            strip_rects: std::collections::HashMap::new(),
        }
    }
}

pub fn render(
    f: &mut ratatui::Frame,
    area: Rect,
    inputs: SidebarInputs<'_>,
) -> SidebarRenderResult {
    let t = crate::theme::theme();

    // When entire sidebar is collapsed (user toggle or narrow terminal lock),
    // force both panels into collapsed stripe mode — uses the same two-panel
    // layout with centered ≡, not the old single-strip.
    let upper_collapsed = inputs.upper_collapsed || inputs.sidebar_collapsed;
    let lower_collapsed = inputs.lower_collapsed || inputs.sidebar_collapsed;

    // Don't blanket-clear the entire sidebar area — only clear where panels
    // actually render. This avoids black gaps next to collapsed stripes.
    let panel_bg = t.modal_bg.into();
    let content_bg_hover: ratatui::style::Color = t.modal_bg.lerp(t.highlight_bg, 0.3);

    let gap: u16 = 1;

    // ── Dynamic height calculation ──

    // Calculate collapsed strip heights based on content
    let upper_strip_h: u16 = {
        let plan = inputs.plans.iter().max_by_key(|p| p.updated_at);
        let p_steps = plan.map(|p| p.steps.len() as u16).unwrap_or(0);
        let has_plan = plan.is_some();
        let has_todos = !inputs.todos.is_empty();
        let t_items = inputs.todos.len() as u16;
        let dots = (if has_plan { 1 } else { 0 })
            + p_steps
            + (if has_plan && has_todos { 1 } else { 0 })
            + (if has_todos { 1 } else { 0 })
            + t_items;
        2 + dots + 1 // top pad + title + dots + bottom pad
    };
    let lower_strip_h: u16 = {
        let mcp_count = inputs.context.mcp_servers.len() as u16;
        let dots = 1 + (if mcp_count > 0 { 1 } else { 0 }) + mcp_count;
        2 + dots + 1 // top pad + title + dots + bottom pad
    };

    // Calculate content needs
    let goal_lines = if inputs.goal_collapsed { 0 } else { 6 };
    let plan_steps = inputs
        .plans
        .iter()
        .max_by_key(|p| p.updated_at)
        .map(|p| p.steps.len() as u16)
        .unwrap_or(0);
    let plan_lines = if inputs.plan_collapsed {
        0
    } else {
        2 + plan_steps
    };
    let todo_count = inputs.todos.len() as u16;
    let todo_lines = if inputs.todo_collapsed {
        0
    } else {
        2 + todo_count * 2
    };
    // upper content = sections + 2 gaps + 4 padding (top+title+blockpad+contentpad) + 1 bottom
    let upper_need: u16 = 4 + 1 + goal_lines + 1 + plan_lines + 1 + todo_lines + 1;

    let ctx_lines = if inputs.context_collapsed { 0 } else { 7 };
    let mcp_count = inputs.context.mcp_servers.len() as u16;
    let mcp_lines = if inputs.mcp_collapsed {
        0
    } else {
        1 + mcp_count + 1 // header + servers + more row
    };
    let lower_need: u16 = 4 + 1 + ctx_lines + 1 + mcp_lines + 1;

    // Meta only visible when both panels expanded
    let meta_height: u16 = if !upper_collapsed && !lower_collapsed {
        4
    } else {
        0
    };
    let available = area.height.saturating_sub(meta_height + gap * 2);
    let strip_w = crate::layout::SIDEBAR_STRIP_WIDTH;

    // Height allocation:
    // - Both expanded: lower takes content height, upper takes remaining
    // - Any collapsed: each panel takes its own content height (no "remaining")
    //   but clamped to available space so it doesn't push other panel off screen
    let (upper_expanded_h, lower_expanded_h) = if !upper_collapsed && !lower_collapsed {
        let lower_h = lower_need.min(available.saturating_sub(4));
        let upper_h = available.saturating_sub(lower_h + gap).max(4);
        (upper_h, lower_h)
    } else if !upper_collapsed {
        // Only upper expanded — clamp to available minus collapsed stripe
        let max_h = available.saturating_sub(lower_strip_h + gap);
        (upper_need.min(max_h).max(4), 0)
    } else if !lower_collapsed {
        // Only lower expanded — clamp to available minus collapsed stripe
        let max_h = available.saturating_sub(upper_strip_h + gap);
        (0, lower_need.min(max_h).max(4))
    } else {
        (0, 0)
    };

    // Build layout items: (is_upper, is_collapsed, height)
    // Expanded first (in original order), then collapsed (in original order)
    let mut layout: Vec<(bool, bool, u16)> = Vec::new();
    if !upper_collapsed {
        layout.push((true, false, upper_expanded_h));
    }
    if !lower_collapsed {
        layout.push((false, false, lower_expanded_h));
    }
    if upper_collapsed {
        layout.push((true, true, upper_strip_h));
    }
    if lower_collapsed {
        layout.push((false, true, lower_strip_h));
    }

    // Compute rects for each layout item
    let mut y = area.y;
    let mut upper_area: Option<Rect> = None;
    let mut lower_area: Option<Rect> = None;
    for &(is_upper, is_collapsed, h) in &layout {
        let (r_x, r_w) = if is_collapsed {
            // Narrow stripe, right-aligned
            let x = area.x + area.width.saturating_sub(strip_w);
            (x, strip_w)
        } else {
            // Full width
            (area.x, area.width)
        };
        let r = Rect {
            x: r_x,
            y,
            width: r_w,
            height: h,
        };
        if is_upper {
            upper_area = Some(r);
        } else {
            lower_area = Some(r);
        }
        y += h + gap;
    }

    let meta_area = Rect {
        x: area.x,
        y: y + gap,
        width: area.width,
        height: meta_height,
    };
    let mut result = SidebarRenderResult::empty();

    // ── Upper panel ──
    if let Some(upper_area) = upper_area {
        crate::sanitize_widget_edges(f, upper_area);
        f.render_widget(ratatui::widgets::Clear, upper_area);
        if upper_collapsed {
            render_collapsed_strip(
                f,
                upper_area,
                panel_bg,
                &t,
                &inputs,
                true,
                inputs.hovered_hamburger,
            );
            result.upper_title_rect = Some(Rect {
                x: upper_area.x,
                y: upper_area.y + 1,
                width: upper_area.width,
                height: 1,
            });
        } else {
            render_panel_frame(
                f,
                upper_area,
                panel_bg,
                "Goal / Plan / Todos",
                &t,
                inputs.hovered_hamburger,
            );
            result.upper_title_rect = Some(Rect {
                x: upper_area.x,
                y: upper_area.y + 1,
                width: upper_area.width,
                height: 1,
            });
            result.collapse_btn_rect = Some(Rect {
                x: upper_area.x,
                y: upper_area.y + 1,
                width: upper_area.width,
                height: 1,
            });
            if upper_area.height > 4 {
                let inner = Rect {
                    x: upper_area.x + 2,
                    y: upper_area.y + 3,
                    width: upper_area.width.saturating_sub(4),
                    height: upper_area.height.saturating_sub(4),
                };
                if inner.height > 0 && inner.width > 0 {
                    result = render_upper_content(
                        f,
                        inner,
                        panel_bg,
                        content_bg_hover,
                        inner.width as usize,
                        &inputs,
                        &t,
                        result,
                    );
                }
            }
        }
        crate::floating_panels::render_shadow(f, upper_area, &t);
    }

    // ── Lower panel ──
    if let Some(lower_area) = lower_area {
        crate::sanitize_widget_edges(f, lower_area);
        f.render_widget(ratatui::widgets::Clear, lower_area);
        if lower_collapsed {
            render_collapsed_strip(
                f,
                lower_area,
                panel_bg,
                &t,
                &inputs,
                false,
                inputs.hovered_lower,
            );
            result.lower_title_rect = Some(Rect {
                x: lower_area.x,
                y: lower_area.y + 1,
                width: lower_area.width,
                height: 1,
            });
        } else {
            render_panel_frame(
                f,
                lower_area,
                panel_bg,
                "Context / MCP",
                &t,
                inputs.hovered_lower,
            );
            result.lower_title_rect = Some(Rect {
                x: lower_area.x,
                y: lower_area.y + 1,
                width: lower_area.width,
                height: 1,
            });
            if lower_area.height > 4 {
                let inner = Rect {
                    x: lower_area.x + 2,
                    y: lower_area.y + 3,
                    width: lower_area.width.saturating_sub(4),
                    height: lower_area.height.saturating_sub(4),
                };
                if inner.height > 0 && inner.width > 0 {
                    result = render_lower_content(
                        f,
                        inner,
                        panel_bg,
                        content_bg_hover,
                        inner.width as usize,
                        &inputs,
                        &t,
                        result,
                    );
                }
            }
        }
        crate::floating_panels::render_shadow(f, lower_area, &t);
    }

    // ── Meta (only when both panels expanded) ──
    if !upper_collapsed && !lower_collapsed {
        render_meta_plain(
            f,
            meta_area,
            inputs.project_root,
            inputs.app_version,
            inputs.latest_release,
            &t,
        );
    }

    result
}

/// Render a collapsed panel as a thin vertical strip with centered ≡
/// and status dots (same logic as original sidebar strip mode).
fn render_collapsed_strip(
    f: &mut ratatui::Frame,
    area: Rect,
    panel_bg: ratatui::style::Color,
    t: &crate::theme::Theme,
    inputs: &SidebarInputs<'_>,
    is_upper: bool,
    hovered: bool,
) {
    let fg = if hovered {
        t.heading.lerp(ratatui::style::Color::White, 0.3)
    } else {
        t.heading.into()
    };
    let accent = Style::default().fg(t.accent.into());
    let tinted = Style::default().fg(t.tinted_fg.into());
    let success = Style::default().fg(t.success.into());

    // top padding row
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(panel_bg)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
    // title row with centered ≡
    let glyph = "≡";
    let glyph_w = crate::width::width(glyph) as u16;
    let left_pad = area.width.saturating_sub(glyph_w) / 2;
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(panel_bg)),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        },
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(Line::from(vec![
            Span::styled(" ".repeat(left_pad as usize), Style::default().bg(panel_bg)),
            Span::styled(glyph, Style::default().fg(fg).add_modifier(Modifier::BOLD)),
            Span::styled(
                " ".repeat(area.width.saturating_sub(left_pad + glyph_w) as usize),
                Style::default().bg(panel_bg),
            ),
        ])),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: 1,
        },
    );

    // ── Status dots ──
    let dot_start_y = area.y + 2;
    let dot_area = Rect {
        x: area.x,
        y: dot_start_y,
        width: area.width,
        height: area.height.saturating_sub(3), // top pad + title + bottom pad
    };
    if dot_area.height == 0 {
        // Still render bottom padding
        return;
    }

    if is_upper {
        let plan = inputs.plans.iter().max_by_key(|p| p.updated_at);
        let has_plan = plan.is_some();
        let has_todos = !inputs.todos.is_empty();
        let p_head: u16 = if has_plan { 1 } else { 0 };
        let p_steps: u16 = plan.map(|p| p.steps.len() as u16).unwrap_or(0);
        let p_gap: u16 = if has_plan && has_todos { 1 } else { 0 };
        let t_head: u16 = if has_todos { 1 } else { 0 };
        let t_items: u16 = inputs.todos.len() as u16;
        let needed = p_head + p_steps + p_gap + t_head + t_items;
        if needed == 0 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(p_head),
                Constraint::Length(p_steps),
                Constraint::Length(p_gap),
                Constraint::Length(t_head),
                Constraint::Length(t_items),
            ])
            .split(dot_area);

        let mut si = 0;
        if has_plan {
            f.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(Span::styled("P", accent)))
                    .alignment(Alignment::Center),
                sections[si],
            );
        }
        si += 1;
        if has_plan {
            if let Some(p) = plan {
                for (r, step) in p.steps.iter().enumerate() {
                    let row_rect = Rect {
                        y: sections[si].y + r as u16,
                        height: 1,
                        ..sections[si]
                    };
                    let (sym, style) = if step.done {
                        ("✓", success)
                    } else {
                        ("○", tinted)
                    };
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(Line::from(Span::styled(sym, style)))
                            .alignment(Alignment::Center),
                        row_rect,
                    );
                }
            }
        }
        si += 1;
        si += 1; // p_gap
        if has_todos {
            f.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(Span::styled("T", accent)))
                    .alignment(Alignment::Center),
                sections[si],
            );
        }
        si += 1;
        if has_todos {
            let sec = sections[si];
            for (r, todo) in inputs.todos.iter().enumerate() {
                use atman_runtime::memory::todo::TodoStatus;
                let row_rect = Rect {
                    y: sec.y + r as u16,
                    height: 1,
                    ..sec
                };
                let (sym, style) = match todo.status {
                    TodoStatus::Done => ("✓", success),
                    TodoStatus::Cancelled => ("✕", Style::default().fg(t.subtle_fg.into())),
                    _ => ("○", tinted),
                };
                f.render_widget(
                    ratatui::widgets::Paragraph::new(Line::from(Span::styled(sym, style)))
                        .alignment(Alignment::Center),
                    row_rect,
                );
            }
        }
    } else {
        // Lower panel: context dot + MCP server status dots
        let ctx = &inputs.context;
        let ctx_dot_h: u16 = 1;
        let mcp_count = ctx.mcp_servers.len() as u16;
        let mcp_gap: u16 = if mcp_count > 0 { 1 } else { 0 };
        let needed = ctx_dot_h + mcp_gap + mcp_count;
        if needed == 0 {
            return;
        }

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(ctx_dot_h),
                Constraint::Length(mcp_gap),
                Constraint::Length(mcp_count),
            ])
            .split(dot_area);

        let ctx_dot_span = if ctx.window_budget > 0 && ctx.window_tokens > ctx.window_budget {
            Span::styled("●", Style::default().fg(t.error.into()))
        } else if ctx.window_budget > 0 && ctx.window_tokens as f64 > ctx.window_budget as f64 * 0.8
        {
            Span::styled("●", Style::default().fg(t.warn.into()))
        } else {
            Span::styled("○", Style::default().fg(t.success.into()))
        };
        f.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(ctx_dot_span)).alignment(Alignment::Center),
            sections[0],
        );

        if mcp_count > 0 {
            let sec = sections[2];
            for (r, s) in ctx.mcp_servers.iter().enumerate() {
                let row_rect = Rect {
                    y: sec.y + r as u16,
                    height: 1,
                    ..sec
                };
                let (g, color) = match &s.state {
                    atman_runtime::mcp::McpServerState::Connected { .. } => ("●", t.success),
                    atman_runtime::mcp::McpServerState::Connecting => ("◐", t.warn),
                    atman_runtime::mcp::McpServerState::Error { .. } => ("✗", t.error),
                    atman_runtime::mcp::McpServerState::Disabled => ("◌", t.subtle_fg),
                    atman_runtime::mcp::McpServerState::Disconnected { .. } => ("○", t.subtle_fg),
                    atman_runtime::mcp::McpServerState::Timeout { .. } => ("⏱", t.warn),
                    atman_runtime::mcp::McpServerState::Pending => ("·", t.subtle_fg),
                };
                f.render_widget(
                    ratatui::widgets::Paragraph::new(Line::from(Span::styled(
                        g,
                        Style::default().fg(color.into()),
                    )))
                    .alignment(Alignment::Center),
                    row_rect,
                );
            }
        }
    }
}

/// Render panel frame exactly like task_panel:
/// Row 0: modal_bg top padding
/// Row 1: modal_bg title row (≡ Title)
/// Row 2+: modal_bg (block inner, content drawn on top by caller)
fn render_panel_frame(
    f: &mut ratatui::Frame,
    area: Rect,
    panel_bg: ratatui::style::Color,
    title: &str,
    t: &crate::theme::Theme,
    hovered_hamburger: bool,
) {
    let hamburger_fg: ratatui::style::Color = if hovered_hamburger {
        t.heading.lerp(ratatui::style::Color::White, 0.3)
    } else {
        t.heading.into()
    };
    let title_fg: ratatui::style::Color = if hovered_hamburger {
        t.heading.lerp(ratatui::style::Color::White, 0.15)
    } else {
        t.heading.into()
    };
    // top padding row
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(panel_bg)),
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        },
    );
    // title block covers rest of area, with top padding for the title
    f.render_widget(
        ratatui::widgets::Block::default()
            .style(Style::default().bg(panel_bg))
            .title(Line::from(vec![
                Span::styled(
                    "  ≡",
                    Style::default()
                        .fg(hamburger_fg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    title,
                    Style::default().fg(title_fg).add_modifier(Modifier::BOLD),
                ),
            ]))
            .padding(ratatui::widgets::Padding {
                left: 0,
                right: 0,
                top: 1,
                bottom: 0,
            }),
        Rect {
            x: area.x,
            y: area.y + 1,
            width: area.width,
            height: area.height.saturating_sub(1),
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn render_upper_content(
    f: &mut ratatui::Frame,
    area: Rect,
    content_bg: ratatui::style::Color,
    content_bg_hover: ratatui::style::Color,
    content_w: usize,
    inputs: &SidebarInputs<'_>,
    t: &crate::theme::Theme,
    mut result: SidebarRenderResult,
) -> SidebarRenderResult {
    let accent = Style::default()
        .fg(t.accent.into())
        .add_modifier(Modifier::BOLD);

    let mut lines: Vec<Line<'_>> = Vec::new();

    // ── Goal section ──
    let goal_glyph = if inputs.goal_collapsed { "›" } else { "⌄" };
    let goal_hdr = format!("{goal_glyph} Goal");
    result.goal_hdr_rect = Some(Rect {
        x: area.x,
        y: area.y + lines.len() as u16,
        width: area.width,
        height: 1,
    });
    lines.push(Line::from(Span::styled(goal_hdr, accent)));

    if !inputs.goal_collapsed {
        let goal_text = inputs.goal.unwrap_or("(none)");
        let goal_w = content_w.saturating_sub(4); // "  " indent
        let wrapped = crate::width::word_wrap(goal_text, goal_w);
        let max_lines = 6usize;
        for (i, l) in wrapped.iter().enumerate() {
            if i >= max_lines {
                lines.push(Line::from(Span::styled(
                    "  …",
                    Style::default().fg(t.subtle_fg.into()),
                )));
                break;
            }
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Style::default().fg(t.tinted_fg.into()),
            )));
        }
        result.goal_rect = Some(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: lines.len() as u16,
        });
    } else {
        result.goal_rect = None;
    }

    lines.push(Line::from(""));

    // ── Plan section ──
    let plan_glyph = if inputs.plan_collapsed { "›" } else { "⌄" };
    let plan_hdr = {
        let latest = inputs.plans.iter().max_by_key(|p| p.updated_at);
        match latest {
            Some(p) => {
                let (done, total) = p.progress();
                format!("{plan_glyph} Plan ({done}/{total})")
            }
            None => format!("{plan_glyph} Plan"),
        }
    };
    result.plan_hdr_rect = Some(Rect {
        x: area.x,
        y: area.y + lines.len() as u16,
        width: area.width,
        height: 1,
    });
    lines.push(Line::from(Span::styled(plan_hdr, accent)));

    if !inputs.plan_collapsed {
        let latest = inputs.plans.iter().max_by_key(|p| p.updated_at);
        match latest {
            None => {
                lines.push(Line::from(Span::styled(
                    "  (no active plan)",
                    Style::default().fg(t.subtle_fg.into()),
                )));
            }
            Some(p) => {
                let title_max = content_w.saturating_sub(2);
                lines.push(Line::from(Span::styled(
                    crate::width::truncate(&p.title, title_max),
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .add_modifier(Modifier::BOLD),
                )));
                let total = p.steps.len();
                let step_max = content_w.saturating_sub(8);

                // Build all plan body lines into a temp vector
                let mut plan_body: Vec<(usize, Line<'_>)> = Vec::new();
                for (i, step) in p.steps.iter().enumerate() {
                    let row_key = format!("plan:{i}");
                    let is_hovered = inputs.hovered_row == Some(row_key.as_str());
                    let (glyph, glyph_style, text_style) = if step.done {
                        (
                            "✓",
                            Style::default().fg(t.success.into()),
                            Style::default().fg(t.meta_fg.into()),
                        )
                    } else {
                        (
                            "○",
                            Style::default().fg(t.subtle_fg.into()),
                            Style::default().fg(t.tinted_fg.into()),
                        )
                    };
                    let indent = if total <= 1 { " " } else { "│" };
                    let display = crate::width::truncate(&step.text, step_max);
                    let bg = if is_hovered {
                        content_bg_hover
                    } else {
                        content_bg
                    };
                    plan_body.push((
                        i,
                        Line::from(vec![
                            Span::styled(
                                format!(" {indent} "),
                                Style::default().fg(t.subtle_fg.into()).bg(bg),
                            ),
                            Span::styled(format!("{glyph} "), glyph_style.bg(bg)),
                            Span::styled(
                                format!("{:>2}. ", i + 1),
                                Style::default().fg(t.meta_fg.into()).bg(bg),
                            ),
                            Span::styled(display, text_style.bg(bg)),
                        ]),
                    ));
                }

                // Calculate visible height: remaining space minus todo minimum (header + 1 item)
                let lines_so_far = lines.len() as u16;
                let todo_min = if inputs.todo_collapsed { 1 } else { 2 };
                let avail = area
                    .height
                    .saturating_sub(lines_so_far + 1 + todo_min)
                    .max(1) as usize;
                let body_len = plan_body.len();
                let scroll = (inputs.plans_scroll as usize).min(body_len.saturating_sub(avail));
                let visible_count = body_len.min(avail);
                let can_scroll_up = scroll > 0;
                let can_scroll_down = scroll + visible_count < body_len;

                for (vi, (step_idx, mut line)) in plan_body
                    .into_iter()
                    .skip(scroll)
                    .take(visible_count)
                    .enumerate()
                {
                    let row_key = format!("plan:{step_idx}");
                    let row_y = area.y + lines.len() as u16;
                    result.strip_rects.insert(
                        row_key,
                        Rect {
                            x: area.x,
                            y: row_y,
                            width: area.width,
                            height: 1,
                        },
                    );
                    // Fade first visible if can scroll up, last visible if can scroll down
                    if (vi == 0 && can_scroll_up) || (vi == visible_count - 1 && can_scroll_down) {
                        line.spans
                            .iter_mut()
                            .for_each(|s| s.style = s.style.add_modifier(Modifier::DIM));
                    }
                    lines.push(line);
                }
            }
        }
        result.plan_rect = Some(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: lines.len() as u16,
        });
    } else {
        result.plan_rect = None;
    }

    lines.push(Line::from(""));

    // ── Todo section ──
    let todo_glyph = if inputs.todo_collapsed { "›" } else { "⌄" };
    let todo_hdr = {
        use atman_runtime::memory::todo::TodoStatus;
        let done = inputs
            .todos
            .iter()
            .filter(|tt| matches!(tt.status, TodoStatus::Done))
            .count();
        let total = inputs.todos.len();
        format!("{todo_glyph} Todos ({done}/{total})")
    };
    result.todo_hdr_rect = Some(Rect {
        x: area.x,
        y: area.y + lines.len() as u16,
        width: area.width,
        height: 1,
    });
    lines.push(Line::from(Span::styled(todo_hdr, accent)));

    if !inputs.todo_collapsed {
        if inputs.todos.is_empty() {
            lines.push(Line::from(Span::styled(
                "  (no todos)",
                Style::default().fg(t.subtle_fg.into()),
            )));
        } else {
            let todo_max = content_w.saturating_sub(4);

            // Build all todo body lines into a temp vector
            let mut todo_body: Vec<(usize, Line<'_>)> = Vec::new();
            for (i, todo) in inputs.todos.iter().enumerate() {
                use atman_runtime::memory::todo::TodoStatus;
                let (glyph, glyph_style) = match todo.status {
                    TodoStatus::Pending => ("○", Style::default().fg(t.subtle_fg.into())),
                    TodoStatus::InProgress => (
                        "⚡",
                        Style::default()
                            .fg(t.warn.into())
                            .add_modifier(Modifier::BOLD),
                    ),
                    TodoStatus::Done => ("✓", Style::default().fg(t.success.into())),
                    TodoStatus::Cancelled => (
                        "✗",
                        Style::default()
                            .fg(t.subtle_fg.into())
                            .add_modifier(Modifier::CROSSED_OUT),
                    ),
                };
                let display = crate::width::truncate(&todo.why, todo_max);
                let is_hovered = inputs.hovered_row == Some(format!("todo:{i}").as_str());
                let bg = if is_hovered {
                    content_bg_hover
                } else {
                    content_bg
                };
                todo_body.push((
                    i,
                    Line::from(vec![
                        Span::styled(format!("  {glyph} "), glyph_style.bg(bg)),
                        Span::styled(display, Style::default().fg(t.tinted_fg.into()).bg(bg)),
                    ]),
                ));
            }

            // Calculate visible height: remaining space
            let lines_so_far = lines.len() as u16;
            let avail = area.height.saturating_sub(lines_so_far).max(1) as usize;
            let body_len = todo_body.len();
            let scroll = (inputs.todos_scroll as usize).min(body_len.saturating_sub(avail));
            let visible_count = body_len.min(avail);
            let can_scroll_up = scroll > 0;
            let can_scroll_down = scroll + visible_count < body_len;

            for (vi, (todo_i, mut line)) in todo_body
                .into_iter()
                .skip(scroll)
                .take(visible_count)
                .enumerate()
            {
                let row_key = format!("todo:{todo_i}");
                let row_y = area.y + lines.len() as u16;
                result.strip_rects.insert(
                    row_key,
                    Rect {
                        x: area.x,
                        y: row_y,
                        width: area.width,
                        height: 1,
                    },
                );
                if (vi == 0 && can_scroll_up) || (vi == visible_count - 1 && can_scroll_down) {
                    line.spans
                        .iter_mut()
                        .for_each(|s| s.style = s.style.add_modifier(Modifier::DIM));
                }
                lines.push(line);
            }
        }
        result.todo_rect = Some(Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: lines.len() as u16,
        });
    } else {
        result.todo_rect = None;
    }

    // Render all lines, clipped to area
    let visible: Vec<Line<'_>> = lines.into_iter().take(area.height as usize).collect();
    f.render_widget(ratatui::widgets::Paragraph::new(visible), area);
    result
}

#[allow(clippy::too_many_arguments)]
fn render_lower_content(
    f: &mut ratatui::Frame,
    area: Rect,
    content_bg: ratatui::style::Color,
    content_bg_hover: ratatui::style::Color,
    content_w: usize,
    inputs: &SidebarInputs<'_>,
    t: &crate::theme::Theme,
    mut result: SidebarRenderResult,
) -> SidebarRenderResult {
    let accent = Style::default()
        .fg(t.accent.into())
        .add_modifier(Modifier::BOLD);
    let plain = Style::default();

    let mut lines: Vec<Line<'_>> = Vec::new();

    // ── Context section ──
    let ctx_glyph = if inputs.context_collapsed {
        "›"
    } else {
        "⌄"
    };
    let ctx_hdr = format!("{ctx_glyph} Context");
    result.ctx_hdr_rect = Some(Rect {
        x: area.x,
        y: area.y + lines.len() as u16,
        width: area.width,
        height: 1,
    });
    lines.push(Line::from(Span::styled(ctx_hdr, accent)));

    if !inputs.context_collapsed {
        let ctx = inputs.context;
        let model = if ctx.model.is_empty() {
            "—".to_string()
        } else {
            ctx.model.clone()
        };
        use atman_runtime::humanize::format_count;
        let window = if ctx.window_budget == 0 && ctx.window_tokens == 0 {
            "—".to_string()
        } else if ctx.window_budget == 0 {
            format_count(ctx.window_tokens)
        } else {
            format!(
                "{} / {} ({}%)",
                format_count(ctx.window_tokens),
                format_count(ctx.window_budget),
                (ctx.window_tokens as f64 / ctx.window_budget as f64 * 100.0) as u64
            )
        };
        let kv_w = content_w.saturating_sub(2);
        lines.push(kv_line_bg("model", &model, plain, kv_w, content_bg));
        lines.push(kv_line_bg("window", &window, plain, kv_w, content_bg));
        lines.push(kv_line_bg(
            "total",
            &format!(
                "↑{} · ↓{}",
                format_count(ctx.tokens_in),
                format_count(ctx.tokens_out)
            ),
            plain,
            kv_w,
            content_bg,
        ));
        let cache_val = if ctx.cache_read > 0 || ctx.cache_write > 0 {
            let hit_rate = if ctx.tokens_in > 0 {
                (ctx.cache_read as f64 / ctx.tokens_in as f64 * 100.0) as u64
            } else {
                0
            };
            format!(
                "read {} · write {} · {}%",
                format_count(ctx.cache_read),
                format_count(ctx.cache_write),
                hit_rate,
            )
        } else {
            "—".to_string()
        };
        lines.push(kv_line_bg("cache", &cache_val, plain, kv_w, content_bg));
        lines.push(kv_line_bg(
            "last",
            &format!(
                "ttft {} · {:.0} tok/s",
                if ctx.last_ttft_ms > 0 {
                    format!("{}ms", ctx.last_ttft_ms)
                } else {
                    "—".to_string()
                },
                ctx.last_tokens_per_sec
            ),
            plain,
            kv_w,
            content_bg,
        ));
        lines.push(kv_line_bg(
            "attach",
            &format!("{}", inputs.attach_count),
            plain,
            kv_w,
            content_bg,
        ));
        lines.push(kv_line_bg(
            "memory",
            &format!("recent×{}", ctx.memory_recent_count),
            plain,
            kv_w,
            content_bg,
        ));
    }

    lines.push(Line::from(""));

    // ── MCP section ──
    let mcp_glyph = if inputs.mcp_collapsed { "›" } else { "⌄" };
    let (ok, total) = atman_runtime::mcp::mcp_counts(&inputs.context.mcp_servers);
    let mcp_hdr = format!("{mcp_glyph} MCP ({ok}/{total})");
    result.mcp_hdr_rect = Some(Rect {
        x: area.x,
        y: area.y + lines.len() as u16,
        width: area.width,
        height: 1,
    });
    lines.push(Line::from(Span::styled(mcp_hdr, accent)));

    if !inputs.mcp_collapsed {
        for s in &inputs.context.mcp_servers {
            let transport = match s.transport {
                atman_runtime::mcp::TransportKind::Stdio => "stdio",
                atman_runtime::mcp::TransportKind::Http => "http",
                atman_runtime::mcp::TransportKind::Sse => "sse",
            };
            let (glyph, color, detail) = match &s.state {
                atman_runtime::mcp::McpServerState::Disabled => ("◌", t.subtle_fg, String::new()),
                atman_runtime::mcp::McpServerState::Pending => ("○", t.subtle_fg, String::new()),
                atman_runtime::mcp::McpServerState::Connecting => {
                    ("◐", t.warn, "connecting".into())
                }
                atman_runtime::mcp::McpServerState::Connected { tool_count, .. } => {
                    ("●", t.success, format!("{tool_count} tools"))
                }
                atman_runtime::mcp::McpServerState::Error { message } => {
                    ("✗", t.error, crate::width::truncate(message, 16))
                }
                atman_runtime::mcp::McpServerState::Disconnected { message } => {
                    ("⏏", t.error, crate::width::truncate(message, 16))
                }
                atman_runtime::mcp::McpServerState::Timeout { message } => {
                    ("⏱", t.warn, crate::width::truncate(message, 16))
                }
            };
            let name_max = content_w.saturating_sub(16);
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {glyph} "),
                    Style::default().fg(color.into()).bg(content_bg),
                ),
                Span::styled(
                    crate::width::pad_right(&crate::width::truncate(&s.name, name_max), name_max),
                    Style::default().fg(t.tinted_fg.into()).bg(content_bg),
                ),
                Span::styled(
                    format!(" {transport:<5} {detail}"),
                    Style::default().fg(t.subtle_fg.into()).bg(content_bg),
                ),
            ]));
        }
    }

    // ── "More" row (··· centered) — opens MCP settings panel ──
    if !inputs.mcp_collapsed {
        let more_y = area.y + lines.len() as u16;
        let more_bg = if inputs.hovered_more {
            content_bg_hover
        } else {
            content_bg
        };
        let more_fg = if inputs.hovered_more {
            t.heading.lerp(ratatui::style::Color::White, 0.3)
        } else {
            t.subtle_fg.into()
        };
        let w = area.width as usize;
        let dots = "···";
        let dots_w = crate::width::width(dots);
        let left_pad = w.saturating_sub(dots_w) / 2;
        let right_pad = w.saturating_sub(left_pad + dots_w);
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(left_pad), Style::default().bg(more_bg)),
            Span::styled(dots, Style::default().fg(more_fg).bg(more_bg)),
            Span::styled(" ".repeat(right_pad), Style::default().bg(more_bg)),
        ]));
        result.mcp_more_rect = Some(Rect {
            x: area.x,
            y: more_y,
            width: area.width,
            height: 1,
        });
    }

    let visible: Vec<Line<'_>> = lines.into_iter().take(area.height as usize).collect();
    f.render_widget(ratatui::widgets::Paragraph::new(visible), area);
    result
}

/// Meta rendered as plain text, no panel, no bg fill.
/// Preserves the original version dot coloring and project path coloring.
fn render_meta_plain(
    f: &mut ratatui::Frame,
    area: Rect,
    project_root: Option<&str>,
    app_version: &str,
    latest_release: Option<&str>,
    _t: &crate::theme::Theme,
) {
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from("")); // move down one line
    if let Some(root) = project_root {
        lines.push(project_dir_line(root));
    }
    lines.push(Line::from("")); // blank between project and version
    lines.push(version_line(app_version, latest_release));
    f.render_widget(
        ratatui::widgets::Paragraph::new(lines).alignment(ratatui::layout::Alignment::Right),
        area,
    );
}

fn project_dir_line<'a>(dir: &str) -> Line<'a> {
    let t = crate::theme::theme();
    let short = abbreviate_dir(dir);
    if let Some(slash) = short.rfind('/') {
        let parent = short[..=slash].to_string();
        let name = short[slash + 1..].to_string();
        Line::from(vec![
            Span::styled(
                format!("  {parent}"),
                Style::default().fg(t.subtle_fg.into()),
            ),
            Span::styled(name, Style::default().fg(t.accent.into())),
        ])
    } else {
        Line::from(Span::styled(
            format!("  {short}"),
            Style::default().fg(t.accent.into()),
        ))
    }
}

fn version_line<'a>(version: &str, latest: Option<&'a str>) -> Line<'a> {
    let t = crate::theme::theme();
    let dot_color = match latest {
        Some(latest_ver) => {
            if version_is_newer(latest_ver, version) {
                t.warn
            } else {
                t.success
            }
        }
        None => t.subtle_fg,
    };
    let dots = Span::styled(" ∴ ", Style::default().fg(dot_color.into()));
    let brand = Span::styled("atman", Style::default().fg(t.accent.into()));
    let ver = Span::styled(
        format!(" v{version}"),
        Style::default().fg(t.tinted_fg.into()),
    );
    match latest {
        Some(latest_ver) if version_is_newer(latest_ver, version) => {
            let latest_span = Span::styled(
                format!("→ v{latest_ver}"),
                Style::default().fg(t.success.into()),
            );
            Line::from(vec![dots, brand, ver, Span::raw("  "), latest_span])
        }
        _ => Line::from(vec![dots, brand, ver]),
    }
}

fn version_is_newer(candidate: &str, current: &str) -> bool {
    let parse =
        |s: &str| -> Vec<u64> { s.split('.').filter_map(|p| p.parse::<u64>().ok()).collect() };
    let c = parse(candidate);
    let v = parse(current);
    if c.is_empty() || v.is_empty() {
        return false;
    }
    for (cv, vv) in c.iter().zip(v.iter()) {
        match cv.cmp(vv) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => {}
        }
    }
    c.len() > v.len()
}

fn abbreviate_dir(dir: &str) -> String {
    let home = std::env::var("HOME").ok();
    let short = if let Some(h) = &home {
        if let Some(rest) = dir.strip_prefix(h) {
            format!("~{rest}")
        } else {
            dir.to_string()
        }
    } else {
        dir.to_string()
    };
    if crate::width::width(&short) <= 28 {
        return short;
    }
    let head: String = short.chars().take(10).collect();
    let tail: String = short
        .chars()
        .rev()
        .take(14)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

/// kv_line with bg fill for content area.
fn kv_line_bg<'a>(
    key: &'a str,
    value: &str,
    value_style: Style,
    max_w: usize,
    bg: ratatui::style::Color,
) -> Line<'a> {
    let key_str = format!("  {key}:");
    let key_w = crate::width::width(&key_str);
    let val_max = max_w.saturating_sub(key_w);
    let val = crate::width::truncate(value, val_max);
    Line::from(vec![
        Span::styled(
            key_str,
            Style::default()
                .fg(crate::theme::theme().subtle_fg.into())
                .bg(bg),
        ),
        Span::styled(val, value_style.bg(bg)),
    ])
}

/// Render a sidebar popup showing the full text of a clicked plan step or todo item.
/// Small floating panel to the left of the sidebar — no buttons, just content + shadow.
pub fn render_sidebar_popup(
    f: &mut ratatui::Frame,
    sidebar_rect: ratatui::layout::Rect,
    kind: SidebarPopupKind,
    plans: &[atman_runtime::memory::plan::Plan],
    todos: &[atman_runtime::memory::todo::Todo],
) -> ratatui::layout::Rect {
    let t = crate::theme::theme();
    let panel_bg = t.modal_bg.into();
    let accent = Style::default()
        .fg(t.accent.into())
        .add_modifier(Modifier::BOLD);

    // Get the full text for the clicked item
    let (title, full_text) = match kind {
        SidebarPopupKind::Plan(idx) => {
            let latest = plans.iter().max_by_key(|p| p.updated_at);
            match latest.and_then(|p| p.steps.get(idx)) {
                Some(step) => {
                    let label = if step.done { "✓" } else { "○" };
                    (format!(" {label} Step {}", idx + 1), step.text.clone())
                }
                None => return ratatui::layout::Rect::default(),
            }
        }
        SidebarPopupKind::Todo(idx) => match todos.get(idx) {
            Some(todo) => {
                use atman_runtime::memory::todo::TodoStatus;
                let label = match todo.status {
                    TodoStatus::Pending => "○",
                    TodoStatus::InProgress => "⚡",
                    TodoStatus::Done => "✓",
                    TodoStatus::Cancelled => "✗",
                };
                let mut text = todo.why.clone();
                if !todo.where_.is_empty() || !todo.how.is_empty() {
                    text.push_str(&format!("\n  {} · {}", todo.where_, todo.how));
                }
                (format!(" {label} Todo"), text)
            }
            None => return ratatui::layout::Rect::default(),
        },
    };

    // Build content lines with word wrap
    let popup_w: u16 = 52;
    let wrap_w = (popup_w as usize).saturating_sub(4);
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled(format!("  ≡{title}"), accent)));
    lines.push(Line::from(""));
    for line in full_text.lines() {
        let wrapped = crate::width::word_wrap(line, wrap_w);
        if wrapped.is_empty() {
            lines.push(Line::from(Span::styled(
                "  ",
                Style::default().fg(t.tinted_fg.into()),
            )));
        }
        for w in &wrapped {
            lines.push(Line::from(Span::styled(
                format!("  {w}"),
                Style::default().fg(t.tinted_fg.into()),
            )));
        }
    }

    let popup_h: u16 = (lines.len() as u16 + 2).min(sidebar_rect.height);
    let popup_x = sidebar_rect.x.saturating_sub(popup_w).saturating_sub(1);
    let popup_y = sidebar_rect.y;
    let popup_area = ratatui::layout::Rect {
        x: popup_x,
        y: popup_y,
        width: popup_w,
        height: popup_h,
    };

    crate::sanitize_widget_edges(f, popup_area);
    f.render_widget(ratatui::widgets::Clear, popup_area);

    // top padding row
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(panel_bg)),
        ratatui::layout::Rect {
            x: popup_area.x,
            y: popup_area.y,
            width: popup_area.width,
            height: 1,
        },
    );
    let content_area = ratatui::layout::Rect {
        x: popup_area.x,
        y: popup_area.y + 1,
        width: popup_area.width,
        height: popup_area.height.saturating_sub(1),
    };
    f.render_widget(
        ratatui::widgets::Block::default()
            .style(Style::default().bg(panel_bg))
            .padding(ratatui::widgets::Padding {
                left: 1,
                right: 1,
                top: 0,
                bottom: 1,
            }),
        content_area,
    );
    let visible: Vec<Line<'_>> = lines
        .into_iter()
        .take(content_area.height.saturating_sub(1) as usize)
        .collect();
    f.render_widget(ratatui::widgets::Paragraph::new(visible), content_area);

    crate::floating_panels::render_shadow(f, popup_area, &t);
    popup_area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_mode_toggle_flips_open_and_closed() {
        assert_eq!(SidebarMode::Open.toggle(), SidebarMode::Closed);
        assert_eq!(SidebarMode::Closed.toggle(), SidebarMode::Open);
    }

    #[test]
    fn sidebar_mode_open_resolves_regardless_of_width() {
        assert!(SidebarMode::Open.resolve(false));
        assert!(SidebarMode::Open.resolve(true));
    }

    #[test]
    fn sidebar_mode_closed_always_hides() {
        assert!(!SidebarMode::Closed.resolve(true));
        assert!(!SidebarMode::Closed.resolve(false));
    }

    #[test]
    fn sidebar_mode_default_is_open() {
        assert_eq!(SidebarMode::default(), SidebarMode::Open);
    }
}
