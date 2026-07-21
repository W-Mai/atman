use atman_runtime::ContextSnapshot;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub const GOAL_MAX_LINES: usize = 5;

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
    pub on_goal_scroll: &'a dyn Fn(u16),
    pub on_plans_scroll: &'a dyn Fn(u16),
    pub on_todos_scroll: &'a dyn Fn(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
}

pub fn render(
    f: &mut ratatui::Frame,
    area: Rect,
    inputs: SidebarInputs<'_>,
) -> SidebarRenderResult {
    let t = crate::theme::theme();
    crate::sanitize_widget_edges(f, area);
    f.render_widget(ratatui::widgets::Clear, area);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(t.subtle_fg))
        .title(" atman ")
        .padding(ratatui::widgets::Padding {
            left: 2,
            right: 2,
            top: 1,
            bottom: 1,
        });
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if inner.height == 0 || inner.width == 0 {
        return SidebarRenderResult {
            goal_rect: None,
            plan_rect: None,
            todo_rect: None,
        };
    }

    let _goal_need: u16 = 7;
    let _plans_need: u16 = 3;

    let goal_lines = inputs
        .goal
        .map(|g| g.lines().count() as u16 + 1)
        .unwrap_or(2);
    let plan_lines_full = {
        let latest = inputs.plans.iter().max_by_key(|p| p.updated_at);
        match latest {
            Some(p) => 1 + p.steps.len() as u16,
            None => 2,
        }
    };
    let todo_lines_full = {
        if inputs.todos.is_empty() {
            2
        } else {
            (inputs.todos.len() * 2 + 1) as u16
        }
    };

    let meta_lines: u16 = 5; // title + pwd + version line
    let context_lines: u16 = 9;
    let divider_gap: u16 = 1; // space between divider and context
    let bottom_min = 1 + divider_gap + context_lines + meta_lines; // divider + gap + context + meta

    // Cap Plan/Todo so they don't push Meta off screen.
    let avail = inner.height.saturating_sub(goal_lines + 3 + bottom_min); // gaps + bottom
    let plan_lines = plan_lines_full.min(avail.saturating_sub(todo_lines_full.min(avail)).max(3));
    let todo_lines = todo_lines_full.min(avail.saturating_sub(plan_lines).max(3));

    let task_total = goal_lines + 1 + plan_lines + 1 + todo_lines;
    let needed = task_total + bottom_min;
    let spacing = inner.height.saturating_sub(needed);

    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(goal_lines),
            Constraint::Length(1),
            Constraint::Length(plan_lines),
            Constraint::Length(1),
            Constraint::Length(todo_lines),
            Constraint::Length(spacing),
            Constraint::Length(bottom_min),
        ])
        .split(inner);

    let mut result = SidebarRenderResult {
        goal_rect: None,
        plan_rect: None,
        todo_rect: None,
    };

    // Goal, Plan, Todo at sections 0, 2, 4 (gaps at 1, 3)
    if goal_lines > 0 {
        result.goal_rect = Some(sections[0]);
        let c = render_scrollable_section(
            f,
            sections[0],
            "▸ Goal",
            goal_body(inputs.goal),
            inputs.goal_scroll,
        );
        (inputs.on_goal_scroll)(c);
    }
    if plan_lines > 0 {
        result.plan_rect = Some(sections[2]);
        let header = plans_header(inputs.plans);
        let body = plans_body(inputs.plans);
        let c = render_scrollable_section(f, sections[2], &header, body, inputs.plans_scroll);
        (inputs.on_plans_scroll)(c);
    }
    if todo_lines > 0 {
        result.todo_rect = Some(sections[4]);
        let header = todos_header(inputs.todos);
        let body = todos_body(inputs.todos);
        let c = render_scrollable_section(f, sections[4], &header, body, inputs.todos_scroll);
        (inputs.on_todos_scroll)(c);
    }

    // Bottom area: divider + gap + context + meta (at section 6)
    let bottom_area = sections[6];
    {
        let mp = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(t.subtle_fg));
        let inner = mp.inner(bottom_area);
        f.render_widget(mp, bottom_area);

        let meta_heights = meta_section_heights(inner.height);
        let meta_sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(divider_gap),
                Constraint::Length(meta_heights.context),
                Constraint::Length(meta_heights.session),
            ])
            .split(inner);

        if meta_heights.context > 0 {
            f.render_widget(
                context_section(inputs.context, inputs.attach_count, inputs.streaming),
                meta_sections[1],
            );
        }
        if meta_heights.session > 0 {
            f.render_widget(
                meta_section(
                    inputs.project_root,
                    inputs.app_version,
                    inputs.latest_release,
                ),
                meta_sections[2],
            );
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MetaHeights {
    context: u16,
    session: u16,
}

fn meta_section_heights(inner_h: u16) -> MetaHeights {
    let context = 9u16;
    let session = 5u16;
    if inner_h >= context + session {
        return MetaHeights { context, session };
    }
    if inner_h >= context {
        return MetaHeights {
            context,
            session: 0,
        };
    }
    MetaHeights {
        context: inner_h,
        session: 0,
    }
}

fn render_scrollable_section(
    f: &mut ratatui::Frame,
    area: Rect,
    header: &str,
    body: Vec<Line<'_>>,
    scroll: u16,
) -> u16 {
    let t = crate::theme::theme();
    let header_line = Line::from(Span::styled(
        header.to_string(),
        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
    ));
    let body_count = body.len() as u16;
    let visible_body = area.height.saturating_sub(1);
    let max_scroll = body_count.saturating_sub(visible_body);
    let clamped = scroll.min(max_scroll);
    let sub = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    f.render_widget(Paragraph::new(header_line), sub[0]);
    f.render_widget(Paragraph::new(body).scroll((clamped, 0)), sub[1]);
    clamped
}

fn goal_body(goal: Option<&str>) -> Vec<Line<'_>> {
    let goal_text = goal.unwrap_or("(none)");
    let mut lines: Vec<Line<'_>> = Vec::new();
    for (i, l) in goal_text.lines().enumerate() {
        if i == GOAL_MAX_LINES {
            lines.push(Line::from(Span::styled(
                "  …",
                Style::default().fg(crate::theme::theme().subtle_fg),
            )));
            break;
        }
        lines.push(Line::from(Span::raw(format!("  {l}"))));
    }
    lines
}

