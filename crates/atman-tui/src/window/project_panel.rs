use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use atman_runtime::project_catalog::ProjectRecord;

use crate::keys::KeyAction;
use crate::wm::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct ProjectPanelContent {
    projects: Vec<ProjectRecord>,
    selected: usize,
    hovered: Option<usize>,
    card_rects: Vec<Rect>,
    columns: usize,
}

impl ProjectPanelContent {
    pub fn new(projects: Vec<ProjectRecord>) -> Self {
        Self {
            projects,
            selected: 0,
            hovered: None,
            card_rects: Vec::new(),
            columns: 1,
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.projects.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.projects.len() - 1);
    }
}

impl WindowComponent for ProjectPanelContent {
    fn render_content(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        _ctx: &RenderCtx,
    ) -> Vec<HitRegion> {
        let t = crate::theme::theme();
        self.card_rects.clear();
        self.columns = if area.width >= 132 {
            3
        } else if area.width >= 84 {
            2
        } else {
            1
        };

        let header = Rect::new(area.x, area.y, area.width, 3.min(area.height));
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        "PROJECTS",
                        Style::default()
                            .fg(t.accent.into())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {} known", self.projects.len()),
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
                Line::from(Span::styled(
                    "↑↓←→ navigate  ·  mouse select  ·  Esc close",
                    Style::default().fg(t.subtle_fg.into()),
                )),
            ]),
            header,
        );

        if self.projects.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No projects registered yet. Start atman inside a project to add it.",
                )
                .style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(
                    area.x,
                    area.y.saturating_add(4),
                    area.width,
                    area.height.saturating_sub(4),
                ),
            );
            return Vec::new();
        }

        let gap = 1u16;
        let body_y = area.y.saturating_add(3);
        let body_h = area.height.saturating_sub(3);
        let columns = self.columns as u16;
        let card_w = area
            .width
            .saturating_sub(gap.saturating_mul(columns.saturating_sub(1)))
            / columns.max(1);
        let card_h = 8u16;

        for (index, project) in self.projects.iter().enumerate() {
            let row = index / self.columns;
            let column = index % self.columns;
            let x = area.x + column as u16 * (card_w + gap);
            let y = body_y + row as u16 * (card_h + gap);
            if y.saturating_add(card_h) > body_y.saturating_add(body_h) {
                break;
            }
            let rect = Rect::new(x, y, card_w, card_h);
            self.card_rects.push(rect);
            let selected = self.selected == index;
            let hovered = self.hovered == Some(index);
            let bg = if hovered {
                t.modal_bg.lerp(t.work_hover_bg, 0.72)
            } else {
                *t.modal_bg
            };
            let border = if selected {
                t.accent.into()
            } else {
                t.border.into()
            };
            let state = if !project.path_available() {
                "○ MISSING PATH"
            } else if project.archived {
                "○ ARCHIVED"
            } else {
                "● AVAILABLE"
            };
            let pin = if project.pinned { "  PINNED" } else { "" };
            let content = vec![
                Line::from(Span::styled(
                    crate::width::truncate(
                        &project.display_name,
                        card_w.saturating_sub(4) as usize,
                    ),
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!("{state}{pin}"),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    project.root.display().to_string(),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                )),
                Line::from(Span::styled(
                    format!(
                        "last opened  {}",
                        project.last_opened.format("%Y-%m-%d %H:%M")
                    ),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                )),
            ];
            frame.render_widget(
                Paragraph::new(content)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .border_style(Style::default().fg(border).bg(bg))
                            .style(Style::default().bg(bg)),
                    )
                    .wrap(Wrap { trim: true }),
                rect,
            );
        }
        Vec::new()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        match event {
            WmEvent::Key(KeyAction::CursorLeft) => self.move_selection(-1),
            WmEvent::Key(KeyAction::CursorRight) => self.move_selection(1),
            WmEvent::Key(KeyAction::HistoryUp) => self.move_selection(-(self.columns as isize)),
            WmEvent::Key(KeyAction::HistoryDown) => self.move_selection(self.columns as isize),
            WmEvent::Mouse(event) => {
                self.hovered = self.card_rects.iter().position(|rect| {
                    event.column >= rect.x
                        && event.column < rect.right()
                        && event.row >= rect.y
                        && event.row < rect.bottom()
                });
                if event.kind == MouseEventKind::Down(MouseButton::Left)
                    && let Some(index) = self.hovered
                {
                    self.selected = index;
                }
            }
            _ => return WmEventResult::Ignored,
        }
        WmEventResult::Consumed(Vec::new())
    }

    fn preferred_size(&self, viewport: Rect) -> SizeHint {
        SizeHint {
            min: (48, 16),
            max: None,
            preferred: (viewport.width, viewport.height),
        }
    }

    fn title_suffix(&self) -> Option<String> {
        Some("Alt+P · Project Hub".into())
    }
}
