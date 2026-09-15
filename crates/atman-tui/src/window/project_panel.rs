use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use atman_runtime::project_catalog::ProjectRecord;

use crate::keys::KeyAction;
use crate::wm::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmCommand, WmEvent, WmEventResult,
};

const CARD_HEIGHT: u16 = 9;
const SESSION_ROW_HEIGHT: u16 = 4;
const WORKSPACE_MAX_WIDTH: u16 = 150;
const INSPECTOR_WIDTH: u16 = 38;

#[derive(Clone)]
struct ProjectSession {
    id: String,
    title: String,
    message_count: u64,
    updated_at: String,
    goal: Option<String>,
    is_current: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectView {
    Grid,
    Detail,
}

pub struct ProjectPanelContent {
    projects: Vec<ProjectRecord>,
    sessions: HashMap<String, Vec<ProjectSession>>,
    selected: usize,
    hovered: Option<usize>,
    card_rects: Vec<(usize, Rect)>,
    columns: usize,
    cards_per_page: usize,
    page: usize,
    previous_page_rect: Option<Rect>,
    next_page_rect: Option<Rect>,
    previous_page_hovered: bool,
    next_page_hovered: bool,
    view: ProjectView,
    session_selected: usize,
    session_hovered: Option<usize>,
    session_rects: Vec<(usize, Rect)>,
    sessions_per_page: usize,
    back_rect: Option<Rect>,
    back_hovered: bool,
    last_card_click: Option<(usize, Instant)>,
    last_session_click: Option<(usize, Instant)>,
}

impl ProjectPanelContent {
    pub fn new(projects: Vec<ProjectRecord>, session: Option<&atman_runtime::Session>) -> Self {
        Self {
            sessions: discover_sessions(session),
            projects,
            selected: 0,
            hovered: None,
            card_rects: Vec::new(),
            columns: 1,
            cards_per_page: 1,
            page: 0,
            previous_page_rect: None,
            next_page_rect: None,
            previous_page_hovered: false,
            next_page_hovered: false,
            view: ProjectView::Grid,
            session_selected: 0,
            session_hovered: None,
            session_rects: Vec::new(),
            sessions_per_page: 1,
            back_rect: None,
            back_hovered: false,
            last_card_click: None,
            last_session_click: None,
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
        self.page = self.selected / self.cards_per_page.max(1);
    }

    fn move_page(&mut self, delta: isize) {
        if self.projects.is_empty() {
            return;
        }
        let page_count = self.projects.len().div_ceil(self.cards_per_page.max(1));
        self.page = self
            .page
            .saturating_add_signed(delta)
            .min(page_count.saturating_sub(1));
        self.selected = (self.page * self.cards_per_page).min(self.projects.len() - 1);
    }

    fn open_detail(&mut self) {
        if self.projects.get(self.selected).is_some() {
            self.view = ProjectView::Detail;
            self.session_selected = 0;
            self.session_hovered = None;
        }
    }

    fn selected_sessions(&self) -> &[ProjectSession] {
        self.projects
            .get(self.selected)
            .and_then(|project| self.sessions.get(&project.fingerprint))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    fn render_grid(&mut self, area: Rect, frame: &mut Frame) {
        let t = crate::theme::theme();
        self.card_rects.clear();
        self.columns = if area.width >= 78 { 2 } else { 1 };
        let body_height = area.height;
        let visible_rows = usize::from(
            body_height
                .saturating_add(1)
                .checked_div(CARD_HEIGHT + 1)
                .unwrap_or(0)
                .max(1),
        );
        self.cards_per_page = (visible_rows * self.columns).max(1);
        let page_count = self.projects.len().div_ceil(self.cards_per_page).max(1);
        self.page = self.page.min(page_count - 1);
        if !self.projects.is_empty() && self.selected / self.cards_per_page != self.page {
            self.selected = (self.page * self.cards_per_page).min(self.projects.len() - 1);
        }

        if self.projects.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No projects registered yet. Start atman inside a project to add it.",
                )
                .style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(area.x, area.y, area.width, body_height),
            );
            return;
        }

