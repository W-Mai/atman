//! Global project navigation. All disk operations belong to the bound host.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use uuid::Uuid;

use crate::{
    ProjectMutation, TuiControl,
    control::{ControlSender, ViewAddress},
    project_hub::{ProjectDirectory, ProjectRequest, SessionTarget},
};

const PROJECTS_TOP: u16 = 9;

struct OpenSession {
    flow: ViewAddress,
    project_root: Option<String>,
    title: Option<String>,
    preview: Option<String>,
    completion: Option<atman_runtime::flow_runtime::live::PreviewCompletion>,
    unread: Option<atman_rt::TurnOutcome>,
    pin_when_resolved: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Consume,
    Show,
    Hide,
    Hub(Option<String>),
    Refresh,
    Expand(String),
    Session(String, SessionTarget),
    New(String),
    Pin(String, bool),
}

impl Action {
    fn symbol(&self) -> Option<&'static str> {
        match self {
            Self::Hub(_) => Some("≡"),
            Self::Refresh => Some("↻"),
            Self::New(_) => Some("+"),
            Self::Pin(_, false) => Some("×"),
            Self::Pin(_, true) => Some("↶"),
            _ => None,
        }
    }

    fn hint(&self) -> &'static str {
        match self {
            Self::Hub(None) => "All projects · Ctrl+L",
            Self::Hub(Some(_)) => "All sessions in this project",
            Self::Refresh => "Refresh projects · R",
            Self::New(_) => "New session in this project",
            Self::Pin(_, false) => "Unpin project · Undo available",
            Self::Pin(_, true) => "Undo unpin",
            Self::Hide => "Hide projects",
            Self::Expand(_) => "Expand or collapse project",
            Self::Session(..) => "Open conversation · Enter",
            _ => "",
        }
    }
}

struct Row {
    action: Action,
    label: String,
    detail: Option<String>,
    disabled: bool,
}

pub(crate) struct ProjectSidebar {
    pub directory: ProjectDirectory,
    pub source: Option<Uuid>,
    indexed_pending: bool,
    pub focused: bool,
    visible: bool,
    drawer: bool,
    area: Rect,
    reveal: Rect,
    reveal_hovered: bool,
    expanded: HashSet<String>,
    selected: Option<Action>,
    hover: Option<Action>,
    hits: Vec<(Rect, Row)>,
    offset: usize,
    pending: Option<ProjectRequest>,
    creating: Option<Uuid>,
    requests: HashMap<Uuid, u64>,
    issued: u64,
    applied: u64,
    refresh_needed: bool,
    undo: Option<String>,
    error: Option<String>,
    opened: Vec<OpenSession>,
    auto_pins: Vec<String>,
}

impl Default for ProjectSidebar {
    fn default() -> Self {
        Self {
            directory: ProjectDirectory::default(),
            source: None,
            indexed_pending: false,
            focused: false,
            visible: true,
            drawer: false,
            area: Rect::default(),
            reveal: Rect::default(),
            reveal_hovered: false,
            expanded: HashSet::new(),
            selected: None,
            hover: None,
            hits: vec![],
            offset: 0,
            pending: None,
            creating: None,
            requests: HashMap::new(),
            issued: 0,
            applied: 0,
            refresh_needed: false,
            undo: None,
            error: None,
            opened: vec![],
            auto_pins: vec![],
        }
    }
}

impl ProjectSidebar {
    pub(crate) fn open_flow(&self, target: &SessionTarget) -> Option<&ViewAddress> {
        self.opened
            .iter()
            .find(|session| session.flow.session_target() == *target)
            .map(|session| &session.flow)
    }

    pub(crate) fn focused(
        &mut self,
        handle: &crate::TuiHandle,
        snapshot: Option<atman_runtime::flow_runtime::live::PreviewSnapshot>,
    ) {
        let Some(binding) = handle.control_tx.as_ref().and_then(|tx| tx.binding()) else {
            return;
        };
        let target = binding.flow.session_target();
        let index = self
            .opened
            .iter()
            .position(|session| session.flow.session_target() == target)
            .unwrap_or_else(|| {
                self.opened.push(OpenSession {
                    flow: binding.flow.clone(),
                    project_root: handle.project_root.clone(),
                    title: None,
                    preview: None,
                    completion: None,
                    unread: None,
                    pin_when_resolved: true,
                });
                self.opened.len() - 1
            });
        let session = &mut self.opened[index];
        if session.flow != binding.flow {
            session.preview = None;
            session.completion = None;
        }
        session.flow = binding.flow.clone();
        session.project_root = handle.project_root.clone();
        if handle.session_name.is_some() {
            session.title = handle.session_name.clone();
        }
        if let Some(snapshot) =
            snapshot.filter(|snapshot| binding.flow.instance() == Some(&snapshot.flow))
        {
            if !snapshot.text.is_empty() {
                session.preview = Some(snapshot.text);
            }
            session.completion = snapshot.completion;
        }
        session.unread = None;
        self.resolve_opened();
    }

    pub(crate) fn observe(
        &mut self,
        snapshot: atman_runtime::flow_runtime::live::PreviewSnapshot,
        title: Option<Option<String>>,
    ) {
        let Some(session) = self
            .opened
            .iter_mut()
            .find(|session| session.flow.instance() == Some(&snapshot.flow))
        else {
            return;
        };
        if !snapshot.text.is_empty() {
            session.preview = Some(snapshot.text);
        }
        if let Some(title) = title {
            session.title = title;
        }
        if session.completion != snapshot.completion {
            session.unread = snapshot
                .completion
                .as_ref()
                .map(|completion| completion.outcome);
            session.completion = snapshot.completion;
        }
        if snapshot.active.is_some() {
            session.unread = None;
        }
    }

