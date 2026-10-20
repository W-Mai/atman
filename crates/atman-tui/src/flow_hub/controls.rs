//! View-local target selection and immutable operator control submissions.

use atman_runtime::{
    event::AtmanUuid,
    flow_runtime::host::ControlServiceIssue,
    flow_store::{
        FlowRef,
        control::ControlAction,
        profile::{ControlBatchRecord, ControlProgress},
    },
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};
use uuid::Uuid;

use super::{
    Action, HubRequest,
    page::{Row, RowKey},
    text::TextDetails,
};
use crate::{keys::KeyAction, settings_form};

#[derive(Clone, Debug)]
pub struct OperatorControl {
    pub target: FlowRef,
    pub expected_revision: u64,
    pub action: ControlAction,
    pub pause_key: Option<Uuid>,
}

#[derive(Clone, Debug)]
pub struct OperatorControls {
    pub id: Uuid,
    pub commands: Vec<OperatorControl>,
}

pub struct ControlBatchPage {
    pub record: ControlBatchRecord,
    pub issues: Vec<ControlServiceIssue>,
}

impl ControlBatchPage {
    pub(super) fn rows(&self) -> Vec<Row> {
        let mut rows = vec![Row {
            key: RowKey::ControlField("batch".into()),
            title: "Control batch · snapshot".into(),
            detail: format!(
                "{}\nCtrl+R refresh · Accepted does not mean applied",
                self.record.id
            ),
            action: Action::None,
        }];
        for item in &self.record.items {
            let label = match item.history.last().map(|entry| &entry.progress) {
                None => "Pending delivery",
                Some(ControlProgress::Accepted { .. }) => "Accepted · awaiting application",
                Some(ControlProgress::Applied { .. }) => "Applied",
                Some(ControlProgress::Failed { .. }) => "Failed",
            };
            let mut detail = format!(
                "{}/{}\n{} · expected revision {}",
                item.request.target.session_id,
                item.request.target.instance_id,
                action_label(item.request.action),
                item.request.expected_revision
            );
            for progress in &item.history {
                match &progress.progress {
                    ControlProgress::Accepted { receipt }
                    | ControlProgress::Applied { receipt } => detail.push_str(&format!(
                        "\n{} · journal {} · sequence {}\nDigest: {}",
                        if matches!(progress.progress, ControlProgress::Accepted { .. }) {
                            "Accepted"
                        } else {
                            "Applied"
                        },
                        receipt.journal_id,
                        receipt.sequence,
                        receipt.digest
                    )),
                    ControlProgress::Failed { reason } => {
                        detail.push_str(&format!("\nFailed: {reason}"))
                    }
                }
            }
            for issue in &self.issues {
                if let ControlServiceIssue::Target {
                    sender,
                    batch_id,
                    target,
                    message,
                } = issue
                    && sender == &self.record.sender
                    && batch_id == &self.record.id
                    && target == &item.request.target
                {
                    detail.push_str(&format!("\nWaiting: {message}"));
                }
            }
            rows.push(Row {
                key: RowKey::ControlField(format!("{:?}", item.request.target)),
                title: format!(
                    "{label} · {}",
                    super::page::short_flow(&item.request.target)
                ),
                detail,
                action: Action::None,
            });
        }
        for issue in &self.issues {
            if !matches!(issue, ControlServiceIssue::Target { .. }) {
                rows.push(Row {
                    key: RowKey::ControlField(issue.to_string()),
                    title: "Control service unavailable".into(),
                    detail: issue.to_string(),
                    action: Action::None,
                });
            }
        }
        if self.record.items.iter().any(|item| !item.is_terminal()) {
            rows.push(Row { key: RowKey::ControlField("retry".into()), title: "Retry pending delivery".into(), detail: "Reuse the original requests and revisions. Applied and failed targets remain unchanged.".into(), action: Action::RetryControls(self.record.id) });
        }
        rows.push(Row {
            key: RowKey::ControlField("history".into()),
            title: "Control history".into(),
            detail: "Browse previous submissions".into(),
            action: Action::Load(HubRequest::ControlHistory { before: None }),
        });
        rows
    }
}

