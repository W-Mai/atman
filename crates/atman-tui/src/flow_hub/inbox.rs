use atman_runtime::flow_store::{
    inbox::{DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, SenderIdentity},
    inbox_view::{InboxHistoryItem, InboxHistoryPage, InboxSide},
    outbox::SendStatus,
};
use crossterm::event::MouseEvent;
#[cfg(test)]
use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::{Frame, layout::Rect};
use uuid::Uuid;

use super::{
    Action,
    page::{Row, RowKey, short_flow},
};
use crate::keys::KeyAction;

pub(super) fn other_side(side: InboxSide) -> InboxSide {
    match side {
        InboxSide::Received => InboxSide::Sent,
        InboxSide::Sent => InboxSide::Received,
    }
}

fn record(
    item: &InboxHistoryItem,
) -> (
    Uuid,
    &DeliveryRequest,
    &'static str,
    chrono::DateTime<chrono::Utc>,
) {
    match item {
        InboxHistoryItem::Received(entry) => (
            entry.envelope.id,
            &entry.envelope.request,
            match entry.acceptance.disposition {
                InboxDisposition::Pending => "Pending input",
                InboxDisposition::InputCommitted { .. } => "Input committed",
                InboxDisposition::Cancelled { .. } => "Cancelled",
            },
            entry.envelope.accepted_at,
        ),
        InboxHistoryItem::Sent(entry) => (
            entry.intent.request.request_id,
            &entry.intent.request,
            match entry.status {
                SendStatus::PendingDelivery => "Pending delivery",
                SendStatus::Accepted { .. } => "Receiver accepted",
                SendStatus::Failed { .. } => "Delivery failed",
            },
            entry.intent.queued_at,
        ),
    }
}

fn source(request: &DeliveryRequest) -> String {
    match &request.sender {
        SenderIdentity::User(id) => format!("User {id}"),
        SenderIdentity::Host(id) => format!("Host {id}"),
        SenderIdentity::Flow(flow) => format!("Flow {}/{}", flow.session_id, flow.instance_id),
    }
}

fn kind(payload: &InboxPayload) -> &str {
    match payload {
        InboxPayload::Command { .. } => "Command",
        InboxPayload::Invocation { .. } => "Invocation",
        InboxPayload::GoalResume { .. } => "Goal resume",
        InboxPayload::GoalContinuation { .. } => "Goal continuation",
        InboxPayload::UserSubmission { .. } => "User input",
        InboxPayload::Interjection { .. } => "Interjection",
        InboxPayload::FormReply { .. } => "Form reply",
        InboxPayload::FlowEvent { .. } => "Flow event",
        InboxPayload::Watcher { .. } => "Watcher",
        InboxPayload::ProcessEvent { .. } => "Process event",
        InboxPayload::Timer { .. } => "Timer",
        InboxPayload::Internal { .. } => "Internal",
    }
}

