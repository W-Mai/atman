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
    let receipt = atman_runtime::flow_store::position::FlowPosition::from(CommitReceipt {
        journal_id: Uuid::new_v4(),
        sequence: 2,
        digest: "fixture".into(),
    });
    QueueItem::Instance {
        flow: flow.clone(),
        entry: Box::new(InboxEntry {
            history_head: atman_runtime::flow_store::branches::HistoryHead::initial(
                &flow,
                receipt.journal_id(),
            ),
            envelope: AcceptedEnvelope {
                id,
                accepted_at: chrono::Utc::now(),
                request: DeliveryRequest {
                    expected_head: None,
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

#[tokio::test]
async fn pending_pages_share_order_and_edits_without_sharing_view_position_or_consuming() {
    use atman_runtime::{
        flow_runtime::{FlowWorker, PENDING_VIEW_PAGE_SIZE as PAGE},
        flow_store::{EntrySnapshot, FlowDescriptor, RuntimeStore, SessionDescriptor},
        source_program::RevisionBundle,
    };
    use std::{num::NonZeroUsize, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let session_id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id: session_id,
            configuration: Default::default(),
            project: None,
            title: "Queue".into(),
        })
        .unwrap();
    let flow = FlowRef {
        session_id: session_id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut store = session
        .create_flow(FlowDescriptor {
            address: flow.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() {}".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    store
        .transition(
            store.state().revision(),
            atman_runtime::flow_store::FlowTransition::Activated,
        )
        .unwrap();
    store
        .transition(
            store.state().revision(),
            atman_runtime::flow_store::FlowTransition::ExecutionPaused {
                key: Uuid::now_v7().into(),
            },
        )
        .unwrap();
    let user = SenderIdentity::User(Uuid::now_v7());
    let mut ids = Vec::new();
    for index in 0..PAGE + 2 {
        ids.push(
            store
                .accept_input(
                    DeliveryRequest {
                        expected_head: None,
                        request_id: Uuid::now_v7(),
                        sender: user.clone(),
                        recipient: flow.clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::Interjection {
                            text: format!("queued {index} {}", "body".repeat(1024)),
                        },
                    },
                    |_| Ok(()),
                )
                .unwrap()
                .message_id,
        );
    }
    let worker = FlowWorker::start(store).await.unwrap();
    let mut main_reader = worker
        .subscribe_view(NonZeroUsize::new(3).unwrap())
        .await
        .unwrap();
    let mut floating_reader = worker
        .subscribe_view(NonZeroUsize::new(3).unwrap())
        .await
        .unwrap();
    let mut main = crate::ui_state::UiState::new(AppState::new("main".into(), None));
    let mut floating = AppState::new("floating".into(), None);
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    floating
        .sync_flow_view(floating_reader.next().await.unwrap())
        .unwrap();
    let initial = main.items.snapshot().unwrap().clone();
    assert_eq!(initial.pending.len(), PAGE);
    assert_eq!(initial.pending_order.len(), PAGE + 2);
    assert!(
        main.items.is_empty(),
        "pending input is not conversation history"
    );

    main.submission_focus = true;
    crate::key_handler::handle_key(
        crate::keys::KeyAction::PageDown,
        &mut main,
        &mut crate::input::InputEditor::default(),
        &mut None,
        None,
        None,
    );
    assert_eq!(main.pending_page_offset, PAGE);
    main_reader.set_pending_offset(main.pending_page_offset);
    assert!(futures::poll!(Box::pin(main_reader.next())).is_pending());
    let page = tokio::time::timeout(Duration::from_secs(2), main_reader.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.snapshot().status.cursor, initial.status.cursor);
    assert!(
        main.items.sync_view(page, None).unwrap(),
        "same-cursor paging changes the view"
    );
    main.sync_flow_view(page).unwrap();
    assert_eq!(main.queued_submissions.len(), 2);
    assert_eq!(floating.pending_page_offset, 0);
    assert_eq!(
        main.items.snapshot().unwrap().pending[0].envelope.id,
        ids[PAGE]
    );

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 12)).unwrap();
    let mut hitmap = QueueHitMap::default();
    terminal
        .draw(|frame| {
            hitmap = render(
                frame,
                frame.area(),
                &main.queued_submissions,
                QueueRenderState {
                    recovery: None,
                    page: main.queue_page(),
                    selected: 0,
                    focused: true,
                    hovered: None,
                    hovered_action: Some(QueueAction::PreviousPage),
                    edit: None,
                    active_turn: false,
                    saving: false,
                },
            );
        })
        .unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(screen.contains("Page 2/2"));
    let &(index, action, rect) = hitmap
        .actions
        .iter()
        .find(|(_, action, _)| *action == QueueAction::PreviousPage)
        .unwrap();
    assert_eq!(rect.height, 3);
    assert_eq!(hitmap.action_at(rect.x, rect.y + 1), Some((index, action)));
    main.queue_action(index, action, None);
    main_reader.set_pending_offset(main.pending_page_offset);
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();

    let (commands, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (control, _replies) = ControlSender::for_flow(
        ViewBinding {
            view_id: Uuid::now_v7(),
            revision: 1,
            flow: flow.clone().into(),
        },
        commands,
    );
    // An edit can move off the visible page while another view reorders the queue.
    main.queue_action(PAGE - 1, QueueAction::Edit, Some(&control));
    main.queued_submission_edit
        .as_mut()
        .unwrap()
        .editor
        .replace_with("retained draft");
    floating.queue_action(PAGE - 1, QueueAction::MoveDown, Some(&control));
    let command = rx.try_recv().unwrap();
    let TuiControl::EditInbox(edit) = command.command else {
        panic!("inbox edit required")
    };
    let InboxChange::Move { expected_order, .. } = &edit.change else {
        panic!("reorder required")
    };
    assert_eq!(
        expected_order, &ids,
        "reordering must validate the entire queue"
    );
    let receipt = worker.edit_input(*edit.clone(), |_| Ok(())).await.unwrap();
    floating.queue_result(Some(&command.ticket), edit.id, Ok(receipt));
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    let draft = main.queued_submission_edit.as_ref().unwrap();
    assert!(draft.error.is_none(), "off-page does not mean consumed");
    assert_eq!(draft.editor.buf(), "retained draft");
    assert_eq!(
        main.items.snapshot().unwrap().pending[PAGE - 1].envelope.id,
        ids[PAGE]
    );
    main.queue_action(0, QueueAction::Cancel, None);
    main.queue_action(0, QueueAction::NextPage, None);
    main_reader.set_pending_offset(main.pending_page_offset);
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(
        main.items.snapshot().unwrap().pending[0].envelope.id,
        ids[PAGE - 1]
    );

    // Removing the last page clamps its reader without hiding any remaining input.
    for entry in main.items.snapshot().unwrap().pending.clone() {
        worker
            .edit_input(
                InboxEditRequest {
                    expected_head: None,
                    id: Uuid::now_v7(),
                    sender: user.clone(),
                    message: entry.envelope.id,
                    expected: entry.revision,
                    change: InboxChange::Cancel,
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
    }
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(main.pending_page_offset, 0);
    assert_eq!(main.queued_submissions.len(), PAGE);
    assert_eq!(
        main.items.snapshot().unwrap().status.execution,
        initial.status.execution
    );
    assert!(main.items.is_empty());
    main_reader.set_pending_offset(main.pending_page_offset);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), main_reader.next())
            .await
            .is_err(),
        "clamping must not produce another refresh without a commit or page request"
    );

    // Presentation groups before paging without changing the durable scheduling order.
    let inserted = worker
        .accept_input(
            DeliveryRequest {
                expected_head: None,
                request_id: Uuid::now_v7(),
                sender: user.clone(),
                recipient: flow.clone(),
                delivery: DeliveryMode::AtBoundary,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection {
                    text: "inserted last".into(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let recorded_order: Vec<_> = worker
        .snapshot()
        .await
        .unwrap()
        .pending
        .iter()
        .map(|entry| entry.id)
        .collect();
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    floating
        .sync_flow_view(floating_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(
        main.items.snapshot().unwrap().pending[0].envelope.id,
        inserted.message_id
    );
    assert_eq!(recorded_order.last(), Some(&inserted.message_id));
    main.select_submission(0);
    let selected = main.queued_submissions[0].key();
    main.queue_action(0, QueueAction::Insert, Some(&control));
    let command = rx.try_recv().unwrap();
    let TuiControl::EditInbox(undo) = command.command else {
        panic!("typed undo")
    };
    assert!(
        matches!(&undo.change, InboxChange::Replace { delivery: DeliveryMode::NextTurn, content } if content.text == "inserted last")
    );
    let receipt = worker.edit_input(*undo.clone(), |_| Ok(())).await.unwrap();
    main.queue_result(Some(&command.ticket), undo.id, Ok(receipt));
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(main.pending_page_offset, PAGE);
    assert_eq!(
        main.queued_submissions[main.selected_submission].key(),
        selected,
        "retain the selected identity while its new page loads"
    );
    let selected_index = main.selected_submission;
    main.queue_action(selected_index, QueueAction::Delete, Some(&control));
    assert!(
        rx.try_recv().is_err(),
        "a stale page cannot operate on another item"
    );
    main_reader.set_pending_offset(main.pending_page_offset);
    main.sync_flow_view(main_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(
        main.queued_submissions[main.selected_submission].key(),
        selected
    );
    assert!(!main.queued_submissions[main.selected_submission].is_inserted());
    assert_eq!(
        worker
            .snapshot()
            .await
            .unwrap()
            .pending
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        recorded_order
    );
    floating
        .sync_flow_view(floating_reader.next().await.unwrap())
        .unwrap();
    assert_eq!(
        floating.pending_page_offset, 0,
        "other views retain their own selection and page"
    );
    assert!(
        worker.edit_input(*undo, |_| Ok(())).await.is_ok(),
        "retry uses the same durable receipt"
    );
    worker.shutdown().await.unwrap();
}
