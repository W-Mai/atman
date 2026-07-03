use atman_runtime::event::TurnId;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::session::Session;

fn user_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
    }
}

fn assistant_msg(turn_id: TurnId, text: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id,
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
    session.end_turn();

    assert!(session.current_turn().is_none());
    let events = session.sink().snapshot();
    assert!(
        events.iter().any(
            |e| matches!(e, atman_runtime::Event::TurnEnd { turn_id: t, .. } if *t == turn_id)
        )
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

#[test]
fn drain_injections_marks_pending_as_injected_and_returns_in_order() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));

    let id1 = session.enqueue_injection("first").unwrap();
    let id2 = session.enqueue_injection("second").unwrap();

    let drained = session.drain_injections(&turn_id);
    assert_eq!(drained.len(), 2);
    assert_eq!(drained[0].id, id1);
    assert_eq!(drained[1].id, id2);
    assert_eq!(drained[0].text, "first");
    assert_eq!(drained[1].text, "second");
    assert_eq!(drained[0].state, atman_runtime::InjectionState::Injected);

    let second_drain = session.drain_injections(&turn_id);
    assert!(second_drain.is_empty(), "drain twice should be empty");
}

#[test]
fn end_turn_marks_pending_injections_cancelled() {
    let session = Session::open_ephemeral();
    let turn_id = TurnId::now();
    session.begin_turn(user_msg(turn_id.clone(), "start"));
    session.enqueue_injection("orphan").unwrap();
    assert_eq!(session.list_pending_injections().len(), 1);
    session.end_turn();
    assert!(
        session.list_pending_injections().is_empty(),
        "end_turn should cancel pending injections"
    );
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
    session.begin_turn(user_msg(t1, "one"));
    let tok1 = session.flow_cancel_token();
    session.cancel_flow();
    assert!(tok1.is_cancelled());
    session.end_turn();

    let t2 = TurnId::now();
    session.begin_turn(user_msg(t2, "two"));
    let tok2 = session.flow_cancel_token();
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