    fn resolve_opened(&mut self) {
        for session in &mut self.opened {
            let target = session.flow.session_target();
            let project = self
                .directory
                .sessions
                .iter()
                .find(|(_, rows)| rows.iter().any(|row| row.target == target))
                .map(|(id, _)| id.clone())
                .or_else(|| {
                    self.directory
                        .projects
                        .iter()
                        .find(|project| {
                            session.project_root.as_deref()
                                == Some(project.root.to_string_lossy().as_ref())
                        })
                        .map(|project| project.fingerprint.clone())
                });
            let Some(id) = project else {
                continue;
            };
            let rows = self.directory.sessions.entry(id.clone()).or_default();
            if !rows.iter().any(|row| row.target == target) {
                rows.push(crate::project_hub::ProjectSession {
                    target,
                    title: session
                        .title
                        .clone()
                        .unwrap_or_else(|| "New session".into()),
                    activity: String::new(),
                    preview: session.preview.clone(),
                    updated_at: String::new(),
                    updated_time: None,
                    goal: None,
                    is_current: false,
                });
            }
            if session.pin_when_resolved {
                session.pin_when_resolved = false;
                self.expanded.insert(id.clone());
                if self
                    .directory
                    .projects
                    .iter()
                    .any(|project| project.fingerprint == id && !project.pinned)
                    && !self.auto_pins.contains(&id)
                {
                    self.auto_pins.push(id);
                }
            }
        }
    }

    pub fn available(&self) -> bool {
        !self.area.is_empty()
    }

    pub fn has_pending(&self) -> bool {
        !self.requests.is_empty() || self.creating.is_some()
    }

    pub(crate) fn has_active_animation(&self) -> bool {
        !self.area.is_empty() && (self.has_pending() || self.indexed_pending)
    }

    pub fn creation_finished(&mut self, id: Uuid, error: Option<String>) -> bool {
        if self.creating != Some(id) {
            return false;
        }
        self.creating = None;
        if error.is_none() {
            self.focused = false;
            if self.drawer {
                self.hide();
            }
        }
        self.error = error;
        true
    }
    pub fn toggle(&mut self, tx: Option<&ControlSender>) {
        if self.area.is_empty() {
            self.visible = true;
            self.drawer = true;
            self.focused = true;
            self.request(tx, None);
        } else {
            self.hide();
        }
    }

    fn hide(&mut self) {
        self.visible = false;
        self.drawer = false;
        self.focused = false;
        self.reveal_hovered = false;
    }

    /// Width negotiation happens once before any flow surface is rendered.
    pub fn layout(&mut self, canvas: Rect) -> Rect {
        self.area = Rect::default();
        self.reveal = Rect::new(
            canvas.x,
            canvas.y + 1,
            canvas.width.min(3),
            canvas.height.saturating_sub(1).min(3),
        );
        self.hits.clear();
        if !self.visible {
            return canvas;
        }
        if canvas.width >= 110 {
            self.reveal = Rect::default();
            self.drawer = false;
            self.area = Rect::new(canvas.x, canvas.y, 28, canvas.height);
            Rect::new(canvas.x + 30, canvas.y, canvas.width - 30, canvas.height)
        } else if self.drawer {
            self.reveal = Rect::default();
            self.area = Rect::new(canvas.x, canvas.y, canvas.width.min(30), canvas.height);
            canvas
        } else {
            self.focused = false;
            canvas
        }
    }

    pub fn request(&mut self, tx: Option<&ControlSender>, mutation: Option<ProjectMutation>) {
        if self.pending.is_some() {
            self.refresh_needed |= mutation.is_none();
            return;
        }
        self.refresh_needed = false;
        let request = ProjectRequest {
            id: Uuid::new_v4(),
            mutation,
        };
        self.pending = Some(request.clone());
        self.track(request.id);
        self.error = None;
        if tx.is_none_or(|tx| tx.send(TuiControl::Projects(request.clone())).is_err()) {
            self.result(request.id, &Err("Project directory is unavailable".into()));
        }
    }

    pub fn track(&mut self, id: Uuid) {
        self.issued += 1;
        self.requests.insert(id, self.issued);
    }

    pub fn result(&mut self, id: Uuid, result: &Result<ProjectDirectory, String>) {
        let own = self
            .pending
            .as_ref()
            .is_some_and(|request| request.id == id);
        let generation = self.requests.remove(&id);
        // A newer read may have completed before an older mutation. Reconcile
        // after outstanding replies rather than restoring an old projection.
        self.refresh_needed |=
            result.is_ok() && generation.is_some_and(|generation| generation < self.applied);
        if let Ok(directory) = result
            && generation.is_some_and(|generation| generation >= self.applied)
        {
            self.applied = generation.unwrap();
            let mut directory = directory.clone();
            crate::project_hub::retain_details(&mut directory.sessions, &self.directory.sessions);
            for project in &self.directory.projects {
                if self.opened.iter().any(|session| {
                    session.project_root.as_deref() == Some(project.root.to_string_lossy().as_ref())
                }) && !directory
                    .projects
                    .iter()
                    .any(|incoming| incoming.fingerprint == project.fingerprint)
                {
                    directory.projects.push(project.clone());
                }
            }
            self.directory = directory;
            self.source = Some(id);
            self.resolve_opened();
        }
        if own {
            let request = self.pending.take().unwrap();
            match result {
                Ok(_) => {
                    if let Some(ProjectMutation::SetPinned {
                        fingerprint,
                        pinned,
                    }) = request.mutation
                    {
                        self.undo = (!pinned).then_some(fingerprint);
                    }
                }
                Err(error) => self.error = Some(error.clone()),
            }
        }
    }

