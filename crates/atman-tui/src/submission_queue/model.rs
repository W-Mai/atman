use atman_runtime::flow_store::{
    FlowRef,
    inbox::{
        DeliveryMode, InboxChange, InboxEditRequest, InboxEntry, InboxMove, InboxPayload,
        UserContent,
    },
};
use atman_runtime::user_input::UserInputPresentation;
use uuid::Uuid;

use super::QueueAction;
use crate::{
    ControlSender, TuiControl,
    app::{AppState, NoteLevel, QueuedSubmissionEdit, ToastPosition},
    control::ControlTicket,
    input::InputEditor,
};

/// Source identities and revisions stay typed through display, editing, and submission.
#[derive(Clone, Debug)]
pub enum QueueItem {
    Session(atman_runtime::QueuedSubmissionView),
    Instance {
        flow: FlowRef,
        entry: Box<InboxEntry>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueKey {
    Session(atman_runtime::SubmissionId),
    Instance(FlowRef, Uuid),
}

impl QueueItem {
    pub fn key(&self) -> QueueKey {
        match self {
            Self::Session(value) => QueueKey::Session(value.id.clone()),
            Self::Instance { flow, entry } => QueueKey::Instance(flow.clone(), entry.envelope.id),
        }
    }

    pub fn editable(&self) -> bool {
        match self {
            Self::Session(_) => true,
            Self::Instance { entry, .. } => entry.envelope.request.is_editable_user_input(),
        }
    }

    pub fn text(&self) -> String {
        match self {
            Self::Session(value) => value.text.clone(),
            Self::Instance { entry, .. } => payload_text(&entry.envelope.request.payload),
        }
    }

    pub fn presentation(&self) -> Option<&UserInputPresentation> {
        match self {
            Self::Session(value) => value.presentation.as_ref(),
            Self::Instance { entry, .. } => match &entry.envelope.request.payload {
                InboxPayload::UserSubmission { content, .. } => content.presentation.as_ref(),
                InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => {
                    user.as_ref().and_then(|body| body.presentation.as_ref())
                }
                _ => None,
            },
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Session(_) => "next",
            Self::Instance { entry, .. } if self.editable() => {
                match entry.envelope.request.delivery {
                    DeliveryMode::NextTurn => "next",
                    DeliveryMode::AtBoundary => "insert",
                }
            }
            Self::Instance { entry, .. } => match entry.envelope.request.payload {
                InboxPayload::Watcher { .. } | InboxPayload::ProcessEvent { .. } => "watch",
                InboxPayload::FlowEvent { .. } | InboxPayload::Interjection { .. } => "peer",
                InboxPayload::Timer { .. } => "timer",
                _ => "internal",
            },
        }
    }

    pub fn insert_block_reason(&self) -> Option<&str> {
        match self {
            Self::Session(value) => value.insert_block_reason.as_deref(),
            Self::Instance { .. } if !self.editable() => {
                Some("runtime notifications are read-only")
            }
            Self::Instance { entry, .. } => match (
                &entry.envelope.request.payload,
                entry.envelope.request.delivery,
            ) {
                (_, DeliveryMode::AtBoundary) => {
                    Some("already waiting for a compatible model call")
                }
                (InboxPayload::Invocation { .. }, _) => {
                    Some("explicit invocation arguments require a separate turn")
                }
                _ => None,
            },
        }
    }

    fn content(&self, text: Option<String>) -> Option<UserContent> {
        let Self::Instance { entry, .. } = self else {
            return None;
        };
        let mut content = match &entry.envelope.request.payload {
            InboxPayload::UserSubmission { content, .. } => content.clone(),
            InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => {
                user.clone()?
            }
            InboxPayload::Interjection { text } => UserContent {
                text: text.clone(),
                images: vec![],
                presentation: None,
            },
            _ => return None,
        };
        if let Some(text) = text {
            content.text = match &mut content.presentation {
                Some(presentation) => {
                    presentation.prompt = text;
                    presentation.model_text()
                }
                None => text,
            };
        }
        Some(content)
    }
}

pub(crate) struct QueueRequest {
    edit: InboxEditRequest,
    ticket: Option<ControlTicket>,
    editor: Option<Uuid>,
}

impl AppState {
    pub(crate) fn queue_recovery_view(&self) -> Option<super::RecoveryView<'_>> {
        let execution = &self.items.snapshot()?.status.execution;
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted {
            reason, ..
        }) = execution.state()
        else {
            return None;
        };
        Some(super::RecoveryView {
            reason: match reason {
                atman_rt::Interruption::Cancelled => "Execution cancelled",
                atman_rt::Interruption::Failed => "Execution failed",
                atman_rt::Interruption::HostLost => "Execution interrupted",
            },
            pending: self.queue_recovery_ticket.is_some(),
            blocked: if self.flow_view_error.is_some() {
                Some("View disconnected · reopen this flow")
            } else if !execution.pauses().is_empty() {
                Some("Execution paused · release the pause before recovering")
            } else if self.pending_submission.is_some() {
                Some("Waiting for input acknowledgement")
            } else {
                None
            },
            error: self.queue_recovery_error.as_deref(),
        })
    }