pub(super) fn history_rows(
    page: &atman_runtime::flow_store::profile::ControlBatchPage,
) -> Vec<Row> {
    page.batches
        .iter()
        .map(|batch| {
            let status = if batch.failed > 0 {
                format!("{} failed", batch.failed)
            } else if batch.pending + batch.accepted > 0 {
                format!("{} unfinished", batch.pending + batch.accepted)
            } else {
                format!("{} applied", batch.applied)
            };
            Row {
                key: RowKey::ControlField(batch.id.to_string()),
                title: format!(
                    "{status} · {} targets",
                    batch.pending + batch.accepted + batch.applied + batch.failed,
                ),
                detail: format!(
                    "{}\n{} pending · {} accepted · {} applied · {} failed",
                    batch.id, batch.pending, batch.accepted, batch.applied, batch.failed
                ),
                action: Action::Load(HubRequest::ControlBatch {
                    request_id: batch.id,
                }),
            }
        })
        .collect()
}

pub(super) struct Target {
    pub title: String,
    pub state: atman_rt::FlowInstance<AtmanUuid>,
}

pub(super) struct Draft {
    pub picking: bool,
    pub origin: HubRequest,
    targets: Vec<Target>,
    id: Uuid,
    action: usize,
    pause: usize,
    focus: usize,
    hovered: Option<usize>,
    buttons: [Rect; 6],
    reader: TextDetails,
    error: Option<String>,
}

const ACTIONS: [ControlAction; 6] = [
    ControlAction::Pause,
    ControlAction::Resume,
    ControlAction::CancelTurn,
    ControlAction::Recover,
    ControlAction::Close,
    ControlAction::ReleasePause,
];
const LABELS: [&str; 6] = [
    "Pause",
    "Resume",
    "Cancel turn",
    "Recover",
    "Close",
    "Release pause",
];

fn action_label(action: ControlAction) -> &'static str {
    LABELS[ACTIONS
        .iter()
        .position(|candidate| *candidate == action)
        .unwrap()]
}

impl Draft {
    pub fn identity(&self) -> Uuid {
        self.id
    }
    pub fn new(target: Option<Target>, origin: HubRequest) -> Self {
        let mut draft = Self {
            picking: false,
            origin,
            targets: target.into_iter().collect(),
            id: Uuid::new_v4(),
            action: 0,
            pause: 0,
            focus: 4,
            hovered: None,
            buttons: [Rect::default(); 6],
            reader: TextDetails::new(String::new(), vec![]),
            error: None,
        };
        draft.rebuild();
        draft
    }

    pub fn contains(&self, address: &FlowRef) -> bool {
        self.targets
            .iter()
            .any(|target| target.state.address() == address)
    }

    pub fn remove(&mut self, address: &FlowRef) -> bool {
        let Some(index) = self
            .targets
            .iter()
            .position(|target| target.state.address() == address)
        else {
            return false;
        };
        self.targets.remove(index);
        self.pause = 0;
        self.changed();
        true
    }

    pub fn toggle(&mut self, target: Target) {
        if self.remove(target.state.address()) {
            return;
        }
        self.targets.push(target);
        self.pause = 0;
        self.changed();
    }

    fn pause_keys(&self) -> &[AtmanUuid] {
        if self.targets.len() == 1 {
            self.targets[0].state.pauses()
        } else {
            &[]
        }
    }

    fn request(&self) -> Option<OperatorControls> {
        if self.targets.is_empty() {
            return None;
        }
        let action = ACTIONS[self.action];
        let pause_key = if action == ControlAction::ReleasePause {
            Some(self.pause_keys().get(self.pause)?.0)
        } else {
            None
        };
        Some(OperatorControls {
            id: self.id,
            commands: self
                .targets
                .iter()
                .map(|target| OperatorControl {
                    target: target.state.address().clone(),
                    expected_revision: target.state.revision(),
                    action,
                    pause_key,
                })
                .collect(),
        })
    }