        let gap = 1u16;
        let body_y = area.y;
        let columns = self.columns as u16;
        let card_w = area
            .width
            .saturating_sub(gap.saturating_mul(columns.saturating_sub(1)))
            / columns.max(1);
        let start = self.page * self.cards_per_page;
        let end = (start + self.cards_per_page).min(self.projects.len());

        for (visible_index, index) in (start..end).enumerate() {
            let project = &self.projects[index];
            let row = visible_index / self.columns;
            let column = visible_index % self.columns;
            let x = area.x + column as u16 * (card_w + gap);
            let y = body_y + row as u16 * (CARD_HEIGHT + gap);
            let rect = Rect::new(
                x,
                y,
                card_w,
                CARD_HEIGHT.min(area.bottom().saturating_sub(y)),
            );
            if rect.height < 3 {
                continue;
            }
            self.card_rects.push((index, rect));
            let selected = self.selected == index;
            let hovered = self.hovered == Some(index);
            let bg = if selected {
                t.modal_bg.lerp(t.accent, 0.22)
            } else if hovered {
                t.modal_bg.lerp(t.work_hover_bg, 0.24)
            } else {
                t.modal_bg.lerp(t.panel_bg, 0.14)
            };
            let marker = if selected { "▌ " } else { "  " };
            let marker_style = Style::default().fg(t.accent.into()).bg(bg);
            let (dot, state, state_color): (&str, &str, ratatui::style::Color) =
                if !project.path_available() {
                    ("○", "MISSING PATH", t.warn.into())
                } else if project.archived {
                    ("○", "ARCHIVED", t.warn.into())
                } else {
                    ("●", "AVAILABLE", t.success.into())
                };
            let pin = if project.pinned { "PINNED" } else { "" };
            let project_sessions = self.sessions.get(&project.fingerprint);
            let session_count = project_sessions.map_or(0, Vec::len);
            let scope = match atman_runtime::storage::load_storage_config(Some(&project.root))
                .scope
                .unwrap_or_default()
            {
                atman_runtime::storage::StorageScope::Global => "GLOBAL",
                atman_runtime::storage::StorageScope::Local => "LOCAL",
            };
            let summary = project_sessions
                .and_then(|sessions| sessions.first())
                .map(|session| session.goal.as_deref().unwrap_or(&session.title))
                .unwrap_or("No recent session activity");
            let inner_width = card_w.saturating_sub(4) as usize;
            let title = crate::width::truncate(
                &project.display_name,
                inner_width.saturating_sub(crate::width::width(pin) + usize::from(!pin.is_empty())),
            );
            let title_gap =
                inner_width.saturating_sub(crate::width::width(&title) + crate::width::width(pin));
            let content = vec![
                Line::from(Span::styled(marker, marker_style)),
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        title,
                        Style::default()
                            .fg(t.tinted_fg.into())
                            .bg(bg)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" ".repeat(title_gap), Style::default().bg(bg)),
                    Span::styled(pin, Style::default().fg(t.accent.into()).bg(bg)),
                ]),
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(dot, Style::default().fg(state_color).bg(bg)),
                    Span::styled(
                        format!(" {state}"),
                        Style::default().fg(t.subtle_fg.into()).bg(bg),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        crate::width::middle_truncate(
                            &project.root.display().to_string(),
                            inner_width,
                        ),
                        Style::default().fg(t.subtle_fg.into()).bg(bg),
                    ),
                ]),
                Line::from(Span::styled(marker, marker_style)),
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        crate::width::truncate(summary, inner_width),
                        Style::default().fg(t.tinted_fg.into()).bg(bg),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(marker, marker_style),
                    Span::styled(
                        crate::width::truncate(
                            &format!(
                                "{scope}  ·  {session_count} SESSIONS  ·  {}",
                                project.last_opened.format("%m-%d %H:%M")
                            ),
                            inner_width,
                        ),
                        Style::default().fg(t.subtle_fg.into()).bg(bg),
                    ),
                ]),
                Line::from(Span::styled(marker, marker_style)),
                Line::from(Span::styled(marker, marker_style)),
            ];
            frame.render_widget(Paragraph::new(content).style(Style::default().bg(bg)), rect);
        }
    }

    fn render_detail(&mut self, area: Rect, frame: &mut Frame) {
        let t = crate::theme::theme();
        self.session_rects.clear();
        self.back_rect = None;
        let Some(project) = self.projects.get(self.selected) else {
            self.view = ProjectView::Grid;
            return;
        };
        let sessions = self
            .sessions
            .get(&project.fingerprint)
            .cloned()
            .unwrap_or_default();
        self.session_selected = self.session_selected.min(sessions.len().saturating_sub(1));

        let back = Rect::new(area.x, area.y, 10.min(area.width), 1.min(area.height));
        self.back_rect = Some(back);
        let back_bg = if self.back_hovered {
            t.modal_bg.lerp(t.work_hover_bg, 0.72)
        } else {
            *t.modal_bg
        };
        frame.render_widget(
            Paragraph::new("‹ PROJECTS").style(Style::default().fg(t.accent.into()).bg(back_bg)),
            back,
        );
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    &project.display_name,
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    project.root.display().to_string(),
                    Style::default().fg(t.subtle_fg.into()),
                )),
                Line::from(Span::styled(
                    format!(
                        "{}  ·  {} sessions  ·  last opened {}",
                        if project.path_available() {
                            "● AVAILABLE"
                        } else {
                            "○ MISSING PATH"
                        },
                        sessions.len(),
                        project.last_opened.format("%Y-%m-%d %H:%M")
                    ),
                    Style::default().fg(t.subtle_fg.into()),
                )),
                Line::from(Span::styled(
                    format!("project id  {}", project.fingerprint),
                    Style::default().fg(t.subtle_fg.into()),
                )),
            ]),
            Rect::new(
                area.x,
                area.y.saturating_add(2),
                area.width,
                5.min(area.height.saturating_sub(2)),
            ),
        );

        let list_y = area.y.saturating_add(7);
        let list_h = area.bottom().saturating_sub(list_y);
        self.sessions_per_page =
            usize::from((list_h.saturating_sub(1) / SESSION_ROW_HEIGHT).max(1));
        let page = self.session_selected / self.sessions_per_page;
        let page_count = sessions.len().div_ceil(self.sessions_per_page).max(1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "SESSIONS",
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  PAGE {:02} / {:02}", page + 1, page_count),
                    Style::default().fg(t.subtle_fg.into()),
                ),
            ])),
            Rect::new(area.x, list_y, area.width, 1.min(list_h)),
        );
        let rows_y = list_y.saturating_add(1);
        if sessions.is_empty() {
            frame.render_widget(
                Paragraph::new("No sessions recorded for this project.")
                    .style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(
                    area.x,
                    rows_y,
                    area.width,
                    area.bottom().saturating_sub(rows_y),
                ),
            );
            return;
        }

        let start = page * self.sessions_per_page;
        let end = (start + self.sessions_per_page).min(sessions.len());
        for (visible, index) in (start..end).enumerate() {
            let row = &sessions[index];
            let y = rows_y + visible as u16 * SESSION_ROW_HEIGHT;
            let rect = Rect::new(
                area.x,
                y,
                area.width,
                SESSION_ROW_HEIGHT.min(area.bottom().saturating_sub(y)),
            );
            if rect.height == 0 {
                continue;
            }
            self.session_rects.push((index, rect));
            let selected = self.session_selected == index;
            let hovered = self.session_hovered == Some(index);
            let bg = if selected {
                t.modal_bg.lerp(t.accent, 0.22)
            } else if hovered {
                t.modal_bg.lerp(t.work_hover_bg, 0.24)
            } else {
                t.modal_bg.lerp(t.panel_bg, 0.14)
            };
            let marker = if selected { "▌ " } else { "  " };
            let marker_style = Style::default().fg(t.accent.into()).bg(bg);
            let current = if row.is_current { "  CURRENT" } else { "" };
            let goal = row.goal.as_deref().unwrap_or("No goal saved");
            let inner_width = area.width.saturating_sub(4) as usize;
            let meta = crate::width::truncate(
                &format!(
                    "{} messages  ·  {}  ·  {}",
                    row.message_count, row.updated_at, goal
                ),
                inner_width,
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(marker, marker_style)),
                    Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::styled(
                            crate::width::truncate(
                                &row.title,
                                area.width.saturating_sub(28) as usize,
                            ),
                            Style::default()
                                .fg(t.tinted_fg.into())
                                .bg(bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(current, Style::default().fg(t.accent.into()).bg(bg)),
                    ]),
                    Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::styled(meta, Style::default().fg(t.subtle_fg.into()).bg(bg)),
                    ]),
                    Line::from(Span::styled(marker, marker_style)),
                ])
                .style(Style::default().bg(bg)),
                rect,
            );
        }
    }

    fn render_header(&self, area: Rect, frame: &mut Frame) {
        let t = crate::theme::theme();
        let (eyebrow, title, subtitle) = match self.view {
            ProjectView::Grid => (
                format!("ALL WORKSPACES  /  {} KNOWN", self.projects.len()),
                "Projects".to_string(),
                "Resume work from a project, then choose its session.".to_string(),
            ),
            ProjectView::Detail => {
                let name = self
                    .projects
                    .get(self.selected)
                    .map(|project| project.display_name.as_str())
                    .unwrap_or("PROJECT");
                (
                    format!("PROJECT  /  {}", name.to_uppercase()),
                    "Sessions".to_string(),
                    format!("Choose a saved session in {name}."),
                )
            }
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    eyebrow,
                    Style::default().fg(t.subtle_fg.into()),
                )),
                Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    subtitle,
                    Style::default().fg(t.subtle_fg.into()),
                )),
            ])
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(t.border.into()))
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            area,
        );
    }

    fn render_inspector(&self, area: Rect, frame: &mut Frame) {
        let Some(project) = self.projects.get(self.selected) else {
            return;
        };
        let t = crate::theme::theme();
        let sessions = self
            .sessions
            .get(&project.fingerprint)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let recent = sessions.first();
        let scope = match atman_runtime::storage::load_storage_config(Some(&project.root))
            .scope
            .unwrap_or_default()
        {
            atman_runtime::storage::StorageScope::Global => "GLOBAL",
            atman_runtime::storage::StorageScope::Local => "LOCAL",
        };
        let state = if !project.path_available() {
            "○ MISSING PATH"
        } else if project.archived {
            "○ ARCHIVED"
        } else {
            "● AVAILABLE"
        };
        let content_width = area.width.saturating_sub(4) as usize;
        let summary = recent
            .map(|session| session.goal.as_deref().unwrap_or(&session.title))
            .unwrap_or("No recent session activity");
        let goal = recent
            .and_then(|session| session.goal.as_deref())
            .unwrap_or("No goal saved");
        let lines = vec![
            Line::from(Span::styled(
                "PROJECT",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(Span::styled(
                crate::width::truncate(&project.display_name, content_width),
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(state, Style::default().fg(t.accent.into()))),
            Line::raw(""),
            Line::from(Span::styled(
                "RECENT ACTIVITY",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(Span::styled(
                crate::width::truncate(summary, content_width),
                Style::default().fg(t.tinted_fg.into()),
            )),
            Line::from(Span::styled(
                recent.map_or("NO SESSION", |session| session.updated_at.as_str()),
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "SESSION CONTEXT  /  GOAL",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(Span::styled(
                crate::width::truncate(goal, content_width),
                Style::default().fg(t.tinted_fg.into()),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "PROJECT STORAGE",
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(vec![
                Span::styled(
                    scope,
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  EFFECTIVE SCOPE", Style::default().fg(t.subtle_fg.into())),
            ]),
            Line::raw(""),
            Line::from(Span::styled(
                match self.view {
                    ProjectView::Grid => "Enter open sessions  ·  double-click supported",
                    ProjectView::Detail => "Enter switch session  ·  Esc back to projects",
                },
                Style::default().fg(t.subtle_fg.into()),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(t.border.into()))
                        .padding(ratatui::widgets::Padding::new(2, 1, 1, 1)),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_footer(&mut self, area: Rect, frame: &mut Frame) {
        let t = crate::theme::theme();
        self.previous_page_rect = None;
        self.next_page_rect = None;
        let (status, hint, page, page_count, can_page) = match self.view {
            ProjectView::Grid => {
                let per_page = self.cards_per_page.max(1);
                let page_count = self.projects.len().div_ceil(per_page).max(1);
                let start = if self.projects.is_empty() {
                    0
                } else {
                    self.page * per_page + 1
                };
                let end = ((self.page + 1) * per_page).min(self.projects.len());
                (
                    format!(
                        "PROJECTS {:02}–{:02} OF {:02}   ·   PAGE {:02} / {:02}",
                        start,
                        end,
                        self.projects.len(),
                        self.page + 1,
                        page_count
                    ),
                    "ARROWS SELECT  ·  ENTER OPEN  ·  PGUP/PGDN OR WHEEL PAGE  ·  ESC CLOSE",
                    self.page,
                    page_count,
                    true,
                )
            }
            ProjectView::Detail => {
                let sessions = self.selected_sessions();
                let per_page = self.sessions_per_page.max(1);
                let page = self.session_selected / per_page;
                let page_count = sessions.len().div_ceil(per_page).max(1);
                (
                    format!(
                        "SESSIONS {:02}   ·   PAGE {:02} / {:02}",
                        sessions.len(),
                        page + 1,
                        page_count
                    ),
                    "↑↓ SELECT  ·  ENTER SWITCH  ·  PGUP/PGDN OR WHEEL PAGE  ·  ESC BACK",
                    page,
                    page_count,
                    false,
                )
            }
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    status,
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(hint, Style::default().fg(t.subtle_fg.into()))),
            ])
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(t.border.into()))
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            ),
            area,
        );
        if can_page && area.width >= 84 {
            let controls_y = area.y.saturating_add(1);
            let next = Rect::new(area.right().saturating_sub(9), controls_y, 9, 1);
            let previous = Rect::new(next.x.saturating_sub(10), controls_y, 9, 1);
            self.previous_page_rect = Some(previous);
            self.next_page_rect = Some(next);
            render_page_control(
                frame,
                previous,
                "‹ PREV",
                page > 0,
                self.previous_page_hovered,
                &t,
            );
            render_page_control(
                frame,
                next,
                "NEXT ›",
                page + 1 < page_count,
                self.next_page_hovered,
                &t,
            );
        }
    }

    fn switch_selected_session(&self) -> Vec<WmCommand> {
        let Some(project) = self.projects.get(self.selected) else {
            return Vec::new();
        };
        if !project.path_available() {
            return vec![WmCommand::PushToast(format!(
                "Project path is unavailable: {}",
                project.root.display()
            ))];
        }
        let Some(session) = self.selected_sessions().get(self.session_selected) else {
            return Vec::new();
        };
        vec![WmCommand::SwitchSession {
            sid: session.id.clone(),
            project_root: project.root.clone(),
        }]
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
        self.session_rects.clear();
        self.previous_page_rect = None;
        self.next_page_rect = None;
        self.back_rect = None;

        let workspace = centered_workspace(area);
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.border.into()))
                .style(Style::default().bg(t.code_bg.into()))
                .title(Span::styled(
                    " PROJECT HUB ",
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                )),
            workspace,
        );
        let inner = inset(workspace, 2, 1);
        if inner.width == 0 || inner.height == 0 {
            return Vec::new();
        }
        let header_height = inner.height.min(4);
        let footer_height = inner.height.saturating_sub(header_height).min(3);
        let header = Rect::new(inner.x, inner.y, inner.width, header_height);
        let footer = Rect::new(
            inner.x,
            inner.bottom().saturating_sub(footer_height),
            inner.width,
            footer_height,
        );
        let main = Rect::new(
            inner.x,
            header.bottom(),
            inner.width,
            footer.y.saturating_sub(header.bottom()),
        );
        self.render_header(header, frame);

        let (board, inspector) = if main.width >= 96 {
            let inspector_width = INSPECTOR_WIDTH.min(main.width / 3);
            (
                Rect::new(
                    main.x,
                    main.y,
                    main.width.saturating_sub(inspector_width + 1),
                    main.height,
                ),
                Some(Rect::new(
                    main.right().saturating_sub(inspector_width),
                    main.y,
                    inspector_width,
                    main.height,
                )),
            )
        } else {
            (main, None)
        };
        let board = inset(board, 1, 1);
        match self.view {
            ProjectView::Grid => self.render_grid(board, frame),
            ProjectView::Detail => self.render_detail(board, frame),
        }
        if let Some(inspector) = inspector {
            self.render_inspector(inspector, frame);
        }
        self.render_footer(footer, frame);
        Vec::new()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        let mut commands = Vec::new();
        match (self.view, event) {
            (ProjectView::Grid, WmEvent::Key(KeyAction::CursorLeft)) => self.move_selection(-1),
            (ProjectView::Grid, WmEvent::Key(KeyAction::CursorRight)) => self.move_selection(1),
            (ProjectView::Grid, WmEvent::Key(KeyAction::HistoryUp)) => {
                self.move_selection(-(self.columns as isize))
            }
            (ProjectView::Grid, WmEvent::Key(KeyAction::HistoryDown)) => {
                self.move_selection(self.columns as isize)
            }
            (ProjectView::Grid, WmEvent::Key(KeyAction::PageUp)) => self.move_page(-1),
            (ProjectView::Grid, WmEvent::Key(KeyAction::PageDown)) => self.move_page(1),
            (ProjectView::Grid, WmEvent::Key(KeyAction::Submit)) => self.open_detail(),
            (ProjectView::Detail, WmEvent::Key(KeyAction::Escape)) => self.view = ProjectView::Grid,
            (ProjectView::Detail, WmEvent::Key(KeyAction::HistoryUp)) => {
                self.session_selected = self.session_selected.saturating_sub(1)
            }
            (ProjectView::Detail, WmEvent::Key(KeyAction::HistoryDown)) => {
                self.session_selected = (self.session_selected + 1)
                    .min(self.selected_sessions().len().saturating_sub(1));
            }
            (ProjectView::Detail, WmEvent::Key(KeyAction::PageUp)) => {
                self.session_selected = self
                    .session_selected
                    .saturating_sub(self.sessions_per_page.max(1));
            }
            (ProjectView::Detail, WmEvent::Key(KeyAction::PageDown)) => {
                self.session_selected = (self.session_selected + self.sessions_per_page.max(1))
                    .min(self.selected_sessions().len().saturating_sub(1));
            }
            (ProjectView::Detail, WmEvent::Key(KeyAction::Submit)) => {
                commands = self.switch_selected_session()
            }
            (ProjectView::Grid, WmEvent::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollUp => self.move_page(-1),
                MouseEventKind::ScrollDown => self.move_page(1),
                MouseEventKind::Moved => {
                    self.hovered = hit_index(&self.card_rects, mouse.column, mouse.row);
                    self.previous_page_hovered = self
                        .previous_page_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                    self.next_page_hovered = self
                        .next_page_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if self
                        .previous_page_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row))
                    {
                        self.move_page(-1);
                    } else if self
                        .next_page_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row))
                    {
                        self.move_page(1);
                    } else if let Some(index) = hit_index(&self.card_rects, mouse.column, mouse.row)
                    {
                        let double_click = self.last_card_click.is_some_and(|(last, at)| {
                            last == index && at.elapsed() <= Duration::from_millis(500)
                        });
                        self.selected = index;
                        self.last_card_click = Some((index, Instant::now()));
                        if double_click {
                            self.open_detail();
                        }
                    }
                }
                _ => {}
            },
            (ProjectView::Detail, WmEvent::Mouse(mouse)) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.session_selected = self
                        .session_selected
                        .saturating_sub(self.sessions_per_page.max(1));
                }
                MouseEventKind::ScrollDown => {
                    self.session_selected = (self.session_selected + self.sessions_per_page.max(1))
                        .min(self.selected_sessions().len().saturating_sub(1));
                }
                MouseEventKind::Moved => {
                    self.session_hovered = hit_index(&self.session_rects, mouse.column, mouse.row);
                    self.back_hovered = self
                        .back_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if self
                        .back_rect
                        .is_some_and(|rect| contains(rect, mouse.column, mouse.row))
                    {
                        self.view = ProjectView::Grid;
                    } else if let Some(index) =
                        hit_index(&self.session_rects, mouse.column, mouse.row)
                    {
                        let double_click = self.last_session_click.is_some_and(|(last, at)| {
                            last == index && at.elapsed() <= Duration::from_millis(500)
                        });
                        self.session_selected = index;
                        self.last_session_click = Some((index, Instant::now()));
                        if double_click {
                            commands = self.switch_selected_session();
                        }
                    }
                }
                _ => {}
            },
            _ => return WmEventResult::Ignored,
        }
        WmEventResult::Consumed(commands)
    }

    fn preferred_size(&self, viewport: Rect) -> SizeHint {
        SizeHint {
            min: (48, 16),
            max: None,
            preferred: (viewport.width, viewport.height),
        }
    }

    fn title_suffix(&self) -> Option<String> {
        Some(match self.view {
            ProjectView::Grid => "Ctrl+L · Project Hub".into(),
            ProjectView::Detail => "Project details".into(),
        })
    }
}