    fn recover_queue(&mut self, control: Option<&ControlSender>) {
        let Some(view) = self.queue_recovery_view() else {
            return;
        };
        if view.pending || view.blocked.is_some() || self.queued_submission_edit.is_some() {
            return;
        }
        let flow = self.items.flow();
        let Some(control) = control.filter(|control| {
            control
                .binding()
                .is_some_and(|binding| binding.flow.instance() == flow)
        }) else {
            self.queue_recovery_error = Some("Recovery controls are disconnected".into());
            self.mark_visual_dirty();
            return;
        };
        let command = TuiControl::Submit(crate::TuiSubmission {
            text: ":recover".into(),
            images: vec![],
            reasoning: None,
            presentation: None,
        });
        match control.send_tracked(command) {
            Ok(Some(ticket)) => {
                self.queue_recovery_ticket = Some(ticket);
                self.queue_recovery_error = None;
            }
            _ => self.queue_recovery_error = Some("Recovery controls are disconnected".into()),
        }
        self.mark_visual_dirty();
    }

    pub(crate) fn queue_recovery_result(
        &mut self,
        ticket: Option<&ControlTicket>,
        result: &Result<(), String>,
    ) -> bool {
        if self
            .queue_recovery_ticket
            .as_ref()
            .is_none_or(|pending| Some(pending) != ticket)
        {
            return false;
        }
        self.queue_recovery_ticket = None;
        self.queue_recovery_error = result.as_ref().err().cloned();
        if let Err(error) = result {
            self.push_toast(
                format!("Recovery failed: {error}"),
                NoteLevel::Warn,
                std::time::Duration::from_secs(6),
                ToastPosition::TopRight,
            );
        }
        self.mark_visual_dirty();
        true
    }

