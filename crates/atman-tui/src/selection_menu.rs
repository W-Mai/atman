use ratatui::layout::Rect;

use crate::selection::CopyPayload;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionAction {
    Copy,
    RichCopy,
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
    pub const ACTIONS: [SelectionAction; 4] = [
        SelectionAction::Copy,
        SelectionAction::RichCopy,
        SelectionAction::Quote,
        SelectionAction::Cancel,
    ];
    pub const LABELS: [&'static str; 4] = ["Copy", "Rich Copy", "Quote", "Cancel"];

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

pub fn layout_menu(canvas: Rect, anchor: (u16, u16)) -> Option<(Rect, [Rect; 4])> {
    let widths = SelectionMenu::LABELS.map(|label| label.len() as u16 + 2);
    let width = widths.iter().copied().sum::<u16>().saturating_add(2);
    let height = 3;
    if canvas.width < width || canvas.height < height {
        return None;
    }

    let max_x = canvas.x.saturating_add(canvas.width - width);
    let max_y = canvas.y.saturating_add(canvas.height - height);
    let rect = Rect {
        x: anchor.0.saturating_add(1).clamp(canvas.x, max_x),
        y: anchor.1.saturating_add(1).clamp(canvas.y, max_y),
        width,
        height,
    };
    let mut item_x = rect.x.saturating_add(1);
    let item_rects = widths.map(|item_width| {
        let item_rect = Rect {
            x: item_x,
            y: rect.y.saturating_add(1),
            width: item_width,
            height: 1,
        };
        item_x = item_x.saturating_add(item_width);
        item_rect
    });
    Some((rect, item_rects))
}

pub fn rich_copy_payload(payload: &CopyPayload) -> (String, String) {
    let text = match payload {
        CopyPayload::Markdown(text) | CopyPayload::PlainText(text) | CopyPayload::Preview(text) => {
            text
        }
    };
    if !matches!(payload, CopyPayload::Markdown(_)) {
        return (format!("<p>{}</p>", html_escape(text)), text.clone());
    }

    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_MATH);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, pulldown_cmark::Parser::new_ext(text, options));
    let mut plain = String::new();
    for event in pulldown_cmark::Parser::new_ext(text, options) {
        match event {
            pulldown_cmark::Event::Text(value)
            | pulldown_cmark::Event::Code(value)
            | pulldown_cmark::Event::InlineMath(value)
            | pulldown_cmark::Event::DisplayMath(value) => plain.push_str(&value),
            pulldown_cmark::Event::SoftBreak | pulldown_cmark::Event::HardBreak => plain.push('\n'),
            pulldown_cmark::Event::TaskListMarker(checked) => {
                plain.push_str(if checked { "[x] " } else { "[ ] " });
            }
            pulldown_cmark::Event::End(
                pulldown_cmark::TagEnd::Paragraph
                | pulldown_cmark::TagEnd::Heading(_)
                | pulldown_cmark::TagEnd::Item
                | pulldown_cmark::TagEnd::TableRow,
            ) => {
                if !plain.ends_with('\n') {
                    plain.push('\n');
                }
            }
            _ => {}
        }
    }
    while plain.ends_with('\n') {
        plain.pop();
    }
    if html.is_empty() {
        html = format!("<p>{}</p>", html_escape(text));
    }
    (html, plain)
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn quote_text(text: &str) -> String {
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

pub fn quote_close_rect(area: Rect) -> Option<Rect> {
    (area.width >= 6 && area.height > 0).then(|| Rect {
        x: area.right() - 4,
        y: area.y,
        width: 3,
        height: 1,
    })
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
        assert_eq!(quote_text("one\n\ntwo"), "> one\n>\n> two\n\n");
    }

    #[test]
    fn quote_close_hitbox_stays_inside_the_displayed_card() {
        assert_eq!(
            quote_close_rect(Rect::new(10, 5, 20, 3)),
            Some(Rect::new(26, 5, 3, 1))
        );
        assert_eq!(quote_close_rect(Rect::new(10, 5, 5, 3)), None);
    }

    #[test]
    fn menu_navigation_wraps() {
        let mut menu = SelectionMenu::new(2, 3);
        menu.move_previous();
        assert_eq!(menu.focused_action(), SelectionAction::Cancel);
        menu.move_next();
        assert_eq!(menu.focused_action(), SelectionAction::Copy);
    }

    #[test]
    fn layout_clamps_menu_to_canvas() {
        let canvas = Rect::new(2, 3, 48, 5);
        let (rect, items) = layout_menu(canvas, (u16::MAX, u16::MAX)).unwrap();
        assert_eq!(rect.right(), canvas.right());
        assert_eq!(rect.bottom(), canvas.bottom());
        assert!(items.iter().all(|item| item.right() <= rect.right()));
    }

    #[test]
    fn rich_copy_preserves_structure_and_plain_text() {
        let (html, plain) = rich_copy_payload(&CopyPayload::Markdown(
            "**bold**\n\n[link](https://example.com)\n\n| A | B |\n|---|---|\n| 1 | 2 |".into(),
        ));
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<td>1</td>"));
        assert!(plain.contains("bold"));
        assert!(plain.contains("link"));
        assert!(!plain.contains("**"));
        assert!(!plain.contains("https://example.com"));
    }

    #[test]
    fn actions_include_markdown_and_rich_copy() {
        assert_eq!(SelectionMenu::ACTIONS[0], SelectionAction::Copy);
        assert_eq!(SelectionMenu::ACTIONS[1], SelectionAction::RichCopy);
        assert_eq!(SelectionMenu::LABELS[0], "Copy");
        assert_eq!(SelectionMenu::LABELS[1], "Rich Copy");
    }

    #[test]
    fn layout_returns_none_when_canvas_is_too_small() {
        assert!(layout_menu(Rect::new(0, 0, 10, 2), (0, 0)).is_none());
    }
}
