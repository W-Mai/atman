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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarMode {
    #[default]
    Auto,
    Force(bool),
}

impl SidebarMode {
    pub fn toggle(self) -> Self {
        match self {
            Self::Auto => Self::Force(false),
            Self::Force(false) => Self::Force(true),
            Self::Force(true) => Self::Auto,
        }
    }

    pub fn resolve(self, wide_enough: bool) -> bool {
        match self {
            Self::Auto => wide_enough,
            Self::Force(v) => v,
        }
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, inputs: SidebarInputs<'_>) {
    let outer = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let (goal_h, context_h, todos_h, session_h) = section_heights(inner.height);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(goal_h),
            Constraint::Length(context_h),
            Constraint::Length(todos_h),
            Constraint::Length(session_h),
        ])
        .split(inner);

    if goal_h > 0 {
        f.render_widget(goal_section(inputs.goal), sections[0]);
    }
    if context_h > 0 {
        f.render_widget(
            context_section(inputs.context, inputs.attach_count, inputs.streaming),
            sections[1],
        );
    }
    if todos_h > 0 {
        f.render_widget(todos_section(), sections[2]);
    }
    if session_h > 0 {
        f.render_widget(
            session_section(inputs.session_id, inputs.session_dir),
            sections[3],
        );
    }
}

fn section_heights(inner_h: u16) -> (u16, u16, u16, u16) {
    let context_h = 8u16;
    let session_h = 4u16;
    let todos_h = 3u16;
    let goal_max = 7u16;
    if inner_h >= context_h + session_h + todos_h + goal_max {
        return (goal_max, context_h, todos_h, session_h);
    }
    if inner_h >= context_h + session_h + todos_h + 3 {
        return (
            inner_h - context_h - session_h - todos_h,
            context_h,
            todos_h,
            session_h,
        );
    }
    if inner_h >= context_h + session_h + 3 {
        return (inner_h - context_h - session_h, context_h, 0, session_h);
    }
    if inner_h >= context_h + 3 {
        return (inner_h - context_h, context_h, 0, 0);
    }
    (0, inner_h, 0, 0)
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
    let lines = vec![
        Line::from(section_title("▸ Context")),
        kv_line("model", model, plain),
        kv_line(
            "tokens",
            format!("{} / {}", ctx.tokens_in, ctx.tokens_out),
            stream_style,
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

fn todos_section<'a>() -> Paragraph<'a> {
    Paragraph::new(vec![
        Line::from(section_title("▸ Todos")),
        Line::from(Span::styled(
            "  (no todos yet)",
            Style::default().fg(Color::DarkGray),
        )),
    ])
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
    fn sidebar_mode_toggle_cycles_through_states() {
        let m = SidebarMode::Auto;
        let m = m.toggle();
        assert_eq!(m, SidebarMode::Force(false));
        let m = m.toggle();
        assert_eq!(m, SidebarMode::Force(true));
        let m = m.toggle();
        assert_eq!(m, SidebarMode::Auto);
    }

    #[test]
    fn sidebar_mode_auto_follows_width() {
        assert!(!SidebarMode::Auto.resolve(false));
        assert!(SidebarMode::Auto.resolve(true));
    }

    #[test]
    fn sidebar_mode_force_ignores_width() {
        assert!(!SidebarMode::Force(false).resolve(true));
        assert!(SidebarMode::Force(true).resolve(false));
    }

    #[test]
    fn section_heights_full_room() {
        let (g, c, t, s) = section_heights(25);
        assert_eq!((g, c, t, s), (7, 8, 3, 4));
    }

    #[test]
    fn section_heights_tight_drops_todos_first() {
        let (g, c, t, s) = section_heights(15);
        assert_eq!(c, 8);
        assert_eq!(s, 4);
        assert_eq!(t, 0);
        assert!(g >= 3);
    }

    #[test]
    fn section_heights_very_tight_drops_session_and_todos() {
        let (g, c, t, s) = section_heights(11);
        assert_eq!(c, 8);
        assert_eq!(t, 0);
        assert_eq!(s, 0);
        assert!(g >= 3);
    }
}
