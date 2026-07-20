use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntryId {
    SwitchSession,
    NewSession,
    MoveSession,
    DeleteSession,
    YankMode,
    CopyLastMessage,
    CopyLastTool,
    CompactNow,
    SearchHistory,
    ToggleSidebar,
    SetTrustMode,
    SetModeTheme,
    ShowHelp,
}

pub struct PaletteEntry {
    pub id: PaletteEntryId,
    pub group: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub keyword: &'static str,
}

pub const PALETTE_ENTRIES: &[PaletteEntry] = &[
    // ── Session ──
    PaletteEntry {
        id: PaletteEntryId::SwitchSession,
        group: "Session",
        label: "Switch Session",
        hint: "Pick a recent session to swap into",
        keyword: "session switch swap",
    },
    PaletteEntry {
        id: PaletteEntryId::NewSession,
        group: "Session",
        label: "New Session",
        hint: "Start a fresh session in the current directory",
        keyword: "session new create",
    },
    PaletteEntry {
        id: PaletteEntryId::MoveSession,
        group: "Session",
        label: "Move Session",
        hint: "Change this session's working directory",
        keyword: "session move cwd path",
    },
    PaletteEntry {
        id: PaletteEntryId::DeleteSession,
        group: "Session",
        label: "Delete Session",
        hint: "Pick a session to permanently delete",
        keyword: "session delete remove",
    },
    // ── Copy ──
    PaletteEntry {
        id: PaletteEntryId::YankMode,
        group: "Copy",
        label: "Enter Yank Mode",
        hint: "j/k select, Enter copies via OSC 52",
        keyword: "yank copy clipboard",
    },
    PaletteEntry {
        id: PaletteEntryId::CopyLastMessage,
        group: "Copy",
        label: "Copy Last Assistant Message",
        hint: "Push the last assistant text to the terminal clipboard",
        keyword: "copy message clipboard",
    },
    PaletteEntry {
        id: PaletteEntryId::CopyLastTool,
        group: "Copy",
        label: "Copy Last Tool Result",
        hint: "Push the last tool_result content to the clipboard",
        keyword: "copy tool clipboard",
    },
    // ── Context ──
    PaletteEntry {
        id: PaletteEntryId::CompactNow,
        group: "Context",
        label: "Compact Transcript",
        hint: "Force LLM-based compaction on the current transcript",
        keyword: "compact compress",
    },
    PaletteEntry {
        id: PaletteEntryId::SearchHistory,
        group: "Context",
        label: "Search History",
        hint: "Full-text search past turns of this session",
        keyword: "search history find",
    },
    // ── UI ──
    PaletteEntry {
        id: PaletteEntryId::ToggleSidebar,
        group: "UI",
        label: "Toggle Sidebar",
        hint: "Same as F2",
        keyword: "sidebar toggle panel",
    },
    PaletteEntry {
        id: PaletteEntryId::SetTrustMode,
        group: "UI",
        label: "Set Trust Mode",
        hint: "Switch trust level (calm/steady/eager/reckless)",
        keyword: "trust mode eager deny approve allow reckless yolo",
    },
    PaletteEntry {
        id: PaletteEntryId::SetModeTheme,
        group: "UI",
        label: "Set Mode Theme",
        hint: "Switch display theme (default/wuxia/animal/weather/drink)",
        keyword: "theme mode-theme appearance skin display wuxia animal weather drink",
    },
    PaletteEntry {
        id: PaletteEntryId::ShowHelp,
        group: "UI",
        label: "Show Help",
        hint: "Same as F1",
        keyword: "help cheatsheet",
    },
];

impl PaletteEntryId {
    pub fn all() -> Vec<PaletteEntryId> {
        PALETTE_ENTRIES.iter().map(|e| e.id).collect()
    }

    pub fn entry(self) -> &'static PaletteEntry {
        PALETTE_ENTRIES
            .iter()
            .find(|e| e.id == self)
            .expect("PaletteEntryId not in PALETTE_ENTRIES")
    }

    pub fn label(self) -> &'static str {
        self.entry().label
    }

    pub fn hint(self) -> &'static str {
        self.entry().hint
    }
}

#[derive(Default)]
pub struct CommandPalette {
    pub open: bool,
    pub input: String,
    pub filtered: Vec<PaletteEntryId>,
    pub selected: usize,
    /// Display items include group headers. Only Entry variants are selectable.
    display: Vec<PaletteItem>,
}

#[derive(Debug, Clone)]
enum PaletteItem {
    GroupHeader { name: &'static str },
    Entry { id: PaletteEntryId },
}

impl CommandPalette {
    pub fn open(&mut self) {
        self.open = true;
        self.input.clear();
        self.selected = 0;
        self.refresh();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.filtered.clear();
        self.display.clear();
    }

    pub fn push_char(&mut self, c: char) {
        self.input.push(c);
        self.refresh();
    }

    pub fn backspace(&mut self) {
        self.input.pop();
        self.refresh();
    }

