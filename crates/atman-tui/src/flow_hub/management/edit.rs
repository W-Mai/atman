use std::collections::BTreeSet;

use atman_runtime::flow_store::{
    FlowRef,
    access::{AccessTarget, FlowAction, FlowGrant},
    profile::ReplaceManagement,
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Paragraph, Wrap},
};
use uuid::Uuid;

use super::{ManagementPage, action_label, target};
use crate::{
    flow_hub::{Action, HubRequest, page::RowKey},
    keys::KeyAction,
    project_hub::SessionTarget,
    settings_form::{self, Field, FieldHit, FormLayout},
};

const PERMISSIONS: [FlowAction; 10] = [
    FlowAction::Discover,
    FlowAction::ReadOutput,
    FlowAction::ReadContext,
    FlowAction::Send,
    FlowAction::Watch,
    FlowAction::Control,
    FlowAction::Rename,
    FlowAction::ManageGoal,
    FlowAction::Delegate,
    FlowAction::ApprovePermission,
];
const FIELDS: usize = 3 + PERMISSIONS.len();

#[derive(Clone, Copy)]
enum Pick {
    Source,
    Flow,
    Session,
}

pub(crate) struct Draft {
    origin: HubRequest,
    original: Option<FlowGrant>,
    base: ReplaceManagement,
    id: Uuid,
    source: Option<FlowRef>,
    target: Option<AccessTarget>,
    scope: usize,
    actions: BTreeSet<FlowAction>,
    pick: Option<Pick>,
    selected: usize,
    request: Option<ReplaceManagement>,
    error: Option<String>,
    hits: Vec<(usize, FieldHit)>,
    buttons: Vec<Rect>,
    hovered: Option<usize>,
}

impl Draft {
    pub fn new(page: &ManagementPage) -> Result<Self, String> {
        if let Some(error) = &page.view.unavailable {
            return Err(error.clone());
        }
        let binding = page
            .view
            .profile
            .management
            .clone()
            .ok_or("Initialize management first")?;
        let original = page
            .grant
            .map(|id| {
                binding
                    .grants
                    .iter()
                    .find(|grant| grant.id == id)
                    .cloned()
                    .ok_or("Grant is no longer present")
            })
            .transpose()?;
        Ok(Self {
            origin: HubRequest::Management { grant: page.grant },
            id: original
                .as_ref()
                .map_or_else(Uuid::now_v7, |grant| grant.id),
            source: original.as_ref().map(|grant| grant.grantee.clone()),
            target: original.as_ref().map(|grant| grant.target.clone()),
            scope: original.as_ref().map_or(0, |grant| match grant.target {
                AccessTarget::Flow(_) => 0,
                AccessTarget::Session(_) => 1,
                AccessTarget::AllSessions => 2,
            }),
            actions: original
                .as_ref()
                .map(|grant| grant.actions.clone())
                .unwrap_or_default(),
            original,
            base: ReplaceManagement {
                request_id: Uuid::now_v7(),
                expected: page.view.profile.cursor.clone(),
                binding,
            },
            pick: None,
            selected: 0,
            request: None,
            error: None,
            hits: vec![],
            buttons: vec![],
            hovered: None,
        })
    }

    pub fn picking(&self) -> bool {
        self.pick.is_some()
    }

    pub fn picker_request(&self) -> Option<HubRequest> {
        self.pick.map(|pick| match pick {
            Pick::Source => HubRequest::Flows {
                session: self.base.binding.session.id,
                before: None,
            },
            Pick::Flow => HubRequest::Overview { after: None },
            Pick::Session => HubRequest::Sessions { before: None },
        })
    }

