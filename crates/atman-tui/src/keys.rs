use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyAction {
    Char(char),
    Backspace,
    Delete,
    DeleteWordBackward,
    Submit,
    Save,
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
    ToggleMouseCapture,
    ToggleLastTool,
    ToggleLastWork,
    OpenCommandPalette,
    OpenProjectHub,
    OpenRelations,
    FilterRelations,
    ToggleActivity,
    ToggleGoal,
    SearchHistory,
    NudgePrefill,
    CoursePrefill,
    RedirectPrefill,
    HardStop,
    PasteImage,
    RemoveAttachment,
    ToggleQuote,
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
        (Char('s' | 'S'), true, _, _) | (Char('\u{13}'), _, _, _) => KeyAction::Save,
        (Char('d'), true, _, _) => KeyAction::Quit,
        (Char('g' | 'G'), true, true, _) => {
            if crate::panel_registry::EXTENDED {
                KeyAction::OpenRelations
            } else {
                KeyAction::Ignore
            }
        }
        (Char('g'), true, _, _) => KeyAction::NudgePrefill,
        (Char('b'), true, _, false) => KeyAction::CoursePrefill,
        (Char('r'), true, _, _) => KeyAction::RedirectPrefill,
        (Char('x'), true, _, _) => KeyAction::HardStop,
        (Char('j'), true, _, _) => KeyAction::Newline,
        (Char('o'), true, _, _) => KeyAction::ToggleLastTool,
        (Char('o'), false, _, true) => KeyAction::ToggleLastWork,
        (Char('p'), true, _, _) => KeyAction::OpenCommandPalette,
        (Char('l'), true, _, _) => KeyAction::OpenProjectHub,
        (Char('y'), true, _, _) => KeyAction::ToggleGoal,
        (Char('k'), true, _, _) => KeyAction::SearchHistory,
        (Char('f' | 'F'), true, _, _) => KeyAction::FilterRelations,
        (Char('w'), true, _, _) => KeyAction::DeleteWordBackward,
        (Char('u'), true, _, _) => KeyAction::RemoveAttachment,
        (Char('\u{15}'), _, _, _) => KeyAction::RemoveAttachment,
        (Char('h' | 'H'), true, _, _) => KeyAction::Backspace,
        (Char('v'), true, _, _) | (Char('v'), false, _, true) => KeyAction::PasteImage,
        (Char('t'), true, _, _) => KeyAction::CycleReasoning,
        (Backspace, _, _, true) => KeyAction::DeleteWordBackward,
        (Char('a' | 'A'), true, true, _) => KeyAction::ToggleActivity,
        (Char(']'), true, _, false) | (Char('\u{1d}'), _, _, _) => KeyAction::ToggleActivity,
        (Char('a'), true, _, _) => KeyAction::CursorHome,
        (Char('e'), true, _, _) => KeyAction::CursorEnd,
        (Esc, _, _, _) => KeyAction::Escape,
        (F(1), _, _, _) => KeyAction::HelpModal,
        (F(3), _, _, _) => KeyAction::ToggleMouseCapture,
        (F(4), _, _, _) => KeyAction::ToggleQuote,
        (Tab, true, true, _) | (BackTab, true, _, _) => KeyAction::CyclePanelBackward,
        (Tab, true, false, _) => KeyAction::CyclePanelForward,
        (Tab, _, _, _) => KeyAction::Tab,
        (BackTab, _, _, _) => KeyAction::BackTab,
        (Enter, _, true, _) => KeyAction::Newline,
        (Enter, _, _, _) => KeyAction::Submit,
        (Backspace, _, _, _) => KeyAction::Backspace,
        (Delete, _, _, _) => KeyAction::Delete,
        (Left, _, _, _) => KeyAction::CursorLeft,
        (Right, _, _, _) => KeyAction::CursorRight,
        (Up, true, _, false) => KeyAction::MoveItemUp,
        (Down, true, _, false) => KeyAction::MoveItemDown,
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
    fn control_s_maps_to_save_without_inserting_text() {
        for (code, modifiers) in [
            (KeyCode::Char('s'), KeyModifiers::CONTROL),
            (
                KeyCode::Char('S'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('\u{13}'), KeyModifiers::NONE),
        ] {
            assert_eq!(map(ke(code, modifiers)), KeyAction::Save);
        }
        assert_eq!(
            map(ke(KeyCode::Char('s'), KeyModifiers::NONE)),
            KeyAction::Char('s')
        );
    }

    #[test]
    fn ctrl_h_maps_to_backspace_for_remote_terminals() {
        assert_eq!(
            map(ke(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            KeyAction::Backspace
        );
        assert_eq!(
            map(ke(
                KeyCode::Char('H'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            KeyAction::Backspace
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
    fn control_arrow_up_down_map_to_item_reordering() {
        assert_eq!(
            map(ke(KeyCode::Up, KeyModifiers::CONTROL)),
            KeyAction::MoveItemUp
        );
        assert_eq!(
            map(ke(KeyCode::Down, KeyModifiers::CONTROL)),
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
            map(ke(KeyCode::Char('u'), KeyModifiers::CONTROL)),
            KeyAction::RemoveAttachment
        );
        assert_eq!(
            map(ke(KeyCode::Char('\u{15}'), KeyModifiers::NONE)),
            KeyAction::RemoveAttachment
        );
        assert_eq!(
            map(ke(KeyCode::Delete, KeyModifiers::ALT)),
            KeyAction::Delete
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

    #[test]
    fn ctrl_l_is_the_only_project_hub_shortcut() {
        assert_eq!(
            map(ke(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            KeyAction::OpenProjectHub
        );
        assert_eq!(
            map(ke(KeyCode::Char('p'), KeyModifiers::ALT)),
            KeyAction::Char('p')
        );
    }

    #[test]
    fn ctrl_shift_g_opens_flow_relations() {
        for key in ['f', 'F'] {
            assert_eq!(
                map(ke(KeyCode::Char(key), KeyModifiers::CONTROL)),
                KeyAction::FilterRelations
            );
        }
        for key in ['a', 'A'] {
            assert_eq!(
                map(ke(
                    KeyCode::Char(key),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                )),
                KeyAction::ToggleActivity
            );
        }
        assert_eq!(
            map(ke(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            KeyAction::CursorHome
        );
        assert_eq!(
            map(ke(
                KeyCode::Char('G'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )),
            if crate::panel_registry::EXTENDED {
                KeyAction::OpenRelations
            } else {
                KeyAction::Ignore
            }
        );
        assert_eq!(
            map(ke(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            if crate::panel_registry::EXTENDED {
                KeyAction::OpenRelations
            } else {
                KeyAction::Ignore
            }
        );
        assert_eq!(
            map(ke(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            KeyAction::NudgePrefill
        );
    }
}