fn plans_header(plans: &[atman_runtime::memory::plan::Plan]) -> String {
    let latest = plans.iter().max_by_key(|p| p.updated_at);
    match latest {
        Some(p) => {
            let (done, total) = p.progress();
            format!("▸ Plan ({done}/{total})")
        }
        None => "▸ Plan".to_string(),
    }
}

fn plans_body(plans: &[atman_runtime::memory::plan::Plan]) -> Vec<Line<'_>> {
    let t = crate::theme::theme();
    let latest = plans.iter().max_by_key(|p| p.updated_at);
    let mut lines: Vec<Line<'_>> = Vec::new();
    match latest {
        None => {
            lines.push(Line::from(Span::styled(
                "  (no active plan)",
                Style::default().fg(t.subtle_fg),
            )));
        }
        Some(p) => {
            lines.push(Line::from(Span::styled(
                truncate_line(&p.title, 30),
                Style::default()
                    .fg(t.tinted_fg)
                    .add_modifier(Modifier::BOLD),
            )));
            let total = p.steps.len();
            for (i, step) in p.steps.iter().enumerate() {
                let num = format!("{}", i + 1);
                let (glyph, glyph_style, text_style) = if step.done {
                    (
                        "✓",
                        Style::default().fg(t.success),
                        Style::default().fg(t.meta_fg),
                    )
                } else {
                    (
                        "○",
                        Style::default().fg(t.subtle_fg),
                        Style::default().fg(t.tinted_fg),
                    )
                };
                let indent = if total <= 1 { "  " } else { " │" };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {indent} "), Style::default().fg(t.subtle_fg)),
                    Span::styled(format!("{glyph} "), glyph_style),
                    Span::styled(format!("{num:>2}. "), Style::default().fg(t.meta_fg)),
                    Span::styled(truncate_line(&step.text, 22), text_style),
                ]));
            }
        }
    }
    lines
}

fn todos_header(todos: &[atman_runtime::memory::todo::Todo]) -> String {
    use atman_runtime::memory::todo::TodoStatus;
    let done = todos
        .iter()
        .filter(|tt| matches!(tt.status, TodoStatus::Done))
        .count();
    let total = todos.len();
    format!("▸ Todos ({done}/{total})")
}

fn todos_body<'a>(todos: &'a [atman_runtime::memory::todo::Todo]) -> Vec<Line<'a>> {
    use atman_runtime::memory::todo::TodoStatus;
    let t = crate::theme::theme();
    let mut lines: Vec<Line<'_>> = Vec::new();
    for todo in todos {
        let (glyph, glyph_style) = match todo.status {
            TodoStatus::Pending => ("○", Style::default().fg(t.subtle_fg)),
            TodoStatus::InProgress => (
                "⚡",
                Style::default().fg(t.warn).add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Done => ("✓", Style::default().fg(t.success)),
            TodoStatus::Cancelled => (
                "✗",
                Style::default()
                    .fg(t.subtle_fg)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), glyph_style),
            Span::styled(
                truncate_line(&todo.why, 32),
                Style::default().fg(t.tinted_fg),
            ),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                "    {} · {}",
                truncate_line(&todo.where_, 20),
                truncate_line(&todo.how, 8)
            ),
            Style::default().fg(t.meta_fg),
        )));
    }
    lines
}

