use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Backspace,
    Delete,
    DeleteWordBackward,
    Submit,
    Newline,
    HistoryUp,
    HistoryDown,
    MoveItemUp,
    MoveItemDown,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Home,
    End,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    Quit,
    Interrupt,
    Escape,
    Tab,
    CyclePanelForward,
    CyclePanelBackward,
    BackTab,
    HelpModal,
    ToggleSidebar,
    ToggleMouseCapture,
    ToggleLastTool,
    ToggleLastWork,
    OpenCommandPalette,
    SearchHistory,
    NudgePrefill,
    CoursePrefill,
    RedirectPrefill,
    HardStop,
    PasteImage,
    RemoveAttachment,
    CycleReasoning,
    Ignore,
}

pub fn map(ev: KeyEvent) -> KeyAction {
    use KeyCode::*;
    if ev.code == Char('v') && ev.modifiers.contains(KeyModifiers::SUPER) {
        return KeyAction::PasteImage;
    }
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let shift = ev.modifiers.contains(KeyModifiers::SHIFT);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    match (ev.code, ctrl, shift, alt) {
        (Char('c'), true, _, _) => KeyAction::Interrupt,
        (Char('d'), true, _, _) => KeyAction::Quit,
        (Char('g'), true, _, _) => KeyAction::NudgePrefill,
        (Char('b'), true, _, false) => KeyAction::CoursePrefill,
        (Char('r'), true, _, _) => KeyAction::RedirectPrefill,
        (Char('x'), true, _, _) => KeyAction::HardStop,
        (Char('j'), true, _, _) => KeyAction::Newline,
        (Char('o'), true, _, _) => KeyAction::ToggleLastTool,
        (Char('o'), false, _, true) => KeyAction::ToggleLastWork,
        (Char('p'), true, _, _) => KeyAction::OpenCommandPalette,
        (Char('k'), true, _, _) => KeyAction::SearchHistory,
        (Char('w'), true, _, _) => KeyAction::DeleteWordBackward,
        (Char('v'), true, _, _) | (Char('v'), false, _, true) => KeyAction::PasteImage,
        (Char('t'), true, _, _) => KeyAction::CycleReasoning,
        (Delete, false, _, true) => KeyAction::RemoveAttachment,
        (Backspace, _, _, true) => KeyAction::DeleteWordBackward,
        (Char('a'), true, _, _) => KeyAction::CursorHome,
        (Char('e'), true, _, _) => KeyAction::CursorEnd,
        (Esc, _, _, _) => KeyAction::Escape,
        (F(1), _, _, _) => KeyAction::HelpModal,
        (F(2), _, _, _) => KeyAction::ToggleSidebar,
        (F(3), _, _, _) => KeyAction::ToggleMouseCapture,
        (Tab, _, _, _) => KeyAction::Tab,
        (BackTab, _, _, _) => KeyAction::BackTab,
        (Enter, _, true, _) => KeyAction::Newline,
        (Enter, _, _, _) => KeyAction::Submit,
        (Backspace, _, _, _) => KeyAction::Backspace,
        (Delete, _, _, _) => KeyAction::Delete,
        (Left, _, _, _) => KeyAction::CursorLeft,
        (Right, _, _, _) => KeyAction::CursorRight,
        (Up, false, _, true) => KeyAction::MoveItemUp,
        (Down, false, _, true) => KeyAction::MoveItemDown,
        (Up, _, _, _) => KeyAction::HistoryUp,
        (Down, _, _, _) => KeyAction::HistoryDown,
        (PageUp, _, _, _) => KeyAction::PageUp,
        (PageDown, _, _, _) => KeyAction::PageDown,
        (Home, _, _, _) => KeyAction::CursorHome,
        (End, _, _, _) => KeyAction::CursorEnd,
        (Char('\u{1b}'), _, _, _) => KeyAction::Escape,
        (Char('\u{7f}'), _, _, _) => KeyAction::Backspace,
        (Char('\u{8}'), _, _, _) => KeyAction::Backspace,
        (Char(c), false, _, _) => KeyAction::Char(c),
        _ => KeyAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventKind;

    fn ke(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::empty(),
        }
    }

    #[test]
    fn plain_char_maps_to_char() {
        assert_eq!(
            map(ke(KeyCode::Char('a'), KeyModifiers::NONE)),
            KeyAction::Char('a')
        );
    }

    #[test]
    fn enter_maps_to_submit() {
        assert_eq!(
            map(ke(KeyCode::Enter, KeyModifiers::NONE)),
            KeyAction::Submit
        );
    }

    #[test]
    fn ctrl_c_maps_to_interrupt() {
        assert_eq!(
            map(ke(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyAction::Interrupt
        );
    }

    #[test]
    fn ctrl_d_maps_to_quit() {
        assert_eq!(
            map(ke(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            KeyAction::Quit
        );
    }

    #[test]
    fn interjection_shortcuts_map() {
        assert_eq!(
            map(ke(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            KeyAction::NudgePrefill
        );
        assert_eq!(
            map(ke(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            KeyAction::CoursePrefill
        );
        assert_eq!(
            map(ke(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            KeyAction::RedirectPrefill
        );
        assert_eq!(
            map(ke(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            KeyAction::HardStop
        );
    }

    #[test]
    fn arrow_up_down_map_to_history() {
        assert_eq!(
            map(ke(KeyCode::Up, KeyModifiers::NONE)),
            KeyAction::HistoryUp
        );
        assert_eq!(
            map(ke(KeyCode::Down, KeyModifiers::NONE)),
            KeyAction::HistoryDown
        );
    }

    #[test]
    fn alt_arrow_up_down_map_to_item_reordering() {
        assert_eq!(
            map(ke(KeyCode::Up, KeyModifiers::ALT)),
            KeyAction::MoveItemUp
        );
        assert_eq!(
            map(ke(KeyCode::Down, KeyModifiers::ALT)),
            KeyAction::MoveItemDown
        );
    }

    #[test]
    fn pgup_pgdn_map_to_scroll() {
        assert_eq!(
            map(ke(KeyCode::PageUp, KeyModifiers::NONE)),
            KeyAction::PageUp
        );
        assert_eq!(
            map(ke(KeyCode::PageDown, KeyModifiers::NONE)),
            KeyAction::PageDown
        );
    }

    #[test]
    fn image_attachment_shortcuts_map() {
        assert_eq!(
            map(ke(KeyCode::Char('v'), KeyModifiers::CONTROL)),
            KeyAction::PasteImage
        );
        assert_eq!(
            map(ke(KeyCode::Char('v'), KeyModifiers::ALT)),
            KeyAction::PasteImage
        );
        assert_eq!(
            map(ke(KeyCode::Char('v'), KeyModifiers::SUPER)),
            KeyAction::PasteImage
        );
        assert_eq!(
            map(ke(KeyCode::Delete, KeyModifiers::ALT)),
            KeyAction::RemoveAttachment
        );
        assert_eq!(
            map(ke(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            KeyAction::CycleReasoning
        );
    }

    #[test]
    fn alt_o_toggles_the_latest_work_section() {
        assert_eq!(
            map(ke(KeyCode::Char('o'), KeyModifiers::ALT)),
            KeyAction::ToggleLastWork
        );
    }
}
