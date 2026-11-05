use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

mod paths;

pub const POPUP_MAX_ROWS: u16 = 8;
pub const POPUP_MAX_WIDTH: u16 = 60;

pub fn builtins() -> Vec<(&'static str, &'static str)> {
    atman_runtime::meta_commands::META_COMMANDS
        .iter()
        .filter(|c| crate::panel_registry::command_enabled(c.name))
        .map(|c| (c.name, c.desc))
        .collect()
}

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
    paths: paths::PathCompletion,
}

impl PopupState {
    pub fn is_open(&self) -> bool {
        !self.items.is_empty() || self.is_waiting()
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.paths.is_waiting()
    }

    pub(crate) fn request_paths(&mut self, prefix: &str) {
        if self.paths.request(prefix) {
            self.items.clear();
            self.selected = 0;
        }
    }

    pub(crate) async fn next_update(&mut self) {
        self.items = self.paths.next().await;
        self.selected = 0;
    }

    pub fn items(&self) -> &[CompletionItem] {
        &self.items
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn set(&mut self, items: Vec<CompletionItem>) {
        self.paths.cancel();
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
        self.paths.cancel();
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
        ':' => {
            let mut candidates: Vec<_> = builtins
                .iter()
                .filter(|(name, _)| name.to_ascii_lowercase().starts_with(&query_lower))
                .map(|(name, hint)| CompletionItem {
                    insert: format!(":{name} "),
                    hint: hint.to_string(),
                })
                .collect();
            candidates.sort_by_key(|item| !item.insert.trim_end().eq_ignore_ascii_case(buf));
            candidates
        }
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

pub fn render_popup(f: &mut ratatui::Frame, input_rect: Rect, state: &PopupState, tick: u32) {
    let items = state.items();
    if state.is_waiting() {
        let area = Rect::new(
            input_rect.x,
            input_rect.y.saturating_sub(3),
            input_rect.width.min(POPUP_MAX_WIDTH),
            3,
        );
        crate::sanitize_widget_edges(f, area);
        f.render_widget(Clear, area);
        let block = crate::surface::block(crate::surface::Role::Overlay)
            .padding(ratatui::widgets::Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(crate::loading::line("Finding paths", tick, inner.width)),
            inner,
        );
        return;
    }
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
                    Style::default().fg(crate::theme::theme().accent.into()),
                ),
                Span::raw("  "),
                Span::styled(
                    item.hint.clone(),
                    Style::default().fg(crate::theme::theme().subtle_fg.into()),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(list_items)
        .block(
            crate::surface::block(crate::surface::Role::Overlay)
                .title(" ↑↓ SELECT · ENTER/TAB COMPLETE ")
                .padding(ratatui::widgets::Padding::new(1, 1, 0, 1)),
        )
        .highlight_style(
            Style::default()
                .bg(crate::theme::theme().work_hover_bg.into())
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
        let text = " [SELECT & COPY] hjkl move · g/G edge · v anchor · Tab surface · Enter copy · Esc cancel";
        let p = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(crate::theme::theme().accent.into()),
        )));
        f.render_widget(p, rect);
        return;
    }
    if !mouse_captured {
        let text = " [SELECT MODE] drag to copy · F3 resume interaction";
        let p = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(crate::theme::theme().warn.into()),
        )));
        f.render_widget(p, rect);
        return;
    }
    let text = if narrow {
        " /  :  !  @  F1  F3  ^P"
    } else {
        " hint: / flow · : cmd · ! interject · @ path · F1 help · F3 select · Ctrl+P palette · Ctrl+K search"
    };
    let p = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(crate::theme::theme().subtle_fg.into()),
    )));
    f.render_widget(p, rect);
}