    pub fn refresh_if_needed(&mut self, tx: Option<&ControlSender>) {
        if self.pending.is_none()
            && self.requests.is_empty()
            && let Some(fingerprint) = self.auto_pins.pop()
        {
            self.request(
                tx,
                Some(ProjectMutation::SetPinned {
                    fingerprint,
                    pinned: true,
                }),
            );
            return;
        }
        if self.refresh_needed && self.pending.is_none() {
            self.request(tx, None);
        }
    }

    pub(crate) fn browse_details(
        &mut self,
        source: Uuid,
        session: Uuid,
        details: &crate::project_hub::SessionBrowseDetails,
    ) {
        if self.source == Some(source) && self.requests.is_empty() {
            self.directory.apply_details(session, details);
        }
    }

    pub(crate) fn indexed(
        &mut self,
        source: Uuid,
        result: Option<&crate::project_hub::IndexedSessionsResult>,
    ) {
        if self.source != Some(source) {
            return;
        }
        self.indexed_pending = result.is_none();
        if !self.requests.is_empty() {
            return;
        }
        if let Some(result) = result {
            match result {
                Ok(indexed) => {
                    crate::project_hub::apply_indexed(&mut self.directory.sessions, indexed)
                }
                Err(error) => self.error = Some(error.clone()),
            }
        }
    }

