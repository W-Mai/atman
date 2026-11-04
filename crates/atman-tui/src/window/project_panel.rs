use std::collections::HashMap;

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use atman_runtime::project_catalog::ProjectRecord;

use crate::keys::KeyAction;
use crate::wm::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmCommand, WmEvent, WmEventResult,
};

const CARD_HEIGHT: u16 = 7;
const SESSION_ROW_HEIGHT: u16 = 4;
const PAGE_CONTROL_HEIGHT: u16 = 3;
const FOOTER_HEIGHT: u16 = PAGE_CONTROL_HEIGHT + 3;
const WORKSPACE_MAX_WIDTH: u16 = 150;
const INSPECTOR_WIDTH: u16 = 38;

use crate::project_hub::{ProjectDirectory, ProjectSession, SessionTarget};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProjectView {
    Grid,
    Detail,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectAction {
    Pin,
    Archive,
    Delete,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProjectConfirmation {
    fingerprint: String,
    action: ProjectAction,
}

pub struct ProjectPanelContent {
    projects: Vec<ProjectRecord>,
    pending: Option<uuid::Uuid>,
    error: Option<String>,
    refresh_rect: Option<Rect>,
    refresh_hovered: bool,
    sessions: HashMap<String, Vec<ProjectSession>>,
    details: HashMap<String, crate::project_hub::ProjectDetails>,
    current_project_fingerprint: Option<String>,
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
    action_rects: Vec<(ProjectAction, Rect)>,
    hovered_action: Option<ProjectAction>,
    confirmation: Option<ProjectConfirmation>,
    view: ProjectView,
    session_selected: usize,
    session_hovered: Option<usize>,
    session_rects: Vec<(usize, Rect)>,
    sessions_per_page: usize,
    back_rect: Option<Rect>,
    back_hovered: bool,
}

impl ProjectPanelContent {
    pub fn new(projects: Vec<ProjectRecord>, current_project_fingerprint: Option<String>) -> Self {
        Self::new_selected(projects, current_project_fingerprint, None)
    }

    pub fn new_selected(
        projects: Vec<ProjectRecord>,
        current_project_fingerprint: Option<String>,
        selected_fingerprint: Option<&str>,
    ) -> Self {
        let selected = selected_fingerprint
            .and_then(|fingerprint| {
                projects
                    .iter()
                    .position(|project| project.fingerprint == fingerprint)
            })
            .unwrap_or_default();
        Self {
            sessions: HashMap::new(),
            details: HashMap::new(),
            pending: None,
            error: None,
            refresh_rect: None,
            refresh_hovered: false,
            projects,
            current_project_fingerprint,
            selected,
            hovered: None,
            card_rects: Vec::new(),
            columns: 1,
            cards_per_page: 1,
            page: 0,
            previous_page_rect: None,
            next_page_rect: None,
            previous_page_hovered: false,
            next_page_hovered: false,
            action_rects: Vec::new(),
            hovered_action: None,
            confirmation: None,
            view: ProjectView::Grid,
            session_selected: 0,
            session_hovered: None,
            session_rects: Vec::new(),
            sessions_per_page: 1,
            back_rect: None,
            back_hovered: false,
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
        self.confirmation = None;
        self.page = self.selected / self.cards_per_page.max(1);
    }

    fn move_page(&mut self, delta: isize) {
        if self.view == ProjectView::Detail {
            self.session_selected = self
                .session_selected
                .saturating_add_signed(delta * self.sessions_per_page.max(1) as isize)
                .min(self.selected_sessions().len().saturating_sub(1));
            return;
        }
        if self.projects.is_empty() {
            return;
        }
        let page_count = self.projects.len().div_ceil(self.cards_per_page.max(1));
        self.page = self
            .page
            .saturating_add_signed(delta)
            .min(page_count.saturating_sub(1));
        self.selected = (self.page * self.cards_per_page).min(self.projects.len() - 1);
        self.confirmation = None;
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

    fn selected_project_is_current(&self) -> bool {
        self.projects.get(self.selected).is_some_and(|project| {
            self.current_project_fingerprint.as_deref() == Some(project.fingerprint.as_str())
        })
    }

    fn project_action(&mut self, action: ProjectAction) -> Vec<WmCommand> {
        if self.pending.is_some() {
            return Vec::new();
        }
        let Some(project) = self.projects.get(self.selected).cloned() else {
            return Vec::new();
        };
        match action {
            ProjectAction::Pin => {
                self.confirmation = None;
                vec![WmCommand::MutateProject(
                    crate::ProjectMutation::SetPinned {
                        fingerprint: project.fingerprint,
                        pinned: !project.pinned,
                    },
                )]
            }
            ProjectAction::Archive if project.archived => {
                self.confirmation = None;
                vec![WmCommand::MutateProject(
                    crate::ProjectMutation::SetArchived {
                        fingerprint: project.fingerprint,
                        archived: false,
                    },
                )]
            }
            ProjectAction::Archive => {
                let confirmation = ProjectConfirmation {
                    fingerprint: project.fingerprint.clone(),
                    action,
                };
                if self.confirmation.as_ref() == Some(&confirmation) {
                    self.confirmation = None;
                    vec![WmCommand::MutateProject(
                        crate::ProjectMutation::SetArchived {
                            fingerprint: project.fingerprint,
                            archived: true,
                        },
                    )]
                } else {
                    self.confirmation = Some(confirmation);
                    Vec::new()
                }
            }
            ProjectAction::Delete if !project.archived => {
                self.confirmation = None;
                vec![WmCommand::PushToast(
                    "Archive this project before deleting it".into(),
                )]
            }
            ProjectAction::Delete if self.selected_project_is_current() => {
                self.confirmation = None;
                vec![WmCommand::PushToast(
                    "Switch away before deleting the active project".into(),
                )]
            }
            ProjectAction::Delete => {
                let confirmation = ProjectConfirmation {
                    fingerprint: project.fingerprint.clone(),
                    action,
                };
                if self.confirmation.as_ref() == Some(&confirmation) {
                    self.confirmation = None;
                    vec![WmCommand::MutateProject(crate::ProjectMutation::Delete {
                        fingerprint: project.fingerprint,
                    })]
                } else {
                    self.confirmation = Some(confirmation);
                    Vec::new()
                }
            }
        }
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
        self.selected = self.selected.min(self.projects.len().saturating_sub(1));
        self.page = self.selected / self.cards_per_page;

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
            let (dot, state, state_color): (&str, &str, ratatui::style::Color) = if !self
                .details
                .get(&project.fingerprint)
                .is_some_and(|detail| detail.path_available)
            {
                ("○", "MISSING PATH", t.warn.into())
            } else if project.archived {
                ("○", "ARCHIVED", t.warn.into())
            } else {
                ("●", "AVAILABLE", t.success.into())
            };
            let pin = if project.pinned { "PINNED" } else { "" };
            let project_sessions = self.sessions.get(&project.fingerprint);
            let session_count = project_sessions.map_or(0, Vec::len);
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
                            &format!("{dot} {state}  ·  {session_count} sessions"),
                            inner_width,
                        ),
                        Style::default().fg(state_color).bg(bg),
                    ),
                ]),
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

        let back = Rect::new(area.x, area.y, 16.min(area.width), 3.min(area.height));
        self.back_rect = Some(back);
        render_page_control(frame, back, "‹ Projects", true, self.back_hovered, &t);
        let list_y = area.y.saturating_add(4);
        let list_h = area.bottom().saturating_sub(list_y);
        self.sessions_per_page =
            usize::from((list_h.saturating_sub(1) / SESSION_ROW_HEIGHT).max(1));
        let page = self.session_selected / self.sessions_per_page;
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "SESSIONS",
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {}", sessions.len()),
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
            let inner_width = area.width.saturating_sub(4) as usize;
            let title = crate::width::truncate(
                &row.title,
                inner_width.saturating_sub(crate::width::width(current)),
            );
            let title_gap = inner_width
                .saturating_sub(crate::width::width(&title) + crate::width::width(current));
            let meta = crate::width::truncate(
                &format!("{}  ·  {}", row.activity, row.updated_at),
                inner_width,
            );
            frame.render_widget(
                Paragraph::new(vec![
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

    fn render_header(&mut self, area: Rect, frame: &mut Frame) {
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
                    self.projects
                        .get(self.selected)
                        .map(|project| project.root.display().to_string())
                        .unwrap_or_default(),
                )
            }
        };
        let subtitle = self.error.clone().unwrap_or_else(|| {
            if self.pending.is_some() {
                "Loading project directory…".into()
            } else {
                subtitle
            }
        });
        let refresh = Rect::new(
            area.right().saturating_sub(15),
            area.y,
            15.min(area.width),
            3.min(area.height),
        );
        self.refresh_rect = Some(refresh);
        let text_area = Rect::new(area.x, area.y, area.width.saturating_sub(16), area.height);
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
                    crate::width::middle_truncate(
                        &subtitle,
                        text_area.width.saturating_sub(2) as usize,
                    ),
                    Style::default().fg(t.subtle_fg.into()),
                )),
            ])
            .block(
                crate::surface::block(crate::surface::Role::ProjectRail)
                    .padding(ratatui::widgets::Padding::new(1, 1, 0, 1)),
            ),
            text_area,
        );
        render_page_control(
            frame,
            refresh,
            "Refresh  R",
            self.pending.is_none(),
            self.refresh_hovered,
            &t,
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
        let recent = match self.view {
            ProjectView::Grid => sessions.first(),
            ProjectView::Detail => sessions.get(self.session_selected),
        };
        let scope = match self
            .details
            .get(&project.fingerprint)
            .and_then(|detail| detail.scope)
        {
            Some(atman_runtime::storage::StorageScope::Global) => "GLOBAL",
            Some(atman_runtime::storage::StorageScope::Local) => "LOCAL",
            None => "UNKNOWN",
        };
        let state = if !self
            .details
            .get(&project.fingerprint)
            .is_some_and(|detail| detail.path_available)
        {
            "○ MISSING PATH"
        } else if project.archived {
            "○ ARCHIVED"
        } else {
            "● AVAILABLE"
        };
        let content_width = area.width.saturating_sub(4) as usize;
        let summary = recent
            .map(|session| {
                if self.view == ProjectView::Detail {
                    session.activity.as_str()
                } else {
                    session.goal.as_deref().unwrap_or(&session.title)
                }
            })
            .unwrap_or("No recent session activity");
        let goal = recent
            .and_then(|session| session.goal.as_deref())
            .unwrap_or("No goal saved");
        let lines = vec![
            Line::from(Span::styled(
                if self.view == ProjectView::Detail {
                    "SESSION"
                } else {
                    "PROJECT"
                },
                Style::default().fg(t.subtle_fg.into()),
            )),
            Line::from(Span::styled(
                crate::width::truncate(
                    if self.view == ProjectView::Detail {
                        recent.map_or("No session selected", |session| session.title.as_str())
                    } else {
                        &project.display_name
                    },
                    content_width,
                ),
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(state, Style::default().fg(t.accent.into()))),
            Line::raw(""),
            Line::from(Span::styled(
                if self.view == ProjectView::Detail {
                    "ACTIVITY"
                } else {
                    "RECENT ACTIVITY"
                },
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
                "GOAL",
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
            Line::from(vec![Span::styled(
                scope,
                Style::default()
                    .fg(t.accent.into())
                    .add_modifier(Modifier::BOLD),
            )]),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    crate::surface::block(crate::surface::Role::ProjectRail)
                        .padding(ratatui::widgets::Padding::new(3, 1, 1, 1)),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    fn render_footer(&mut self, area: Rect, frame: &mut Frame) {
        let t = crate::theme::theme();
        self.previous_page_rect = None;
        self.next_page_rect = None;
        self.action_rects.clear();
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(
            crate::surface::block(crate::surface::Role::ProjectRail),
            area,
        );
        let controls = area.height >= FOOTER_HEIGHT;
        let summary = if controls {
            Rect::new(
                area.x,
                area.y + PAGE_CONTROL_HEIGHT,
                area.width,
                area.height - PAGE_CONTROL_HEIGHT,
            )
        } else {
            area
        };
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
                    "ARROWS SELECT  ·  ENTER OPEN  ·  ESC CLOSE",
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
                    "↑↓ SELECT  ·  ENTER OPEN  ·  ESC PROJECTS",
                    page,
                    page_count,
                    true,
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
                crate::surface::block(crate::surface::Role::ProjectRail)
                    .padding(ratatui::widgets::Padding::new(1, 1, 1, 0)),
            ),
            summary,
        );
        if controls && let Some(project) = self.projects.get(self.selected) {
            let confirmed = |action| {
                self.confirmation.as_ref().is_some_and(|confirmation| {
                    confirmation.fingerprint == project.fingerprint && confirmation.action == action
                })
            };
            let controls_y = area.y;
            let mut x = area.x.saturating_add(1);
            let current = self.selected_project_is_current();
            let controls = [
                (
                    ProjectAction::Pin,
                    if project.pinned {
                        "[P] Unpin"
                    } else {
                        "[P] Pin"
                    },
                    10,
                    true,
                ),
                (
                    ProjectAction::Archive,
                    if confirmed(ProjectAction::Archive) {
                        "[X] Confirm"
                    } else if project.archived {
                        "[X] Unarchive"
                    } else {
                        "[X] Archive"
                    },
                    14,
                    true,
                ),
                (
                    ProjectAction::Delete,
                    if confirmed(ProjectAction::Delete) {
                        "[Del] Confirm"
                    } else {
                        "[Del] Delete"
                    },
                    14,
                    project.archived && !current,
                ),
            ];
            for (action, label, width, enabled) in controls {
                let rect = Rect::new(x, controls_y, width, PAGE_CONTROL_HEIGHT);
                if rect.right() >= area.right() {
                    break;
                }
                self.action_rects.push((action, rect));
                render_project_action(
                    frame,
                    rect,
                    label,
                    enabled,
                    self.hovered_action == Some(action),
                    confirmed(action),
                    &t,
                );
                x = rect.right().saturating_add(1);
            }
        }
        if can_page && page_count > 1 && area.width >= 84 && controls {
            let controls_y = area.y;
            let next = Rect::new(
                area.right().saturating_sub(10),
                controls_y,
                9,
                PAGE_CONTROL_HEIGHT,
            );
            let previous = Rect::new(
                next.x.saturating_sub(10),
                controls_y,
                9,
                PAGE_CONTROL_HEIGHT,
            );
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
        let Some(session) = self.selected_sessions().get(self.session_selected) else {
            return Vec::new();
        };
        match &session.target {
            SessionTarget::Legacy(id) => vec![WmCommand::SwitchSession {
                sid: id.clone(),
                project_root: project.root.clone(),
            }],
            SessionTarget::Persistent(id) => vec![WmCommand::OpenSession(*id)],
            SessionTarget::Indexed(source) => vec![WmCommand::BrowseIndexedHistory(source.clone())],
        }
    }
}

impl WindowComponent for ProjectPanelContent {
    fn begin_project_request(&mut self) -> Option<uuid::Uuid> {
        if self.pending.is_some() {
            return None;
        }
        let id = uuid::Uuid::now_v7();
        self.pending = Some(id);
        self.error = None;
        Some(id)
    }

    fn show_project_sessions(&mut self, fingerprint: &str) {
        if let Some(selected) = self
            .projects
            .iter()
            .position(|p| p.fingerprint == fingerprint)
        {
            self.selected = selected;
            self.page = selected / self.cards_per_page.max(1);
            self.session_selected = 0;
            self.view = ProjectView::Detail;
            self.confirmation = None;
        }
    }

    fn project_result(
        &mut self,
        request: uuid::Uuid,
        result: &Result<ProjectDirectory, String>,
    ) -> bool {
        if self.pending != Some(request) {
            return false;
        }
        self.pending = None;
        match result {
            Err(error) => self.error = Some(error.clone()),
            Ok(directory) => {
                let project = self
                    .projects
                    .get(self.selected)
                    .map(|p| p.fingerprint.clone());
                let session = self
                    .selected_sessions()
                    .get(self.session_selected)
                    .map(|s| s.target.clone());
                self.projects = directory.projects.clone();
                self.sessions = directory.sessions.clone();
                self.details = directory.details.clone();
                self.selected = project
                    .and_then(|id| self.projects.iter().position(|p| p.fingerprint == id))
                    .unwrap_or(self.selected.min(self.projects.len().saturating_sub(1)));
                self.page = self.selected / self.cards_per_page.max(1);
                self.session_selected = session
                    .and_then(|target| {
                        self.selected_sessions()
                            .iter()
                            .position(|s| s.target == target)
                    })
                    .unwrap_or(
                        self.session_selected
                            .min(self.selected_sessions().len().saturating_sub(1)),
                    );
                self.confirmation = None;
                self.error = None;
            }
        }
        true
    }

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
            crate::surface::block(crate::surface::Role::ProjectRail)
                .padding(ratatui::widgets::Padding::uniform(1)),
            workspace,
        );
        let inner = inset(workspace, 2, 1);
        if inner.width == 0 || inner.height == 0 {
            return Vec::new();
        }
        let header_height = inner.height.min(4);
        let footer_height = inner
            .height
            .saturating_sub(header_height)
            .min(FOOTER_HEIGHT);
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

        if let Some(error) = &self.error {
            frame.render_widget(
                Paragraph::new(format!(
                    "Could not refresh project directory\n\n{error}\n\nRefresh  R"
                ))
                .style(Style::default().fg(t.subtle_fg.into()))
                .wrap(Wrap { trim: false }),
                inset(main, 1, 1),
            );
            return Vec::new();
        }

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
        if let WmEvent::Mouse(mouse) = event {
            self.hovered_action = hit_action(&self.action_rects, mouse.column, mouse.row);
            self.previous_page_hovered = self
                .previous_page_rect
                .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
            self.next_page_hovered = self
                .next_page_rect
                .is_some_and(|rect| contains(rect, mouse.column, mouse.row));
            self.refresh_hovered = self
                .refresh_rect
                .is_some_and(|r| contains(r, mouse.column, mouse.row));
        }
        let refresh = matches!(event, WmEvent::Key(KeyAction::Char('r' | 'R')))
            || matches!(event, WmEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) && self.refresh_hovered);
        if refresh {
            return WmEventResult::Consumed(if self.pending.is_none() {
                vec![WmCommand::RefreshProjects]
            } else {
                vec![]
            });
        }
        if self.error.is_some() && !matches!(event, WmEvent::Key(KeyAction::Escape)) {
            return WmEventResult::Consumed(vec![]);
        }
        let project_action = match event {
            WmEvent::Key(KeyAction::Char('p' | 'P')) => Some(ProjectAction::Pin),
            WmEvent::Key(KeyAction::Char('x' | 'X')) => Some(ProjectAction::Archive),
            WmEvent::Key(KeyAction::Delete | KeyAction::Backspace) => Some(ProjectAction::Delete),
            WmEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                self.hovered_action
            }
            _ => None,
        };
        if let Some(action) = project_action {
            return WmEventResult::Consumed(self.project_action(action));
        }
        if matches!(event, WmEvent::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left))
        {
            if self.previous_page_hovered {
                self.move_page(-1);
                return WmEventResult::Consumed(vec![]);
            }
            if self.next_page_hovered {
                self.move_page(1);
                return WmEventResult::Consumed(vec![]);
            }
        }
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
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) = hit_index(&self.card_rects, mouse.column, mouse.row) {
                        self.selected = index;
                        self.confirmation = None;
                        self.open_detail();
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
                        self.session_selected = index;
                        commands = self.switch_selected_session();
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

fn hit_index(rects: &[(usize, Rect)], x: u16, y: u16) -> Option<usize> {
    rects
        .iter()
        .find_map(|(index, rect)| contains(*rect, x, y).then_some(*index))
}

fn hit_action(rects: &[(ProjectAction, Rect)], x: u16, y: u16) -> Option<ProjectAction> {
    rects
        .iter()
        .find_map(|(action, rect)| contains(*rect, x, y).then_some(*action))
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
    frame.render_widget(ratatui::widgets::Clear, rect);
    let bg = if hovered && enabled {
        theme.modal_bg.lerp(theme.work_hover_bg, 0.72)
    } else {
        *theme.modal_bg
    };
    frame.render_widget(
        Paragraph::new(vec![Line::raw(""), Line::raw(label), Line::raw("")])
            .alignment(ratatui::layout::Alignment::Center)
            .style(
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

fn render_project_action(
    frame: &mut Frame,
    rect: Rect,
    label: &str,
    enabled: bool,
    hovered: bool,
    confirmed: bool,
    theme: &crate::theme::Theme,
) {
    frame.render_widget(ratatui::widgets::Clear, rect);
    let bg = if confirmed {
        theme.modal_bg.lerp(theme.warn, 0.24)
    } else if hovered && enabled {
        theme.modal_bg.lerp(theme.work_hover_bg, 0.72)
    } else {
        *theme.modal_bg
    };
    let fg = if confirmed {
        theme.warn
    } else if enabled {
        theme.tinted_fg
    } else {
        theme.subtle_fg
    };
    frame.render_widget(
        Paragraph::new(vec![Line::raw(""), Line::raw(label), Line::raw("")])
            .alignment(ratatui::layout::Alignment::Center)
            .style(Style::default().fg(fg.into()).bg(bg)),
        rect,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use crossterm::event::{KeyModifiers, MouseEvent};
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

    fn dispatch(panel: &mut ProjectPanelContent, event: WmEvent) -> Vec<WmCommand> {
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        match panel.handle_event(&event, &mut ctx) {
            WmEventResult::Consumed(commands) => commands,
            WmEventResult::Ignored => panic!("event was ignored"),
        }
    }

    #[test]
    fn project_refresh_preserves_navigation_and_rejects_stale_replies() {
        let target = uuid::Uuid::new_v4();
        let mut panel = ProjectPanelContent::new((0..13).map(project).collect(), None);
        panel.cards_per_page = 6;
        panel.selected = 7;
        panel.page = 1;
        panel.view = ProjectView::Detail;
        let row = ProjectSession {
            target: SessionTarget::Persistent(target),
            title: "Retained".into(),
            activity: "2 flows".into(),
            updated_at: "Now".into(),
            goal: None,
            is_current: false,
        };
        panel.sessions.insert("project-7".into(), vec![row.clone()]);
        let request = panel.begin_project_request().unwrap();
        assert!(panel.begin_project_request().is_none());
        let mut directory = ProjectDirectory {
            projects: panel.projects.clone(),
            sessions: [("project-7".into(), vec![row])].into(),
            ..Default::default()
        };
        directory.projects.swap(0, 7);
        assert!(!panel.project_result(uuid::Uuid::new_v4(), &Ok(directory.clone())));
        assert!(panel.project_result(request, &Ok(directory)));
        assert_eq!(panel.selected, 0);
        assert!(panel.view == ProjectView::Detail);
        assert!(
            matches!(panel.switch_selected_session().as_slice(), [WmCommand::OpenSession(id)] if *id == target)
        );
        assert!(!panel.project_result(request, &Err("stale".into())));
        let request = panel.begin_project_request().unwrap();
        assert!(panel.project_result(request, &Err("retry this request".into())));
        assert_eq!(panel.error.as_deref(), Some("retry this request"));
        assert_eq!(panel.selected_sessions().len(), 1);
        panel.selected = 7;
        panel.page = 1;
        panel.cards_per_page = 6;
        let mut terminal = Terminal::new(TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| panel.render_grid(frame.area(), frame))
            .unwrap();
        assert_eq!(panel.selected, 7, "reflow must retain the selected project");
        assert_eq!(panel.page, panel.selected / panel.cards_per_page);
    }

    #[test]
    fn project_refresh_has_matching_keyboard_and_mouse_actions() {
        let mut panel = ProjectPanelContent::new(vec![project(0)], None);
        let mut terminal = Terminal::new(TestBackend::new(80, 4)).unwrap();
        terminal
            .draw(|frame| panel.render_header(frame.area(), frame))
            .unwrap();
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        let rect = panel.refresh_rect.unwrap();
        for event in [
            WmEvent::Key(KeyAction::Char('r')),
            WmEvent::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
        ] {
            assert!(
                matches!(panel.handle_event(&event, &mut ctx), WmEventResult::Consumed(commands) if matches!(commands.as_slice(), [WmCommand::RefreshProjects]))
            );
        }
        panel.begin_project_request();
        assert!(
            matches!(panel.handle_event(&WmEvent::Key(KeyAction::Char('r')), &mut ctx), WmEventResult::Consumed(commands) if commands.is_empty())
        );
        assert!(panel.project_action(ProjectAction::Pin).is_empty());
    }

    #[test]
    fn session_activation_uses_the_selected_project_for_enter_and_one_click() {
        let mut panel = ProjectPanelContent::new(vec![project(0), project(1)], None);
        let targets = [uuid::Uuid::now_v7(), uuid::Uuid::now_v7()];
        for (index, target) in targets.iter().enumerate() {
            panel.sessions.insert(
                format!("project-{index}"),
                vec![ProjectSession {
                    target: SessionTarget::Persistent(*target),
                    title: format!("Session {index}"),
                    activity: "Idle".into(),
                    updated_at: "Now".into(),
                    goal: None,
                    is_current: index == 0,
                }],
            );
        }
        panel.move_selection(1);
        panel.open_detail();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| panel.render_detail(frame.area(), frame))
            .unwrap();
        let key = dispatch(&mut panel, WmEvent::Key(KeyAction::Submit));
        assert!(matches!(key.as_slice(), [WmCommand::OpenSession(id)] if *id == targets[1]));
        let (_, rect) = panel.session_rects[0];
        for row in rect.y..rect.bottom() {
            let click = dispatch(
                &mut panel,
                WmEvent::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 2,
                    row,
                    modifiers: KeyModifiers::NONE,
                }),
            );
            assert!(matches!(click.as_slice(), [WmCommand::OpenSession(id)] if *id == targets[1]));
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
    fn project_page_controls_use_three_row_hit_targets() {
        let mut panel = ProjectPanelContent::new((0..13).map(project).collect(), None);
        panel.cards_per_page = 6;
        panel.page = 1;
        panel.previous_page_hovered = true;
        let theme = crate::theme::theme();
        let hover_background = theme.modal_bg.lerp(theme.work_hover_bg, 0.72);
        let mut terminal = Terminal::new(TestBackend::new(100, FOOTER_HEIGHT)).unwrap();

        terminal
            .draw(|frame| panel.render_footer(frame.area(), frame))
            .unwrap();

        let previous = panel.previous_page_rect.unwrap();
        let next = panel.next_page_rect.unwrap();
        assert_eq!(previous.height, PAGE_CONTROL_HEIGHT);
        assert_eq!(next.height, PAGE_CONTROL_HEIGHT);
        assert_eq!(next.right() + 1, terminal.backend().buffer().area.right());
        let buffer = terminal.backend().buffer();
        let status = (0..buffer.area.width)
            .map(|x| buffer[(x, previous.bottom() + 1)].symbol())
            .collect::<String>();
        assert!(status.contains("PROJECTS 07–12 OF 13"));
        assert!((0..buffer.area.width).all(|x| buffer[(x, previous.bottom())].symbol() == " "));
        for y in previous.y..previous.bottom() {
            for x in previous.x..previous.right() {
                assert_eq!(buffer[(x, y)].bg, hover_background);
            }
        }
        assert!((previous.x..previous.right()).all(|x| buffer[(x, previous.y)].symbol() == " "));
        assert!(
            (previous.x..previous.right())
                .all(|x| buffer[(x, previous.bottom() - 1)].symbol() == " ")
        );
    }

    #[test]
    fn project_actions_support_keyboard_and_archive_gates_delete() {
        let mut panel = ProjectPanelContent::new(vec![project(0)], None);

        let commands = dispatch(&mut panel, WmEvent::Key(KeyAction::Char('p')));
        assert!(matches!(
            commands.as_slice(),
            [WmCommand::MutateProject(crate::ProjectMutation::SetPinned {
                fingerprint,
                pinned: true
            })] if fingerprint == "project-0"
        ));

        assert!(dispatch(&mut panel, WmEvent::Key(KeyAction::Char('x'))).is_empty());
        let commands = dispatch(&mut panel, WmEvent::Key(KeyAction::Char('x')));
        assert!(matches!(
            commands.as_slice(),
            [WmCommand::MutateProject(crate::ProjectMutation::SetArchived {
                fingerprint,
                archived: true
            })] if fingerprint == "project-0"
        ));

        let commands = dispatch(&mut panel, WmEvent::Key(KeyAction::Delete));
        assert!(matches!(commands.as_slice(), [WmCommand::PushToast(_)]));

        panel.projects[0].archived = true;
        assert!(dispatch(&mut panel, WmEvent::Key(KeyAction::Backspace)).is_empty());
        let commands = dispatch(&mut panel, WmEvent::Key(KeyAction::Backspace));
        assert!(matches!(
            commands.as_slice(),
            [WmCommand::MutateProject(crate::ProjectMutation::Delete { fingerprint })]
                if fingerprint == "project-0"
        ));
    }

    #[test]
    fn project_action_buttons_use_three_row_mouse_targets() {
        let mut panel = ProjectPanelContent::new(vec![project(0)], None);
        let mut terminal = Terminal::new(TestBackend::new(100, FOOTER_HEIGHT)).unwrap();
        terminal
            .draw(|frame| panel.render_footer(frame.area(), frame))
            .unwrap();

        assert_eq!(panel.action_rects.len(), 3);
        assert!(
            panel
                .action_rects
                .iter()
                .all(|(_, rect)| rect.height == PAGE_CONTROL_HEIGHT)
        );
        let pin = panel.action_rects[0].1;
        let commands = dispatch(
            &mut panel,
            WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: pin.x + 1,
                row: pin.y + 1,
                modifiers: KeyModifiers::NONE,
            }),
        );
        assert!(matches!(
            commands.as_slice(),
            [WmCommand::MutateProject(
                crate::ProjectMutation::SetPinned { pinned: true, .. }
            )]
        ));
    }

    #[test]
    fn missing_project_path_does_not_block_session_resume() {
        let mut panel = ProjectPanelContent::new(vec![project(0)], None);
        panel.sessions.insert(
            "project-0".into(),
            vec![ProjectSession {
                target: SessionTarget::Legacy("session-0".into()),
                title: "Stored session".into(),
                activity: "4 messages".into(),
                updated_at: "2026-09-15 18:00".into(),
                goal: None,
                is_current: false,
            }],
        );

        let commands = panel.switch_selected_session();

        assert!(matches!(
            commands.as_slice(),
            [WmCommand::SwitchSession { sid, project_root }]
                if sid == "session-0" && project_root == std::path::Path::new("/missing/project-0")
        ));
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
                target: SessionTarget::Legacy("session-0".into()),
                title: "Selected session".into(),
                activity: "12 messages".into(),
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
