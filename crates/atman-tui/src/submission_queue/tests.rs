use super::*;
use crate::{ControlSender, TuiControl, app::AppState, control::ViewBinding};
use atman_runtime::{
    event_log::journal::CommitReceipt,
    flow_store::{FlowInstanceId, FlowRef, inbox::*},
    message::{ImageData, ImageSource},
    user_input::{QuoteSnapshot, UserInputPresentation},
};
use uuid::Uuid;

fn item() -> QueueItem {
    let flow = FlowRef {
        session_id: Uuid::new_v4().into(),
        instance_id: FlowInstanceId::now(),
    };
    let presentation = UserInputPresentation {
        prompt: "original".into(),
        quote: Some(QuoteSnapshot {
            text: "quoted context".into(),
        }),
    };
    let id = Uuid::new_v4();
    let receipt = CommitReceipt {
        journal_id: Uuid::new_v4(),
        sequence: 2,
        digest: "fixture".into(),
    };
    QueueItem::Instance {
        flow: flow.clone(),
        entry: Box::new(InboxEntry {
            envelope: AcceptedEnvelope {
                id,
                accepted_at: chrono::Utc::now(),
                request: DeliveryRequest {
                    request_id: Uuid::new_v4(),
                    sender: SenderIdentity::User(Uuid::new_v4()),
                    recipient: flow,
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::UserSubmission {
                        content: UserContent {
                            text: presentation.model_text(),
                            presentation: Some(presentation),
                            images: vec![ImageSource {
                                media_type: "image/png".into(),
                                data: ImageData::Base64 {
                                    data: "fixture".into(),
                                },
                                detail: Default::default(),
                            }],
                        },
                        invocation: [("effort".into(), "high".into())].into(),
                    },
                },
            },
            acceptance: Acceptance {
                message_id: id,
                receipt: receipt.clone(),
                disposition: InboxDisposition::Pending,
            },
            revision: receipt,
        }),
    }
}

#[test]
fn durable_queue_controls_keep_view_revision_draft_and_retry_ownership() {
    let item = item();
    let QueueItem::Instance { flow, entry } = &item else {
        unreachable!()
    };
    let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (sender, _replies) = ControlSender::for_flow(
        ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: flow.clone().into(),
        },
        commands,
    );
    let mut app = AppState::new("view".into(), None);
    app.sync_queue(vec![item.clone()]);
    app.queue_action(0, QueueAction::Edit, Some(&sender));
    app.queued_submission_edit
        .as_mut()
        .unwrap()
        .editor
        .replace_with("edited\n你好");
    app.queue_action(0, QueueAction::Save, Some(&sender));
    assert!(app.queue_saving());
    assert!(app.queued_submission_edit.is_some());
    assert_eq!(app.queued_submissions[0].text(), "original");
    let command = rx.try_recv().unwrap();
    let TuiControl::EditInbox(request) = command.command else {
        panic!("typed edit required")
    };
    assert_eq!(request.message, entry.envelope.id);
    assert_eq!(request.expected, entry.revision);
    let InboxChange::Replace { content, delivery } = &request.change else {
        panic!("replace required")
    };
    assert_eq!(*delivery, DeliveryMode::NextTurn);
    assert_eq!(content.text, "> quoted context\n\nedited\n你好");
    assert_eq!(content.images.len(), 1);
    app.queue_result(None, request.id, Ok(entry.revision.clone()));
    assert!(
        app.queue_saving(),
        "unbound replies cannot acknowledge a view edit"
    );
    app.queue_result(Some(&command.ticket), request.id, Err("reserved".into()));
    assert!(!app.queue_saving());
    assert_eq!(
        app.queued_submission_edit.as_ref().unwrap().editor.buf(),
        "edited\n你好"
    );
    app.queue_action(0, QueueAction::Save, Some(&sender));
    let retry = rx.try_recv().unwrap();
    let TuiControl::EditInbox(retry_request) = retry.command else {
        panic!("typed retry")
    };
    assert_eq!(retry_request, request);
    assert_ne!(retry.ticket, command.ticket);
    app.queue_result(
        Some(&command.ticket),
        request.id,
        Ok(entry.revision.clone()),
    );
    assert!(
        app.queue_saving(),
        "a previous transport reply must not complete a retry"
    );
    app.queue_action(0, QueueAction::Cancel, Some(&sender));
    assert!(app.queued_submission_edit.is_none());
    app.queue_result(Some(&retry.ticket), request.id, Err("stale".into()));
    app.queue_action(0, QueueAction::Edit, Some(&sender));
    app.queued_submission_edit
        .as_mut()
        .unwrap()
        .editor
        .replace_with("new draft");
    app.queue_result(Some(&retry.ticket), request.id, Ok(entry.revision.clone()));
    assert_eq!(
        app.queued_submission_edit.as_ref().unwrap().editor.buf(),
        "new draft"
    );
    app.queue_action(0, QueueAction::Save, Some(&sender));
    let current = rx.try_recv().unwrap();
    let TuiControl::EditInbox(current_request) = current.command else {
        panic!("typed edit")
    };
    assert_ne!(current_request.id, request.id);
    app.sync_queue(vec![]);
    assert!(
        app.queued_submission_edit.is_some(),
        "consumption cannot discard the draft"
    );
    app.queue_result(
        Some(&current.ticket),
        current_request.id,
        Ok(entry.revision.clone()),
    );
    assert!(app.queued_submission_edit.is_none());
    assert!(
        app.items.is_empty(),
        "editing pending input cannot append transcript text"
    );
}

#[test]
fn queue_sources_and_bound_destinations_cannot_be_replaced_by_user_edits() {
    let mut item = item();
    let QueueItem::Instance { entry, .. } = &mut item else {
        unreachable!()
    };
    entry.envelope.request.sender = SenderIdentity::Host(Uuid::new_v4());
    entry.envelope.request.payload = InboxPayload::Internal {
        kind: "notice".into(),
        content: "background result".into(),
    };
    let mut app = AppState::new("view".into(), None);
    app.sync_queue(vec![item]);
    let (sender, mut rx) = ControlSender::channel();
    for action in [
        QueueAction::Edit,
        QueueAction::Insert,
        QueueAction::Delete,
        QueueAction::MoveDown,
    ] {
        app.queue_action(0, action, Some(&sender));
        assert!(rx.try_recv().is_err());
        assert!(app.queued_submission_edit.is_none());
    }
    app.sync_queue(vec![self::item()]);
    app.queue_action(0, QueueAction::Delete, Some(&sender));
    assert!(
        rx.try_recv().is_err(),
        "persistent messages cannot enter a legacy control channel"
    );
}