    fn rows(&self) -> Vec<Row> {
        let row = |action, label: &str| Row {
            action,
            label: label.into(),
            detail: None,
            disabled: false,
        };
        let mut rows = vec![
            row(Action::Hide, "Projects"),
            row(Action::Hub(None), "All projects"),
            row(
                Action::Refresh,
                if self.pending.is_some() {
                    "…"
                } else {
                    "Refresh"
                },
            ),
        ];
        if let Some(id) = &self.undo {
            rows.push(row(Action::Pin(id.clone(), true), "Undo unpin"));
        }
        let mut projects: Vec<_> = self
            .directory
            .projects
            .iter()
            .filter(|p| {
                p.pinned
                    || self
                        .directory
                        .sessions
                        .get(&p.fingerprint)
                        .is_some_and(|rows| {
                            rows.iter().any(|row| self.open_flow(&row.target).is_some())
                        })
            })
            .collect();
        projects.sort_by_key(|p| {
            (
                !self
                    .directory
                    .sessions
                    .get(&p.fingerprint)
                    .is_some_and(|rows| {
                        rows.iter().any(|row| self.open_flow(&row.target).is_some())
                    }),
                p.display_name.to_lowercase(),
                &p.fingerprint,
            )
        });
        for project in projects {
            let id = &project.fingerprint;
            let expanded = self.expanded.contains(id);
            let unavailable = self
                .directory
                .details
                .get(id)
                .is_some_and(|d| !d.path_available);
            rows.push(Row {
                action: Action::Expand(id.clone()),
                label: project.display_name.clone(),
                detail: Some(if project.archived {
                    format!("Archived · {}", project.root.display())
                } else if unavailable {
                    format!("Unavailable · {}", project.root.display())
                } else {
                    project.root.display().to_string()
                }),
                disabled: false,
            });
            if !expanded {
                continue;
            }
            if let Some(sessions) = self.directory.sessions.get(id) {
                let ordered = sessions
                    .iter()
                    .filter(|row| self.open_flow(&row.target).is_some())
                    .chain(
                        sessions
                            .iter()
                            .filter(|row| self.open_flow(&row.target).is_none())
                            .take(5),
                    );
                for session in ordered {
                    let opened = self
                        .opened
                        .iter()
                        .find(|open| open.flow.session_target() == session.target);
                    rows.push(Row {
                        action: Action::Session(id.clone(), session.target.clone()),
                        label: opened
                            .and_then(|open| open.title.clone())
                            .unwrap_or_else(|| session.title.clone()),
                        detail: opened
                            .and_then(|open| open.preview.clone())
                            .or_else(|| session.preview.clone())
                            .filter(|preview| !preview.is_empty()),
                        disabled: false,
                    });
                }
            }
            rows.push(row(Action::Hub(Some(id.clone())), "All"));
            let mut new = row(Action::New(id.clone()), "+ New");
            new.disabled = unavailable || project.archived || self.creating.is_some();
            rows.push(new);
            let mut unpin = row(Action::Pin(id.clone(), false), "Unpin");
            unpin.disabled = self.pending.is_some();
            rows.push(unpin);
        }
        rows
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        current: Option<&SessionTarget>,
        animation_frame: u32,
    ) {
        self.hits.clear();
        if self.area.is_empty() {
            if self.reveal.height == 3 {
                let t = crate::theme::theme();
                let background = if self.reveal_hovered {
                    *t.work_hover_bg
                } else {
                    *t.work_bg
                };
                frame.render_widget(Clear, self.reveal);
                frame.render_widget(
                    crate::surface::block(crate::surface::Role::ProjectRail)
                        .style(Style::default().bg(background)),
                    self.reveal,
                );
                frame.render_widget(
                    Paragraph::new("P").style(
                        Style::default()
                            .fg(t.project_title_fg.into())
                            .bg(background)
                            .add_modifier(if self.reveal_hovered {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Rect::new(self.reveal.x + 1, self.reveal.y + 1, 1, 1),
                );
            }
            return;
        }
        let t = crate::theme::theme();
        frame.render_widget(Clear, self.area);
        frame.render_widget(
            crate::surface::block(crate::surface::Role::ProjectRail),
            self.area,
        );
        let rows = self.rows();
        if !rows
            .iter()
            .any(|row| Some(&row.action) == self.selected.as_ref())
        {
            self.selected = rows.first().map(|row| row.action.clone());
        }
        self.offset = self.offset.min(rows.len().saturating_sub(1));
        let footer_height = 3u16;
        let bottom = self.area.bottom().saturating_sub(footer_height);
        let positions = record_positions(&rows, self.area.width);
        let max_scroll = positions.last().map_or(0, |rect| {
            rect.bottom()
                .saturating_sub(self.area.height.saturating_sub(3))
        });
        let scroll = positions
            .get(self.offset.max(3))
            .map_or(0, |rect| rect.y.saturating_sub(PROJECTS_TOP))
            .min(max_scroll);
        frame.render_widget(
            Paragraph::new("PINNED PROJECTS").style(Style::default().fg(t.project_meta_fg.into())),
            Rect::new(
                self.area.x + 2,
                self.area.y + 7,
                self.area.width.saturating_sub(4),
                1,
            )
            .intersection(self.area),
        );
        for (index, (row, position)) in rows.into_iter().zip(positions).enumerate() {
            if index >= 3 && position.y < scroll + PROJECTS_TOP {
                continue;
            }
            let y = self.area.y
                + position
                    .y
                    .saturating_sub(if index >= 3 { scroll } else { 0 });
            if y + position.height > bottom {
                continue;
            }
            let rect = Rect::new(self.area.x + position.x, y, position.width, position.height);
            let bg = crate::settings_form::record_background(
                &t,
                self.focused && self.selected.as_ref() == Some(&row.action),
                self.hover.as_ref() == Some(&row.action),
            );
            frame.render_widget(
                crate::surface::block(crate::surface::Role::ProjectRail)
                    .style(Style::default().bg(bg)),
                rect,
            );
            let current_session =
                matches!(&row.action, Action::Session(_, target) if Some(target) == current);
            let session = matches!(row.action, Action::Session(..));
            let symbol = row.action.symbol();
            let style = Style::default()
                .bg(bg)
                .fg(if row.disabled || symbol.is_some() {
                    t.project_meta_fg.into()
                } else if session {
                    t.project_session_fg.into()
                } else {
                    t.project_title_fg.into()
                });
            let style = if row.disabled {
                style.add_modifier(Modifier::DIM)
            } else {
                style
            };
            let opened = if let Action::Session(_, target) = &row.action {
                self.opened
                    .iter()
                    .find(|session| session.flow.session_target() == *target)
            } else {
                None
            };
            let marker = opened
                .and_then(|session| session.unread)
                .map(|outcome| match outcome {
                    atman_rt::TurnOutcome::Completed => *t.project_completed_fg,
                    atman_rt::TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled) => {
                        *t.warn
                    }
                    atman_rt::TurnOutcome::Interrupted(_) => *t.error,
                })
                .or_else(|| current_session.then_some(*t.accent))
                .or_else(|| opened.map(|_| t.accent.lerp(t.work_bg, 0.65)));
            if let Some(marker) = marker {
                for y in rect.y..rect.bottom() {
                    frame.render_widget(
                        Paragraph::new("▌").style(Style::default().fg(marker).bg(bg)),
                        Rect::new(rect.x, y, 1, 1),
                    );
                }
            }
            let padding = if matches!(row.action, Action::Hub(None)) {
                2
            } else if rect.width < self.area.width {
                1
            } else {
                2
            };
            let width = rect.width.saturating_sub(padding * 2);
            let label = if let Some(symbol) = symbol {
                if row.action == Action::Refresh && (self.has_pending() || self.indexed_pending) {
                    crate::loading::glyph(animation_frame).into()
                } else {
                    symbol.into()
                }
            } else if matches!(row.action, Action::Hide) {
                format!("Projects{}‹", " ".repeat(width.saturating_sub(9) as usize))
            } else {
                crate::width::truncate(
                    &row.label,
                    width.saturating_sub(if session || matches!(row.action, Action::Expand(_)) {
                        2
                    } else {
                        0
                    }) as usize,
                )
            };
            let line = if session {
                Line::from(vec![Span::raw("  "), Span::styled(label, style)])
            } else if let Action::Expand(id) = &row.action {
                let gap = width.saturating_sub(crate::width::width(&label) as u16 + 1);
                Line::from(vec![
                    Span::styled(label, style.add_modifier(Modifier::BOLD)),
                    Span::raw(" ".repeat(gap as usize)),
                    Span::styled(
                        if self.expanded.contains(id) {
                            "−"
                        } else {
                            "+"
                        },
                        style.fg(t.project_meta_fg.into()),
                    ),
                ])
            } else {
                Line::from(Span::styled(
                    label,
                    if symbol.is_none() {
                        style.add_modifier(Modifier::BOLD)
                    } else {
                        style
                    },
                ))
            };
            frame.render_widget(
                Paragraph::new(line)
                    .style(style)
                    .alignment(if symbol.is_some() {
                        Alignment::Center
                    } else {
                        Alignment::Left
                    }),
                Rect::new(
                    rect.x + padding,
                    y + if symbol.is_some() { rect.height / 2 } else { 1 },
                    width,
                    1,
                ),
            );
            if let Some(detail) = &row.detail {
                let indent = if session { 2 } else { 0 };
                let width = width.saturating_sub(indent);
                let text = if session {
                    Line::from(crate::width::streaming_ticker_spans(
                        vec![Span::styled(
                            detail.clone(),
                            style.fg(t.project_meta_fg.into()),
                        )],
                        width as usize,
                        bg,
                    ))
                } else {
                    Line::from(crate::width::middle_truncate(detail, width as usize))
                };
                frame.render_widget(
                    Paragraph::new(text).style(style.fg(t.project_meta_fg.into())),
                    Rect::new(rect.x + padding + indent, y + 2, width, 1),
                );
            }
            self.hits.push((rect, row));
        }
        if self.opened.is_empty()
            && !self.directory.projects.iter().any(|project| project.pinned)
            && self.undo.is_none()
        {
            frame.render_widget(
                Paragraph::new("Pin a project in\nProject Hub · Ctrl+L")
                    .style(Style::default().fg(t.project_meta_fg.into())),
                Rect::new(
                    self.area.x + 2,
                    self.area.y + 9,
                    self.area.width.saturating_sub(4),
                    bottom.saturating_sub(self.area.y + 9),
                )
                .intersection(self.area),
            );
        }
        let hint = self.error.as_deref().unwrap_or_else(|| {
            self.hover
                .as_ref()
                .or(self.selected.as_ref().filter(|_| self.focused))
                .map_or("", |action| {
                    if let Action::Session(_, target) = action
                        && let Some(outcome) = self
                            .opened
                            .iter()
                            .find(|session| session.flow.session_target() == *target)
                            .and_then(|session| session.unread)
                    {
                        return match outcome {
                            atman_rt::TurnOutcome::Completed => {
                                "Completed · unread · Enter to open"
                            }
                            atman_rt::TurnOutcome::Interrupted(
                                atman_rt::Interruption::Cancelled,
                            ) => "Cancelled · unread · Enter to open",
                            atman_rt::TurnOutcome::Interrupted(_) => {
                                "Failed · unread · Enter to open"
                            }
                        };
                    }
                    action.hint()
                })
        });
        frame.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(if self.error.is_some() {
                    t.error.into()
                } else {
                    t.project_meta_fg.into()
                }))
                .wrap(ratatui::widgets::Wrap { trim: false }),
            Rect::new(
                self.area.x + 2,
                bottom + 1,
                self.area.width.saturating_sub(4),
                footer_height.saturating_sub(1),
            ),
        );
    }