pub fn cheatsheet_lines() -> Vec<Line<'static>> {
    let mut body = vec![
        Line::from(section("Editing")),
        kv("← / →", "move cursor"),
        kv("↑ / ↓", "move line ↑↓, or history at ends"),
        kv("Home / Ctrl+A", "cursor to start"),
        kv("End / Ctrl+E", "cursor to end"),
        kv("Ctrl+W / Alt+Backspace", "delete word"),
        kv("Ctrl+J / Shift+Enter", "insert newline"),
        kv("Cmd/Ctrl/Alt+V", "attach clipboard image"),
        kv("Ctrl+U", "remove pending quote or last image"),
        kv("Ctrl+T", "cycle input reasoning depth"),
        kv("Tab", "expand [Pasted Text …] chip"),
        kv("Enter", "submit"),
        kv("Ctrl+C", "clear input, or twice to quit"),
        kv("wheel over input", "move caret line ↑↓"),
        Line::from(""),
        Line::from(section("Scrollback")),
        kv("PageUp / PageDown", "scroll transcript"),
        kv(
            "Ctrl+End / click ↓ new",
            "scroll to latest and follow output",
        ),
        kv("wheel outside input", "scroll transcript"),
        Line::from(""),
        Line::from(section("Interjection (while streaming)")),
        kv("Esc", "cancel current flow"),
        kv(
            "Ctrl+G / Ctrl+B / Ctrl+R / Ctrl+X",
            "!nudge / course / redirect / stop",
        ),
        Line::from(""),
        Line::from(section("Approvals (when pending)")),
        kv("1..9", "approve the Nth request"),
        kv("a", "approve all"),
        kv("d (single pending)", "deny it"),
        kv("d then d (multi)", "deny the first"),
        kv("d then 1..9", "deny the Nth"),
        kv("Esc", "deny all + cancel flow"),
        Line::from(""),
        Line::from(section("Flows & Tools")),
        kv("click a tool / flow panel", "toggle expansion"),
        kv("Ctrl+O", "toggle last tool expansion"),
        kv("Alt+O", "toggle latest work section"),
        Line::from(""),
        Line::from(section("Windows")),
        kv("F1", "this cheatsheet"),
        kv("≡ / :sidebar", "expand or collapse sidebar"),
        kv(
            "Ctrl+] / :activity",
            "switch Overview / Activity without expanding",
        ),
        kv("Ctrl+P / :panel", "command palette"),
        kv(
            "Ctrl+Tab / Ctrl+Shift+Tab",
            "next / previous floating window",
        ),
        kv("Esc / — / ✕", "close flow view without stopping execution"),
        kv(
            "▢ / double-click title / F10",
            "maximize or restore flow view",
        ),
        kv("drag title / corner", "move or resize floating window"),
        kv("F6 / F7 in flow view", "move / resize with arrow keys"),
        kv(
            "Ctrl+Enter in flow view",
            "show this conversation in main view",
        ),
        kv(
            "Ctrl+Y / Goal button",
            "pause or resume the displayed agent goal",
        ),
        kv(":goal", "edit and inspect root or child goals"),
        kv("Ctrl+K / :history", "search history"),
        Line::from(""),
        Line::from(section("Search History (Ctrl+K)")),
        kv("Enter", "search"),
        kv("↑↓ / j k", "navigate results"),
        kv("Tab", "toggle scope (session / project)"),
        kv("Esc", "close"),
        kv("/pattern/", "regex match"),
        Line::from(""),
        Line::from(section("Meta Commands")),
    ];
    for cmd in atman_runtime::meta_commands::META_COMMANDS {
        if !crate::panel_registry::command_enabled(cmd.name) {
            continue;
        }
        body.push(kv(cmd.usage, cmd.desc));
    }
    body
}

fn section(text: &str) -> Span<'_> {
    Span::styled(
        text,
        Style::default()
            .fg(crate::theme::theme().accent.into())
            .add_modifier(Modifier::BOLD),
    )
}

fn kv<'a>(key: &'a str, value: &'a str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!(" {key:<28}"),
            Style::default().fg(crate::theme::theme().warn.into()),
        ),
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
        atman_runtime::meta_commands::META_COMMANDS
            .iter()
            .map(|c| (c.name, c.desc))
            .collect()
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
    fn complete_command_precedes_longer_prefix_matches() {
        for command in ["model", "mode", "session"] {
            let out = compute_candidates(&format!(":{command}"), &[], &builtins(), &[], false);
            assert!(out.len() > 1);
            assert_eq!(out[0].insert, format!(":{command} "));
        }
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
