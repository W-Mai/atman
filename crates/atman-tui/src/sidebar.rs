use atman_runtime::ContextSnapshot;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub const GOAL_MAX_LINES: usize = 5;

pub struct SidebarInputs<'a> {
    pub goal: Option<&'a str>,
    pub context: &'a ContextSnapshot,
    pub attach_count: usize,
    pub session_id: &'a str,
    pub session_dir: &'a str,
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
    // Clear underneath so the transcript rows behind the card don't
    // bleed through when messages scroll past the sidebar's y range.
    f.render_widget(ratatui::widgets::Clear, area);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" atman ")
        .padding(ratatui::widgets::Padding::horizontal(1));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let heights = section_heights(inner.height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(heights.goal),
            Constraint::Length(heights.context),
            Constraint::Length(heights.plans),
            Constraint::Length(heights.todos),
            Constraint::Length(heights.session),
        ])
        .split(inner);

    if heights.goal > 0 {
        f.render_widget(goal_section(inputs.goal), sections[0]);
    }
    if heights.context > 0 {
        f.render_widget(
            context_section(inputs.context, inputs.attach_count, inputs.streaming),
            sections[1],
        );
    }
    if heights.plans > 0 {
        f.render_widget(plans_section(inputs.plans, heights.plans), sections[2]);
    }
    if heights.todos > 0 {
        f.render_widget(todos_section(inputs.todos, heights.todos), sections[3]);
    }
    if heights.session > 0 {
        f.render_widget(
            session_section(inputs.session_id, inputs.session_dir),
            sections[4],
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SectionHeights {
    goal: u16,
    context: u16,
    plans: u16,
    todos: u16,
    session: u16,
}

fn section_heights(inner_h: u16) -> SectionHeights {
    let context = 9u16;
    let session = 4u16;
    let todos = 3u16;
    let plans = 3u16;
    let goal_max = 7u16;
    let full = context + session + todos + plans + goal_max;
    if inner_h >= full {
        return SectionHeights {
            goal: goal_max,
            context,
            plans,
            todos,
            session,
        };
    }
    if inner_h >= context + session + todos + plans + 3 {
        return SectionHeights {
            goal: inner_h - context - session - todos - plans,
            context,
            plans,
            todos,
            session,
        };
    }
    if inner_h >= context + session + todos + 3 {
        return SectionHeights {
            goal: inner_h - context - session - todos,
            context,
            plans: 0,
            todos,
            session,
        };
    }
    if inner_h >= context + session + 3 {
        return SectionHeights {
            goal: inner_h - context - session,
            context,
            plans: 0,
            todos: 0,
            session,
        };
    }
    if inner_h >= context + 3 {
        return SectionHeights {
            goal: inner_h - context,
            context,
            plans: 0,
            todos: 0,
            session: 0,
        };
    }
    SectionHeights {
        goal: 0,
        context: inner_h,
        plans: 0,
        todos: 0,
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
                Style::default().fg(Color::DarkGray),
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
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    match latest {
        None => {
            lines.push(Line::from(Span::styled(
                "  (no active plan)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(p) => {
            lines.push(Line::from(vec![
                Span::styled("  ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    truncate_line(&p.title, 28),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
        }
    }
    Paragraph::new(lines)
}

fn todos_section<'a>(todos: &'a [atman_runtime::memory::todo::Todo], max_h: u16) -> Paragraph<'a> {
    use atman_runtime::memory::todo::TodoStatus;
    let done = todos
        .iter()
        .filter(|t| matches!(t.status, TodoStatus::Done))
        .count();
    let total = todos.len();
    let mut lines: Vec<Line<'_>> = Vec::with_capacity(max_h as usize);
    lines.push(Line::from(Span::styled(
        format!("▸ Todos ({done}/{total})"),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    if todos.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no todos yet)",
            Style::default().fg(Color::DarkGray),
        )));
        return Paragraph::new(lines);
    }
    let show_cap = (max_h as usize).saturating_sub(1);
    for todo in todos.iter().take(show_cap) {
        let (glyph, style) = match todo.status {
            TodoStatus::Pending => ("○", Style::default().fg(Color::DarkGray)),
            TodoStatus::InProgress => (
                "⚡",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Done => ("✓", Style::default().fg(Color::Green)),
            TodoStatus::Cancelled => (
                "✗",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT),
            ),
        };
        let content = &todo.where_;
        lines.push(Line::from(vec![
            Span::styled(format!("  {glyph} "), Style::default().fg(Color::DarkGray)),
            Span::styled(truncate_line(content, 24), style),
        ]));
    }
    if todos.len() > show_cap {
        lines.push(Line::from(Span::styled(
            format!("  … +{} more", todos.len() - show_cap),
            Style::default().fg(Color::DarkGray),
        )));
    }
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

fn session_section<'a>(session_id: &'a str, session_dir: &'a str) -> Paragraph<'a> {
    let lines = vec![
        Line::from(section_title("▸ Session")),
        Line::from(Span::raw(format!("  {session_id}"))),
        Line::from(Span::styled(
            format!("  {}", abbreviate_dir(session_dir)),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    Paragraph::new(lines).wrap(Wrap { trim: false })
}

fn section_title(text: &str) -> Span<'_> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn kv_line<'a>(key: &'a str, value: String, value_style: Style) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("  {key:<7}"), Style::default().fg(Color::DarkGray)),
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
    fn section_heights_full_room_keeps_every_section() {
        let h = section_heights(29);
        assert_eq!(h.context, 9);
        assert_eq!(h.session, 4);
        assert_eq!(h.todos, 3);
        assert_eq!(h.plans, 3);
        assert_eq!(h.goal, 7);
    }

    #[test]
    fn section_heights_tight_drops_plans_first() {
        let h = section_heights(20);
        assert_eq!(h.context, 9);
        assert_eq!(h.session, 4);
        assert_eq!(h.todos, 3);
        assert_eq!(h.plans, 0);
        assert!(h.goal >= 3);
    }

    #[test]
    fn section_heights_very_tight_drops_session_and_below() {
        let h = section_heights(12);
        assert_eq!(h.context, 9);
        assert_eq!(h.todos, 0);
        assert_eq!(h.plans, 0);
        assert_eq!(h.session, 0);
        assert!(h.goal >= 3);
    }
}
