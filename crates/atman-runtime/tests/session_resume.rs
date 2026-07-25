use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::session::{Session, SessionOpenError};

fn user_msg(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id: atman_runtime::event::TurnId::now(),
    }
}

fn assistant_msg(text: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        parts: vec![MessagePart::Text { text: text.into() }],
        turn_id: atman_runtime::event::TurnId::now(),
    }
}

#[tokio::test]
async fn open_existing_rehydrates_messages_in_order() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        s.append_message(user_msg("first"), None);
        s.append_message(assistant_msg("second"), None);
        s.append_message(user_msg("third"), None);
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };
    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].text_concat(), "first");
    assert_eq!(msgs[1].text_concat(), "second");
    assert_eq!(msgs[2].text_concat(), "third");
    reopened.shutdown().await;
}

#[test]
fn open_existing_invalid_id_errors() {
    let tmp = tempfile::tempdir().unwrap();
    match Session::open_existing(tmp.path(), "not-a-uuid") {
        Err(SessionOpenError::InvalidId { .. }) => {}
        Err(other) => panic!("want InvalidId, got {other:?}"),
        Ok(_) => panic!("want error, got Ok"),
    }
}

#[test]
fn open_existing_missing_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let fake = "0198feed-0000-7000-8000-abcdef012345";
    match Session::open_existing(tmp.path(), fake) {
        Err(SessionOpenError::NotFound { sid, .. }) => assert_eq!(sid, fake),
        Err(other) => panic!("want NotFound, got {other:?}"),
        Ok(_) => panic!("want error, got Ok"),
    }
}

#[tokio::test]
async fn open_existing_ignores_non_message_events_and_bad_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        s.append_message(user_msg("real"), None);
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };
    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    let mut body = std::fs::read_to_string(&events_path).unwrap();
    body.push_str(
        r#"{"type":"flow_start","seq":99,"run_id":{"raw":"r"},"flow_name":"x","ts":"2026-07-05T12:00:00Z"}
malformed line here
{"type":"turn_start","seq":100,"turn_id":{"raw":"t"},"ts":"2026-07-05T12:00:00Z"}
"#,
    );
    std::fs::write(&events_path, body).unwrap();

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 1, "only real user_msg should be replayed");
    assert_eq!(msgs[0].text_concat(), "real");
}

#[tokio::test]
async fn open_existing_preserves_session_id() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };
    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    assert_eq!(reopened.id().to_string(), sid);
}

#[tokio::test]
async fn open_existing_loads_old_format_without_seq_and_ts() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        s.append_message(user_msg("real"), None);
        s.append_message(assistant_msg("reply"), None);
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };

    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    let body = std::fs::read_to_string(&events_path).unwrap();
    let stripped: String = body
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut v: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(obj) = v.as_object_mut() {
                obj.remove("seq");
                obj.remove("ts");
            }
            serde_json::to_string(&v).unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&events_path, stripped + "\n").unwrap();

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(
        msgs.len(),
        2,
        "both messages must be loaded from old format"
    );
    assert_eq!(msgs[0].text_concat(), "real");
    assert_eq!(msgs[1].text_concat(), "reply");
    reopened.shutdown().await;
}

#[tokio::test]
async fn open_existing_skips_lines_with_broken_event_fields() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        s.append_message(user_msg("good"), None);
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };

    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    let mut body = std::fs::read_to_string(&events_path).unwrap();
    body.push_str(r#"{"type":"user_msg","turn_id":{"bad":"obj"},"message":{"role":"user","parts":[{"type":"text","text":"bad"}],"turn_id":"019f0000-0000-7000-0000-000000000001"}}
"#);
    std::fs::write(&events_path, body).unwrap();

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert_eq!(msgs.len(), 1, "bad line must be skipped");
    assert_eq!(msgs[0].text_concat(), "good");
    reopened.shutdown().await;
}

#[tokio::test]
async fn open_existing_empty_or_blank_events_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sid = {
        let s = Session::open(tmp.path()).unwrap();
        let id = s.id().to_string();
        s.shutdown().await;
        id
    };

    let events_path = tmp.path().join("sessions").join(&sid).join("events.jsonl");
    std::fs::write(&events_path, "\n\n").unwrap();

    let reopened = Session::open_existing(tmp.path(), &sid).unwrap();
    let msgs = reopened.messages();
    assert!(msgs.is_empty());
    reopened.shutdown().await;
}
