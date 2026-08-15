mod common;

use std::sync::Arc;
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::event::Event;
use atman_runtime::injection::InjectionLevel;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Session, Value};

#[tokio::test(flavor = "multi_thread")]
async fn l2_injection_mid_stream_triggers_restart_with_correction() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "mock-slow",
            "mock",
            8_192,
            None,
        )]))
        .await;
    let root = tempfile::tempdir().unwrap();
    let session = std::sync::Arc::new(Session::open(root.path()).unwrap());
    let sink = session.sink().clone();

    let ex = Executor::with_events(sink.clone());
    ex.providers.register(Arc::new(
        MockProvider::new("mock")
            .with_chunk_delay(Duration::from_millis(200))
            .with_model("mock-slow", Value::Str("a".repeat(500))),
    ));

    let src = r#"
flow t(user: string) -> string {
    reply = llm.call(model: "mock-slow", prompt: user, context: "session")
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
        let entry = session
            .flow_registry
            .lookup("root")
            .expect("root flow entry");
        entry.pending_injections.lock().unwrap().push(
            atman_runtime::injection::Injection::with_level(
                turn_id.clone(),
                "use tokio not std::thread",
                InjectionLevel::L2CourseCorrect,
                None,
            ),
        );
        entry.injection_notify.notify_one();
    };

    let flow = ex.run_in_turn(
        &file,
        "t",
        vec![("user".into(), Value::Str("start".into()))],
        Some(turn_id.clone()),
        Some(session.clone()),
    );

    let (result, ()) = tokio::join!(flow, injector);
    let result = result.unwrap();
    session.end_turn();

    match result {
        Value::Message(_) | Value::Err(_) => {}
        other => panic!("expected message or err, got {other:?}"),
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
