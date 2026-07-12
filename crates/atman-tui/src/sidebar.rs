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
    let context_need: u16 = 9;
    let session_need: u16 = 5;
    let meta_needs = context_need + session_need;
    let total = inner.height;
    let (task_h, meta_h) = if total > meta_needs {
        (total - meta_needs, meta_needs)
    } else {
        (total, 0u16)
    };

    let panels = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(task_h), Constraint::Length(meta_h)])
        .split(inner);

    let task_panel = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t.subtle_fg));
    let meta_panel = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(t.subtle_fg));

    let task_area = task_panel.inner(panels[0]);
    f.render_widget(task_panel, panels[0]);
    let meta_area = meta_panel.inner(panels[1]);
    f.render_widget(meta_panel, panels[1]);

    let goal_lines = inputs
        .goal
        .map(|g| g.lines().count() as u16 + 1)
        .unwrap_or(2);
    let plan_lines = {
        let latest = inputs.plans.iter().max_by_key(|p| p.updated_at);
        match latest {
            Some(p) => 1 + p.steps.len() as u16,
            None => 2,
        }
    };
    let todo_lines = {
        if inputs.todos.is_empty() {
            2
        } else {
            (inputs.todos.len() * 2 + 1) as u16
        }
    };
    let task_heights = task_section_heights(task_area.height, goal_lines, plan_lines, todo_lines);
    let task_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(task_heights.goal),
            Constraint::Length(task_heights.plans),
            Constraint::Length(task_heights.todos),
        ])
        .split(task_area);

    let mut result = SidebarRenderResult {
        goal_rect: None,
        plan_rect: None,
        todo_rect: None,
    };

    if task_heights.goal > 0 {
        result.goal_rect = Some(task_sections[0]);
        let c = render_scrollable_section(
            f,
            task_sections[0],
            "▸ Goal",
            goal_body(inputs.goal),
            inputs.goal_scroll,
        );
        (inputs.on_goal_scroll)(c);
    }
    if task_heights.plans > 0 {
        result.plan_rect = Some(task_sections[1]);
        let header = plans_header(inputs.plans);
        let body = plans_body(inputs.plans);
        let c = render_scrollable_section(f, task_sections[1], &header, body, inputs.plans_scroll);
        (inputs.on_plans_scroll)(c);
    }
    if task_heights.todos > 0 {
        result.todo_rect = Some(task_sections[2]);
        let header = todos_header(inputs.todos);
        let body = todos_body(inputs.todos);
        let c = render_scrollable_section(f, task_sections[2], &header, body, inputs.todos_scroll);
        (inputs.on_todos_scroll)(c);
    }

    let meta_heights = meta_section_heights(meta_area.height);
    let meta_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(meta_heights.context),
            Constraint::Length(meta_heights.session),
        ])
        .split(meta_area);

    if meta_heights.context > 0 {
        f.render_widget(
            context_section(inputs.context, inputs.attach_count, inputs.streaming),
            meta_sections[0],
        );
    }
    if meta_heights.session > 0 {
        f.render_widget(
            session_section(inputs.session_id, inputs.session_dir, inputs.project_root),
            meta_sections[1],
        );
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskHeights {
    goal: u16,
    plans: u16,
    todos: u16,
}

fn task_section_heights(
    inner_h: u16,
    goal_lines: u16,
    plan_lines: u16,
    todo_lines: u16,
) -> TaskHeights {
    let goal_need = goal_lines.min(inner_h);
    let plan_need = plan_lines.min(inner_h.saturating_sub(goal_need));
    let todo_need = todo_lines.min(inner_h.saturating_sub(goal_need).saturating_sub(plan_need));
    let used = goal_need + plan_need + todo_need;
    let extra = inner_h.saturating_sub(used);
    let goal = goal_need + (extra / 3);
    let plans = plan_need + (extra / 3);
    let todos = inner_h.saturating_sub(goal).saturating_sub(plans);
    TaskHeights { goal, plans, todos }
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
    use crate::humanize::format_count;
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
            "spent",
            format!(
                "in {} · out {}",
                format_count(ctx.tokens_in),
                format_count(ctx.tokens_out)
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

fn session_section<'a>(
    session_id: &'a str,
    session_dir: &'a str,
    project_root: Option<&'a str>,
) -> Paragraph<'a> {
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(4);
    lines.push(Line::from(section_title("▸ Session")));
    lines.push(Line::from(Span::raw(format!("  {session_id}"))));
    if let Some(root) = project_root {
        lines.push(Line::from(vec![
            Span::styled(
                "  pwd ",
                Style::default().fg(crate::theme::theme().subtle_fg),
            ),
            Span::styled(
                abbreviate_dir(root),
                Style::default().fg(crate::theme::theme().accent),
            ),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("  {}", abbreviate_dir(session_dir)),
        Style::default().fg(crate::theme::theme().subtle_fg),
    )));
    Paragraph::new(lines).wrap(Wrap { trim: false })
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
    fn task_heights_content_fits() {
        let h = task_section_heights(30, 2, 6, 41);
        assert!(h.goal >= 2);
        assert!(h.plans >= 6);
        assert!(h.todos >= 20);
    }

    #[test]
    fn task_heights_tight_shrinks() {
        let h = task_section_heights(9, 2, 6, 41);
        assert_eq!(h.goal, 2);
        assert!(h.plans + h.todos <= 7);
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
