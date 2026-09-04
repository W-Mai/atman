use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessageOrigin, MessagePart, MessageRole};
use atman_runtime::session::Session;

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

fn assistant_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
        origin: MessageOrigin::User,
    }
}

#[test]
fn append_message_pushes_to_messages_and_emits_event() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    let msg = user_msg(turn_id.clone(), "hi");

    session.append_message(msg.clone(), None);

    assert_eq!(session.message_count(), 1);
    let msgs = session.messages();
    assert_eq!(msgs[0].role, MessageRole::User);

    let events = session.sink().snapshot();
    assert!(
        events.iter().any(
            |e| matches!(e, atman_runtime::Event::UserMsg { turn_id: t, .. } if *t == turn_id)
        )
    );
}

#[test]
fn context_records_are_append_only_with_per_key_digest_noops() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    let spec = |text: &str| {
        atman_runtime::ContextRecordSpec::new(
            "session.goal",
            atman_runtime::ContextRecordAuthority::User,
            atman_runtime::ContextRecordRetention::Latest,
            atman_runtime::ContextRecordBody::text(text),
        )
    };

    let first = session.append_context_records(turn_id.clone(), [spec("first")]);
    let unchanged = session.append_context_records(turn_id.clone(), [spec("first")]);
    let second = session.append_context_records(turn_id, [spec("second")]);

    assert_eq!(first[0].revision(), 1);
    assert!(unchanged.is_empty());
    assert_eq!(second[0].revision(), 2);
    let messages = session.messages();
    let records: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ContextRecord(record) => Some(record),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].revision(), 1);
    assert_eq!(records[1].revision(), 2);
}

#[test]
fn begin_turn_records_turn_start_and_user_msg() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    let msg = user_msg(turn_id.clone(), "start");

    let out_turn = session.begin_turn(msg);
    assert_eq!(out_turn, turn_id);
    assert_eq!(session.current_turn(), Some(turn_id.clone()));

    let events = session.sink().snapshot();
    let turn_start = events
        .iter()
        .find(|e| matches!(e, atman_runtime::Event::TurnStart { turn_id: t, .. } if *t == turn_id));
    let user_msg = events
        .iter()
        .find(|e| matches!(e, atman_runtime::Event::UserMsg { turn_id: t, .. } if *t == turn_id));
    assert!(turn_start.is_some());
    assert!(user_msg.is_some());
}

#[test]
fn end_turn_emits_turn_end_and_clears_current() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "x"));
    session.end_turn(&turn_id);

    assert!(session.current_turn().is_none());
    let events = session.sink().snapshot();
    assert!(
        events.iter().any(
            |e| matches!(e, atman_runtime::Event::TurnEnd { turn_id: t, .. } if *t == turn_id)
        )
    );
}

#[test]
fn concurrent_turn_lifecycle_is_identity_scoped() {
    let session = Session::open_ephemeral();
    let first = session.begin_turn(user_msg(TurnId::now(), "first"));
    let first_injection = session.enqueue_injection("first nudge").unwrap();
    session.mark_streamed(&first);
    let first_cancel = session.flow_cancel_token(&first).unwrap();

    let second = session.begin_turn(user_msg(TurnId::now(), "second"));
    let second_cancel = session.flow_cancel_token(&second).unwrap();
    assert!(session.current_turn().is_none());
    assert!(matches!(
        session.enqueue_injection("ambiguous"),
        Err(atman_runtime::session::EnqueueError::AmbiguousTurn)
    ));
    session.cancel_flow();
    assert!(!first_cancel.is_cancelled());
    assert!(!second_cancel.is_cancelled());
    first_cancel.cancel();
    assert!(!second_cancel.is_cancelled());
    assert!(session.take_streamed_flag(&first));
    assert!(!session.take_streamed_flag(&first));
    assert!(!session.take_streamed_flag(&second));

    session.end_turn(&first);
    assert!(session.flow_cancel_token(&first).is_none());
    assert_eq!(session.current_turn(), Some(second.clone()));
    let second_injection = session.enqueue_injection("second nudge").unwrap();
    session.end_turn(&first);
    session.mark_streamed(&first);
    assert!(!session.take_streamed_flag(&second));
    let pending = session.list_pending_injections();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, second_injection);
    assert_ne!(pending[0].id, first_injection);
    session.mark_streamed(&second);
    assert!(session.take_streamed_flag(&second));
    session.end_turn(&second);
    assert!(session.list_pending_injections().is_empty());
    assert_eq!(
        session
            .sink()
            .snapshot()
            .iter()
            .filter(|event| matches!(event, atman_runtime::Event::TurnEnd { .. }))
            .count(),
        2,
    );
}

