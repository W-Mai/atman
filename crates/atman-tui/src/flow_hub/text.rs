//! Shared read-only text viewport for committed record details.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::keys::KeyAction;
use crate::window::output_panel::preparation::{Key, Preparation, Rendered, Source};
use std::{sync::Arc, time::Instant};

pub(super) struct TextDetails {
    title: String,
    paragraphs: Arc<Vec<String>>,
    pub scroll: usize,
    pub back: Rect,
    back_hovered: bool,
    preparation: Preparation,
    retry: Rect,
    retry_hovered: bool,
    visible: bool,
    started: Instant,
}

impl TextDetails {
    pub fn new(title: String, paragraphs: Vec<String>) -> Self {
        Self {
            title,
            paragraphs: Arc::new(paragraphs),
            scroll: 0,
            back: Rect::default(),
            back_hovered: false,
            preparation: Default::default(),
            retry: Rect::default(),
            retry_hovered: false,
            visible: false,
            started: Instant::now(),
        }
    }

    /// Return true only when the reader is explicitly closed.
    pub fn key(&mut self, key: KeyAction) -> bool {
        match key {
            KeyAction::Char('r' | 'R') | KeyAction::RedirectPrefill
                if self.preparation.error().is_some() =>
            {
                self.preparation.retry()
            }
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
        self.retry_hovered = self.retry.contains((event.column, event.row).into());
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.retry_hovered => {
                self.preparation.retry();
                false
            }
            MouseEventKind::Down(MouseButton::Left) if self.back_hovered => true,
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp),
            _ => false,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        self.back = Rect::default();
        self.retry = Rect::default();
        self.visible = false;
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
        let body = Rect::new(left, top + 6, width, area.height.saturating_sub(10));
        let inner = Rect::new(
            body.x + 2,
            body.y + 1,
            width - 4,
            body.height.saturating_sub(2),
        );
        self.visible = !inner.is_empty();
        let key = Key {
            window: crate::wm::WindowId(0),
            width: inner.width,
            theme: t,
            preferences: crate::render_preferences::RenderPreferences::current(),
            revision: crate::app::OutputRevision {
                id: 1,
                ..Default::default()
            },
        };
        if self.visible {
            let small = self
                .paragraphs
                .iter()
                .try_fold(0usize, |size, paragraph| {
                    let size = size.saturating_add(paragraph.len()).saturating_add(1);
                    (size <= 1024).then_some(size)
                })
                .is_some();
            if small {
                if self
                    .preparation
                    .ready
                    .as_ref()
                    .is_none_or(|ready| ready.key != key)
                {
                    self.preparation
                        .set_ready_lines(key, wrap(&self.paragraphs, key.width));
                }
            } else {
                self.preparation.update_retaining(key, || {
                    let paragraphs = self.paragraphs.clone();
                    Source::Render {
                        surface: None,
                        operation: Box::new(move || {
                            Some(Rendered {
                                lines: wrap(&paragraphs, key.width),
                                paint: Default::default(),
                                regions: Arc::from([]),
                                inspector: None,
                                animated: false,
                            })
                        }),
                    }
                });
            }
        }
        frame.render_widget(Paragraph::new("").style(muted.bg(t.work_bg.into())), body);
        let mut total = 0;
        if let Some(ready) = self
            .preparation
            .ready
            .as_ref()
            .filter(|ready| ready.key == key)
        {
            total = ready.lines.len();
            self.scroll = self
                .scroll
                .min(total.saturating_sub(usize::from(inner.height)));
            frame.render_widget(
                Paragraph::new(
                    ready
                        .lines
                        .iter()
                        .skip(self.scroll)
                        .take(usize::from(inner.height))
                        .cloned()
                        .collect::<Vec<_>>(),
                )
                .style(
                    Style::default()
                        .fg(t.work_title_fg.into())
                        .bg(t.work_bg.into()),
                ),
                inner,
            );
        } else if self.is_pending() {
            frame.render_widget(
                Paragraph::new(crate::loading::line(
                    "Loading",
                    (self.started.elapsed().as_millis() / 100) as u32,
                    inner.width,
                )),
                inner,
            );
        }
        if let Some(error) = self.preparation.error() {
            frame.render_widget(Paragraph::new(error.to_owned()).style(muted), inner);
            self.retry = Rect::new(
                self.back.right() + 2,
                self.back.y,
                18.min(width.saturating_sub(20)),
                3,
            );
            crate::settings_form::render_action(
                frame,
                self.retry,
                &crate::settings_form::Action {
                    label: "Retry",
                    shortcut: "R",
                    hovered: self.retry_hovered,
                    ..Default::default()
                },
                &t,
            );
        }
        frame.render_widget(
            Paragraph::new(format!(
                "Lines {}–{} / {} · ↑↓ PgUp/Dn · Esc back",
                self.scroll.saturating_add(1).min(total),
                self.scroll.saturating_add(inner.height as usize).min(total),
                total
            ))
            .style(muted),
            Rect::new(left, area.bottom() - 2, width, 1),
        );
    }

