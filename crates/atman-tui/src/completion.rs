use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

pub const POPUP_MAX_ROWS: u16 = 8;
pub const POPUP_MAX_WIDTH: u16 = 60;

pub const BUILTINS: &[(&str, &str)] = &[
    (":help", "show help"),
    (":exit", "leave repl"),
    (":session", "print current session id"),
    (":cost", "cost summary hint"),
    (":attach", "attach file / list / clear"),
    (":suggest", "meta-LLM flow suggestion"),
    (":goal", "get / set / clear session goal"),
    (":sessions", "list recent sessions"),
    (":sidebar", "sidebar on / off / auto"),
    (":todo", "list / done <id> / cancel <id> / clear"),
    (":rename", "set / clear this session's title"),
    (":compact", "compact transcript now"),
];

pub const INTERJECTIONS: &[(&str, &str)] = &[
    ("!nudge", "queue nudge to next chunk boundary"),
    ("!course-correct", "L2 restart at chunk boundary"),
    ("!redirect", "L3 redirect to another flow"),
    ("!stop", "L4 hard stop"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    pub insert: String,
    pub hint: String,
}

#[derive(Debug, Default)]
pub struct PopupState {
    items: Vec<CompletionItem>,
    selected: usize,
}

impl PopupState {
    pub fn is_open(&self) -> bool {
        !self.items.is_empty()
    }

    pub fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn set(&mut self, items: Vec<CompletionItem>) {
        if items.is_empty() {
            self.close();
            return;
        }
        let same = self.items.len() == items.len()
            && self.items.iter().zip(items.iter()).all(|(a, b)| a == b);
        if !same {
            self.items = items;
            self.selected = 0;
        }
    }

    pub fn close(&mut self) {
        self.items.clear();
        self.selected = 0;
    }

    pub fn next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.items.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn accept(&mut self) -> Option<CompletionItem> {
        if self.items.is_empty() {
            return None;
        }
        let item = self.items[self.selected].clone();
        self.close();
        Some(item)
    }
}

pub fn compute_candidates(
    buf: &str,
    flows: &[(String, String)],
    builtins: &[(&str, &str)],
    interjections: &[(&str, &str)],
    streaming: bool,
) -> Vec<CompletionItem> {
    if buf.contains('\n') {
        return Vec::new();
    }
    let (sigil, rest) = split_prefix(buf);
    let query_lower = rest.to_ascii_lowercase();
    match sigil {
        '/' => flows
            .iter()
            .filter(|(name, _)| name.to_ascii_lowercase().starts_with(&query_lower))
            .map(|(name, hint)| CompletionItem {
                insert: format!("/{name} "),
                hint: hint.clone(),
            })
            .collect(),
        ':' => builtins
            .iter()
            .filter(|(name, _)| {
                name.trim_start_matches(':')
                    .to_ascii_lowercase()
                    .starts_with(&query_lower)
            })
            .map(|(name, hint)| CompletionItem {
                insert: format!("{name} "),
                hint: hint.to_string(),
            })
            .collect(),
        '!' => interjections
            .iter()
            .filter(|(name, _)| {
                name.trim_start_matches('!')
                    .to_ascii_lowercase()
                    .starts_with(&query_lower)
            })
            .map(|(name, hint)| CompletionItem {
                insert: format!("{name} "),
                hint: if streaming {
                    hint.to_string()
                } else {
                    format!("{hint} (no active flow)")
                },
            })
            .collect(),
        '@' => complete_paths(rest),
        _ => Vec::new(),
    }
}

fn split_prefix(buf: &str) -> (char, &str) {
    if let Some(first) = buf.chars().next()
        && matches!(first, '/' | ':' | '!' | '@')
    {
        (first, &buf[first.len_utf8()..])
    } else {
        ('\0', buf)
    }
}

fn complete_paths(prefix: &str) -> Vec<CompletionItem> {
    let (dir, basename) = if let Some(idx) = prefix.rfind('/') {
        (&prefix[..=idx], &prefix[idx + 1..])
    } else {
        ("./", prefix)
    };
    let read_dir = std::fs::read_dir(dir).ok();
    let Some(entries) = read_dir else {
        return Vec::new();
    };
    let base_lower = basename.to_ascii_lowercase();
    let mut out: Vec<CompletionItem> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.to_ascii_lowercase().starts_with(&base_lower) {
            continue;
        }
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let insert = if is_dir {
            format!("@{dir}{name}/")
        } else {
            format!("@{dir}{name} ")
        };
        out.push(CompletionItem {
            insert,
            hint: if is_dir { "dir".into() } else { "file".into() },
        });
        if out.len() >= 32 {
            break;
        }
    }
    out.sort_by(|a, b| a.insert.cmp(&b.insert));
    out
}

