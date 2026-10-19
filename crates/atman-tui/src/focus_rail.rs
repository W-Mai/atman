//! View-owned overview/activity selection; execution remains with the host.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::{Clear, Paragraph},
};

#[derive(Default)]
pub(crate) struct FocusRail {
    pub activity: bool,
    hits: Vec<(Rect, bool)>,
    hovered: Option<bool>,
}

impl FocusRail {
    pub fn clear_hits(&mut self) {
        self.hits.clear();
    }

    pub fn content_area(area: Rect) -> Rect {
        let header = if area.width >= 24 { 4 } else { 7 };
        Rect::new(
            area.x,
            area.y + header.min(area.height),
            area.width,
            area.height.saturating_sub(header),
        )
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.clear_hits();
        let t = crate::theme::theme();
        for (index, activity) in [false, true].into_iter().enumerate() {
            let wide = area.width >= 24;
            let width = if wide {
                (area.width - 1) / 2
            } else {
                area.width
            };
            let rect = Rect::new(
                area.x + if wide { index as u16 * (width + 1) } else { 0 },
                area.y + if wide { 0 } else { index as u16 * 3 },
                width,
                3,
            )
            .intersection(area);
            if rect.height < 3 {
                continue;
            }
            let selected = activity == self.activity;
            let bg = if selected {
                t.work_bg.lerp(t.accent, 0.15)
            } else if self.hovered == Some(activity) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(format!(
                    "\n{}",
                    match (wide, activity) {
                        (true, false) => "Overview",
                        (true, true) if width >= 18 => "Activity  Ctrl⇧A",
                        (true, true) => "Activity",
                        (false, false) => "O",
                        (false, true) => "A",
                    }
                ))
                .alignment(Alignment::Center)
                .style(Style::default().fg(t.heading.into()).bg(bg)),
                rect,
            );
            if selected {
                frame.render_widget(
                    Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, 3),
                );
            }
            self.hits.push((rect, activity));
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Option<bool> {
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains((event.column, event.row).into()))
            .map(|(_, activity)| *activity);
        matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            .then_some(self.hovered)
            .flatten()
    }
}

impl crate::app::AppState {
    pub(crate) fn select_focus_rail(&mut self, activity: bool) {
        self.focus_rail.activity = activity;
        self.sidebar_collapsed = false;
        self.sidebar_upper_runtime_collapsed = false;
        self.sidebar_lower_runtime_collapsed = false;
        self.task_panel_runtime_collapsed = false;
        if activity {
            self.task_panel_collapsed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_and_conversation_keep_disjoint_bounds_and_full_button_targets() {
        for (x, y) in [(0, 0), (7, 6)] {
            for width in [40, 60, 100, 140, 200] {
                for height in [8, 24, 45, 80] {
                    for expanded in [false, true] {
                        let area = Rect::new(x, y, width, height);
                        let layout = crate::layout::FocusLayout::new(area, true, expanded);
                        let sidebar = layout.rail.unwrap();
                        assert_eq!(sidebar.intersection(area), sidebar);
                        assert_eq!(layout.conversation.intersection(area), layout.conversation);
                        assert!(sidebar.intersection(layout.conversation).is_empty());
                        let input = crate::layout::compute_input_rect(layout.conversation, 3);
                        assert!(sidebar.intersection(input).is_empty());
                        let mut rail = FocusRail::default();
                        let mut terminal = ratatui::Terminal::new(
                            ratatui::backend::TestBackend::new(area.right(), area.bottom()),
                        )
                        .unwrap();
                        terminal.draw(|frame| rail.render(frame, sidebar)).unwrap();
                        for (rect, activity) in rail.hits.clone() {
                            for row in rect.y..rect.bottom() {
                                for column in rect.x..rect.right() {
                                    assert_eq!(
                                        rail.mouse(MouseEvent {
                                            kind: MouseEventKind::Down(MouseButton::Left),
                                            row,
                                            column,
                                            modifiers: crossterm::event::KeyModifiers::NONE
                                        }),
                                        Some(activity)
                                    );
                                }
                            }
                            assert!(
                                rect.intersection(FocusRail::content_area(sidebar))
                                    .is_empty()
                            );
                        }
                        rail.clear_hits();
                        assert!(
                            rail.mouse(MouseEvent {
                                kind: MouseEventKind::Down(MouseButton::Left),
                                row: sidebar.y,
                                column: sidebar.x,
                                modifiers: crossterm::event::KeyModifiers::NONE
                            })
                            .is_none()
                        );
                    }
                }
            }
        }
    }
}