fn discover_sessions(
    session: Option<&atman_runtime::Session>,
) -> HashMap<String, Vec<ProjectSession>> {
    let mut grouped: HashMap<String, Vec<ProjectSession>> = HashMap::new();
    let Some(session) = session else {
        return grouped;
    };
    let Some(sessions_root) = session.dir().parent() else {
        return grouped;
    };
    let current_id = session.id().to_string();
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return grouped;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(meta) = atman_runtime::session_meta::SessionMeta::load(&path) else {
            continue;
        };
        let Some(fingerprint) = meta.project_fingerprint else {
            continue;
        };
        let stats =
            atman_runtime::session_meta::SessionStats::load_or_rebuild(&path).unwrap_or_default();
        if stats.user_message_count == 0 {
            continue;
        }
        let id = entry.file_name().to_string_lossy().to_string();
        let updated_at = std::fs::metadata(path.join("events.jsonl"))
            .and_then(|metadata| metadata.modified())
            .or_else(|_| entry.metadata().and_then(|metadata| metadata.modified()))
            .ok()
            .map(|timestamp| {
                let local: chrono::DateTime<chrono::Local> = timestamp.into();
                local.format("%Y-%m-%d %H:%M").to_string()
            })
            .unwrap_or_else(|| "unknown".into());
        let goal = atman_runtime::memory::goal::GoalStore::at(&path).get().ok();
        grouped
            .entry(fingerprint)
            .or_default()
            .push(ProjectSession {
                title: meta
                    .title
                    .unwrap_or_else(|| format!("Session {}", &id[..id.len().min(8)])),
                message_count: stats.message_count,
                updated_at,
                goal,
                is_current: id == current_id,
                id,
            });
    }
    for sessions in grouped.values_mut() {
        sessions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
    }
    grouped
}