pub fn render_popup(f: &mut ratatui::Frame, input_rect: Rect, state: &PopupState) {
    let items = state.items();
    if items.is_empty() {
        return;
    }
    let popup_h = (items.len() as u16 + 2).min(POPUP_MAX_ROWS);
    let width = input_rect.width.min(POPUP_MAX_WIDTH);
    let x = input_rect.x;
    let y = input_rect.y.saturating_sub(popup_h);
    let popup_rect = Rect {
        x,
        y,
        width,
        height: popup_h,
    };
    crate::sanitize_widget_edges(f, popup_rect);
    f.render_widget(Clear, popup_rect);
    let list_items: Vec<ListItem<'_>> = items
        .iter()
        .map(|item| {
            let line = Line::from(vec![
                Span::styled(
                    item.insert.trim_end().to_string(),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw("  "),
                Span::styled(item.hint.clone(), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(list_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    let mut list_state = ListState::default();
    list_state.select(Some(state.selected()));
    f.render_stateful_widget(list, popup_rect, &mut list_state);
}

pub fn render_hint_strip(
    f: &mut ratatui::Frame,
    rect: Rect,
    narrow: bool,
    mouse_captured: bool,
    yank_mode: bool,
) {
    if yank_mode {
        let text = " [YANK MODE] j/k select · Enter copy · Esc cancel";
        let p = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Magenta),
        )));
        f.render_widget(p, rect);
        return;
    }
    if !mouse_captured {
        let text = " [SELECT MODE] drag to copy · F3 resume interaction";
        let p = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(Color::Yellow),
        )));
        f.render_widget(p, rect);
        return;
    }
    let text = if narrow {
        " /  :  !  @  F1  F3  ^P"
    } else {
        " hint: / flow · : cmd · ! interject · @ path · F1 help · F2 sidebar · F3 select · Ctrl+P palette"
    };
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(Color::DarkGray),
    )));
    f.render_widget(p, rect);
}

pub fn render_cheatsheet(f: &mut ratatui::Frame, area: Rect) {
    let w = area.width.saturating_sub(4).min(80);
    let h = area.height.saturating_sub(4).min(22);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" atman keybindings — Esc / F1 to close ");
    let body = vec![
        Line::from(section("Editing")),
        kv("← / →", "move cursor"),
        kv("Home / Ctrl+A", "cursor to start"),
        kv("End / Ctrl+E", "cursor to end"),
        kv("Ctrl+W / Alt+Backspace", "delete word"),
        kv("Ctrl+J / Shift+Enter", "insert newline"),
        kv("Enter", "submit"),
        Line::from(""),
        Line::from(section("Scrollback")),
        kv("PageUp / PageDown", "scroll transcript"),
        kv("Home / End", "top / follow tail"),
        Line::from(""),
        Line::from(section("Interjection (while streaming)")),
        kv("Esc", "cancel current flow"),
        kv(
            "Ctrl+G / Ctrl+B / Ctrl+R / Ctrl+X",
            "!nudge / course / redirect / stop",
        ),
        Line::from(""),
        Line::from(section("Flows & Tools")),
        kv("click a tool / flow panel", "toggle expansion"),
        kv("Ctrl+O", "toggle last tool expansion"),
        Line::from(""),
        Line::from(section("Windows")),
        kv("F1", "this cheatsheet"),
        kv("F2 / :sidebar", "toggle sidebar"),
        kv("Ctrl+C twice", "quit"),
    ];
    let p = Paragraph::new(body)
        .block(block)
        .wrap(Wrap { trim: false })
        .alignment(Alignment::Left);
    f.render_widget(p, rect);
}

