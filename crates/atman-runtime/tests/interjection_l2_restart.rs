use std::sync::Arc;
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::event::Event;
use atman_runtime::injection::InjectionLevel;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Session, Value};

#[tokio::test(flavor = "multi_thread")]
async fn l2_injection_mid_stream_triggers_restart_with_correction() {
    let root = tempfile::tempdir().unwrap();
    let session = Session::open(root.path()).unwrap();
    let sink = session.sink().clone();

    let mut ex = Executor::with_events(sink.clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_chunk_delay(Duration::from_millis(200))
            .with_model("mock-slow", Value::Str("a".repeat(500))),
    ));

    let src = r#"
flow t(user: string) -> string {
    reply = llm { model: "mock-slow", prompt: user }
    watch reply {
        on token(match: "___never_match_but_forces_streaming___") {
            abort("unused")
        }
    }
    return reply
}
"#;
    let file = parse_file(src).unwrap();

    let turn_id = atman_runtime::event::TurnId::now();
    let user_msg = atman_runtime::message::Message::user_text(turn_id.clone(), "start");
    session.begin_turn(user_msg);

    let injector = async {
        tokio::time::sleep(Duration::from_millis(150)).await;
        session
            .enqueue_injection_with_level(
                "use tokio not std::thread",
                InjectionLevel::L2CourseCorrect,
                None,
            )
            .expect("enqueue");
    };

    let flow = ex.run_in_turn(
        &file,
        "t",
        vec![("user".into(), Value::Str("start".into()))],
        Some(turn_id.clone()),
        Some(&session),
    );

    let (result, ()) = tokio::join!(flow, injector);
    let result = result.unwrap();
    session.end_turn();

    match result {
        Value::Str(_) | Value::Err(_) => {}
        other => panic!("expected str or err, got {other:?}"),
    }

    let events = sink.snapshot();
    let partial_hits: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::LlmPartialCall {
                restart_reason,
                tokens_before_abort,
                ..
            } => Some((restart_reason.clone(), *tokens_before_abort)),
            _ => None,
        })
        .collect();
    assert!(
        !partial_hits.is_empty(),
        "expected at least one llm_partial_call event, event count: {}",
        events.len()
    );
    assert_eq!(partial_hits[0].0, "l2_course_correct");
}
