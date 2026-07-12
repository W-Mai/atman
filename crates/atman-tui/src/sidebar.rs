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

pub fn render(f: &mut ratatui::Frame, area: Rect, inputs: SidebarInputs<'_>) {
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
        return;
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

    let task_heights = task_section_heights(task_area.height);
    let task_sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(task_heights.goal),
            Constraint::Length(task_heights.plans),
            Constraint::Length(task_heights.todos),
        ])
        .split(task_area);

    if task_heights.goal > 0 {
        f.render_widget(goal_section(inputs.goal), task_sections[0]);
    }
    if task_heights.plans > 0 {
        f.render_widget(
            plans_section(inputs.plans, task_heights.plans),
            task_sections[1],
        );
    }
    if task_heights.todos > 0 {
        f.render_widget(
            todos_section(inputs.todos, task_heights.todos),
            task_sections[2],
        );
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TaskHeights {
    goal: u16,
    plans: u16,
    todos: u16,
}

fn task_section_heights(inner_h: u16) -> TaskHeights {
    let goal = 7u16;
    let plans = 3u16;
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

fn goal_section(goal: Option<&str>) -> Paragraph<'_> {
    let mut lines: Vec<Line<'_>> = vec![Line::from(section_title("▸ Goal"))];
    let goal_text = goal.unwrap_or("(none)");
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
    Paragraph::new(lines).wrap(Wrap { trim: false })
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

fn plans_section<'a>(plans: &'a [atman_runtime::memory::plan::Plan], max_h: u16) -> Paragraph<'a> {
    let latest = plans.iter().max_by_key(|p| p.updated_at);
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(max_h as usize);
    let header = match latest {
        Some(p) => {
            let (done, total) = p.progress();
            format!("▸ Plan ({done}/{total})")
        }
        None => "▸ Plan".to_string(),
    };
    lines.push(Line::from(Span::styled(
        header,
        Style::default()
            .fg(crate::theme::theme().accent)
            .add_modifier(Modifier::BOLD),
    )));
    match latest {
        None => {
            lines.push(Line::from(Span::styled(
                "  (no active plan)",
                Style::default().fg(crate::theme::theme().subtle_fg),
            )));
        }
        Some(p) => {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(crate::theme::theme().subtle_fg)),
                Span::styled(
                    truncate_line(&p.title, 28),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }
    Paragraph::new(lines)
}

fn todos_section<'a>(todos: &'a [atman_runtime::memory::todo::Todo], _max_h: u16) -> Paragraph<'a> {
    use atman_runtime::memory::todo::TodoStatus;
    let done = todos
        .iter()
        .filter(|t| matches!(t.status, TodoStatus::Done))
        .count();
    let total = todos.len();
    let mut lines: Vec<Line<'_>> = Vec::new();
    lines.push(Line::from(Span::styled(
        format!("▸ Todos ({done}/{total})"),
        Style::default()
            .fg(crate::theme::theme().accent)
            .add_modifier(Modifier::BOLD),
    )));
    if todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no todos yet)",
            Style::default().fg(crate::theme::theme().subtle_fg),
        )));
        return Paragraph::new(lines);
    }
    for todo in todos {
        let (glyph, style) = match todo.status {
            TodoStatus::Pending => ("○", Style::default().fg(crate::theme::theme().subtle_fg)),
            TodoStatus::InProgress => (
                "⚡",
                Style::default()
                    .fg(crate::theme::theme().warn)
                    .add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Done => ("✓", Style::default().fg(crate::theme::theme().success)),
            TodoStatus::Cancelled => (
                "✗",
                Style::default()
                    .fg(crate::theme::theme().subtle_fg)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };
        let content = &todo.where_;
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {glyph} "),
                Style::default().fg(crate::theme::theme().subtle_fg),
            ),
            Span::styled(truncate_line(content, 24), style),
        ]));
    }
    Paragraph::new(lines).scroll((0, 0))
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
    fn task_heights_full_room_gives_todos_remaining() {
        let h = task_section_heights(20);
        assert_eq!(h.goal, 7);
        assert_eq!(h.plans, 3);
        assert_eq!(h.todos, 10);
    }

    #[test]
    fn task_heights_tight_shrinks_todos() {
        let h = task_section_heights(9);
        assert_eq!(h.goal, 7);
        assert_eq!(h.plans, 3);
        assert_eq!(h.todos, 0);
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