    fn event(&mut self, event: &Event) -> Option<Action> {
        match event {
            Event::Mouse(mouse) => {
                self.reveal_hovered = self.reveal.contains((mouse.column, mouse.row).into());
                if self.reveal.contains((mouse.column, mouse.row).into()) {
                    return Some(
                        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                            Action::Show
                        } else {
                            Action::Consume
                        },
                    );
                }
                let inside = self.area.contains((mouse.column, mouse.row).into());
                self.hover = self
                    .hits
                    .iter()
                    .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
                    .map(|(_, row)| row.action.clone());
                if !inside {
                    if matches!(mouse.kind, MouseEventKind::Down(_)) {
                        self.focused = false;
                        if self.drawer {
                            self.hide();
                            return Some(Action::Hide);
                        }
                    }
                    return None;
                }
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.focused = true;
                        if let Some((_, row)) = self
                            .hits
                            .iter()
                            .find(|(r, _)| r.contains((mouse.column, mouse.row).into()))
                        {
                            self.selected = Some(row.action.clone());
                            if !row.disabled {
                                return Some(row.action.clone());
                            }
                        }
                    }
                    MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                        let rows = self.rows();
                        let positions = record_positions(&rows, self.area.width);
                        let max_scroll = positions.last().map_or(0, |rect| {
                            rect.bottom()
                                .saturating_sub(self.area.height.saturating_sub(3))
                        });
                        let current = positions
                            .get(self.offset.max(3))
                            .map_or(PROJECTS_TOP, |rect| rect.y)
                            .min(max_scroll + PROJECTS_TOP);
                        let next = if mouse.kind == MouseEventKind::ScrollDown {
                            (current < max_scroll + PROJECTS_TOP)
                                .then(|| {
                                    positions
                                        .iter()
                                        .enumerate()
                                        .find(|(_, rect)| rect.y > current)
                                })
                                .flatten()
                        } else {
                            positions
                                .iter()
                                .enumerate()
                                .rev()
                                .find(|(_, rect)| rect.y >= PROJECTS_TOP && rect.y < current)
                        };
                        if let Some((index, _)) = next {
                            self.offset = index;
                        }
                    }
                    _ => {}
                }
                Some(Action::Consume)
            }
            Event::Key(key) if self.focused && key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return None;
                }
                let rows = self.rows();
                let selected = rows
                    .iter()
                    .position(|row| Some(&row.action) == self.selected.as_ref())
                    .unwrap_or(0);
                match key.code {
                    KeyCode::Char('r' | 'R') => return Some(Action::Refresh),
                    KeyCode::Esc => {
                        self.focused = false;
                        if self.drawer {
                            self.hide();
                        }
                    }
                    KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
                        let next = match key.code {
                            KeyCode::Up => selected.saturating_sub(1),
                            KeyCode::Down => (selected + 1).min(rows.len() - 1),
                            KeyCode::Home => 0,
                            _ => rows.len() - 1,
                        };
                        self.selected = Some(rows[next].action.clone());
                        if next < self.offset {
                            self.offset = next;
                        }
                        if !self
                            .hits
                            .iter()
                            .any(|(_, row)| Some(&row.action) == self.selected.as_ref())
                        {
                            self.offset = next;
                        }
                    }
                    KeyCode::Enter | KeyCode::Char(' ') if !rows[selected].disabled => {
                        return Some(rows[selected].action.clone());
                    }
                    _ => {}
                }
                Some(Action::Consume)
            }
            Event::Paste(_) if self.focused => Some(Action::Consume),
            _ => None,
        }
    }
}