fn context_section<'a>(
    ctx: &'a ContextSnapshot,
    attach_count: usize,
    streaming: bool,
) -> Paragraph<'a> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let plain = Style::default();
    let model = if ctx.model.is_empty() {
        "(none)".to_string()
    } else {
        ctx.model.clone()
    };
    let stream_style = if streaming { bold } else { plain };
    use atman_runtime::humanize::format_count;
    let window = if ctx.window_budget == 0 {
        format_count(ctx.window_tokens)
    } else {
        format!(
            "{} / {} ({}%)",
            format_count(ctx.window_tokens),
            format_count(ctx.window_budget),
            (ctx.window_tokens as f64 / ctx.window_budget as f64 * 100.0) as u64
        )
    };
    let lines = vec![
        Line::from(section_title("▸ Context")),
        kv_line("model", model, plain),
        kv_line("window", window, stream_style),
        kv_line(
            "total",
            format!(
                "↑{} · ↓{}",
                format_count(ctx.tokens_in),
                format_count(ctx.tokens_out)
            ),
            plain,
        ),
        kv_line(
            "cache",
            if ctx.cache_read > 0 || ctx.cache_write > 0 {
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
            },
            plain,
        ),
        kv_line(
            "last",
            format!(
                "ttft {} · {:.0} tok/s",
                if ctx.last_ttft_ms > 0 {
                    format!("{}ms", ctx.last_ttft_ms)
                } else {
                    "—".to_string()
                },
                ctx.last_tokens_per_sec
            ),
            plain,
        ),
        kv_line("attach", format!("{attach_count}"), plain),
        kv_line("mcp", format!("{}/{}", ctx.mcp_ok, ctx.mcp_total), plain),
        kv_line(
            "memory",
            format!("recent×{}", ctx.memory_recent_count),
            plain,
        ),
    ];
    Paragraph::new(lines)
}

fn truncate_line(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            out.push('…');
            return out;
        }
        out.push(c);
    }
    out
}

fn meta_section<'a>(
    project_root: Option<&'a str>,
    app_version: &'a str,
    latest_release: Option<&'a str>,
) -> Paragraph<'a> {
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(5);
    lines.push(Line::from(section_title("▸ Meta")));

    if let Some(root) = project_root {
        lines.push(Line::from(""));
        lines.push(project_dir_line(root));
    }
    lines.push(Line::from(""));
    lines.push(version_line(app_version, latest_release));

    Paragraph::new(lines).wrap(Wrap { trim: false })
}

fn project_dir_line<'a>(dir: &str) -> Line<'a> {
    let t = crate::theme::theme();
    let short = abbreviate_dir(dir);
    if let Some(slash) = short.rfind('/') {
        let parent = short[..=slash].to_string();
        let name = short[slash + 1..].to_string();
        Line::from(vec![
            Span::styled(format!("  {parent}"), Style::default().fg(t.subtle_fg)),
            Span::styled(name, Style::default().fg(t.accent)),
        ])
    } else {
        Line::from(Span::styled(
            format!("  {short}"),
            Style::default().fg(t.accent),
        ))
    }
}

fn version_line<'a>(version: &str, latest: Option<&'a str>) -> Line<'a> {
    let t = crate::theme::theme();
    let brand = Span::styled("  atman", Style::default().fg(t.accent));
    let ver = Span::styled(format!(" v{version}"), Style::default().fg(t.tinted_fg));
    match latest {
        Some(latest_ver) if latest_ver != version => {
            let arrow = Span::styled(" ↑ ", Style::default().fg(t.warn));
            let latest = Span::styled(latest_ver, Style::default().fg(t.success));
            Line::from(vec![brand, ver, arrow, latest])
        }
        _ => Line::from(vec![brand, ver]),
    }
}

fn section_title(text: &str) -> Span<'_> {
    Span::styled(
        text,
        Style::default()
            .fg(crate::theme::theme().accent)
            .add_modifier(Modifier::BOLD),
    )
}

fn kv_line<'a>(key: &'a str, value: String, value_style: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("  {key:<7}"),
            Style::default().fg(crate::theme::theme().subtle_fg),
        ),
        Span::styled(value, value_style),
    ])
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
    if short.chars().count() <= 28 {
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

    #[test]
    fn meta_heights_full() {
        let h = meta_section_heights(20);
        assert_eq!(h.context, 9);
        assert_eq!(h.session, 5);
    }

    #[test]
    fn meta_heights_tight_drops_session() {
        let h = meta_section_heights(9);
        assert_eq!(h.context, 9);
        assert_eq!(h.session, 0);
    }
}