    pub fn is_pending(&self) -> bool {
        self.visible && self.preparation.is_pending()
    }
}

fn wrap(paragraphs: &[String], width: u16) -> Arc<[ratatui::text::Line<'static>]> {
    paragraphs
        .iter()
        .flat_map(|paragraph| paragraph.split('\n'))
        .flat_map(|line| crate::width::word_wrap(line, usize::from(width)))
        .map(ratatui::text::Line::from)
        .collect::<Vec<_>>()
        .into()
}

impl Drop for TextDetails {
    fn drop(&mut self) {
        crate::render_work::discard(std::mem::take(&mut self.paragraphs));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use std::{sync::mpsc, time::Duration};

    fn draw(details: &mut TextDetails, terminal: &mut Terminal<TestBackend>) {
        terminal
            .draw(|frame| details.render(frame, frame.area()))
            .unwrap();
    }

    fn settle(details: &mut TextDetails, terminal: &mut Terminal<TestBackend>) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            draw(details, terminal);
            if !details.is_pending() {
                break;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn property_reader_keeps_input_and_source_independent_of_large_wrapping() {
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            let _ = wait.recv();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut details =
            TextDetails::new("Large".into(), vec!["Detail 汉 e\u{301}\n".repeat(10_000)]);
        let source = details.paragraphs.clone();
        details.preparation = Preparation::with_pool(pool);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        let started = Instant::now();
        draw(&mut details, &mut terminal);
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(details.is_pending());
        assert!(Arc::ptr_eq(&source, &details.paragraphs));
        details.key(KeyAction::CursorEnd);
        assert_eq!(details.scroll, usize::MAX);
        let mut tiny = Terminal::new(TestBackend::new(30, 10)).unwrap();
        draw(&mut details, &mut tiny);
        assert!(!details.is_pending(), "hidden feedback must not animate");
        release.send(()).unwrap();
        settle(&mut details, &mut terminal);
        let ready = details.preparation.ready.as_ref().unwrap();
        assert!(ready.text.is_none());
        assert_eq!(
            ready
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            wrap(&source, ready.key.width)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        );
        assert!(details.scroll > 0);
        let lines = ready.lines.clone();
        draw(&mut details, &mut terminal);
        assert!(Arc::ptr_eq(
            &lines,
            &details.preparation.ready.as_ref().unwrap().lines
        ));
        assert!(!details.is_pending());
        assert!(details.key(KeyAction::Escape));
    }

    #[test]
    fn property_reader_failure_has_keyboard_and_pointer_retry() {
        let mut details = TextDetails::new("Retry".into(), vec!["large text\n".repeat(200)]);
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        settle(&mut details, &mut terminal);
        let key = details.preparation.ready.as_ref().unwrap().key;
        for mouse in [false, true] {
            details.preparation.clear();
            details.preparation.update(key, || Source::Render {
                surface: None,
                operation: Box::new(|| None),
            });
            settle(&mut details, &mut terminal);
            assert!(details.preparation.error().is_some());
            assert!(!details.retry.is_empty());
            if mouse {
                let rect = details.retry;
                assert!(!details.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row: rect.y + 1,
                    modifiers: crossterm::event::KeyModifiers::NONE
                }));
            } else {
                details.key(KeyAction::Char('r'));
            }
            assert!(details.preparation.error().is_none());
            settle(&mut details, &mut terminal);
            assert!(details.preparation.ready.is_some());
        }
    }
}
