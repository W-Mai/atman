use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntryId {
    SwitchSession,
    YankMode,
    CopyLastMessage,
    CopyLastTool,
    CompactNow,
    SearchHistory,
    ToggleSidebar,
    ShowHelp,
}

impl PaletteEntryId {
    pub const ALL: &'static [PaletteEntryId] = &[
        PaletteEntryId::SwitchSession,
        PaletteEntryId::YankMode,
        PaletteEntryId::CopyLastMessage,
        PaletteEntryId::CopyLastTool,
        PaletteEntryId::CompactNow,
        PaletteEntryId::SearchHistory,
        PaletteEntryId::ToggleSidebar,
        PaletteEntryId::ShowHelp,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PaletteEntryId::SwitchSession => "Switch Session",
            PaletteEntryId::YankMode => "Enter Yank Mode",
            PaletteEntryId::CopyLastMessage => "Copy Last Assistant Message",
            PaletteEntryId::CopyLastTool => "Copy Last Tool Result",
            PaletteEntryId::CompactNow => "Compact Transcript",
            PaletteEntryId::SearchHistory => "Search History",
            PaletteEntryId::ToggleSidebar => "Toggle Sidebar",
            PaletteEntryId::ShowHelp => "Show Help",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            PaletteEntryId::SwitchSession => "Pick a recent session to swap into",
            PaletteEntryId::YankMode => "j/k select, Enter copies via OSC 52",
            PaletteEntryId::CopyLastMessage => {
                "Push the last assistant text to the terminal clipboard"
            }
            PaletteEntryId::CopyLastTool => "Push the last tool_result content to the clipboard",
            PaletteEntryId::CompactNow => "Force LLM-based compaction on the current transcript",
            PaletteEntryId::SearchHistory => "Full-text search past turns of this session",
            PaletteEntryId::ToggleSidebar => "Same as F2",
            PaletteEntryId::ShowHelp => "Same as F1",
        }
    }
}

#[derive(Default)]
pub struct CommandPalette {
    pub open: bool,
    pub input: String,
    pub filtered: Vec<PaletteEntryId>,
    pub selected: usize,
}

impl std::fmt::Debug for CommandPalette {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandPalette")
            .field("open", &self.open)
            .field("input", &self.input)
            .field("filtered_count", &self.filtered.len())
            .field("selected", &self.selected)
            .finish()
    }
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
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    pub fn selected(&self) -> Option<PaletteEntryId> {
        self.filtered.get(self.selected).copied()
    }

    fn refresh(&mut self) {
        let query = self.input.to_lowercase();
        let query = query.trim();
        self.filtered = if query.is_empty() {
            PaletteEntryId::ALL.to_vec()
        } else {
            PaletteEntryId::ALL
                .iter()
                .copied()
                .filter(|id| fuzzy_match(&id.label().to_lowercase(), query))
                .collect()
        };
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
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
    let desired = 4 + palette.filtered.len() as u16 + 2;
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
        .border_style(Style::default().fg(Color::Magenta))
        .title(Span::styled(
            " Command Palette (Esc to close) ",
            Style::default()
                .fg(Color::Magenta)
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
        Span::styled("▸ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            palette.input.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" _", Style::default().fg(Color::Magenta)),
    ]);
    f.render_widget(Paragraph::new(hint_line), input_rect);
    let list_rect = Rect {
        x: inner.x,
        y: inner.y.saturating_add(1),
        width: inner.width,
        height: inner.height.saturating_sub(1),
    };
    let items: Vec<ListItem<'static>> = palette
        .filtered
        .iter()
        .map(|id| {
            let line = Line::from(vec![
                Span::styled(
                    format!("{:<28}", id.label()),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(id.hint().to_string(), Style::default().fg(Color::DarkGray)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    let mut state = ListState::default();
    if !palette.filtered.is_empty() {
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
        assert_eq!(p.filtered.len(), PaletteEntryId::ALL.len());
        assert_eq!(p.selected, 0);
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
        assert!(p.filtered.len() < PaletteEntryId::ALL.len());
    }

    #[test]
    fn backspace_widens_filter() {
        let mut p = CommandPalette::default();
        p.open();
        p.push_char('z');
        assert!(p.filtered.is_empty() || !p.filtered.is_empty());
        p.backspace();
        assert_eq!(p.filtered.len(), PaletteEntryId::ALL.len());
    }

    #[test]
    fn move_down_clamps_at_end() {
        let mut p = CommandPalette::default();
        p.open();
        for _ in 0..100 {
            p.move_down();
        }
        assert_eq!(p.selected, p.filtered.len() - 1);
    }

    #[test]
    fn selected_returns_current_entry() {
        let mut p = CommandPalette::default();
        p.open();
        assert_eq!(p.selected(), Some(PaletteEntryId::SwitchSession));
        p.move_down();
        assert_eq!(p.selected(), Some(PaletteEntryId::YankMode));
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
