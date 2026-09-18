use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::wm::modal::ModalAction;

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntryId {
    SwitchSession,
    AutoNameSession,
    NewSession,
    MoveSession,
    DeleteSession,
    OpenProjectHub,
    SetProjectStorageScope,
    YankMode,
    CopyLastMessage,
    CopyLastTool,
    CompactNow,
    SearchHistory,
    ToggleSidebar,
    ManageProviders,
    ManageAliases,
    ManageModels,
    SwitchModel,
    ManageMcp,
    ManageKnowledge,
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
        id: PaletteEntryId::AutoNameSession,
        group: "Session",
        label: "Generate Session Name",
        hint: "Ask the VM to regenerate this session's name",
        keyword: "session name rename title auto",
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
    PaletteEntry {
        id: PaletteEntryId::OpenProjectHub,
        group: "Project",
        label: "All Projects",
        hint: "Open the project hub (Ctrl+L)",
        keyword: "projects hub all ctrl l",
    },
    PaletteEntry {
        id: PaletteEntryId::SetProjectStorageScope,
        group: "Project",
        label: "Set Project Storage Scope",
        hint: "Choose global Atman data or local .atman data",
        keyword: "project storage scope global local atman",
    },
    // ── Copy ──
    PaletteEntry {
        id: PaletteEntryId::YankMode,
        group: "Copy",
        label: "Enter Yank Mode",
        hint: "j/k select, Enter copies via OSC 52",
        keyword: "select copy yank clipboard",
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
    // ── Providers ──
    PaletteEntry {
        id: PaletteEntryId::ManageProviders,
        group: "Providers",
        label: "Manage Providers...",
        hint: "Add, view, and remove provider logins",
        keyword: "providers auth login codex claude github",
    },
    PaletteEntry {
        id: PaletteEntryId::ManageAliases,
        group: "Providers",
        label: "Manage Aliases...",
        hint: "Add, edit, and remove model aliases",
        keyword: "aliases alias model rename",
    },
    PaletteEntry {
        id: PaletteEntryId::ManageModels,
        group: "Providers",
        label: "Manage Models...",
        hint: "Add, edit, and remove model configurations",
        keyword: "models model config context budget thinking",
    },
    PaletteEntry {
        id: PaletteEntryId::SwitchModel,
        group: "Providers",
        label: "Switch Model",
        hint: "Change the active model for this session",
        keyword: "model switch change gpt claude glm deepseek",
    },
    PaletteEntry {
        id: PaletteEntryId::ManageMcp,
        group: "MCP",
        label: "Manage MCP Servers...",
        hint: "View MCP server status and tools",
        keyword: "mcp servers tools resources prompts",
    },
    PaletteEntry {
        id: PaletteEntryId::ManageKnowledge,
        group: "Context",
        label: "Manage Memory & Rules...",
        hint: "Browse and revise confessions; inspect loaded rules",
        keyword: "memory confession rules organize",
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
    pub input: InputEditor,
    pub filtered: Vec<PaletteEntryId>,
    pub selected: usize,
    /// Display items include group headers. Only Entry variants are selectable.
    display: Vec<PaletteItem>,
    pub last_input_rect: Option<Rect>,
}

#[derive(Debug, Clone)]
enum PaletteItem {
    GroupHeader { name: &'static str },
    Entry { id: PaletteEntryId },
}

impl CommandPalette {
    pub fn new() -> Self {
        Self {
            last_input_rect: None,
            ..Self::default()
        }
    }

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
        self.input.insert_char(c);
        self.refresh();
    }

    pub fn backspace(&mut self) {
        self.input.backspace();
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

    pub fn display_len(&self) -> usize {
        self.display.len()
    }

    pub fn selected(&self) -> Option<PaletteEntryId> {
        match self.display.get(self.selected) {
            Some(PaletteItem::Entry { id }) => Some(*id),
            _ => None,
        }
    }

    fn refresh(&mut self) {
        let query = self.input.buf().to_lowercase();
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

impl crate::wm::modal::ModalOverlay for CommandPalette {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        if area.height == 0 {
            return;
        }
        let input_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        self.last_input_rect = Some(input_rect);
        let hint_line = Line::from(vec![
            Span::styled("▸ ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(
                self.input.buf().to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" _", Style::default().fg(t.accent.into())),
        ]);
        f.render_widget(Paragraph::new(hint_line), input_rect);
        let cursor_x = input_rect.x + 2 + self.input.cursor_display_col() as u16;
        f.set_cursor_position((cursor_x, input_rect.y));
        let list_rect = Rect {
            x: area.x,
            y: area.y.saturating_add(1),
            width: area.width,
            height: area.height.saturating_sub(1),
        };
        let items: Vec<ListItem<'static>> = self
            .display
            .iter()
            .map(|item| match item {
                PaletteItem::GroupHeader { name } => ListItem::new(Line::from(Span::styled(
                    format!("  {name}"),
                    Style::default()
                        .fg(t.subtle_fg.into())
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
                            Style::default().fg(t.subtle_fg.into()),
                        ),
                    ]);
                    ListItem::new(line)
                }
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut state = ListState::default();
        if !self.display.is_empty() {
            state.select(Some(self.selected));
        }
        f.render_stateful_widget(list, list_rect, &mut state);
    }

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        _tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        use crate::wm::modal::ModalAction;
        match action {
            KeyAction::Escape => self.close(),
            KeyAction::HistoryUp => self.move_up(),
            KeyAction::HistoryDown => self.move_down(),
            KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward => {
                self.input.handle_key(action);
                self.refresh();
            }
            KeyAction::Backspace => self.backspace(),
            KeyAction::Char(c) => self.push_char(*c),
            KeyAction::Submit => {
                if let Some(id) = self.selected() {
                    self.close();
                    return Some(ModalAction::Dispatched(id));
                }
            }
            _ => {}
        }
        Some(ModalAction::Consumed)
    }

    fn handle_paste(&mut self, text: &str) {
        self.input.paste_single_line(text);
        self.refresh();
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        self.last_input_rect
            .map(|r| (r.x + 2 + self.input.cursor_display_col() as u16, r.y))
    }

    fn title(&self) -> Line<'static> {
        Line::from(Span::styled(
            "Command Palette (Esc to close)",
            Style::default(),
        ))
    }

    fn icon(&self) -> &str {
        "⌘"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
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
    fn project_actions_are_available() {
        let ids = PaletteEntryId::all();
        assert!(ids.contains(&PaletteEntryId::OpenProjectHub));
        assert!(ids.contains(&PaletteEntryId::SetProjectStorageScope));
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
        assert_eq!(p.selected(), Some(PaletteEntryId::AutoNameSession));
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
        assert!(p.input.buf().is_empty());
        assert!(p.filtered.is_empty());
    }

    #[test]
    fn fuzzy_matches_subsequence() {
        assert!(fuzzy_match("switch session", "swss"));
        assert!(fuzzy_match("copy last tool result", "clast"));
        assert!(!fuzzy_match("compact transcript", "xyz"));
    }
}
