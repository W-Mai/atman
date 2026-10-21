//! Workspace navigation reuses inspected directories and their existing actions.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};

use super::{Action, FlowHubPage, HubRequest};
use crate::{control::ViewAddress, keys::KeyAction, project_hub::SessionTarget};

#[derive(Clone, Default)]
pub(super) struct Catalog {
    sessions: Vec<(SessionTarget, String)>,
}

impl Catalog {
    pub fn observe(&mut self, page: &FlowHubPage) {
        let mut add = |target, title: String| {
            if let Some(row) = self.sessions.iter_mut().find(|row| row.0 == target) {
                row.1 = title;
            } else {
                self.sessions.push((target, title));
            }
        };
        match page {
            FlowHubPage::Sessions(directory) => {
                for row in &directory.sessions {
                    if !row.cleanup_pending {
                        add(row.target.clone(), row.title.clone());
                    }
                }
            }
            FlowHubPage::Flows { session, title, .. } => {
                add(SessionTarget::Persistent(*session), title.clone());
            }
            FlowHubPage::LegacyFlows { session, title, .. } => {
                add(SessionTarget::Legacy(session.to_string()), title.clone());
            }
            FlowHubPage::Overview(directory) => {
                for entry in &directory.page.entries {
                    if let atman_runtime::flow_runtime::catalog::FlowCatalogEntry::Flow {
                        owner,
                        session_title,
                        ..
                    } = entry
                    {
                        add(
                            SessionTarget::Persistent(owner.session_id.0),
                            session_title
                                .clone()
                                .unwrap_or_else(|| "Untitled session".into()),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Section {
    #[default]
    Flows,
    Watches,
    Worktrees,
}

impl Section {
    fn of(page: Option<&FlowHubPage>) -> Self {
        match page {
            Some(FlowHubPage::Watchers(_) | FlowHubPage::ProcessSources { .. }) => Self::Watches,
            Some(
                FlowHubPage::Workspaces(_)
                | FlowHubPage::Workspace(_)
                | FlowHubPage::WorkspaceRecord(_)
                | FlowHubPage::FlowWorkspace(_),
            ) => Self::Worktrees,
            _ => Self::Flows,
        }
    }

    fn action(self, focused: &ViewAddress) -> Action {
        match self {
            Self::Flows => Action::Load(HubRequest::Overview { after: None }),
            Self::Watches => focused.instance().map_or(Action::None, |owner| {
                Action::Load(HubRequest::WatchCatalog {
                    owner: owner.clone(),
                    after: None,
                })
            }),
            Self::Worktrees => Action::Load(HubRequest::Workspaces { after: None }),
        }
    }
}

#[derive(Default)]
pub(super) struct Navigation {
    pub catalog: Catalog,
    focused: Option<ViewAddress>,
    section: Section,
    selecting: bool,
    selected: usize,
    offset: usize,
    hits: Vec<(Rect, Action)>,
    hovered: Option<usize>,
    rows: Vec<(String, String, Action, bool)>,
    rail: Rect,
    overlay: bool,
}

impl Navigation {
    pub fn owns_input(&self) -> bool {
        self.selecting
    }
    pub fn clear(&mut self) {
        self.hits.clear();
        self.rail = Rect::default();
        self.focused = None;
        self.overlay = false;
    }

    pub fn key(&mut self, key: &KeyAction) -> Option<Action> {
        let focused = self.focused.as_ref()?;
        if matches!(
            key,
            KeyAction::CyclePanelForward | KeyAction::CyclePanelBackward
        ) {
            let sections = [Section::Flows, Section::Watches, Section::Worktrees];
            let index = sections
                .iter()
                .position(|section| *section == self.section)
                .unwrap();
            let step = if *key == KeyAction::CyclePanelForward {
                1
            } else {
                2
            };
            self.selecting = false;
            return Some(sections[(index + step) % 3].action(focused));
        }
        if *key == KeyAction::ToggleSidebar {
            self.selecting = !self.selecting;
            return Some(Action::None);
        }
        if !self.selecting {
            return None;
        }
        match key {
            KeyAction::Escape => self.selecting = false,
            KeyAction::HistoryUp | KeyAction::BackTab => {
                self.selected = self.selected.saturating_sub(1)
            }
            KeyAction::HistoryDown | KeyAction::Tab => {
                self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1))
            }
            KeyAction::Submit => {
                self.selecting = false;
                return Some(
                    self.rows
                        .get(self.selected)
                        .map_or(Action::None, |row| row.2.clone()),
                );
            }
            _ => {}
        }
        Some(Action::None)
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Option<Action> {
        let point = (event.column, event.row).into();
        self.hovered = self.hits.iter().position(|(rect, _)| rect.contains(point));
        if let Some(index) = self.hovered {
            if matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.selecting = false;
                return Some(if pending {
                    Action::None
                } else {
                    self.hits[index].1.clone()
                });
            }
            if self.rail.contains(point) {
                match event.kind {
                    MouseEventKind::ScrollDown => {
                        self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1))
                    }
                    MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(1),
                    _ => {}
                }
            }
            return Some(Action::None);
        }
        if self.rail.contains(point) || self.overlay {
            return Some(Action::None);
        }
        None
    }

    /// Return the content viewport, or None while the narrow navigation page owns the surface.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        focused: &ViewAddress,
        page: Option<&FlowHubPage>,
    ) -> Option<Rect> {
        let t = crate::theme::theme();
        self.focused = Some(focused.clone());
        self.section = Section::of(page);
        self.hits.clear();
        if let Some(page) = page {
            self.catalog.observe(page);
        }
        let width = area
            .width
            .saturating_sub(if area.width < 50 { 0 } else { 4 })
            .min(164);
        let shell = Rect::new(
            area.x + (area.width - width) / 2,
            area.y,
            width,
            area.height,
        );
        let brand = if width >= 100 { 16 } else { 0 };
        if brand > 0 {
            frame.render_widget(
                Paragraph::new("\n∴ ATMAN").style(Style::default().fg(t.accent.into())),
                Rect::new(shell.x, shell.y, brand, 3),
            );
        }
        let cell = if brand > 0 { 18 } else { width / 3 };
        for (index, (section, title)) in [
            (Section::Flows, "FlowHub"),
            (Section::Watches, "WatchHub"),
            (Section::Worktrees, "Worktrees"),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = Rect::new(
                shell.x + brand + index as u16 * cell,
                shell.y,
                cell.saturating_sub(1),
                3,
            );
            let action = section.action(focused);
            self.button(frame, rect, title, "", action, self.section == section);
        }
        let hint = if width >= 80 {
            "Ctrl+Tab / Ctrl+Shift+Tab views · F2 sessions"
        } else {
            "Ctrl+Tab views · F2 sessions"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(t.meta_fg.into())),
            Rect::new(shell.x + 1, shell.y + 3, width.saturating_sub(2), 1),
        );
        let wide = width >= 126 && area.height >= 30;
        let body = Rect::new(shell.x, shell.y + 4, width, shell.height.saturating_sub(4));
        self.rail = if wide {
            Rect::new(body.x, body.y, 26, body.height)
        } else if self.selecting {
            body
        } else {
            Rect::default()
        };
        self.overlay = !wide && self.selecting;
        self.rows = vec![
            (
                "Atman".into(),
                "Management session".into(),
                Action::OpenManagement,
                false,
            ),
            (
                "All sessions".into(),
                "Flow overview".into(),
                Action::Load(HubRequest::Overview { after: None }),
                matches!(page, Some(FlowHubPage::Overview(_))),
            ),
            (
                "Browse sessions".into(),
                "Complete directory".into(),
                Action::Load(HubRequest::Sessions { before: None }),
                matches!(page, Some(FlowHubPage::Sessions(_))),
            ),
        ];
        for (target, title) in &self.catalog.sessions {
            let request = match target {
                SessionTarget::Persistent(session) => HubRequest::Flows {
                    session: *session,
                    before: None,
                },
                SessionTarget::Legacy(session) => match uuid::Uuid::parse_str(session) {
                    Ok(session) => HubRequest::LegacyFlows {
                        session,
                        after: None,
                    },
                    Err(_) => continue,
                },
            };
            let active = page.is_some_and(|page| page.first() == request);
            let detail = match target {
                SessionTarget::Persistent(id) => format!("Session · {}", &id.to_string()[28..]),
                SessionTarget::Legacy(_) => "Read-only history".into(),
            };
            self.rows
                .push((title.clone(), detail, Action::Load(request), active));
        }
        if !self.rail.is_empty() {
            frame.render_widget(
                Paragraph::new("Workspace · F2 focus").style(Style::default().fg(t.meta_fg.into())),
                Rect::new(
                    self.rail.x + 1,
                    self.rail.y,
                    self.rail.width.saturating_sub(2),
                    1,
                ),
            );
            let visible = usize::from(self.rail.height.saturating_sub(4) / 4).max(1);
            self.selected = self.selected.min(self.rows.len().saturating_sub(1));
            self.offset = self.offset.min(self.selected);
            if self.selected >= self.offset + visible {
                self.offset = self.selected + 1 - visible;
            }
            for index in self.offset..(self.offset + visible).min(self.rows.len()) {
                let rect = Rect::new(
                    self.rail.x,
                    self.rail.y + 2 + (index - self.offset) as u16 * 4,
                    self.rail.width,
                    4,
                );
                let (title, detail, action, active) = self.rows[index].clone();
                self.button(
                    frame,
                    rect,
                    &title,
                    &detail,
                    action,
                    active || self.selecting && self.selected == index,
                );
            }
            frame.render_widget(
                Paragraph::new(format!(
                    "{} / {} · ↑↓ Enter",
                    self.selected + 1,
                    self.rows.len()
                ))
                .style(Style::default().fg(t.meta_fg.into())),
                Rect::new(
                    self.rail.x + 1,
                    self.rail.bottom().saturating_sub(1),
                    self.rail.width.saturating_sub(2),
                    1,
                ),
            );
        }
        if !wide && self.selecting {
            return None;
        }
        Some(if wide {
            Rect::new(body.x + 29, body.y, body.width - 29, body.height)
        } else {
            body
        })
    }

    fn button(
        &mut self,
        frame: &mut Frame,
        rect: Rect,
        title: &str,
        detail: &str,
        action: Action,
        selected: bool,
    ) {
        let t = crate::theme::theme();
        let enabled = !matches!(action, Action::None);
        let bg = if selected {
            t.panel_bg.lerp(t.accent, 0.15)
        } else if self.hovered == Some(self.hits.len()) {
            t.panel_bg.lerp(t.work_hover_bg, 0.24)
        } else {
            t.work_bg.into_inner()
        };
        let marker = if selected { "▌" } else { " " };
        let title = crate::width::truncate(title, rect.width.saturating_sub(3) as usize);
        let detail = crate::width::truncate(detail, rect.width.saturating_sub(4) as usize);
        let text = if rect.height == 3 {
            format!("{marker}\n{marker} {title}\n{marker}")
        } else {
            format!("{marker}\n{marker} {title}\n{marker} {detail}\n{marker}")
        };
        frame.render_widget(
            Paragraph::new(text).style(
                Style::default()
                    .fg(if enabled { t.heading } else { t.meta_fg }.into())
                    .bg(bg),
            ),
            rect,
        );
        if selected {
            frame.render_widget(
                Paragraph::new("▌\n".repeat(usize::from(rect.height)))
                    .style(Style::default().fg(t.accent.into()).bg(bg)),
                Rect::new(rect.x, rect.y, 1, rect.height),
            );
        }
        self.hits.push((rect, action));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_hub::{FlowHub, History, SessionDirectory};
    use atman_runtime::flow_store::{FlowInstanceId, FlowRef};

    #[test]
    fn navigation_uses_bound_actions_and_keeps_narrow_input_out_of_the_directory() {
        let owner = FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let focused = ViewAddress::Instance(owner.clone());
        let mut hub = FlowHub {
            page: Some(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![],
                next: None,
                total: 0,
            })),
            ..Default::default()
        };
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 45)).unwrap();
        terminal.draw(|frame| hub.render(frame, &focused)).unwrap();
        assert!(
            matches!(hub.key(KeyAction::CyclePanelForward), Action::Load(HubRequest::WatchCatalog { owner: target, .. }) if target == owner)
        );
        let rect = hub.navigation.hits[1].0;
        assert!(
            matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 2, row: rect.y + 1, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Load(HubRequest::WatchCatalog { owner: target, .. }) if target == owner)
        );
        hub.key(KeyAction::ToggleSidebar);
        assert!(matches!(hub.key(KeyAction::Submit), Action::OpenManagement));
        hub.key(KeyAction::ToggleSidebar);
        hub.key(KeyAction::HistoryDown);
        assert!(matches!(
            hub.key(KeyAction::Submit),
            Action::Load(HubRequest::Overview { .. })
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        hub.key(KeyAction::ToggleSidebar);
        terminal.draw(|frame| hub.render(frame, &focused)).unwrap();
        assert!(hub.navigation.overlay);
        hub.paste("not a directory query");
        assert!(hub.query.is_empty());
        assert!(matches!(hub.key(KeyAction::Escape), Action::None));
        assert!(!hub.navigation.selecting);
        terminal.draw(|frame| hub.render(frame, &focused)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        for label in ["FlowHub", "WatchHub", "Worktrees", "F2 sessions"] {
            assert!(text.contains(label), "missing {label}: {text}");
        }
        let mut history = History::default();
        hub.navigation.catalog.sessions.push((
            SessionTarget::Persistent(owner.session_id.0),
            "Example".into(),
        ));
        // Catalog ownership comes from inspected pages, never from the view's selection.
        history.catalog.sessions = hub.navigation.catalog.sessions.clone();
        history.save(hub);
        let (restored, _) = history.open(Action::Return, HubRequest::Sessions { before: None });
        assert_eq!(restored.navigation.catalog.sessions.len(), 1);
    }
}