    fn changed(&mut self) {
        self.id = Uuid::new_v4();
        self.error = None;
        self.rebuild();
    }

    fn rebuild(&mut self) {
        let mut text = vec![format!("{} targets · {}\n", self.targets.len(), LABELS[self.action]),
            match ACTIONS[self.action] {
                ControlAction::Pause => "Pause execution at suspension boundaries. The active turn and pending messages remain.",
                ControlAction::Resume => "Release only your operator pause. Other controllers and Goal automation remain unchanged.",
                ControlAction::CancelTurn => "Cancel the active turn. The Flow, history and pending messages remain.",
                ControlAction::Recover => "Recover interrupted execution. Unknown effects are not replayed and other pauses remain.",
                ControlAction::Close => "Permanently close these instances and stop their resources. History remains readable.",
                ControlAction::ReleasePause => "Release one selected pause on a single Flow. Other pauses and the active turn remain.",
            }.into()];
        if ACTIONS[self.action] == ControlAction::ReleasePause {
            text.push(self.pause_keys().get(self.pause).map_or_else(
                || "Select one Flow with a recorded pause.".into(),
                |key| format!("Selected pause: {key}"),
            ));
        }
        if let Some(error) = &self.error {
            text.push(format!("\nNot confirmed: {error}\nRetry retains the original request; change targets or action to create a new request."));
        }
        for target in &self.targets {
            text.push(format!(
                "\n{}\n{}/{} · revision {} · {} · {} pauses",
                target.title,
                target.state.address().session_id,
                target.state.address().instance_id,
                target.state.revision(),
                super::execution_label(target.state.state()),
                target.state.pauses().len()
            ));
        }
        self.reader = TextDetails::new("FlowHub / Review controls".into(), text);
    }

