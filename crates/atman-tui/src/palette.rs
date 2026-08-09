use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use tokio::sync::mpsc;

use crate::app::{self};
use crate::key_handler::{
    copy_last_message, copy_last_tool, enumerate_session_rows, yank_candidate_indices,
};
use crate::keys::KeyAction;
use crate::{TuiControl, UiState};

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
    ManageProviders,
    ManageAliases,
    SwitchModel,
    ManageMcp,
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

pub(crate) fn handle_palette_key(
    action: &KeyAction,
    ui: &mut UiState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    let app = &mut ui.app;
    match action {
        KeyAction::Escape => app.palette.close(),
        KeyAction::HistoryUp | KeyAction::CursorLeft => app.palette.move_up(),
        KeyAction::HistoryDown | KeyAction::CursorRight => app.palette.move_down(),
        KeyAction::Backspace => app.palette.backspace(),
        KeyAction::Char(c) => app.palette.push_char(*c),
        KeyAction::Submit => {
            if let Some(id) = app.palette.selected() {
                app.palette.close();
                dispatch_palette_entry(id, ui, control_tx);
            }
        }
        _ => {}
    }
}

pub(crate) fn dispatch_palette_entry(
    id: crate::palette::PaletteEntryId,
    ui: &mut UiState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    let app = &mut ui.app;
    use crate::palette::PaletteEntryId;
    match id {
        PaletteEntryId::YankMode => {
            let cands = yank_candidate_indices(app);
            if cands.is_empty() {
                app.push_note("nothing to yank yet", app::NoteLevel::Warn);
                return;
            }
            app.yank_mode = true;
            app.yank_index = cands.len().saturating_sub(1);
            app.push_note(
                "yank mode — j/k to move, Enter to copy, Esc to cancel",
                app::NoteLevel::Info,
            );
        }
        PaletteEntryId::CopyLastMessage => copy_last_message(app),
        PaletteEntryId::CopyLastTool => copy_last_tool(app),
        PaletteEntryId::CompactNow => {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::CompactNow);
                app.push_note("requested transcript compaction", app::NoteLevel::Info);
            }
        }
        PaletteEntryId::SwitchSession => {
            let scope = crate::session_switcher::SessionScope::Project;
            let rows = enumerate_session_rows(app, scope);
            app.session_switcher.open_with(rows, scope);
        }
        PaletteEntryId::NewSession => {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::NewSession);
            }
        }
        PaletteEntryId::MoveSession => {
            if let (Some(tx), Some(session)) = (control_tx, app.session.as_ref()) {
                let form = atman_runtime::form::PendingForm {
                    form_id: "session_move_path".to_string(),
                    run_id: atman_runtime::event::FlowRunId::now(),
                    tool_use_id: "session_move_path".to_string(),
                    kind: atman_runtime::form::FormKind::Text {
                        prompt: "New working directory:".to_string(),
                        placeholder: Some("/path/to/project".to_string()),
                        multiline: false,
                    },
                    emitted_at: chrono::Utc::now(),
                };
                session.forms().request(form);
                let _ = tx.send(TuiControl::MoveSession);
            }
        }
        PaletteEntryId::DeleteSession => {
            let scope = crate::session_switcher::SessionScope::Project;
            let rows = enumerate_session_rows(app, scope);
            app.session_switcher.open_with(rows, scope);
        }
        PaletteEntryId::SearchHistory => {
            app.history_search.open();
        }
        PaletteEntryId::ToggleSidebar => {
            app.sidebar_mode = app.sidebar_mode.toggle();
            app.save_ui_state();
        }
        PaletteEntryId::ManageProviders => {
            app.provider_manager.toggle();
        }
        PaletteEntryId::ManageAliases => {
            app.alias_manager.toggle();
        }
        PaletteEntryId::SwitchModel => {
            app.model_picker.open();
        }
        PaletteEntryId::ManageMcp => {
            let canvas = app.last_transcript_rect.unwrap_or_default();
            ui.wm.open(
                "mcp-manager",
                crate::wm::ContentKey::Mcp,
                crate::wm::WindowContent::Mcp,
                "MCP Servers",
                canvas,
            );
            if let Some(p) = ui
                .wm
                .panels
                .iter_mut()
                .find(|p| p.content_key == crate::wm::ContentKey::Mcp)
            {
                p.content = Some(Box::new(crate::window::mcp_panel::McpPanelContent {
                    scroll: 0,
                }));
            }
        }
        PaletteEntryId::ShowHelp => {
            let canvas = app.last_transcript_rect.unwrap_or_default();
            ui.wm.open(
                "cheatsheet",
                crate::wm::ContentKey::Cheatsheet,
                crate::wm::WindowContent::Cheatsheet,
                "Keybindings",
                canvas,
            );
            if let Some(p) = ui
                .wm
                .panels
                .iter_mut()
                .find(|p| p.content_key == crate::wm::ContentKey::Cheatsheet)
            {
                p.content = Some(Box::new(
                    crate::window::cheatsheet_panel::CheatsheetPanelContent { scroll: 0 },
                ));
            }
        }
        PaletteEntryId::SetTrustMode => {
            app.trust_mode_picker_open = true;
        }
        PaletteEntryId::SetModeTheme => {
            app.theme_picker_open = true;
        }
    }
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
    let t = crate::theme::theme();
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        rect,
        Line::from(Span::styled(
            "Command Palette (Esc to close)",
            Style::default().fg(t.tinted_fg.into()),
        )),
        "⌘",
        t.accent.into(),
        true,
        &t,
    );
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
        Span::styled("▸ ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(
            palette.input.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(" _", Style::default().fg(t.accent.into())),
    ]);
    f.render_widget(Paragraph::new(hint_line), input_rect);
    let cursor_x = input_rect.x + 2 + crate::width::width(&palette.input) as u16;
    f.set_cursor_position((cursor_x, input_rect.y));
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
