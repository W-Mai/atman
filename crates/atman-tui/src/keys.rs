use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Backspace,
    DeleteWordBackward,
    Submit,
    Newline,
    HistoryUp,
    HistoryDown,
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
    HelpModal,
    ToggleSidebar,
    ToggleLastTool,
    NudgePrefill,
    CoursePrefill,
    RedirectPrefill,
    HardStop,
    Ignore,
}

pub fn map(ev: KeyEvent) -> KeyAction {
    use KeyCode::*;
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
        (Char('w'), true, _, _) => KeyAction::DeleteWordBackward,
        (Backspace, _, _, true) => KeyAction::DeleteWordBackward,
        (Char('a'), true, _, _) => KeyAction::CursorHome,
        (Char('e'), true, _, _) => KeyAction::CursorEnd,
        (Esc, _, _, _) => KeyAction::Escape,
        (F(1), _, _, _) => KeyAction::HelpModal,
        (F(2), _, _, _) => KeyAction::ToggleSidebar,
        (Tab, _, _, _) => KeyAction::Tab,
        (Enter, _, true, _) => KeyAction::Newline,
        (Enter, _, _, _) => KeyAction::Submit,
        (Backspace, _, _, _) => KeyAction::Backspace,
        (Left, _, _, _) => KeyAction::CursorLeft,
        (Right, _, _, _) => KeyAction::CursorRight,
        (Up, _, _, _) => KeyAction::HistoryUp,
        (Down, _, _, _) => KeyAction::HistoryDown,
        (PageUp, _, _, _) => KeyAction::PageUp,
        (PageDown, _, _, _) => KeyAction::PageDown,
        (Home, _, _, _) => KeyAction::CursorHome,
        (End, _, _, _) => KeyAction::CursorEnd,
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
}