pub(super) fn rows(page: &InboxHistoryPage) -> Vec<Row> {
    page.items
        .iter()
        .map(|item| {
            let (id, request, status, _) = record(item);
            Row {
                key: RowKey::Delivery {
                    owner: page.owner.clone(),
                    side: page.side,
                    id,
                },
                title: format!("{} · {status}", kind(&request.payload)),
                detail: format!(
                    "{} · {}",
                    if page.side == InboxSide::Sent {
                        format!("To {}", short_flow(&request.recipient))
                    } else {
                        match &request.sender {
                            SenderIdentity::User(_) => "User".into(),
                            SenderIdentity::Host(_) => "Host".into(),
                            SenderIdentity::Flow(flow) => short_flow(flow),
                        }
                    },
                    crate::submission_queue::payload_text(&request.payload)
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
                action: Action::None,
            }
        })
        .collect()
}

#[derive(Default)]
pub(super) struct Details {
    pub id: Option<Uuid>,
    text: Option<super::text::TextDetails>,
    cursor: Option<atman_runtime::event_log::journal::CommitReceipt>,
}

impl Details {
    pub fn open(&mut self, id: Uuid) {
        *self = Self {
            id: Some(id),
            ..Self::default()
        };
    }

    pub fn key(&mut self, key: KeyAction) -> Action {
        if key == KeyAction::Escape || self.text.as_mut().is_some_and(|text| text.key(key)) {
            *self = Self::default();
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Action {
        if self.text.as_mut().is_some_and(|text| text.mouse(event)) {
            *self = Self::default();
        }
        Action::None
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, page: &InboxHistoryPage) {
        let Some(item) = page
            .items
            .iter()
            .find(|item| Some(record(item).0) == self.id)
        else {
            *self = Self::default();
            return;
        };
        if self.text.is_none() || self.cursor.as_ref() != Some(&page.cursor) {
            let scroll = self.text.as_ref().map_or(0, |text| text.scroll);
            let mut text = super::text::TextDetails::new(
                format!("Inbox / Details · {}", short_flow(&page.owner)),
                detail_text(page, item),
            );
            text.scroll = scroll;
            self.text = Some(text);
            self.cursor = Some(page.cursor.clone());
        }
        self.text.as_mut().unwrap().render(frame, area);
    }
}

fn detail_text(page: &InboxHistoryPage, item: &InboxHistoryItem) -> Vec<String> {
    let (id, request, status, time) = record(item);
    let mut metadata = vec![
        format!("Message: {id}"),
        format!("Request: {}", request.request_id),
        format!("Source: {}", source(request)),
        format!(
            "Recipient: {}/{}",
            request.recipient.session_id, request.recipient.instance_id
        ),
        format!(
            "Recorded: {}",
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
        ),
        format!(
            "Delivery: {}",
            match request.delivery {
                DeliveryMode::AtBoundary => "Next compatible model call",
                DeliveryMode::NextTurn => "Next turn",
            }
        ),
    ];
    if let Some(cause) = &request.cause {
        metadata.push(format!(
            "Cause: {}/{} · event {}:{}",
            cause.flow.session_id, cause.flow.instance_id, cause.sequence, cause.index
        ));
    }
    if let Some(correlation) = request.correlation {
        metadata.push(format!("Correlation: {correlation}"));
    }
    if let Some(scope) = &request.scope {
        metadata.push(format!("Scope: {}", scope.id));
        metadata.push(format!(
            "Scope owner: {}/{}",
            scope.owner.session_id, scope.owner.instance_id
        ));
    }
    match item {
        InboxHistoryItem::Received(entry) => {
            metadata.push(format!(
                "Accepted at commit {} · revision {}",
                entry.acceptance.receipt.sequence, entry.revision.sequence
            ));
            if let InboxDisposition::InputCommitted { by } | InboxDisposition::Cancelled { by } =
                &entry.acceptance.disposition
            {
                metadata.push(format!("Disposition commit: {}", by.sequence));
            }
        }
        InboxHistoryItem::Sent(entry) => {
            metadata.push(format!("Queued at commit {}", entry.queued.sequence));
            if let SendStatus::Accepted { delivery, .. } = &entry.status {
                metadata.push(format!(
                    "Receiver message: {} · commit {}",
                    delivery.message_id, delivery.receipt.sequence
                ));
            }
            if let SendStatus::Failed { failure, by } = &entry.status {
                let atman_runtime::flow_store::outbox::DeliveryFailure::RecipientClosed {
                    observed,
                } = failure;
                metadata.push(format!(
                    "Recipient closed without acceptance · receiver commit {} · failure commit {}",
                    observed.sequence, by.sequence
                ));
            }
        }
    }
    let mut text = vec![
        format!("{} · {status}", kind(&request.payload)),
        String::new(),
        crate::submission_queue::payload_text(&request.payload),
    ];
    let user = match &request.payload {
        InboxPayload::UserSubmission { content, .. } => Some(content),
        InboxPayload::Command { user, .. } | InboxPayload::Invocation { user, .. } => user.as_ref(),
        _ => None,
    };
    if let Some(user) = user {
        if let Some(quote) = user.presentation.as_ref().and_then(|p| p.quote.as_ref()) {
            text.extend([
                String::new(),
                "Quote".into(),
                String::new(),
                quote.text.clone(),
            ]);
        }
        if !user.images.is_empty() {
            text.push(format!("Attachments: {}", user.images.len()));
        }
    }
    text.extend([String::new(), "Delivery details".into(), String::new()]);
    text.extend(metadata);
    text.push("Delivery status does not establish task completion.".into());
    match &request.payload {
        InboxPayload::Watcher {
            subscription,
            source,
            ..
        } => text.push(format!(
            "Subscription: {subscription}\nSource event: {}/{} · {}:{}",
            source.flow.session_id, source.flow.instance_id, source.sequence, source.index
        )),
        InboxPayload::FlowEvent { source, event, .. } => text.push(format!(
            "Event: {event}\nSource event: {}/{} · {}:{}",
            source.flow.session_id, source.flow.instance_id, source.sequence, source.index
        )),
        InboxPayload::ProcessEvent { source, .. } => text.push(format!("Process: {source:?}")),
        InboxPayload::Timer { timer, due_at, .. } => text.push(format!(
            "Timer: {timer}\nDue: {}",
            due_at
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
        )),
        InboxPayload::Command {
            entry, arguments, ..
        } => text.push(format!(
            "Entry: {}\nArguments: {}",
            entry.name,
            serde_json::to_string_pretty(arguments).unwrap_or_default()
        )),
        InboxPayload::Invocation { arguments, .. } => text.push(format!(
            "Arguments: {}",
            serde_json::to_string_pretty(arguments).unwrap_or_default()
        )),
        _ => {}
    }
    text.extend([
        String::new(),
        format!(
            "Snapshot: {} · commit {} · trailing bytes {}",
            page.cursor.journal_id, page.cursor.sequence, page.trailing_bytes
        ),
    ]);

    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::{ControlTicket, ViewBinding},
        flow_hub::{FlowHub, FlowHubPage, History, HubRequest},
    };
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_store::{
            FlowInstanceId, FlowRef,
            inbox::{Acceptance, AcceptedEnvelope, InboxEntry},
            inbox_view::InboxHistoryCursor,
        },
    };

    #[test]
    fn inbox_navigation_keeps_page_identity_and_read_only_detail_controls() {
        let owner = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: FlowInstanceId::now(),
        };
        let receipt = CommitReceipt {
            journal_id: Uuid::new_v4(),
            sequence: 3,
            digest: "digest".into(),
        };
        let id = Uuid::new_v4();
        let scope = atman_runtime::flow_store::task_scope::TaskScopeRef {
            owner: FlowRef {
                session_id: Uuid::new_v4().into(),
                instance_id: FlowInstanceId::now(),
            },
            id: Uuid::new_v4(),
        };
        let anchor = InboxHistoryCursor {
            owner: owner.clone(),
            side: InboxSide::Received,
            id,
            receipt: receipt.clone(),
        };
        let page = |side, next| {
            Box::new(FlowHubPage::Inbox(InboxHistoryPage {
                owner: owner.clone(),
                side,
                cursor: receipt.clone(),
                trailing_bytes: 0,
                total: 51,
                next,
                items: vec![InboxHistoryItem::Received(Box::new(InboxEntry {
                    envelope: AcceptedEnvelope {
                        id,
                        accepted_at: chrono::Utc::now(),
                        request: DeliveryRequest {
                            request_id: Uuid::new_v4(),
                            sender: SenderIdentity::User(Uuid::new_v4()),
                            recipient: owner.clone(),
                            delivery: DeliveryMode::AtBoundary,
                            cause: None,
                            correlation: None,
                            scope: Some(scope.clone()),
                            payload: InboxPayload::Interjection {
                                text: "A long message 中文 wraps correctly. ".repeat(100),
                            },
                        },
                    },
                    acceptance: Acceptance {
                        message_id: id,
                        receipt: receipt.clone(),
                        disposition: InboxDisposition::Pending,
                    },
                    revision: receipt.clone(),
                }))],
            }))
        };
        let sample = page(InboxSide::Received, None);
        let FlowHubPage::Inbox(sample) = sample.as_ref() else {
            unreachable!()
        };
        let detail = detail_text(sample, &sample.items[0]).join("\n");
        for identity in [
            scope.id.to_string(),
            scope.owner.session_id.to_string(),
            scope.owner.instance_id.to_string(),
        ] {
            assert!(detail.contains(&identity));
        }
        let mut ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::new_v4(),
                revision: 1,
                flow: owner.clone().into(),
            },
            request_id: Uuid::new_v4(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..FlowHub::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(page(InboxSide::Received, Some(anchor.clone()))),
        );
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Inbox { before: Some(cursor), .. }) if cursor == anchor)
        );
        ticket.request_id = Uuid::new_v4();
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.binding.revision += 1;
        hub.receive(Some(&stale), Ok(page(InboxSide::Sent, None)));
        assert!(hub.pending.is_some());
        hub.receive(Some(&ticket), Err("temporary read failure".into()));
        assert_eq!(hub.index, 0);
        assert!(hub.page.as_ref().unwrap().next().is_some());
        assert!(matches!(
            hub.key(KeyAction::PageDown),
            Action::Load(HubRequest::Inbox {
                before: Some(_),
                ..
            })
        ));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page(InboxSide::Received, None)));
        assert_eq!(hub.index, 1);
        assert!(matches!(
            hub.key(KeyAction::RedirectPrefill),
            Action::Load(HubRequest::Inbox { before: None, .. })
        ));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page(InboxSide::Received, Some(anchor))));
        assert_eq!(hub.index, 0);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert!(
            hub.hits
                .iter()
                .all(|(rect, _)| !rect.intersects(hub.relations))
        );
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hub.relations.x + 1,
                row: hub.relations.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE
            }),
            Action::Load(HubRequest::Inbox {
                side: InboxSide::Sent,
                ..
            })
        ));
        let hit = hub.hits[0].0;
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x + 2,
                row: hit.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE
            }),
            Action::None
        ));
        assert_eq!(hub.inbox_details.id, Some(id));
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let before_scroll = hub.inbox_details.text.as_ref().unwrap().scroll;
        hub.key(KeyAction::PageDown);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        assert!(hub.inbox_details.text.as_ref().unwrap().scroll > before_scroll);
        hub.paste("not a search");
        assert!(hub.query.is_empty());
        hub.key(KeyAction::Escape);
        assert!(hub.inbox_details.id.is_none());
        hub.key(KeyAction::Submit);
        terminal
            .draw(|frame| hub.render(frame, &owner.clone().into()))
            .unwrap();
        let back = hub.inbox_details.text.as_ref().unwrap().back;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: back.x + 1,
            row: back.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(hub.inbox_details.id.is_none());
        assert!(matches!(
            hub.key(KeyAction::Tab),
            Action::Load(HubRequest::Inbox {
                side: InboxSide::Sent,
                before: None,
                ..
            })
        ));
        let mut history = History::default();
        hub.query = "long".into();
        history.save(hub);
        let (saved, _) = history.open(Action::Return, HubRequest::Sessions { before: None });
        assert_eq!(saved.query, "long");
        assert!(matches!(saved.page, Some(FlowHubPage::Inbox(page)) if page.owner == owner));
    }
}