    pub fn picker_label(&self) -> &'static str {
        match self.pick {
            Some(Pick::Source) => "Select grantee",
            Some(Pick::Flow) => "Select target flow",
            _ => "Select target session",
        }
    }

    fn changed(&mut self) {
        self.request = None;
        self.error = None;
    }

    pub fn choose(&mut self, key: &RowKey) -> Result<bool, String> {
        let Some(pick) = self.pick else {
            return Ok(false);
        };
        let previous = (self.source.clone(), self.target.clone());
        match (pick, key) {
            (Pick::Source | Pick::Flow, RowKey::Flow(address)) => {
                let flow = address.instance().ok_or("Select a persistent Flow")?;
                if matches!(pick, Pick::Source) {
                    if flow.session_id.0 != self.base.binding.session.id {
                        return Err(
                            "The source Flow must belong to the bound management Session".into(),
                        );
                    }
                    self.source = Some(flow.clone());
                } else {
                    self.target = Some(AccessTarget::Flow(flow.clone()));
                }
            }
            (Pick::Session, RowKey::Session(SessionTarget::Persistent(id))) => {
                self.target = Some(AccessTarget::Session(*id))
            }
            (Pick::Session, RowKey::Session(_)) => return Err("Select a persistent Session".into()),
            _ => return Ok(false),
        }
        self.pick = None;
        if previous != (self.source.clone(), self.target.clone()) {
            self.changed();
        }
        Ok(true)
    }

    fn submit(&mut self) -> Result<Action, String> {
        if let Some(request) = &self.request {
            return Ok(Action::UpdateManagement(Box::new(request.clone())));
        }
        let grant = FlowGrant {
            id: self.id,
            grantee: self.source.clone().ok_or("Select a source Flow")?,
            target: self.target.clone().ok_or("Select a target")?,
            actions: self.actions.clone(),
        };
        if grant.actions.is_empty() {
            return Err("Select at least one permission".into());
        }
        if self.original.as_ref() == Some(&grant) {
            return Err("No changes to save".into());
        }
        let mut request = self.base.clone();
        request.request_id = Uuid::now_v7();
        if let Some(old) = request
            .binding
            .grants
            .iter_mut()
            .find(|old| old.id == grant.id)
        {
            *old = grant;
        } else {
            request.binding.grants.push(grant);
        }
        self.request = Some(request.clone());
        Ok(Action::UpdateManagement(Box::new(request)))
    }

    pub fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            if self.pick.take().is_some() {
                return Action::None;
            }
            return Action::Load(self.origin.clone());
        }
        if pending {
            return Action::None;
        }
        if self.picking() {
            return Action::None;
        }
        if key == KeyAction::Save || (key == KeyAction::Submit && self.selected == FIELDS) {
            return match self.submit() {
                Ok(action) => action,
                Err(error) => {
                    self.error = Some(error);
                    Action::None
                }
            };
        }
        match key {
            KeyAction::Tab | KeyAction::HistoryDown => {
                self.selected = (self.selected + 1) % (FIELDS + 2)
            }
            KeyAction::BackTab | KeyAction::HistoryUp => {
                self.selected = (self.selected + FIELDS + 1) % (FIELDS + 2)
            }
            KeyAction::Submit if self.selected == FIELDS + 1 => {
                return Action::Load(self.origin.clone());
            }
            KeyAction::Submit
            | KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::Char(' ') => match self.selected {
                0 => {
                    self.pick = Some(Pick::Source);
                    return Action::Load(self.picker_request().unwrap());
                }
                1 => {
                    self.scope =
                        (self.scope + if key == KeyAction::CursorLeft { 2 } else { 1 }) % 3;
                    self.target = (self.scope == 2).then_some(AccessTarget::AllSessions);
                    self.changed();
                }
                2 if self.scope != 2 => {
                    self.pick = Some(if self.scope == 0 {
                        Pick::Flow
                    } else {
                        Pick::Session
                    });
                    return Action::Load(self.picker_request().unwrap());
                }
                index @ 3..FIELDS => {
                    let action = PERMISSIONS[index - 3];
                    if !self.actions.remove(&action) {
                        self.actions.insert(action);
                    }
                    self.changed();
                }
                _ => {}
            },
            _ => {}
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        let point = (event.column, event.row).into();
        self.hovered = self
            .hits
            .iter()
            .find(|(_, hit)| hit.record.contains(point))
            .map(|(index, _)| *index)
            .or_else(|| {
                self.buttons
                    .iter()
                    .position(|rect| rect.contains(point))
                    .map(|index| FIELDS + index)
            });
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(index) = self.hovered
        {
            if index == FIELDS + 1 {
                return self.key(KeyAction::Escape, pending);
            }
            if pending && index != FIELDS + 1 {
                return Action::None;
            }
            self.selected = index;
            let left = self
                .hits
                .iter()
                .find(|(index, _)| *index == self.selected)
                .and_then(|(_, hit)| hit.arrows)
                .is_some_and(|(left, _)| left.contains(point));
            return self.key(
                if left {
                    KeyAction::CursorLeft
                } else {
                    KeyAction::Submit
                },
                pending,
            );
        }
        match event.kind {
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown, pending),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp, pending),
            _ => Action::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, pending: bool, error: Option<&str>) {
        self.hits.clear();
        self.buttons.clear();
        if area.width < 40 || area.height < 16 {
            frame.render_widget(Paragraph::new("Grant form needs 40×16 · Esc back"), area);
            return;
        }
        let theme = crate::theme::theme();
        let width = area.width.saturating_sub(4).min(100);
        let height = area.height.saturating_sub(2).min(46);
        let content = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(
            Paragraph::new(if self.original.is_some() {
                "Management / Edit grant"
            } else {
                "Management / New grant"
            })
            .style(Style::default().fg(theme.heading.into())),
            Rect::new(content.x + 1, content.y, width - 2, 1),
        );
        let layout = FormLayout::with_actions(
            Rect::new(content.x, content.y + 2, width, height - 2),
            FIELDS,
            self.selected,
            2,
        );
        let source = self.source.as_ref().map_or_else(
            || "Select a Flow".into(),
            |source| {
                format!(
                    "Flow {} · Session {}",
                    source.instance_id, source.session_id
                )
            },
        );
        let destination = self
            .target
            .as_ref()
            .map(target)
            .unwrap_or_else(|| "Select a target".into())
            .replace('\n', " · ");
        for (index, rect) in layout.fields {
            let (label, value) = match index {
                0 => ("Source Flow", source.as_str()),
                1 => (
                    "Target scope",
                    ["One Flow", "One Session", "All sessions on this host"][self.scope],
                ),
                2 => ("Target", destination.as_str()),
                _ => (
                    action_label(PERMISSIONS[index - 3]),
                    if self.actions.contains(&PERMISSIONS[index - 3]) {
                        "Included"
                    } else {
                        "Not included"
                    },
                ),
            };
            let hit = settings_form::render_field(
                frame,
                rect,
                &Field {
                    label,
                    value,
                    focused: self.selected == index && !pending,
                    hovered: self.hovered == Some(index) && !pending,
                    selector: true,
                    locked: pending || (index == 2 && self.scope == 2),
                    ..Default::default()
                },
                &theme,
            );
            self.hits.push((index, hit));
        }
        let feedback = if pending {
            "Waiting for host acknowledgement"
        } else {
            self.error.as_deref().or(error).unwrap_or(
                "Only selected permissions are granted. Saving does not start or resume execution.",
            )
        };
        frame.render_widget(
            Paragraph::new(feedback)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(theme.meta_fg.into())),
            layout.info,
        );
        for (index, rect) in layout.actions.iter().copied().enumerate() {
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label: if index == 0 { "Save" } else { "Back" },
                    shortcut: if index == 0 { "Ctrl+S" } else { "Esc" },
                    focused: self.selected == FIELDS + index,
                    hovered: self.hovered == Some(FIELDS + index),
                    disabled: pending && index == 0,
                },
                &theme,
            );
        }
        self.buttons = layout.actions;
        frame.render_widget(
            Paragraph::new("Tab field · Enter select · Ctrl+S save · Esc back")
                .style(Style::default().fg(theme.meta_fg.into())),
            layout.help,
        );
        if layout.scroll.height > 0 {
            frame.render_widget(
                Paragraph::new(format!(
                    "Field {} / {FIELDS} · ↑↓ scroll",
                    (self.selected + 1).min(FIELDS)
                ))
                .style(Style::default().fg(theme.meta_fg.into())),
                layout.scroll,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, History},
    };
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_runtime::{
            access::{InstancePage, InstanceSummary},
            host::ProfileView,
        },
        flow_store::{
            FlowInstanceId, SessionDescriptor,
            profile::{ManagementBinding, ProfileSnapshot},
        },
    };
    use crossterm::event::KeyModifiers;

    fn fixture() -> (ProfileView, FlowRef) {
        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        (
            ProfileView {
                unavailable: None,
                profile: ProfileSnapshot {
                    profile_id: Uuid::now_v7(),
                    cursor: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "profile".into(),
                    },
                    management: Some(ManagementBinding {
                        session: SessionDescriptor {
                            id: source.session_id.0,
                            project: None,
                            configuration: Default::default(),
                            title: "Atman".into(),
                        },
                        grants: vec![],
                    }),
                },
            },
            source,
        )
    }

    #[test]
    fn grant_editor_keeps_explicit_scope_permissions_and_frozen_requests() {
        let (view, source) = fixture();
        let mut draft = Draft::new(&ManagementPage::new(view.clone(), None).unwrap()).unwrap();
        assert!(draft.actions.is_empty());
        assert!(draft.target.is_none());
        assert!(draft.submit().is_err());
        assert!(
            matches!(draft.key(KeyAction::Submit, false), Action::Load(HubRequest::Flows { session, .. }) if session == source.session_id.0)
        );
        let foreign = FlowRef {
            session_id: Uuid::now_v7().into(),
            ..source.clone()
        };
        assert!(draft.choose(&RowKey::Flow(foreign.clone().into())).is_err());
        assert!(draft.source.is_none());
        draft.choose(&RowKey::Flow(source.clone().into())).unwrap();
        draft.selected = 2;
        assert!(matches!(
            draft.key(KeyAction::Submit, false),
            Action::Load(HubRequest::Overview { .. })
        ));
        draft.choose(&RowKey::Flow(foreign.clone().into())).unwrap();
        assert_eq!(draft.target, Some(AccessTarget::Flow(foreign.clone())));
        draft.selected = 1;
        draft.key(KeyAction::CursorRight, false);
        assert!(draft.target.is_none());
        draft.selected = 2;
        assert!(matches!(
            draft.key(KeyAction::Submit, false),
            Action::Load(HubRequest::Sessions { .. })
        ));
        draft
            .choose(&RowKey::Session(SessionTarget::Persistent(
                foreign.session_id.0,
            )))
            .unwrap();
        assert_eq!(
            draft.target,
            Some(AccessTarget::Session(foreign.session_id.0))
        );
        draft.selected = 1;
        draft.key(KeyAction::CursorRight, false);
        assert_eq!(draft.target, Some(AccessTarget::AllSessions));
        assert!(draft.submit().is_err());
        for index in 3..FIELDS {
            draft.selected = index;
            draft.key(KeyAction::Submit, false);
        }
        let Action::UpdateManagement(first) = draft.submit().unwrap() else {
            panic!("missing save")
        };
        assert_eq!(first.binding.grants[0].actions, PERMISSIONS.into());
        assert_eq!(first.expected, view.profile.cursor);
        let Action::UpdateManagement(retry) = draft.submit().unwrap() else {
            panic!("missing retry")
        };
        assert_eq!(first, retry);
        draft.selected = 0;
        draft.key(KeyAction::Submit, false);
        draft.choose(&RowKey::Flow(source.clone().into())).unwrap();
        assert_eq!(draft.request.as_ref(), Some(first.as_ref()));
        draft.selected = 3;
        draft.key(KeyAction::Submit, true);
        assert_eq!(draft.request.as_ref(), Some(first.as_ref()));
        draft.key(KeyAction::Submit, false);
        let Action::UpdateManagement(changed) = draft.submit().unwrap() else {
            panic!("missing changed save")
        };
        assert_ne!(first.request_id, changed.request_id);
        assert_eq!(first.expected, changed.expected);
        assert_eq!(first.binding.grants[0].id, changed.binding.grants[0].id);
        let mut persisted = view;
        persisted.profile.management = Some(changed.binding.clone());
        let mut editing = Draft::new(
            &ManagementPage::new(persisted, Some(changed.binding.grants[0].id)).unwrap(),
        )
        .unwrap();
        assert!(editing.submit().is_err());
        for (width, height) in [(40, 24), (120, 48)] {
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            editing.selected = FIELDS - 1;
            terminal
                .draw(|frame| editing.render(frame, frame.area(), false, None))
                .unwrap();
            let hit = editing.hits.last().unwrap().1.record;
            let last_permission = PERMISSIONS.last().unwrap();
            let before = editing.actions.contains(last_permission);
            editing.mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hit.x + 1,
                    row: hit.y,
                    modifiers: KeyModifiers::NONE,
                },
                false,
            );
            assert_ne!(before, editing.actions.contains(last_permission));
            let save = editing.buttons[0];
            let Action::UpdateManagement(mouse) = editing.mouse(
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: save.x + 1,
                    row: save.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
                false,
            ) else {
                panic!("missing mouse save")
            };
            let Action::UpdateManagement(key) = editing.key(KeyAction::Save, false) else {
                panic!("missing keyboard save")
            };
            assert_eq!(mouse, key);
            // Reset to the persisted draft before inspecting another viewport.
            editing.original = None;
        }
    }

    #[test]
    fn management_picker_and_save_share_history_and_original_reply_ownership() {
        let (mut view, source) = fixture();
        let mut ticket = ControlTicket {
            history_head: None,
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: source.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let mut history = History::default();
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view.clone(), None).unwrap(),
            )))),
        );
        hub.query = "New grant".into();
        assert!(matches!(hub.key(KeyAction::Submit), Action::None));
        assert!(matches!(
            hub.key(KeyAction::Submit),
            Action::Load(HubRequest::Flows { .. })
        ));
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Err("Directory unavailable".into()));
        assert!(hub.edit_management.is_some());
        assert!(
            matches!(hub.key(KeyAction::RedirectPrefill), Action::Load(HubRequest::Flows { session, .. }) if session == source.session_id.0)
        );
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Flows {
                session: source.session_id.0,
                title: "Manager flows".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        name: None,
                        archived: false,
                        address: source.clone(),
                        entry: Some("Manager".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                        activity: None,
                        observation: None,
                    }],
                    next: None,
                    total: 1,
                    catalog: view.profile.cursor.clone(),
                },
                relations: Ok(vec![]),
            })),
        );
        assert!(hub.edit_management.as_ref().unwrap().picking());
        assert!(matches!(hub.key(KeyAction::ToggleLastTool), Action::None));
        assert!(!hub.edit_management.as_ref().unwrap().picking());
        hub.key(KeyAction::Tab);
        hub.key(KeyAction::CursorRight);
        hub.key(KeyAction::CursorRight);
        hub.key(KeyAction::Tab);
        hub.key(KeyAction::Tab);
        hub.key(KeyAction::Submit);
        let Action::UpdateManagement(save) = hub.key(KeyAction::Save) else {
            panic!("missing save")
        };
        let old = ticket.clone();
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        view.profile.management = Some(save.binding.clone());
        let reply = || {
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view.clone(), None).unwrap(),
            ))))
        };
        history.receive(&mut hub, Some(&old), reply());
        assert!(hub.edit_management.is_some());
        history.receive(
            &mut hub,
            Some(&ticket),
            Err("Profile revision changed".into()),
        );
        assert!(hub.edit_management.is_some());
        let Action::UpdateManagement(retry) = hub.key(KeyAction::Save) else {
            panic!("missing retry")
        };
        assert_eq!(retry, save);
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), reply());
        assert!(hub.edit_management.is_none());
        assert_eq!(hub.query, "New grant");
        assert!(matches!(hub.page, Some(FlowHubPage::Management(_))));
        // Editing without opening a picker also closes the detail snapshot on ACK.
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view.clone(), Some(save.binding.grants[0].id)).unwrap(),
            )))),
        );
        hub.query = "Edit grant".into();
        hub.key(KeyAction::Submit);
        let super::super::Draft::Grant(draft) = hub.edit_management.as_mut().unwrap() else {
            panic!("expected grant editor")
        };
        draft.selected = 4;
        draft.key(KeyAction::Submit, false);
        let Action::UpdateManagement(edit) = hub.key(KeyAction::Save) else {
            panic!("missing edit")
        };
        view.profile.management = Some(edit.binding);
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Management(Box::new(
                ManagementPage::new(view, None).unwrap(),
            )))),
        );
        assert!(hub.edit_management.is_none());
        assert!(history.pages.iter().all(|saved| !matches!(saved.page, Some(FlowHubPage::Management(ref page)) if page.grant.is_some())));
    }
}