    pub(crate) fn queue_page(&self) -> Option<super::QueuePage> {
        self.items.snapshot().map(|snapshot| super::QueuePage {
            offset: snapshot.pending_offset,
            total: snapshot.pending_order.len(),
        })
    }
    pub(crate) fn queue_active_turn(&self) -> bool {
        self.items.snapshot().map_or_else(
            || self.session.as_ref().and_then(|session| session.current_turn()).is_some(),
            |snapshot| matches!(snapshot.status.execution.state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) if !matches!(active.phase, atman_rt::TurnPhase::Finishing | atman_rt::TurnPhase::Cancelling)),
        )
    }

    pub(crate) fn queue_saving(&self) -> bool {
        self.queue_request
            .as_ref()
            .is_some_and(|request| request.ticket.is_some())
    }

    pub(crate) fn sync_queue(&mut self, items: Vec<QueueItem>) {
        let selected = self
            .queued_submissions
            .get(self.selected_submission)
            .map(QueueItem::key);
        self.queued_submissions = items;
        self.selected_submission = selected
            .and_then(|key| {
                self.queued_submissions
                    .iter()
                    .position(|item| item.key() == key)
            })
            .unwrap_or_else(|| {
                self.selected_submission
                    .min(self.queued_submissions.len().saturating_sub(1))
            });
        if let Some(edit) = &mut self.queued_submission_edit {
            let pending = match edit.source.key() {
                QueueKey::Instance(flow, id) => self.items.snapshot().is_some_and(|snapshot| {
                    snapshot.status.execution.address() == &flow
                        && snapshot
                            .pending_order
                            .iter()
                            .any(|input| input.envelope.id == id)
                }),
                key @ QueueKey::Session(_) => {
                    self.queued_submissions.iter().any(|item| item.key() == key)
                }
            };
            if !pending {
                edit.error = Some(
                    "Input is no longer pending. The draft is retained; Esc discards it.".into(),
                );
            }
        } else if self.queued_submissions.is_empty() {
            self.submission_focus = false;
        }
        self.mark_visual_dirty();
    }

    pub(crate) fn queue_action(
        &mut self,
        index: usize,
        action: QueueAction,
        control: Option<&ControlSender>,
    ) {
        if action == QueueAction::Recover {
            self.recover_queue(control);
            return;
        }
        if matches!(action, QueueAction::PreviousPage | QueueAction::NextPage) {
            if self.queued_submission_edit.is_some() || self.queue_saving() {
                return;
            }
            let Some(page) = self.queue_page() else {
                return;
            };
            let size = atman_runtime::flow_runtime::PENDING_VIEW_PAGE_SIZE;
            let offset = if action == QueueAction::PreviousPage {
                self.pending_page_offset.saturating_sub(size)
            } else {
                self.pending_page_offset.saturating_add(size)
            };
            if offset < page.total && offset != self.pending_page_offset {
                self.pending_page_offset = offset;
                self.selected_submission = 0;
                self.submission_focus = true;
                self.mark_visual_dirty();
            }
            return;
        }
        if action == QueueAction::History {
            if let Some(flow) = control
                .and_then(|tx| tx.binding())
                .and_then(|binding| binding.flow.instance())
            {
                self.open_flow_hub = Some(crate::flow_hub::Action::Load(
                    crate::flow_hub::HubRequest::Inbox {
                        owner: flow.clone(),
                        side: atman_runtime::flow_store::inbox_view::InboxSide::Received,
                        before: None,
                    },
                ));
            }
            return;
        }
        if action == QueueAction::Cancel {
            self.queued_submission_edit = None;
            if self.queued_submissions.is_empty() {
                self.submission_focus = false;
            }
            self.mark_visual_dirty();
            return;
        }
        if action == QueueAction::Save {
            self.save_queue_edit(control);
            return;
        }
        if self.queue_saving() {
            return;
        }
        if self
            .queue_page()
            .is_some_and(|page| page.offset != self.pending_page_offset)
        {
            return;
        }
        let Some(item) = self.queued_submissions.get(index).cloned() else {
            return;
        };
        self.selected_submission = index;
        self.submission_focus = true;
        if !item.editable() {
            self.queue_error("Runtime notifications are read-only".into());
            return;
        }
        if action == QueueAction::Edit {
            let mut editor = InputEditor::default();
            editor.replace_with(&item.text());
            self.queued_submission_edit = Some(QueuedSubmissionEdit {
                id: Uuid::new_v4(),
                source: item,
                editor,
                error: None,
            });
            return;
        }
        if action == QueueAction::Insert {
            let reason = item.insert_block_reason().map(str::to_owned).or_else(|| {
                (!self.queue_active_turn()).then(|| "No active turn; input remains queued".into())
            });
            if let Some(reason) = reason {
                self.queue_error(reason);
                return;
            }
        }
        self.send_queue_change(item, action, None, control);
    }

    pub(crate) fn save_queue_edit(&mut self, control: Option<&ControlSender>) {
        if self.queue_saving() {
            return;
        }
        let Some(edit) = &self.queued_submission_edit else {
            return;
        };
        if edit.editor.buf() == edit.source.text() {
            self.queued_submission_edit = None;
            if self.queued_submissions.is_empty() {
                self.submission_focus = false;
            }
            self.mark_visual_dirty();
            return;
        }
        if matches!(edit.source, QueueItem::Session(_))
            && edit.editor.buf().trim().is_empty()
            && edit.source.presentation().is_none_or(|p| p.quote.is_none())
        {
            self.queue_error("Input requires text or a quote".into());
            return;
        }
        self.send_queue_change(
            edit.source.clone(),
            QueueAction::Edit,
            Some(edit.editor.buf().to_owned()),
            control,
        );
    }

    fn send_queue_change(
        &mut self,
        item: QueueItem,
        action: QueueAction,
        text: Option<String>,
        control: Option<&ControlSender>,
    ) {
        let Some(control) = control else {
            self.queue_error("Input controls are unavailable".into());
            return;
        };
        if let QueueItem::Session(value) = item {
            let command = match action {
                QueueAction::Edit => TuiControl::EditQueuedSubmission {
                    id: value.id,
                    expected_revision: value.revision,
                    text: text.unwrap_or_default(),
                },
                QueueAction::Insert => TuiControl::InsertQueuedSubmission {
                    id: value.id,
                    expected_revision: value.revision,
                },
                QueueAction::Delete => TuiControl::DeleteQueuedSubmission {
                    id: value.id,
                    expected_revision: value.revision,
                },
                QueueAction::MoveUp | QueueAction::MoveDown => TuiControl::MoveQueuedSubmission {
                    id: value.id,
                    expected_revision: value.revision,
                    direction: if action == QueueAction::MoveUp {
                        atman_runtime::SubmissionMove::Up
                    } else {
                        atman_runtime::SubmissionMove::Down
                    },
                },
                QueueAction::Recover
                | QueueAction::Save
                | QueueAction::Cancel
                | QueueAction::History
                | QueueAction::PreviousPage
                | QueueAction::NextPage => return,
            };
            if control.send(command).is_ok() {
                if action == QueueAction::Edit {
                    self.queued_submission_edit = None;
                }
            } else {
                self.queue_error("Input controls disconnected".into());
            }
            return;
        }
        let QueueItem::Instance {
            ref flow,
            ref entry,
        } = item
        else {
            unreachable!()
        };
        if control
            .binding()
            .is_none_or(|binding| binding.flow.instance() != Some(flow))
        {
            self.queue_error("Input belongs to another view".into());
            return;
        }
        if matches!(action, QueueAction::MoveUp | QueueAction::MoveDown)
            && self.items.snapshot().is_none()
        {
            self.queue_error("Queue order is unavailable".into());
            return;
        }
        let change = match action {
            QueueAction::Delete => InboxChange::Cancel,
            QueueAction::MoveUp | QueueAction::MoveDown => InboxChange::Move {
                direction: if action == QueueAction::MoveUp {
                    InboxMove::Earlier
                } else {
                    InboxMove::Later
                },
                expected_order: self
                    .items
                    .snapshot()
                    .into_iter()
                    .flat_map(|snapshot| snapshot.pending_order.iter())
                    .filter(|other| {
                        other.envelope.request.sender == entry.envelope.request.sender
                            && other.envelope.request.delivery == DeliveryMode::NextTurn
                            && other.envelope.request.is_editable_user_input()
                    })
                    .map(|other| other.envelope.id)
                    .collect(),
            },
            QueueAction::Edit | QueueAction::Insert => {
                let Some(content) = item.content(text) else {
                    self.queue_error("Input cannot be edited".into());
                    return;
                };
                InboxChange::Replace {
                    content,
                    delivery: if action == QueueAction::Insert {
                        DeliveryMode::AtBoundary
                    } else {
                        entry.envelope.request.delivery
                    },
                }
            }
            QueueAction::Recover
            | QueueAction::Save
            | QueueAction::Cancel
            | QueueAction::History
            | QueueAction::PreviousPage
            | QueueAction::NextPage => return,
        };
        let mut edit = InboxEditRequest {
            id: Uuid::new_v4(),
            sender: entry.envelope.request.sender.clone(),
            message: entry.envelope.id,
            expected: entry.revision.clone(),
            change,
        };
        if let Some(previous) = &self.queue_request {
            let mut retry = edit.clone();
            retry.id = previous.edit.id;
            if retry == previous.edit {
                edit = retry;
            }
        }
        match control.send_tracked(TuiControl::EditInbox(Box::new(edit.clone()))) {
            Ok(ticket) => {
                self.queue_request = Some(QueueRequest {
                    edit,
                    ticket,
                    editor: self
                        .queued_submission_edit
                        .as_ref()
                        .filter(|_| action == QueueAction::Edit)
                        .map(|value| value.id),
                })
            }
            Err(_) => self.queue_error("Input controls disconnected; draft retained".into()),
        }
    }

    pub(crate) fn queue_result(
        &mut self,
        ticket: Option<&ControlTicket>,
        id: Uuid,
        result: Result<atman_runtime::event_log::journal::CommitReceipt, String>,
    ) {
        let Some(pending) = &mut self.queue_request else {
            return;
        };
        if pending.ticket.as_ref() != ticket || pending.ticket.is_none() || pending.edit.id != id {
            return;
        }
        pending.ticket = None;
        match result {
            Ok(_) => {
                if pending.editor.is_some_and(|id| {
                    self.queued_submission_edit
                        .as_ref()
                        .is_some_and(|edit| edit.id == id)
                }) {
                    self.queued_submission_edit = None;
                }
                self.queue_request = None;
                if self.queued_submissions.is_empty() && self.queued_submission_edit.is_none() {
                    self.submission_focus = false;
                }
            }
            Err(error) => self.queue_error(error),
        }
        self.mark_visual_dirty();
    }

    fn queue_error(&mut self, message: String) {
        if let Some(edit) = &mut self.queued_submission_edit {
            edit.error = Some(message.clone());
        }
        self.push_toast(
            message,
            NoteLevel::Warn,
            std::time::Duration::from_secs(5),
            ToastPosition::TopRight,
        );
    }
}

/// Authored text or typed notification content, without captured entry source.
pub(crate) fn payload_text(payload: &InboxPayload) -> String {
    let body = match payload {
        InboxPayload::UserSubmission { content, .. } => Some(content),
        InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => user.as_ref(),
        _ => None,
    };
    if let Some(body) = body {
        return body
            .presentation
            .as_ref()
            .map_or_else(|| body.text.clone(), |p| p.prompt.clone());
    }
    match payload {
        InboxPayload::Interjection { text } | InboxPayload::GoalContinuation { text, .. } => {
            text.clone()
        }
        InboxPayload::GoalResume { objective, .. } => objective.clone(),
        InboxPayload::Watcher { content, .. }
        | InboxPayload::FlowEvent { content, .. }
        | InboxPayload::ProcessEvent { content, .. }
        | InboxPayload::Timer { content, .. }
        | InboxPayload::Internal { content, .. } => content
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| content.to_string()),
        InboxPayload::FormReply { answer } => answer.as_user_text(),
        _ => "Entry invocation".into(),
    }
}
