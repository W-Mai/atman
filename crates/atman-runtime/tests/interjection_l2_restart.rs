mod common;

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, NodeEvent, Observable, TurnId};
use atman_runtime::injection::{InjectionLevel, InjectionState};
use atman_runtime::message::Message;
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider};
use atman_runtime::tool::BoxFut;
use atman_runtime::{Executor, RuntimeError, Session};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

struct CorrectingProvider {
    session: Weak<Session>,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
}

impl Provider for CorrectingProvider {
    fn name(&self) -> &str {
        "correcting"
    }

    fn call<'a>(&'a self, _: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        panic!("run controls must also be monitored without a UI stream subscriber")
    }

    fn call_streaming(&self, request: LlmRequest) -> Observable<AssistantMessage> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push(request);
            index
        };
        let session = self.session.upgrade().unwrap();
        let (tx, events) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let output = Box::pin(async move {
            let text = if index < 4 {
                format!("partial-{index}")
            } else {
                "complete".into()
            };
            tx.send(NodeEvent::LlmChunk {
                text: text.clone(),
                cumulative_tokens: 1,
            })
            .unwrap();
            if index < 4 {
                if index % 2 == 0 {
                    session
                        .enqueue_injection_with_level(
                            "same correction",
                            InjectionLevel::L2CourseCorrect,
                            None,
                        )
                        .unwrap();
                } else {
                    session
                        .flow_registry
                        .interject(
                            "root",
                            "same correction",
                            InjectionLevel::L2CourseCorrect,
                            None,
                        )
                        .unwrap();
                }
                request_cancel.cancelled().await;
                return Err(RuntimeError::Cancelled("interrupted".into()));
            }
            tx.send(NodeEvent::LlmDone { total_tokens: 1 }).unwrap();
            Ok(AssistantMessage::text_only(Message::assistant_text(
                TurnId::now(),
                text,
            )))
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

#[tokio::test]
async fn corrections_rebuild_canonical_context_without_a_restart_limit_or_duplicate_prompt() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "model",
            "correcting",
            100_000,
            None,
        )]))
        .await;
    for inline in [false, true] {
        for selection in [
            "context: \"session\"",
            "context: \"session_recent(1)\"",
            "prompt: \"explicit\"",
            "messages: [message.user(\"explicit\")]",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let session = Arc::new(Session::open(dir.path()).unwrap());
            let turn = session.begin_turn(Message::user_text(TurnId::now(), "task"));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let executor = Executor::with_events(session.sink().clone());
            executor.providers.register(Arc::new(CorrectingProvider {
                session: Arc::downgrade(&session),
                calls: calls.clone(),
            }));
            let source = if inline {
                format!(
                    "flow main() -> string {{ return subflow(agent) }}\nflow agent() -> string {{ return llm.call(model: \"model\", {selection}) }}"
                )
            } else {
                format!(
                    "flow main() -> string {{ return llm.call(model: \"model\", {selection}) }}"
                )
            };
            let file = parse_file(&source).unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                executor.run_in_turn(
                    &file,
                    "main",
                    vec![],
                    Some(turn.clone()),
                    Some(session.clone()),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                !matches!(result, atman_runtime::Value::Err(_)),
                "{result:?}"
            );
            session.end_turn(&turn);
            let requests = calls.lock().unwrap().clone();
            assert_eq!(requests.len(), 5, "{selection}, inline={inline}");
            for (index, request) in requests.iter().enumerate() {
                let texts: Vec<_> = request.messages.iter().map(Message::text_concat).collect();
                assert_eq!(
                    texts
                        .iter()
                        .filter(|text| text.contains("same correction"))
                        .count(),
                    index
                );
                for partial in 0..index {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| **text == format!("partial-{partial}"))
                            .count(),
                        1
                    );
                }
                if selection.starts_with("prompt:") || selection.starts_with("messages:") {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| text.as_str() == "explicit")
                            .count(),
                        1
                    );
                }
                if index > 0 && selection == "context: \"session\"" {
                    let previous = &requests[index - 1].messages;
                    assert_eq!(&request.messages[..previous.len()], previous);
                }
            }
            let entry = session.flow_registry.lookup("root").unwrap();
            assert!(Arc::ptr_eq(&entry.messages, &session.messages_handle()));
            assert!(Arc::ptr_eq(
                &entry.compact_lock,
                &session.compact_lock_handle()
            ));
            assert!(entry.pending_injections().is_empty());
            let events = session.sink().snapshot();
            let consumed: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::UserInject {
                        injection,
                        context_message: Some(message),
                        ..
                    } if injection.state == InjectionState::Injected => Some((injection, message)),
                    _ => None,
                })
                .collect();
            assert_eq!(consumed.len(), 4);
            for (injection, message) in consumed {
                assert_eq!(injection.flow_run_id.as_ref(), Some(&entry.child_run_id));
                assert_eq!(message.turn_id, turn);
            }
            let expected = session.messages().to_vec();
            assert_eq!(
                expected
                    .iter()
                    .filter(|message| message.text_concat().starts_with("partial-"))
                    .count(),
                4
            );
            session.flush_writer().await.unwrap();
            let restored = Session::open_existing(dir.path(), &session.id().to_string()).unwrap();
            assert_eq!(restored.messages().to_vec(), expected);
            session.shutdown().await;
            restored.shutdown().await;
        }
    }
}