fn hit_index(rects: &[(usize, Rect)], x: u16, y: u16) -> Option<usize> {
    rects
        .iter()
        .find_map(|(index, rect)| contains(*rect, x, y).then_some(*index))
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn centered_workspace(area: Rect) -> Rect {
    let width = area.width.saturating_sub(6).min(WORKSPACE_MAX_WIDTH);
    let height = area.height.saturating_sub(4);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

fn inset(area: Rect, horizontal: u16, vertical: u16) -> Rect {
    Rect::new(
        area.x.saturating_add(horizontal),
        area.y.saturating_add(vertical),
        area.width.saturating_sub(horizontal.saturating_mul(2)),
        area.height.saturating_sub(vertical.saturating_mul(2)),
    )
}

fn render_page_control(
    frame: &mut Frame,
    rect: Rect,
    label: &str,
    enabled: bool,
    hovered: bool,
    theme: &crate::theme::Theme,
) {
    let bg = if hovered && enabled {
        theme.modal_bg.lerp(theme.work_hover_bg, 0.72)
    } else {
        *theme.modal_bg
    };
    frame.render_widget(
        Paragraph::new(label).style(
            Style::default()
                .fg(if enabled {
                    theme.tinted_fg.into()
                } else {
                    theme.subtle_fg.into()
                })
                .bg(bg),
        ),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn project(index: usize) -> ProjectRecord {
        ProjectRecord {
            fingerprint: format!("project-{index}"),
            root: std::path::PathBuf::from(format!("/missing/project-{index}")),
            display_name: format!("Project {index}"),
            pinned: false,
            archived: false,
            first_seen: Utc::now(),
            last_opened: Utc::now(),
        }
    }

    #[test]
    fn centered_workspace_retains_breathing_room() {
        let workspace = centered_workspace(Rect::new(0, 0, 180, 50));

        assert_eq!(workspace.width, WORKSPACE_MAX_WIDTH);
        assert_eq!(workspace.height, 46);
        assert_eq!(workspace.x, 15);
        assert_eq!(workspace.y, 2);
    }

    #[test]
    fn project_footer_always_shows_item_range_and_page_count() {
        let mut panel = ProjectPanelContent::new((0..13).map(project).collect(), None);
        panel.cards_per_page = 6;
        panel.page = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 3)).unwrap();

        terminal
            .draw(|frame| panel.render_footer(frame.area(), frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>();
        assert!(rendered.contains("PROJECTS 07–12 OF 13"), "{rendered}");
        assert!(rendered.contains("PAGE 02 / 03"), "{rendered}");
    }

    #[test]
    fn project_items_use_session_style_backgrounds_and_full_height_marker() {
        let mut panel = ProjectPanelContent::new((0..2).map(project).collect(), None);
        panel.hovered = Some(1);
        let theme = crate::theme::theme();
        let selected_background = theme.modal_bg.lerp(theme.accent, 0.22);
        let hovered_background = theme.modal_bg.lerp(theme.work_hover_bg, 0.24);
        let mut terminal = Terminal::new(TestBackend::new(90, CARD_HEIGHT)).unwrap();

        terminal
            .draw(|frame| panel.render_grid(frame.area(), frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let selected = panel.card_rects[0].1;
        let hovered = panel.card_rects[1].1;
        for y in selected.y..selected.bottom() {
            assert_eq!(buffer[(selected.x, y)].symbol(), "▌");
            for x in selected.x..selected.right() {
                assert_eq!(buffer[(x, y)].bg, selected_background);
            }
        }
        for y in hovered.y..hovered.bottom() {
            assert_eq!(buffer[(hovered.x, y)].symbol(), " ");
            for x in hovered.x..hovered.right() {
                assert_eq!(buffer[(x, y)].bg, hovered_background);
            }
        }
    }

    #[test]
    fn project_session_items_keep_the_selection_marker_for_every_row() {
        let mut panel = ProjectPanelContent::new(vec![project(0)], None);
        panel.sessions.insert(
            "project-0".into(),
            vec![ProjectSession {
                id: "session-0".into(),
                title: "Selected session".into(),
                message_count: 12,
                updated_at: "2026-09-15 16:20".into(),
                goal: Some("Keep the project context".into()),
                is_current: false,
            }],
        );
        let theme = crate::theme::theme();
        let selected_background = theme.modal_bg.lerp(theme.accent, 0.22);
        let mut terminal = Terminal::new(TestBackend::new(90, 12)).unwrap();

        terminal
            .draw(|frame| panel.render_detail(frame.area(), frame))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let selected = panel.session_rects[0].1;
        assert_eq!(selected.height, SESSION_ROW_HEIGHT);
        for y in selected.y..selected.bottom() {
            assert_eq!(buffer[(selected.x, y)].symbol(), "▌");
            for x in selected.x..selected.right() {
                assert_eq!(buffer[(x, y)].bg, selected_background);
            }
        }
    }
}