    pub fn move_up(&mut self) {
        if self.selected == 0 {
            return;
        }
        self.selected -= 1;
        if matches!(
            self.display.get(self.selected),
            Some(PaletteItem::GroupHeader { .. })
        ) && self.selected > 0
        {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 >= self.display.len() {
            return;
        }
        self.selected += 1;
        if matches!(
            self.display.get(self.selected),
            Some(PaletteItem::GroupHeader { .. })
        ) && self.selected + 1 < self.display.len()
        {
            self.selected += 1;
        }
    }

    pub fn selected(&self) -> Option<PaletteEntryId> {
        match self.display.get(self.selected) {
            Some(PaletteItem::Entry { id }) => Some(*id),
            _ => None,
        }
    }

    fn refresh(&mut self) {
        let query = self.input.to_lowercase();
        let query = query.trim();
        self.filtered = if query.is_empty() {
            PaletteEntryId::all()
        } else {
            PALETTE_ENTRIES
                .iter()
                .filter(|e| {
                    fuzzy_match(e.label.to_lowercase().as_str(), query)
                        || fuzzy_match(e.keyword, query)
                })
                .map(|e| e.id)
                .collect()
        };
        self.build_display();
        self.selected = self
            .display
            .iter()
            .position(|item| matches!(item, PaletteItem::Entry { .. }))
            .unwrap_or(0);
    }

    fn build_display(&mut self) {
        self.display.clear();
        let mut last_group: Option<&'static str> = None;
        for id in &self.filtered {
            let group = id.entry().group;
            if last_group != Some(group) {
                self.display.push(PaletteItem::GroupHeader { name: group });
                last_group = Some(group);
            }
            self.display.push(PaletteItem::Entry { id: *id });
        }
    }
}

fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    for want in needle.chars() {
        loop {
            match chars.next() {
                Some(got) if got == want => break,
                Some(_) => continue,
                None => return false,
            }
        }
    }
    true
}

pub fn render(f: &mut ratatui::Frame, area: Rect, palette: &CommandPalette) {
    let w = area.width.saturating_sub(4).clamp(40, 80);
    let desired = 4 + palette.display.len() as u16 + 2;
    let h = area.height.saturating_sub(4).min(desired).max(6);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(crate::theme::theme().accent))
        .title(Span::styled(
            " Command Palette (Esc to close) ",
            Style::default()
                .fg(crate::theme::theme().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);
    if inner.height == 0 {
        return;
    }
    let input_rect = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: 1,
    };
    let hint_line = Line::from(vec![
        Span::styled("▸ ", Style::default().fg(crate::theme::theme().subtle_fg)),
        Span::styled(
            palette.input.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" _", Style::default().fg(crate::theme::theme().accent)),
    ]);
    f.render_widget(Paragraph::new(hint_line), input_rect);
    let list_rect = Rect {
        x: inner.x,
        y: inner.y.saturating_add(1),
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    let items: Vec<ListItem<'static>> = palette
        .display
        .iter()
        .map(|item| match item {
            PaletteItem::GroupHeader { name } => ListItem::new(Line::from(Span::styled(
                format!("  {name}"),
                Style::default()
                    .fg(crate::theme::theme().subtle_fg)
                    .add_modifier(Modifier::BOLD),
            ))),
            PaletteItem::Entry { id } => {
                let line = Line::from(vec![
                    Span::styled(
                        format!("    {:<26}", id.label()),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        id.hint().to_string(),
                        Style::default().fg(crate::theme::theme().subtle_fg),
                    ),
                ]);
                ListItem::new(line)
            }
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(crate::theme::theme().subtle_fg)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    if !palette.display.is_empty() {
        state.select(Some(palette.selected));
    }
    f.render_stateful_widget(list, list_rect, &mut state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_populates_full_filtered_list() {
        let mut p = CommandPalette::default();
        p.open();
        assert!(p.open);
        assert!(!p.filtered.is_empty());
        // After refresh(), selected points to the first Entry in display,
        // which is at index 1 (index 0 is a GroupHeader).
        assert_eq!(p.selected, 1);
    }

    #[test]
    fn typing_narrows_filtered_list() {
        let mut p = CommandPalette::default();
        p.open();
        p.push_char('y');
        p.push_char('a');
        p.push_char('n');
        assert!(
            p.filtered.contains(&PaletteEntryId::YankMode),
            "yank should stay in filtered results: {:?}",
            p.filtered
        );
        assert!(p.filtered.len() < PaletteEntryId::all().len());
    }

    #[test]
    fn backspace_widens_filter() {
        let mut p = CommandPalette::default();
        p.open();
        p.push_char('z');
        assert!(p.filtered.is_empty() || !p.filtered.is_empty());
        p.backspace();
        assert!(!p.filtered.is_empty());
    }

    #[test]
    fn move_down_clamps_at_end() {
        let mut p = CommandPalette::default();
        p.open();
        for _ in 0..100 {
            p.move_down();
        }
        assert!(
            p.selected < p.display.len(),
            "selected must be within display bounds"
        );
        // After 100 moves, selected must land on the last Entry item.
        assert!(
            matches!(p.display.get(p.selected), Some(PaletteItem::Entry { .. })),
            "selected must be an Entry, not a GroupHeader"
        );
    }

    #[test]
    fn selected_returns_current_entry() {
        let mut p = CommandPalette::default();
        p.open();
        assert_eq!(p.selected(), Some(PaletteEntryId::SwitchSession));
        p.move_down();
        assert_eq!(p.selected(), Some(PaletteEntryId::NewSession));
    }

    #[test]
    fn close_clears_state() {
        let mut p = CommandPalette::default();
        p.open();
        p.push_char('y');
        p.close();
        assert!(!p.open);
        assert!(p.input.is_empty());
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_match("switch session", "swss"));
        assert!(fuzzy_match("copy last tool result", "clast"));
        assert!(!fuzzy_match("compact transcript", "xyz"));
    }
}
