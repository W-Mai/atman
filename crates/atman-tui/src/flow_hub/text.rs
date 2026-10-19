//! Shared read-only text viewport for committed record details.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::keys::KeyAction;

pub(super) struct TextDetails {
    title: String,
    paragraphs: Vec<String>,
    pub scroll: usize,
    pub back: Rect,
    back_hovered: bool,
    width: Option<u16>,
    lines: Vec<String>,
}

impl TextDetails {
    pub fn new(title: String, paragraphs: Vec<String>) -> Self {
        Self {
            title,
            paragraphs,
            scroll: 0,
            back: Rect::default(),
            back_hovered: false,
            width: None,
            lines: Vec::new(),
        }
    }

    /// Return true only when the reader is explicitly closed.
    pub fn key(&mut self, key: KeyAction) -> bool {
        match key {
            KeyAction::Escape => return true,
            KeyAction::HistoryDown => self.scroll = self.scroll.saturating_add(1),
            KeyAction::HistoryUp => self.scroll = self.scroll.saturating_sub(1),
            KeyAction::PageDown => self.scroll = self.scroll.saturating_add(10),
            KeyAction::PageUp => self.scroll = self.scroll.saturating_sub(10),
            KeyAction::Home | KeyAction::CursorHome => self.scroll = 0,
            KeyAction::End | KeyAction::CursorEnd => self.scroll = usize::MAX,
            _ => {}
        }
        false
    }

    pub fn mouse(&mut self, event: MouseEvent) -> bool {
        self.back_hovered = self.back.contains((event.column, event.row).into());
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.back_hovered => true,
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp),
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.back = Rect::default();
        if area.width < 40 || area.height < 16 {
            frame.render_widget(Paragraph::new("Details need 40×16 · Esc back"), area);
            return;
        }
        let t = crate::theme::theme();
        let width = area.width.saturating_sub(4).min(100);
        let left = area.x + (area.width - width) / 2;
        let top = area.y + 1;
        let muted = Style::default().fg(t.meta_fg.into());
        frame.render_widget(
            Paragraph::new(self.title.as_str()).style(Style::default().fg(t.heading.into())),
            Rect::new(left, top, width, 1),
        );
        self.back = Rect::new(left, top + 2, 18, 3);
        frame.render_widget(
            Paragraph::new("\nBack Esc")
                .alignment(Alignment::Center)
                .style(muted.bg(if self.back_hovered {
                    t.work_bg.lerp(t.work_hover_bg, 0.5)
                } else {
                    t.work_bg.into_inner()
                })),
            self.back,
        );
        if self.width != Some(width) {
            self.lines = self
                .paragraphs
                .iter()
                .flat_map(|paragraph| paragraph.split('\n'))
                .flat_map(|line| crate::width::word_wrap(line, width.saturating_sub(4) as usize))
                .collect();
            self.width = Some(width);
        }
        let body = Rect::new(left, top + 6, width, area.height.saturating_sub(10));
        let inner = Rect::new(
            body.x + 2,
            body.y + 1,
            width - 4,
            body.height.saturating_sub(2),
        );
        self.scroll = self
            .scroll
            .min(self.lines.len().saturating_sub(inner.height as usize));
        frame.render_widget(Paragraph::new("").style(muted.bg(t.work_bg.into())), body);
        frame.render_widget(
            Paragraph::new(
                self.lines
                    .iter()
                    .skip(self.scroll)
                    .take(inner.height as usize)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .style(
                Style::default()
                    .fg(t.work_title_fg.into())
                    .bg(t.work_bg.into()),
            ),
            inner,
        );
        frame.render_widget(
            Paragraph::new(format!(
                "Lines {}–{} / {} · ↑↓ PgUp/Dn · Esc back",
                (self.scroll + 1).min(self.lines.len()),
                (self.scroll + inner.height as usize).min(self.lines.len()),
                self.lines.len()
            ))
            .style(muted),
            Rect::new(left, area.bottom() - 2, width, 1),
        );
    }
}