#[test]
fn assistant_msg_with_flow_run_id_records_correlation() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    let flow_run_id = atman_runtime::FlowRunId::now();
    let msg = assistant_msg(turn_id.clone(), "done");

    session.append_message(msg, Some(flow_run_id.clone()));

    let events = session.sink().snapshot();
    let found = events.iter().any(|e| {
        matches!(
            e,
            atman_runtime::Event::AssistantMsg {
                turn_id: t,
                flow_run_id: Some(f),
                ..
            } if *t == turn_id && *f == flow_run_id
        )
    });
    assert!(found);
}

#[test]
fn enqueue_injection_requires_active_turn() {
    let session = Session::open_ephemeral();
    let err = session.enqueue_injection("hi").unwrap_err();
    assert!(format!("{err}").contains("no active turn"));
}

#[tokio::test]
async fn drain_injections_persists_each_identity_once_and_preserves_controls() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let id1 = session.enqueue_injection("same steering").unwrap();
    let id2 = session.enqueue_injection("same steering").unwrap();
    for level in [
        atman_runtime::injection::InjectionLevel::L3Redirect,
        atman_runtime::injection::InjectionLevel::L4HardStop,
    ] {
        session
            .enqueue_injection_with_level("control", level, Some("target".into()))
            .unwrap();
    }

    let drained = session.drain_injections(&turn_id).await;
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].id, id1);
    assert_eq!(drained[1].id, id2);
    assert_eq!(drained[0].text, "same steering");
    assert_eq!(drained[1].text, "same steering");
    assert_eq!(drained[0].state, atman_runtime::InjectionState::Injected);

    let first_states = session
        .sink()
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::UserInject { injection, .. } if injection.id == id1 => {
                Some(injection.state)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        first_states,
        vec![
            atman_runtime::InjectionState::Pending,
            atman_runtime::InjectionState::Injected,
        ]
    );

    let second_drain = session.drain_injections(&turn_id).await;
    assert!(second_drain.is_empty(), "drain twice should be empty");
    assert_eq!(session.list_pending_injections().len(), 2);
    let messages = session.messages();
    assert_eq!(messages.len(), 3);
    assert!(messages[1].text_concat().contains(&id1.to_string()));
    assert!(messages[2].text_concat().contains(&id2.to_string()));
    assert_eq!(
        *session.messages_handle().lock().unwrap(),
        messages.to_vec()
    );
    let applied = session
        .sink()
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::UserInject {
                context_message: Some(message),
                ..
            } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(applied, messages[1..]);
}

#[test]
fn end_turn_marks_pending_injections_cancelled() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    let injection_id = session.enqueue_injection("orphan").unwrap();
    assert_eq!(session.list_pending_injections().len(), 1);
    session.end_turn(&turn_id);
    assert!(
        session.list_pending_injections().is_empty(),
        "end_turn should cancel pending injections"
    );
    let final_state = session
        .sink()
        .snapshot()
        .into_iter()
        .filter_map(|event| match event {
            atman_runtime::Event::UserInject { injection, .. } if injection.id == injection_id => {
                Some(injection.state)
            }
            _ => None,
        })
        .next_back();
    assert_eq!(final_state, Some(atman_runtime::InjectionState::Cancelled));
}

#[test]
fn user_inject_event_is_emitted_on_enqueue() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.enqueue_injection("nudge!").unwrap();

    let events = session.sink().snapshot();
    let has_inject = events.iter().any(|e| {
        matches!(
            e,
            atman_runtime::Event::UserInject { turn_id: t, injection, .. }
                if *t == turn_id && injection.text == "nudge!"
        )
    });
    assert!(has_inject);
}

#[test]
fn flow_cancel_token_is_reset_on_new_turn() {
    let session = Session::open_ephemeral();
    let t1 = TurnId::now();
    session.begin_turn(user_msg(t1.clone(), "one"));
    let tok1 = session.flow_cancel_token(&t1).unwrap();
    session.cancel_flow();
    assert!(tok1.is_cancelled());
    session.end_turn(&t1);

    let t2 = TurnId::now();
    session.begin_turn(user_msg(t2.clone(), "two"));
    let tok2 = session.flow_cancel_token(&t2).unwrap();
    assert!(
        !tok2.is_cancelled(),
        "new turn must have fresh cancel token"
    );
}

#[test]
fn multiple_messages_preserve_order() {
    let session = Session::open_ephemeral();
    let t1 = TurnId::now();
    session.append_message(user_msg(t1.clone(), "first"), None);
    session.append_message(assistant_msg(t1.clone(), "reply"), None);
    session.append_message(user_msg(t1, "second"), None);

    let msgs = session.messages();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].role, MessageRole::User);
    assert_eq!(msgs[1].role, MessageRole::Assistant);
    assert_eq!(msgs[2].role, MessageRole::User);
    assert_eq!(msgs[0].text_concat(), "first");
    assert_eq!(msgs[1].text_concat(), "reply");
    assert_eq!(msgs[2].text_concat(), "second");
}
