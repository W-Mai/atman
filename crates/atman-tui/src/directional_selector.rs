use ratatui::style::{Modifier, Style};

use crate::keys::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectorDirection {
    Previous,
    Next,
}

impl SelectorDirection {
    pub(crate) fn from_key(action: &KeyAction) -> Option<Self> {
        match action {
            KeyAction::CursorLeft => Some(Self::Previous),
            KeyAction::CursorRight => Some(Self::Next),
            _ => None,
        }
    }
}

pub(crate) fn move_wrapped(
    selected: &mut usize,
    item_count: usize,
    direction: SelectorDirection,
) -> bool {
    if item_count == 0 {
        *selected = 0;
        return false;
    }

    *selected = match direction {
        SelectorDirection::Previous => (*selected + item_count - 1) % item_count,
        SelectorDirection::Next => (*selected + 1) % item_count,
    };
    true
}

pub(crate) fn value_style(theme: &crate::theme::Theme, focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(theme.accent.into())
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.tinted_fg.into())
    }
}

pub(crate) fn footer_help(prefix: &str, suffix: &str) -> String {
    format!("{prefix}  ←/→:switch  {suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direction_keys_map_to_selector_direction() {
        assert_eq!(
            SelectorDirection::from_key(&KeyAction::CursorLeft),
            Some(SelectorDirection::Previous)
        );
        assert_eq!(
            SelectorDirection::from_key(&KeyAction::CursorRight),
            Some(SelectorDirection::Next)
        );
        assert_eq!(SelectorDirection::from_key(&KeyAction::Tab), None);
    }

    #[test]
    fn wrapped_movement_handles_edges_and_empty_items() {
        let mut selected = 0;
        assert!(move_wrapped(&mut selected, 3, SelectorDirection::Previous));
        assert_eq!(selected, 2);
        assert!(move_wrapped(&mut selected, 3, SelectorDirection::Next));
        assert_eq!(selected, 0);

        selected = 4;
        assert!(!move_wrapped(&mut selected, 0, SelectorDirection::Next));
        assert_eq!(selected, 0);
    }
}
