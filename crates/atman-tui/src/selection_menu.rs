use ratatui::layout::Rect;

use crate::selection::CopyPayload;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionAction {
    Copy,
    Quote,
    Cancel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionMenu {
    pub anchor: (u16, u16),
    pub selected: usize,
    pub hovered: Option<usize>,
    pub rect: Option<Rect>,
    pub item_rects: Vec<Rect>,
}

impl SelectionMenu {
    pub const ACTIONS: [SelectionAction; 3] = [
        SelectionAction::Copy,
        SelectionAction::Quote,
        SelectionAction::Cancel,
    ];

    pub fn new(column: u16, row: u16) -> Self {
        Self {
            anchor: (column, row),
            selected: 0,
            hovered: None,
            rect: None,
            item_rects: Vec::new(),
        }
    }

    pub fn focused_action(&self) -> SelectionAction {
        Self::ACTIONS[self
            .hovered
            .unwrap_or(self.selected)
            .min(Self::ACTIONS.len() - 1)]
    }

    pub fn move_previous(&mut self) {
        self.hovered = None;
        self.selected = self
            .selected
            .checked_sub(1)
            .unwrap_or(Self::ACTIONS.len() - 1);
    }

    pub fn move_next(&mut self) {
        self.hovered = None;
        self.selected = (self.selected + 1) % Self::ACTIONS.len();
    }

    pub fn update_hover(&mut self, column: u16, row: u16) -> bool {
        let next = self
            .item_rects
            .iter()
            .position(|rect| contains(*rect, column, row));
        if self.hovered == next {
            return false;
        }
        self.hovered = next;
        true
    }

    pub fn action_at(&self, column: u16, row: u16) -> Option<SelectionAction> {
        self.item_rects
            .iter()
            .position(|rect| contains(*rect, column, row))
            .map(|index| Self::ACTIONS[index])
    }

    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.rect.is_some_and(|rect| contains(rect, column, row))
    }
}

pub fn quote_payload(payload: &CopyPayload) -> String {
    let text = match payload {
        CopyPayload::Markdown(text) | CopyPayload::PlainText(text) | CopyPayload::Preview(text) => {
            text
        }
    };
    let quoted = text
        .lines()
        .map(|line| {
            if line.is_empty() {
                ">".to_string()
            } else {
                format!("> {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{quoted}\n\n")
}

fn contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_payload_prefixes_every_line() {
        assert_eq!(
            quote_payload(&CopyPayload::PlainText("one\n\ntwo".into())),
            "> one\n>\n> two\n\n"
        );
    }

    #[test]
    fn menu_navigation_wraps() {
        let mut menu = SelectionMenu::new(2, 3);
        menu.move_previous();
        assert_eq!(menu.focused_action(), SelectionAction::Cancel);
        menu.move_next();
        assert_eq!(menu.focused_action(), SelectionAction::Copy);
    }
}
