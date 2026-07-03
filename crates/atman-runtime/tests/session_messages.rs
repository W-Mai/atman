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