    pub fn key(&mut self, key: KeyAction, pending: bool) -> Action {
        if key == KeyAction::Escape {
            return Action::Return;
        }
        if pending {
            self.reader.key(key);
            return Action::None;
        }
        match key {
            KeyAction::Tab => self.focus = (self.focus + 1) % 6,
            KeyAction::BackTab => self.focus = (self.focus + 5) % 6,
            KeyAction::RedirectPrefill => {
                return Action::Load(HubRequest::ControlHistory { before: None });
            }
            KeyAction::Save => {
                return self.request().map_or(Action::None, |request| {
                    Action::SubmitControls(Box::new(request))
                });
            }
            KeyAction::CursorLeft | KeyAction::CursorRight if self.focus > 1 => (),
            KeyAction::Submit
            | KeyAction::Char(' ')
            | KeyAction::CursorLeft
            | KeyAction::CursorRight => match self.focus {
                0 => {
                    self.action =
                        (self.action + if key == KeyAction::CursorLeft { 5 } else { 1 }) % 6;
                    self.changed();
                }
                1 if ACTIONS[self.action] == ControlAction::ReleasePause
                    && !self.pause_keys().is_empty() =>
                {
                    self.pause = (self.pause
                        + if key == KeyAction::CursorLeft {
                            self.pause_keys().len() - 1
                        } else {
                            1
                        })
                        % self.pause_keys().len();
                    self.changed();
                }
                2 => {
                    self.picking = true;
                    return Action::Load(HubRequest::Overview { after: None });
                }
                3 => {
                    return self.request().map_or(Action::None, |request| {
                        Action::SubmitControls(Box::new(request))
                    });
                }
                4 => return Action::Return,
                5 => return Action::Load(HubRequest::ControlHistory { before: None }),
                _ => (),
            },
            _ => {
                self.reader.key(key);
            }
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent, pending: bool) -> Action {
        self.hovered = self
            .buttons
            .iter()
            .position(|rect| rect.contains((event.column, event.row).into()));
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(index) = self.hovered
        {
            self.focus = index;
            if index == 4 {
                return Action::Return;
            }
            return self.key(KeyAction::Submit, pending);
        }
        if self.reader.mouse(event) {
            return Action::Return;
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, pending: bool, error: Option<&str>) {
        self.buttons = [Rect::default(); 6];
        if area.width < 40 || area.height < 24 {
            self.reader.back = Rect::default();
            frame.render_widget(Paragraph::new("Controls need 40×24 · Esc back"), area);
            return;
        }
        if self.error.as_deref() != error {
            self.error = error.map(str::to_owned);
            self.rebuild();
        }
        let width = area.width.saturating_sub(4).min(100);
        let left = area.x + (area.width - width) / 2;
        self.reader.render(
            frame,
            Rect::new(area.x, area.y, area.width, area.height - 8),
        );
        let cell = (width - 1) / 2;
        for index in 0..4 {
            self.buttons[index] = Rect::new(
                left + (index % 2) as u16 * (cell + 1),
                area.bottom() - 8 + (index / 2) as u16 * 4,
                cell,
                3,
            );
        }
        self.buttons[4] = self.reader.back;
        self.buttons[4].width = cell;
        self.reader.back = self.buttons[4];
        self.buttons[5] = Rect::new(left + cell + 1, self.reader.back.y, cell, 3);
        let label = if cell < 25 {
            LABELS[self.action].to_owned()
        } else {
            format!("Action: {} ↔", LABELS[self.action])
        };
        let key_label = format!(
            "Pause key {}/{}",
            (self.pause + 1).min(self.pause_keys().len()),
            self.pause_keys().len()
        );
        let labels = [
            label.as_str(),
            key_label.as_str(),
            "Targets",
            if pending {
                "Submitting"
            } else {
                "Submit Ctrl+S"
            },
            "Back Esc",
            "History Ctrl+R",
        ];
        let t = crate::theme::theme();
        for (index, rect) in self.buttons.iter().copied().enumerate() {
            settings_form::render_action(
                frame,
                rect,
                &settings_form::Action {
                    label: labels[index],
                    focused: self.focus == index,
                    hovered: self.hovered == Some(index),
                    disabled: (pending && index != 4)
                        || (index == 1
                            && (ACTIONS[self.action] != ControlAction::ReleasePause
                                || self.pause_keys().is_empty()))
                        || (index == 3 && self.request().is_none()),
                    ..Default::default()
                },
                &t,
            );
        }
        frame.render_widget(
            Paragraph::new("Tab focus · Enter act · ←→ change")
                .style(Style::default().fg(t.meta_fg.into())),
            Rect::new(left, area.bottom() - 1, width, 1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::FlowInstance;
    use ratatui::{Terminal, backend::TestBackend};

    fn target(address: FlowRef) -> Target {
        let mut state = FlowInstance::prepared(address);
        state.activate().unwrap();
        Target {
            title: "Worker".into(),
            state,
        }
    }

    fn address() -> FlowRef {
        FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: atman_rt::FlowInstanceId::now(),
        }
    }

    #[test]
    fn control_draft_keeps_immutable_identity_and_requires_explicit_activation() {
        let first = address();
        let mut other = first.clone();
        other.session_id = Uuid::new_v4().into();
        let mut draft = Draft::new(
            Some(target(first.clone())),
            HubRequest::Overview { after: None },
        );
        let initial = draft.request().unwrap();
        draft.focus = 3;
        for key in [KeyAction::CursorLeft, KeyAction::CursorRight] {
            assert!(matches!(draft.key(key, false), Action::None));
            assert_eq!(draft.identity(), initial.id);
        }
        assert!(
            matches!(draft.key(KeyAction::Save, false), Action::SubmitControls(request) if request.id == initial.id)
        );
        assert!(matches!(draft.key(KeyAction::Save, true), Action::None));
        draft.toggle(target(other.clone()));
        assert_ne!(draft.identity(), initial.id);
        assert_eq!(draft.request().unwrap().commands.len(), 2);
        assert!(draft.contains(&first) && draft.contains(&other));
        assert_eq!(
            draft.request().unwrap().commands[0].expected_revision,
            initial.commands[0].expected_revision
        );
        draft.focus = 0;
        draft.key(KeyAction::CursorLeft, false);
        assert_eq!(ACTIONS[draft.action], ControlAction::ReleasePause);
        assert!(draft.request().is_none());
        draft.toggle(target(other));
        let pause = Uuid::new_v4();
        draft.targets[0].state.pause(pause.into()).unwrap();
        assert_eq!(draft.request().unwrap().commands[0].pause_key, Some(pause));
        let retry = draft.identity();
        draft.key(KeyAction::PageDown, false);
        assert_eq!(draft.identity(), retry);
        assert!(matches!(draft.key(KeyAction::Escape, true), Action::Return));
    }

    #[test]
    fn control_review_retains_padded_mouse_targets_at_minimum_size() {
        for (width, height) in [(40, 24), (120, 40)] {
            let mut draft = Draft::new(
                Some(target(address())),
                HubRequest::Overview { after: None },
            );
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| draft.render(frame, frame.area(), false, None))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Review controls"));
            assert!(!text.contains("need 40"));
            assert!(text.contains("Submit Ctrl+S"));
            for (index, rect) in draft.buttons.iter().enumerate() {
                assert_eq!(rect.height, 3);
                assert!(
                    Rect::new(0, 0, width, height)
                        .contains((rect.right() - 1, rect.bottom() - 1).into())
                );
                for other in draft.buttons.iter().skip(index + 1) {
                    assert!(rect.intersection(*other).is_empty());
                }
            }
            let submit = draft.buttons[3];
            let event = MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: submit.x + 1,
                row: submit.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            assert!(matches!(
                draft.mouse(event, false),
                Action::SubmitControls(_)
            ));
            assert!(matches!(draft.mouse(event, true), Action::None));
            let back = draft.buttons[4];
            assert!(matches!(
                draft.mouse(
                    MouseEvent {
                        column: back.x + 1,
                        row: back.y + 1,
                        ..event
                    },
                    true
                ),
                Action::Return
            ));
        }
    }

    #[test]
    fn control_picker_retains_selection_across_pages_and_owns_its_receipts() {
        use super::super::{FlowHub, FlowHubPage, History};
        use crate::control::{ControlTicket, ViewBinding};
        use atman_runtime::{
            event_log::journal::CommitReceipt,
            flow_runtime::access::{InstancePage, InstanceSummary},
            flow_store::{inbox::SenderIdentity, profile::ControlBatchItem},
        };
        let first = address();
        let second = address();
        let page = |address: &FlowRef| {
            Box::new(FlowHubPage::Flows {
                session: address.session_id.0,
                title: "Session".into(),
                default_flow: None,
                relations: Ok(vec![]),
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        archived: false,
                        address: address.clone(),
                        entry: Some("Worker".into()),
                        state: Some(target(address.clone()).state),
                        unavailable: None,
                        loaded: true,
                        trailing_bytes: None,
                        activity: None,
                    }],
                    next: None,
                    total: 1,
                    catalog: CommitReceipt {
                        journal_id: address.session_id.0,
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
            })
        };
        let mut hub = FlowHub::default();
        let mut history = History::default();
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 1,
                flow: first.clone().into(),
            },
            request_id: Uuid::new_v4(),
        };
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Ok(page(&first)));
        hub.key(KeyAction::CycleReasoning);
        hub.controls.as_mut().unwrap().picking = true;
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Ok(page(&second)));
        hub.key(KeyAction::Submit);
        assert_eq!(
            hub.controls
                .as_ref()
                .unwrap()
                .request()
                .unwrap()
                .commands
                .len(),
            2
        );
        if let Some(FlowHubPage::Flows { page, .. }) = &mut hub.page {
            page.instances[0].state = None;
        }
        hub.key(KeyAction::Submit);
        assert_eq!(
            hub.controls
                .as_ref()
                .unwrap()
                .request()
                .unwrap()
                .commands
                .len(),
            1
        );
        if let Some(FlowHubPage::Flows { page, .. }) = &mut hub.page {
            page.instances[0].state = Some(target(second.clone()).state);
        }
        hub.key(KeyAction::Submit);
        hub.key(KeyAction::CycleReasoning);
        let preserved = hub.controls.as_ref().unwrap().identity();
        assert!(matches!(
            hub.key(KeyAction::RedirectPrefill),
            Action::Load(HubRequest::ControlHistory { before: None })
        ));
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::ControlHistory(
                atman_runtime::flow_store::profile::ControlBatchPage {
                    batches: vec![],
                    next: None,
                    total: 0,
                },
            ))),
        );
        assert!(hub.controls.is_none());
        let back = hub.key(KeyAction::Escape);
        (hub, _) = history.open(back, HubRequest::Overview { after: None });
        assert_eq!(hub.controls.as_ref().unwrap().identity(), preserved);
        let Action::SubmitControls(request) = hub.key(KeyAction::Save) else {
            panic!("missing batch")
        };
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        let stale = ControlTicket {
            request_id: Uuid::new_v4(),
            ..ticket.clone()
        };
        history.receive(&mut hub, Some(&stale), Err("stale".into()));
        assert_eq!(hub.pending.as_ref(), Some(&ticket));
        history.receive(&mut hub, Some(&ticket), Err("transport unavailable".into()));
        assert!(
            matches!(hub.key(KeyAction::Save), Action::SubmitControls(retry) if retry.id == request.id)
        );
        hub.pending = Some(ticket.clone());
        let sender = SenderIdentity::User(Uuid::new_v4());
        let result = ControlBatchPage {
            issues: vec![],
            record: ControlBatchRecord {
                id: request.id,
                sender: sender.clone(),
                submitted: CommitReceipt {
                    journal_id: Uuid::new_v4(),
                    sequence: 1,
                    digest: "test".into(),
                },
                items: request
                    .commands
                    .iter()
                    .map(|command| ControlBatchItem {
                        request: atman_runtime::flow_store::control::ControlRequest {
                            id: request.id,
                            sender: sender.clone(),
                            target: command.target.clone(),
                            expected_revision: command.expected_revision,
                            action: command.action,
                            pause_key: Uuid::new_v4().into(),
                        },
                        history: vec![],
                    })
                    .collect(),
            },
        };
        history.receive(
            &mut hub,
            Some(&ticket),
            Ok(Box::new(FlowHubPage::ControlBatch(Box::new(result)))),
        );
        assert!(hub.controls.is_none());
        assert!(hub.rows[1].has_text_details());
        assert!(
            hub.rows
                .iter()
                .any(|row| matches!(row.action, Action::RetryControls(id) if id == request.id))
        );
        assert_eq!(
            hub.rows.iter().filter(|row| row.has_text_details()).count(),
            3
        );
        if let Some(FlowHubPage::ControlBatch(page)) = &mut hub.page {
            for item in &mut page.record.items {
                item.history
                    .push(atman_runtime::flow_store::profile::ControlProgressRecord {
                        progress: ControlProgress::Failed {
                            reason: "terminal conflict".into(),
                        },
                        recorded: page.record.submitted.clone(),
                    });
            }
            assert!(
                !page
                    .rows()
                    .iter()
                    .any(|row| matches!(row.action, Action::RetryControls(_)))
            );
        }
        let action = hub.key(KeyAction::Escape);
        let (restored, _) = history.open(action, HubRequest::Overview { after: None });
        assert_eq!(restored.page.unwrap().first(), page(&first).first());
    }
}
