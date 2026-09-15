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

const CARD_HEIGHT: u16 = 8;
const SESSION_ROW_HEIGHT: u16 = 4;

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
        self.previous_page_rect = None;
        self.next_page_rect = None;
        self.columns = if area.width >= 132 {
            3
        } else if area.width >= 84 {
            2
        } else {
            1
        };

        let header_height = 4.min(area.height);
        let body_height = area.height.saturating_sub(header_height);
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
                        format!(
                            "  {} known  ·  PAGE {}/{}",
                            self.projects.len(),
                            self.page + 1,
                            page_count
                        ),
                        Style::default().fg(t.subtle_fg.into()),
                    ),
                ]),
                Line::from(Span::styled(
                    "←→↑↓ select  Enter / double-click open  PgUp/PgDn or wheel page  Esc close",
                    Style::default().fg(t.subtle_fg.into()),
                )),
            ]),
            Rect::new(area.x, area.y, area.width, header_height),
        );

        if area.width >= 32 {
            let controls_y = area.y.saturating_add(2);
            let next = Rect::new(area.right().saturating_sub(10), controls_y, 10, 1);
            let previous = Rect::new(next.x.saturating_sub(11), controls_y, 10, 1);
            self.previous_page_rect = Some(previous);
            self.next_page_rect = Some(next);
            render_page_control(
                frame,
                previous,
                "‹ PREV",
                self.page > 0,
                self.previous_page_hovered,
                &t,
            );
            render_page_control(
                frame,
                next,
                "NEXT ›",
                self.page + 1 < page_count,
                self.next_page_hovered,
                &t,
            );
        }

        if self.projects.is_empty() {
            frame.render_widget(
                Paragraph::new(
                    "No projects registered yet. Start atman inside a project to add it.",
                )
                .style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(
                    area.x,
                    area.y.saturating_add(header_height),
                    area.width,
                    body_height,
                ),
            );
            return;
        }

        let gap = 1u16;
        let body_y = area.y.saturating_add(header_height);
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
                Line::from(Span::styled(
                    format!("{scope}  ·  {session_count} sessions"),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                )),
                Line::from(Span::styled(
                    crate::width::truncate(
                        &project.root.display().to_string(),
                        card_w.saturating_sub(4) as usize,
                    ),
                    Style::default().fg(t.subtle_fg.into()).bg(bg),
                )),
                Line::from(Span::styled(
                    crate::width::truncate(summary, card_w.saturating_sub(4) as usize),
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
        self.sessions_per_page = usize::from((list_h / SESSION_ROW_HEIGHT).max(1));
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
                    format!(
                        "  PAGE {}/{}  ·  ↑↓ select  Enter / double-click switch  PgUp/PgDn page  Esc back",
                        page + 1,
                        page_count
                    ),
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
            let current = if row.is_current { "  CURRENT" } else { "" };
            let goal = row.goal.as_deref().unwrap_or("No goal saved");
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(vec![
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
                    Line::from(Span::styled(
                        format!(
                            "{} messages  ·  {}  ·  {}",
                            row.message_count,
                            row.updated_at,
                            crate::width::truncate(goal, area.width.saturating_sub(32) as usize)
                        ),
                        Style::default().fg(t.subtle_fg.into()).bg(bg),
                    )),
                ])
                .block(
                    Block::default()
                        .borders(Borders::BOTTOM)
                        .border_style(Style::default().fg(border).bg(bg))
                        .style(Style::default().bg(bg)),
                ),
                rect,
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
        match self.view {
            ProjectView::Grid => self.render_grid(area, frame),
            ProjectView::Detail => self.render_detail(area, frame),
        }
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
            ProjectView::Grid => "Alt+P · Project Hub".into(),
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