// One projection governs the visible cards, adjacent action bands and input targets.
fn record_positions(rows: &[Row], width: u16) -> Vec<Rect> {
    let mut positions = Vec::with_capacity(rows.len());
    let mut index = 0;
    let mut y = 0;
    while index < rows.len() {
        if index == 1 && rows.len() >= 3 {
            let refresh = width / 2;
            positions.push(Rect::new(0, 3, width - refresh, 3));
            positions.push(Rect::new(width - refresh, 3, refresh, 3));
            index += 2;
            y = PROJECTS_TOP;
        } else if matches!(rows[index].action, Action::Hub(Some(_))) && index + 2 < rows.len() {
            let cell = width / 3;
            for column in 0..3 {
                let x = column * cell;
                positions.push(Rect::new(
                    x,
                    y,
                    if column == 2 { width - x } else { cell },
                    1,
                ));
            }
            index += 3;
            y += 1;
        } else {
            let height = if rows[index].detail.is_some() { 4 } else { 3 };
            positions.push(Rect::new(0, y, width, height));
            index += 1;
            y += height;
        }
    }
    positions
}

pub(crate) fn handle_event(
    ui: &mut crate::UiState,
    event: &Event,
    tx: Option<&ControlSender>,
) -> bool {
    let shared = ui.app.project_sidebar.clone();
    let mut sidebar = shared.lock().unwrap();
    let Some(action) = sidebar.event(event) else {
        return false;
    };
    match action {
        Action::Consume => {}
        Action::Show => sidebar.toggle(tx),
        Action::Hide => sidebar.hide(),
        Action::Refresh => sidebar.request(tx, None),
        Action::Expand(id) => {
            if !id.is_empty() && !sidebar.expanded.remove(&id) {
                sidebar.expanded.insert(id);
            }
        }
        Action::Pin(fingerprint, pinned) => sidebar.request(
            tx,
            Some(ProjectMutation::SetPinned {
                fingerprint,
                pinned,
            }),
        ),
        Action::New(fingerprint) => {
            match tx.map(|tx| tx.send_tracked(TuiControl::NewProjectSession { fingerprint })) {
                Some(Ok(Some(ticket))) => {
                    sidebar.creating = Some(ticket.request_id);
                    sidebar.error = None;
                }
                _ => sidebar.error = Some("Session creation is unavailable".into()),
            }
        }
        Action::Hub(id) => {
            let root = ui.app.project_root.clone();
            sidebar.focused = false;
            if sidebar.drawer {
                sidebar.hide();
            }
            drop(sidebar);
            ui.wm.open_project_hub(
                ui.app.maximized_canvas(),
                shared.clone(),
                root.as_deref(),
                tx,
            );
            if let Some(id) = id {
                ui.wm.show_project_sessions(&id);
            }
        }
        Action::Session(id, target) => {
            let root: Option<PathBuf> = sidebar
                .directory
                .projects
                .iter()
                .find(|p| p.fingerprint == id)
                .map(|p| p.root.clone());
            sidebar.focused = false;
            if sidebar.drawer {
                sidebar.hide();
            }
            drop(sidebar);
            let command = match target {
                SessionTarget::Legacy(sid) => {
                    if let Some(root) = root {
                        crate::wm::WmCommand::SwitchSession {
                            sid,
                            project_root: root,
                        }
                    } else {
                        crate::wm::WmCommand::PushToast(
                            "This session's project is unavailable".into(),
                        )
                    }
                }
                SessionTarget::Persistent(session) => crate::wm::WmCommand::OpenSession(session),
                SessionTarget::Indexed(source) => {
                    crate::wm::WmCommand::BrowseIndexedHistory(source)
                }
            };
            ui.wm.apply_commands(&mut ui.app, vec![command], tx);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::project_catalog::ProjectRecord;
    use crossterm::event::{KeyEvent, MouseEvent};

    fn project(id: &str, name: &str) -> ProjectRecord {
        ProjectRecord {
            fingerprint: id.into(),
            root: PathBuf::from(format!("/missing/{id}")),
            display_name: name.into(),
            pinned: true,
            archived: false,
            first_seen: chrono::Utc::now(),
            last_opened: chrono::Utc::now(),
        }
    }

    fn session_handle(session: Uuid, root: &str) -> crate::TuiHandle {
        let mut handle = crate::TuiHandle::from_session(std::sync::Arc::new(
            atman_runtime::Session::open_ephemeral(),
        ));
        let (commands, _) = tokio::sync::mpsc::unbounded_channel();
        let (sender, _) = ControlSender::for_flow(
            crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: atman_runtime::flow_store::FlowRef {
                    session_id: session.into(),
                    instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
                }
                .into(),
            },
            commands,
        );
        handle.control_tx = Some(sender);
        handle.project_root = Some(root.into());
        handle
    }

    #[test]
    fn opened_membership_is_unlimited_and_focus_cannot_change_catalog_order() {
        let mut sidebar = ProjectSidebar::default();
        sidebar.directory.projects = vec![project("a", "Alpha"), project("b", "Beta")];
        let ids: Vec<_> = (0..9).map(|_| Uuid::now_v7()).collect();
        sidebar.directory.sessions.insert(
            "a".into(),
            ids.iter()
                .enumerate()
                .map(|(index, id)| crate::project_hub::ProjectSession {
                    target: SessionTarget::Persistent(*id),
                    title: format!("Session {index}"),
                    preview: None,
                    activity: String::new(),
                    updated_at: String::new(),
                    updated_time: None,
                    goal: None,
                    is_current: false,
                })
                .collect(),
        );
        let handles: Vec<_> = ids
            .iter()
            .map(|id| session_handle(*id, "/missing/a"))
            .collect();
        for handle in handles.iter().take(7).rev() {
            sidebar.focused(handle, None);
        }
        let order = |sidebar: &ProjectSidebar| {
            sidebar
                .rows()
                .into_iter()
                .filter_map(|row| match row.action {
                    Action::Session(_, target) => Some(target),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            order(&sidebar),
            ids.iter()
                .map(|id| SessionTarget::Persistent(*id))
                .collect::<Vec<_>>()
        );
        let original = order(&sidebar);
        for handle in &handles[..7] {
            sidebar.focused(handle, None);
        }
        assert_eq!(order(&sidebar), original);
        assert_eq!(sidebar.opened.len(), 7);
    }

    #[test]
    fn live_overlays_are_source_bound_and_completion_baselines_do_not_alert_on_restore() {
        use atman_runtime::flow_runtime::live::{PreviewCompletion, PreviewSnapshot};
        let mut sidebar = ProjectSidebar::default();
        let handle = session_handle(Uuid::now_v7(), "/missing/a");
        let flow = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .flow
            .instance()
            .unwrap()
            .clone();
        let lease = atman_rt::TurnLease {
            flow: flow.clone(),
            epoch: 1,
            turn: atman_rt::TurnId::now(),
            run: atman_rt::RunId::now(),
        };
        let mut snapshot = PreviewSnapshot {
            flow,
            text: "Initial output".into(),
            active: None,
            completion: Some(PreviewCompletion {
                lease,
                outcome: atman_rt::TurnOutcome::Completed,
            }),
        };
        sidebar.focused(&handle, Some(snapshot.clone()));
        assert!(sidebar.opened[0].unread.is_none());
        snapshot.text = "Latest streamed output".into();
        snapshot.completion.as_mut().unwrap().lease.epoch += 1;
        sidebar.observe(snapshot.clone(), None);
        assert_eq!(
            sidebar.opened[0].unread,
            Some(atman_rt::TurnOutcome::Completed)
        );
        let mut wrong = snapshot.clone();
        wrong.flow.instance_id = atman_runtime::flow_store::FlowInstanceId::now();
        wrong.text = "Other flow".into();
        sidebar.observe(wrong, None);
        assert_eq!(
            sidebar.opened[0].preview.as_deref(),
            Some("Latest streamed output")
        );
        sidebar.focused(&handle, Some(snapshot.clone()));
        assert!(sidebar.opened[0].unread.is_none());
        snapshot.active = Some(snapshot.completion.as_ref().unwrap().lease.clone());
        sidebar.observe(snapshot, None);
        assert!(sidebar.opened[0].unread.is_none());
    }

    #[test]
    fn catalog_receipts_preserve_pins_until_success_and_reject_stale_projection() {
        let (tx, mut rx) = ControlSender::channel();
        let mut sidebar = ProjectSidebar::default();
        sidebar.directory.projects.push(project("a", "A"));
        sidebar.request(
            Some(&tx),
            Some(ProjectMutation::SetPinned {
                fingerprint: "a".into(),
                pinned: false,
            }),
        );
        let TuiControl::Projects(request) = rx.try_recv().unwrap() else {
            panic!("project request")
        };
        assert!(sidebar.directory.projects[0].pinned);
        sidebar.result(Uuid::new_v4(), &Err("unrelated failure".into()));
        assert!(sidebar.error.is_none());
        sidebar.result(request.id, &Err("disk unavailable".into()));
        assert!(sidebar.directory.projects[0].pinned);
        sidebar.request(
            Some(&tx),
            Some(ProjectMutation::SetPinned {
                fingerprint: "a".into(),
                pinned: false,
            }),
        );
        let TuiControl::Projects(request) = rx.try_recv().unwrap() else {
            panic!("project request")
        };
        let old = sidebar.directory.clone();
        let mut updated = old.clone();
        updated.projects[0].pinned = false;
        sidebar.result(request.id, &Ok(updated.clone()));
        assert_eq!(sidebar.undo.as_deref(), Some("a"));
        assert!(!sidebar.directory.projects[0].pinned);
        let earlier = Uuid::new_v4();
        let later = Uuid::new_v4();
        sidebar.track(earlier);
        sidebar.track(later);
        sidebar.result(later, &Ok(updated));
        sidebar.result(earlier, &Ok(old));
        assert!(!sidebar.directory.projects[0].pinned);
    }

    #[test]
    fn sidebar_records_share_keyboard_mouse_identity_and_keep_pins_when_hidden() {
        let mut sidebar = ProjectSidebar::default();
        sidebar.directory.projects = vec![project("b", "Beta"), project("a", "Alpha")];
        let canvas = Rect::new(7, 3, 120, 40);
        let content = sidebar.layout(canvas);
        assert_eq!(content, Rect::new(37, 3, 90, 40));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 50)).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    Paragraph::new(("UNDERLAY".repeat(20) + "\n").repeat(50)),
                    f.area(),
                );
                sidebar.render(f, None, 0);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(8, 30)].symbol(), " ");
        let row = sidebar
            .hits
            .iter()
            .find(|(_, row)| row.action == Action::Expand("a".into()))
            .unwrap()
            .0;
        for y in row.y..row.bottom() {
            let event = Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x + 2,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
            assert_eq!(sidebar.event(&event), Some(Action::Expand("a".into())));
        }
        assert_eq!(
            sidebar.event(&Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Some(Action::Expand("a".into()))
        );
        sidebar.hide();
        assert_eq!(sidebar.layout(canvas), canvas);
        assert!(sidebar.directory.projects.iter().all(|p| p.pinned));
        assert_eq!(
            sidebar.layout(Rect::new(7, 3, 80, 30)),
            Rect::new(7, 3, 80, 30)
        );
        sidebar.toggle(None);
        let small = Rect::new(7, 3, 80, 30);
        assert_eq!(sidebar.layout(small), small);
        assert_eq!(sidebar.area, Rect::new(7, 3, 30, 30));
    }

    #[test]
    fn grouped_actions_share_padded_geometry_without_overflow_or_empty_scrolling() {
        let mut sidebar = ProjectSidebar::default();
        sidebar.directory.projects = vec![project("a", "Alpha"), project("b", "Beta")];
        sidebar.expanded.insert("a".into());
        let target = SessionTarget::Persistent(Uuid::now_v7());
        sidebar.directory.sessions.insert(
            "a".into(),
            vec![crate::project_hub::ProjectSession {
                target: target.clone(),
                title: "A long session title · 中文".into(),
                activity: "Idle".into(),
                preview: Some("Recent output".into()),
                updated_at: "Now".into(),
                updated_time: None,
                goal: None,
                is_current: true,
            }],
        );
        sidebar.layout(Rect::new(0, 0, 150, 45));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(150, 45)).unwrap();
        terminal
            .draw(|frame| sidebar.render(frame, Some(&target), 0))
            .unwrap();
        let all = sidebar
            .hits
            .iter()
            .find(|(_, row)| row.action == Action::Hub(None))
            .unwrap()
            .0;
        let refresh = sidebar
            .hits
            .iter()
            .find(|(_, row)| row.action == Action::Refresh)
            .unwrap()
            .0;
        assert_eq!(all.y, refresh.y);
        assert_eq!(all.height, 3);
        assert_eq!(all.width, refresh.width);
        let theme = crate::theme::theme();
        let buffer = terminal.backend().buffer();
        for (rect, row) in &sidebar.hits {
            if let Some(symbol) = row.action.symbol() {
                let y = rect.y + rect.height / 2;
                let x = (rect.x..rect.right())
                    .find(|x| buffer[(*x, y)].symbol() == symbol)
                    .unwrap();
                let center = 2 * x + crate::width::width(symbol) as u16;
                assert!(
                    center.abs_diff(2 * rect.x + rect.width) <= 1,
                    "centering must stay within half a terminal cell"
                );
                assert_eq!(buffer[(x, y)].fg, theme.project_meta_fg.into());
                assert!(!row.action.hint().is_empty());
            }
            if matches!(row.action, Action::Expand(_)) {
                assert_eq!(
                    buffer[(rect.x + 2, rect.y + 1)].fg,
                    theme.project_title_fg.into()
                );
                assert_eq!(
                    buffer[(rect.x + 2, rect.y + 2)].fg,
                    theme.project_meta_fg.into()
                );
            }
            if matches!(row.action, Action::Session(..)) {
                assert_eq!(
                    buffer[(rect.x + 6, rect.y + 1)].fg,
                    theme.project_session_fg.into()
                );
                assert!((rect.y..rect.bottom()).all(|y| buffer[(rect.x, y)].symbol() == "▌"));
            }
        }
        let mut bands = Vec::new();
        for action in [
            Action::Hub(Some("a".into())),
            Action::New("a".into()),
            Action::Pin("a".into(), false),
        ] {
            let rect = sidebar
                .hits
                .iter()
                .find(|(_, row)| row.action == action)
                .unwrap()
                .0;
            assert_eq!(rect.height, 1);
            for row in rect.y..rect.bottom() {
                assert_eq!(
                    sidebar.event(&Event::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.x + 1,
                        row,
                        modifiers: KeyModifiers::NONE,
                    })),
                    Some(action.clone())
                );
            }
            bands.push(rect);
        }
        assert!(bands.iter().all(|rect| rect.y == bands[0].y));
        for (index, (rect, _)) in sidebar.hits.iter().enumerate() {
            assert_eq!(rect.intersection(sidebar.area), *rect);
            assert!(
                sidebar.hits[index + 1..]
                    .iter()
                    .all(|(other, _)| rect.intersection(*other).is_empty())
            );
        }
        assert_eq!(sidebar.offset, 0);
        sidebar.event(&Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: sidebar.area.x + 2,
            row: sidebar.area.y + 10,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(sidebar.offset, 0);
        let count = sidebar.hits.len();
        terminal
            .draw(|frame| sidebar.render(frame, Some(&target), 0))
            .unwrap();
        assert_eq!(sidebar.hits.len(), count);
    }
}
