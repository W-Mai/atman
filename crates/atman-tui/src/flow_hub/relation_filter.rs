//! Visible relation selection edits the same query used by graph and list views.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Clear, Paragraph},
};

use super::{FlowHub, FlowHubPage};
use crate::{keys::KeyAction, settings_form};

const CHOICES: [&str; 5] = ["All", "Created", "Read", "Watch", "Interject"];

pub(super) struct Picker {
    selected: usize,
    hovered: Option<usize>,
    hits: Vec<(Rect, usize)>,
    back: Rect,
    back_hovered: bool,
}

impl Picker {
    pub(super) fn clear_hits(&mut self) {
        self.hits.clear();
        self.back = Rect::default();
    }

    pub(super) fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let width = area.width.saturating_sub(4).min(44);
        let height = area.height.saturating_sub(2).min(20);
        let rect = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let theme = crate::theme::theme();
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(theme.work_bg.into())),
            rect,
        );
        frame.render_widget(
            Paragraph::new("Relations").style(Style::default().fg(theme.heading.into())),
            Rect::new(rect.x + 2, rect.y + 1, width.saturating_sub(18), 1),
        );
        self.back = Rect::new(rect.right() - 14, rect.y, 12, 3);
        settings_form::render_action(
            frame,
            self.back,
            &settings_form::Action {
                label: "Back",
                shortcut: "Esc",
                hovered: self.back_hovered,
                ..Default::default()
            },
            &theme,
        );
        for (index, label) in CHOICES.iter().enumerate() {
            let item = Rect::new(
                rect.x + 2,
                rect.y + 3 + index as u16 * 3,
                width.saturating_sub(4),
                3,
            );
            if item.bottom() >= rect.bottom() {
                break;
            }
            settings_form::render_action(
                frame,
                item,
                &settings_form::Action {
                    label,
                    focused: self.selected == index,
                    hovered: self.hovered == Some(index),
                    ..Default::default()
                },
                &theme,
            );
            self.hits.push((item, index));
        }
        frame.render_widget(
            Paragraph::new(if width < 40 {
                "↑↓ Tab · Enter · Esc"
            } else {
                "↑↓ / Tab · Enter apply · Esc back"
            })
            .style(Style::default().fg(theme.meta_fg.into())),
            Rect::new(rect.x + 2, rect.bottom() - 1, width.saturating_sub(4), 1),
        );
    }
}

impl FlowHub {
    pub(super) fn can_filter_relations(&self) -> bool {
        self.create_watch.is_none()
            && self.edit_management.is_none()
            && self.controls.is_none()
            && matches!(
                self.page,
                Some(
                    FlowHubPage::Flows { .. }
                        | FlowHubPage::Overview(_)
                        | FlowHubPage::Relations { .. }
                )
            )
    }

    pub(super) fn relation_filter_label(&self) -> &'static str {
        let query = self.query.to_lowercase();
        let filters: Vec<_> = query
            .split_whitespace()
            .filter_map(|term| term.strip_prefix("link:"))
            .collect();
        match filters.as_slice() {
            [] => "All",
            [kind] => CHOICES
                .iter()
                .copied()
                .skip(1)
                .find(|choice| choice.eq_ignore_ascii_case(kind))
                .unwrap_or("Custom"),
            _ => "Custom",
        }
    }

    pub(super) fn open_relation_filter(&mut self) {
        self.relation_picker = Some(Picker {
            selected: CHOICES
                .iter()
                .position(|choice| *choice == self.relation_filter_label())
                .unwrap_or(0),
            hovered: None,
            hits: Vec::new(),
            back: Rect::default(),
            back_hovered: false,
        });
    }

    fn apply_relation_filter(&mut self, selected: usize) {
        let key = self
            .filtered()
            .get(self.selected)
            .map(|index| self.rows[*index].key.clone());
        let mut terms: Vec<_> = self
            .query
            .split_whitespace()
            .filter(|term| !term.to_lowercase().starts_with("link:"))
            .map(str::to_owned)
            .collect();
        if selected > 0 {
            terms.push(format!("link:{}", CHOICES[selected].to_lowercase()));
        }
        self.query = terms.join(" ");
        self.selected = key
            .and_then(|key| {
                self.filtered()
                    .iter()
                    .position(|index| self.rows[*index].key == key)
            })
            .unwrap_or(0);
        self.relation_picker = None;
    }

    pub(super) fn relation_filter_key(&mut self, key: KeyAction) {
        let picker = self.relation_picker.as_mut().unwrap();
        match key {
            KeyAction::Escape | KeyAction::FilterRelations => self.relation_picker = None,
            KeyAction::HistoryUp | KeyAction::BackTab => {
                picker.selected = picker.selected.saturating_sub(1)
            }
            KeyAction::HistoryDown | KeyAction::Tab => {
                picker.selected = (picker.selected + 1).min(CHOICES.len() - 1)
            }
            KeyAction::Submit if self.pending.is_none() => {
                let selected = picker.selected;
                self.apply_relation_filter(selected);
            }
            _ => {}
        }
    }

    pub(super) fn relation_filter_mouse(&mut self, event: MouseEvent) {
        let picker = self.relation_picker.as_mut().unwrap();
        picker.back_hovered = picker.back.contains((event.column, event.row).into());
        picker.hovered = picker
            .hits
            .iter()
            .find(|(rect, _)| rect.contains((event.column, event.row).into()))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if picker.back_hovered => {
                self.relation_filter_key(KeyAction::Escape);
            }
            MouseEventKind::Down(MouseButton::Left) if self.pending.is_none() => {
                if let Some(index) = picker.hovered {
                    self.apply_relation_filter(index);
                }
            }
            MouseEventKind::ScrollUp => self.relation_filter_key(KeyAction::HistoryUp),
            MouseEventKind::ScrollDown => self.relation_filter_key(KeyAction::HistoryDown),
            _ => {}
        }
    }
}
