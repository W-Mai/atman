//! Global project navigation. All disk operations belong to the bound host.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Clear, Paragraph},
};
use uuid::Uuid;

use crate::{
    ProjectMutation, TuiControl,
    control::ControlSender,
    project_hub::{ProjectDirectory, ProjectRequest, SessionTarget},
};

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

struct Row {
    action: Action,
    label: String,
    detail: Option<String>,
    disabled: bool,
}

pub(crate) struct ProjectSidebar {
    pub directory: ProjectDirectory,
    pub focused: bool,
    visible: bool,
    drawer: bool,
    area: Rect,
    reveal: Rect,
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
}

impl Default for ProjectSidebar {
    fn default() -> Self {
        Self {
            directory: ProjectDirectory::default(),
            focused: false,
            visible: true,
            drawer: false,
            area: Rect::default(),
            reveal: Rect::default(),
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
        }
    }
}

impl ProjectSidebar {
    pub fn available(&self) -> bool {
        !self.area.is_empty()
    }

    pub fn has_pending(&self) -> bool {
        !self.requests.is_empty() || self.creating.is_some()
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
            self.directory = directory.clone();
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
        if self.refresh_needed && self.pending.is_none() {
            self.request(tx, None);
        }
    }

    fn rows(&self, current: Option<&SessionTarget>) -> Vec<Row> {
        let row = |action, label: &str| Row {
            action,
            label: label.into(),
            detail: None,
            disabled: false,
        };
        let mut rows = vec![
            row(Action::Hide, "Projects    Hide"),
            row(Action::Hub(None), "All projects"),
            row(
                Action::Refresh,
                if self.pending.is_some() {
                    "Refreshing…"
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
            .filter(|p| p.pinned)
            .collect();
        projects.sort_by_key(|p| (p.display_name.to_lowercase(), &p.fingerprint));
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
                label: format!(
                    "{} {}{}",
                    if expanded { "▾" } else { "▸" },
                    if project.archived {
                        "Archived · "
                    } else if unavailable {
                        "Unavailable · "
                    } else {
                        ""
                    },
                    project.display_name
                ),
                detail: Some(project.root.display().to_string()),
                disabled: false,
            });
            if !expanded {
                continue;
            }
            if let Some(sessions) = self.directory.sessions.get(id) {
                for session in sessions.iter().take(3) {
                    rows.push(row(
                        Action::Session(id.clone(), session.target.clone()),
                        &format!(
                            "{} {}",
                            if Some(&session.target) == current {
                                "•"
                            } else {
                                " "
                            },
                            session.title
                        ),
                    ));
                }
            }
            rows.push(row(Action::Hub(Some(id.clone())), "  All sessions"));
            let mut new = row(Action::New(id.clone()), "  + New session");
            new.disabled = unavailable || project.archived || self.creating.is_some();
            rows.push(new);
            let mut unpin = row(Action::Pin(id.clone(), false), "  Unpin");
            unpin.disabled = self.pending.is_some();
            rows.push(unpin);
        }
        rows
    }

    pub fn render(&mut self, frame: &mut Frame, current: Option<&SessionTarget>) {
        if self.area.is_empty() {
            if self.reveal.height == 3 {
                frame.render_widget(Clear, self.reveal);
                frame.render_widget(
                    crate::surface::block(crate::surface::Role::ProjectRail),
                    self.reveal,
                );
                frame.render_widget(
                    Paragraph::new("P")
                        .style(Style::default().fg(crate::theme::theme().tinted_fg.into())),
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
        let rows = self.rows(current);
        if !rows
            .iter()
            .any(|row| Some(&row.action) == self.selected.as_ref())
        {
            self.selected = rows.first().map(|row| row.action.clone());
        }
        self.offset = self.offset.min(rows.len().saturating_sub(1));
        let footer_height = if self.error.is_some() { 4 } else { 2 };
        let bottom = self.area.bottom().saturating_sub(footer_height);
        let mut y = self.area.y;
        for row in rows.into_iter().skip(self.offset) {
            let height = if row.detail.is_some() { 4 } else { 3 };
            if y + height > bottom {
                break;
            }
            let rect = Rect::new(self.area.x, y, self.area.width, height);
            let bg = crate::settings_form::render_record(
                frame,
                rect,
                self.focused && self.selected.as_ref() == Some(&row.action),
                self.hover.as_ref() == Some(&row.action),
                &t,
            );
            let style = Style::default().bg(bg).fg(if row.disabled {
                t.meta_fg.into()
            } else {
                t.tinted_fg.into()
            });
            frame.render_widget(
                Paragraph::new(row.label.clone()).style(style),
                Rect::new(rect.x + 2, y + 1, rect.width.saturating_sub(4), 1),
            );
            if let Some(detail) = &row.detail {
                frame.render_widget(
                    Paragraph::new(crate::width::middle_truncate(
                        detail,
                        rect.width.saturating_sub(4) as usize,
                    ))
                    .style(style.fg(t.meta_fg.into())),
                    Rect::new(rect.x + 2, y + 2, rect.width.saturating_sub(4), 1),
                );
            }
            self.hits.push((rect, row));
            y += height;
        }
        let hint =
            self.error
                .as_deref()
                .unwrap_or(if self.directory.projects.iter().any(|p| p.pinned) {
                    "↑↓ Select · Enter open"
                } else {
                    "Pin in All projects"
                });
        frame.render_widget(
            Paragraph::new(hint)
                .style(Style::default().fg(t.meta_fg.into()))
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
                    MouseEventKind::ScrollDown => self.offset = self.offset.saturating_add(1),
                    MouseEventKind::ScrollUp => self.offset = self.offset.saturating_sub(1),
                    _ => {}
                }
                Some(Action::Consume)
            }
            Event::Key(key) if self.focused && key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return None;
                }
                let rows = self.rows(None);
                let selected = rows
                    .iter()
                    .position(|row| Some(&row.action) == self.selected.as_ref())
                    .unwrap_or(0);
                match key.code {
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
                sidebar.render(f, None);
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
}
