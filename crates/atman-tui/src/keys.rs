use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Backspace,
    Submit,
    HistoryUp,
    HistoryDown,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    Home,
    End,
    Quit,
    Interrupt,
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
    match (ev.code, ctrl, shift) {
        (Char('c'), true, _) => KeyAction::Interrupt,
        (Char('d'), true, _) => KeyAction::Quit,
        (Char('g'), true, _) => KeyAction::NudgePrefill,
        (Char('b'), true, _) => KeyAction::CoursePrefill,
        (Char('r'), true, _) => KeyAction::RedirectPrefill,
        (Char('x'), true, _) => KeyAction::HardStop,
        (Enter, _, _) => KeyAction::Submit,
        (Backspace, _, _) => KeyAction::Backspace,
        (Up, _, _) => KeyAction::HistoryUp,
        (Down, _, _) => KeyAction::HistoryDown,
        (PageUp, _, _) => KeyAction::PageUp,
        (PageDown, _, _) => KeyAction::PageDown,
        (Home, _, _) => KeyAction::Home,
        (End, _, _) => KeyAction::End,
        (Char(c), false, _) => KeyAction::Char(c),
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
