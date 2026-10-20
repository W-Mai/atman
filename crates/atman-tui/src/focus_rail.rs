//! View-owned overview/activity selection; execution remains with the host.

use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::{Clear, Paragraph},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Select(bool),
    Back,
}

#[derive(Default)]
pub(crate) struct FocusRail {
    pub activity: bool,
    compact: bool,
    page_open: bool,
    hits: Vec<(Rect, Action)>,
    hovered: Option<Action>,
}

impl FocusRail {
    pub fn configure(&mut self, compact: bool) {
        self.compact = compact;
        if !compact {
            self.page_open = false;
        }
        self.clear_hits();
    }

    pub fn page_open(&self) -> bool {
        self.compact && self.page_open
    }

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
            } else if self.hovered == Some(Action::Select(activity)) {
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
            self.hits.push((rect, Action::Select(activity)));
        }
    }

    pub fn render_tabs(&mut self, frame: &mut Frame, area: Rect) {
        let t = crate::theme::theme();
        let actions: &[(Action, &str)] = if self.page_open() {
            &[
                (Action::Back, "Back Esc"),
                (Action::Select(false), "Overview"),
                (Action::Select(true), "Activity"),
            ]
        } else {
            &[
                (Action::Select(false), "Overview F2"),
                (Action::Select(true), "Activity Ctrl⇧A"),
            ]
        };
        let width = area.width.saturating_sub(actions.len() as u16 - 1) / actions.len() as u16;
        for (index, &(action, label)) in actions.iter().enumerate() {
            let rect = Rect::new(
                area.x + index as u16 * (width + 1),
                area.y,
                width,
                area.height.min(3),
            );
            let selected = self.page_open() && action == Action::Select(self.activity);
            let bg = if selected {
                t.work_bg.lerp(t.accent, 0.15)
            } else if self.hovered == Some(action) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(
                Paragraph::new(format!("\n{label}"))
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(t.heading.into()).bg(bg)),
                rect,
            );
            if selected {
                frame.render_widget(
                    Paragraph::new("▌\n▌\n▌").style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, rect.height),
                );
            }
            self.hits.push((rect, action));
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Option<Action> {
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
        self.focus_rail.page_open = self.focus_rail.compact;
        self.sidebar_collapsed = false;
        self.sidebar_upper_runtime_collapsed = false;
        self.sidebar_lower_runtime_collapsed = false;
        self.task_panel_runtime_collapsed = false;
        if activity {
            self.task_panel_collapsed = false;
        }
    }

    pub(crate) fn apply_focus_action(&mut self, action: Action) {
        match action {
            Action::Select(activity) => self.select_focus_rail(activity),
            Action::Back => self.focus_rail.page_open = false,
        }
    }

    pub(crate) fn toggle_focus_sidebar(&mut self) {
        if self.focus_rail.compact {
            if self.focus_rail.page_open() {
                self.apply_focus_action(Action::Back);
            } else {
                self.select_focus_rail(false);
            }
        } else {
            self.sidebar_collapsed = !self.sidebar_collapsed;
            self.sidebar_upper_runtime_collapsed = false;
            self.sidebar_lower_runtime_collapsed = false;
            self.task_panel_runtime_collapsed = false;
            self.save_ui_state();
        }
    }

    pub(crate) fn toggle_focus_activity(&mut self) {
        let activity =
            (self.focus_rail.compact && !self.focus_rail.page_open()) || !self.focus_rail.activity;
        self.select_focus_rail(activity);
    }

    /// Handle view navigation before window closure and composer editing.
    pub(crate) fn focus_page_event(&mut self, event: &Event) -> bool {
        use crate::keys::KeyAction;
        if !self.focus_rail.page_open() {
            return false;
        }
        if let Event::Key(key) = event {
            use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
            if key.kind != KeyEventKind::Press {
                return true;
            }
            if matches!(key.code, KeyCode::F(6 | 7 | 10))
                || (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return false;
            }
        }
        match event {
            Event::Key(key) => match crate::keys::map(*key) {
                KeyAction::Escape => self.apply_focus_action(Action::Back),
                KeyAction::Tab
                | KeyAction::BackTab
                | KeyAction::CursorLeft
                | KeyAction::CursorRight => self.select_focus_rail(!self.focus_rail.activity),
                KeyAction::ToggleSidebar
                | KeyAction::ToggleActivity
                | KeyAction::OpenCommandPalette
                | KeyAction::OpenProjectHub
                | KeyAction::OpenRelations
                | KeyAction::ToggleGoal
                | KeyAction::SearchHistory
                | KeyAction::Quit
                | KeyAction::Interrupt
                | KeyAction::HardStop
                | KeyAction::HelpModal
                | KeyAction::ToggleMouseCapture
                | KeyAction::CyclePanelForward
                | KeyAction::CyclePanelBackward => return false,
                KeyAction::HistoryUp | KeyAction::PageUp => {
                    self.goal_scroll = self.goal_scroll.saturating_sub(1);
                    self.plans_scroll = self.plans_scroll.saturating_sub(1);
                    self.todos_scroll = self.todos_scroll.saturating_sub(1);
                }
                KeyAction::HistoryDown | KeyAction::PageDown => {
                    self.goal_scroll = self.goal_scroll.saturating_add(1);
                    self.plans_scroll = self.plans_scroll.saturating_add(1);
                    self.todos_scroll = self.todos_scroll.saturating_add(1);
                }
                _ => {}
            },
            Event::Paste(_) => {}
            _ => return false,
        }
        true
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
                        let sidebar = layout.rail.or(layout.tabs).unwrap();
                        assert!(layout.conversation.width >= 40);
                        if layout.tabs.is_some() {
                            assert_eq!(layout.conversation.width, area.width);
                        }
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
                        terminal
                            .draw(|frame| {
                                if layout.tabs.is_some() {
                                    rail.render_tabs(frame, sidebar);
                                } else {
                                    rail.render(frame, sidebar);
                                }
                            })
                            .unwrap();
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