fn section(text: &str) -> Span<'_> {
    Span::styled(
        text,
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )
}

fn kv<'a>(key: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!(" {key:<28}"), Style::default().fg(Color::Yellow)),
        Span::raw(value),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flows() -> Vec<(String, String)> {
        vec![
            ("hello".into(), "smoke test".into()),
            ("agent".into(), "default route".into()),
            ("help".into(), "list flows".into()),
        ]
    }

    fn builtins() -> Vec<(&'static str, &'static str)> {
        vec![
            (":help", "show help"),
            (":exit", "leave repl"),
            (":goal", "manage goal"),
        ]
    }

    fn interjections() -> Vec<(&'static str, &'static str)> {
        vec![("!nudge", "queue nudge"), ("!stop", "hard stop")]
    }

    #[test]
    fn slash_prefix_lists_matching_flows() {
        let out = compute_candidates("/h", &flows(), &builtins(), &interjections(), false);
        let names: Vec<_> = out.iter().map(|c| c.insert.clone()).collect();
        assert!(names.contains(&"/hello ".to_string()));
        assert!(names.contains(&"/help ".to_string()));
        assert!(!names.contains(&"/agent ".to_string()));
    }

    #[test]
    fn colon_prefix_lists_matching_builtins() {
        let out = compute_candidates(":he", &flows(), &builtins(), &interjections(), false);
        let inserts: Vec<_> = out.iter().map(|c| c.insert.clone()).collect();
        assert_eq!(inserts, vec![":help "]);
    }

    #[test]
    fn bang_prefix_lists_interjections_and_marks_idle_state() {
        let idle = compute_candidates("!", &flows(), &builtins(), &interjections(), false);
        assert_eq!(idle.len(), 2);
        assert!(idle.iter().all(|c| c.hint.contains("no active flow")));
        let streaming = compute_candidates("!", &flows(), &builtins(), &interjections(), true);
        assert_eq!(streaming.len(), 2);
        assert!(streaming.iter().all(|c| !c.hint.contains("no active flow")));
    }

    #[test]
    fn no_prefix_returns_empty() {
        let out = compute_candidates("hello", &flows(), &builtins(), &interjections(), false);
        assert!(out.is_empty());
    }

    #[test]
    fn multiline_disables_completion() {
        let out = compute_candidates(
            "/hello\nmore",
            &flows(),
            &builtins(),
            &interjections(),
            false,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn popup_state_next_prev_wraps() {
        let mut s = PopupState::default();
        s.set(vec![
            CompletionItem {
                insert: "a".into(),
                hint: "".into(),
            },
            CompletionItem {
                insert: "b".into(),
                hint: "".into(),
            },
        ]);
        assert_eq!(s.selected(), 0);
        s.next();
        assert_eq!(s.selected(), 1);
        s.next();
        assert_eq!(s.selected(), 0);
        s.prev();
        assert_eq!(s.selected(), 1);
    }

    #[test]
    fn popup_accept_returns_item_and_closes() {
        let mut s = PopupState::default();
        s.set(vec![CompletionItem {
            insert: "x".into(),
            hint: "".into(),
        }]);
        let item = s.accept().unwrap();
        assert_eq!(item.insert, "x");
        assert!(!s.is_open());
    }

    #[test]
    fn set_preserves_selection_when_identical() {
        let items = vec![
            CompletionItem {
                insert: "a".into(),
                hint: "".into(),
            },
            CompletionItem {
                insert: "b".into(),
                hint: "".into(),
            },
        ];
        let mut s = PopupState::default();
        s.set(items.clone());
        s.next();
        assert_eq!(s.selected(), 1);
        s.set(items);
        assert_eq!(s.selected(), 1);
    }
}
