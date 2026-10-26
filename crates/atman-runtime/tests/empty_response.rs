mod common;

use std::sync::{Arc, Mutex};

use atman_rt::parse_file;
use atman_runtime::event::{LlmCallStatus, NodeEvent, Observable};
use atman_runtime::message::{Message, MessageOrigin, MessagePart};
use atman_runtime::provider::{
    AssistantMessage, CallTiming, LlmRequest, Provider, StopReason, TokenUsage,
};
use atman_runtime::session::Session;
use atman_runtime::tool::BoxFut;
use atman_runtime::{AtmanPayload, Executor, RuntimeError, Value};
use tokio_util::sync::CancellationToken;

type TurnId = atman_rt::TurnId<atman_runtime::event::AtmanUuid>;

#[derive(Clone, Copy)]
enum Reply {
    Empty,
    Thinking,
    Whitespace,
    Text,
    Tool,
    Final,
}

struct Scripted {
    replies: Vec<Reply>,
    requests: Mutex<Vec<LlmRequest>>,
    cancel: Option<CancellationToken>,
}

impl Scripted {
    fn response(&self, request: LlmRequest) -> AssistantMessage {
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        requests.push(request);
        if index == 2
            && let Some(cancel) = &self.cancel
        {
            cancel.cancel();
        }
        let reply = self.replies.get(index).copied().unwrap_or(Reply::Empty);
        let mut message = Message::assistant_text(TurnId::now(), "");
        message.parts = match reply {
            Reply::Empty => vec![],
            Reply::Thinking => vec![MessagePart::Thinking {
                thinking: "Checking".into(),
                signature: None,
            }],
            Reply::Whitespace => vec![MessagePart::Text {
                text: " \n\t\u{3000}".into(),
            }],
            Reply::Text => vec![MessagePart::Text {
                text: "Done.".into(),
            }],
            Reply::Tool | Reply::Final => vec![MessagePart::ToolUse {
                id: format!("call-{index}"),
                name: if matches!(reply, Reply::Final) {
                    "final.answer"
                } else {
                    "rule.fetch"
                }
                .into(),
                input: if matches!(reply, Reply::Final) {
                    serde_json::json!({"message": "Done."})
                } else {
                    serde_json::json!({})
                },
                intent: atman_runtime::message::ToolCallIntent::new("Deliver requested work"),
            }],
        };
        AssistantMessage {
            message,
            stop_reason: StopReason::End,
            token_usage: TokenUsage::default(),
            timing: CallTiming::default(),
            model: "probe".into(),
            response_id: None,
        }
    }
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        "probe"
    }
    fn call<'a>(
        &'a self,
        request: LlmRequest,
    ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        Box::pin(async move { Ok(self.response(request)) })
    }
    fn call_streaming(&self, request: LlmRequest) -> Observable<AssistantMessage> {
        let message = self.response(request);
        let (tx, events) = tokio::sync::broadcast::channel(4);
        Observable {
            output: Box::pin(async move {
                let _ = tx.send(NodeEvent::LlmDone { total_tokens: 0 });
                Ok(message)
            }),
            events,
            cancel: CancellationToken::new(),
        }
    }
}

fn source(main: bool) -> String {
    let mut source = if main {
        let template = atman_runtime::templates::AGENT_AT;
        format!(
            "{}flow probe() -> string {{\n    contract {{ invocation {{ goal: true }} }}\n    {}",
            template.split_once("flow agent(").unwrap().0,
            &template[template.find("tools_called = false").unwrap()..]
        )
        .replace("system: system_prompt", "system: \"Test system prompt\"")
    } else {
        atman_runtime::templates::SUBAGENT_AT.to_string()
    };
    let mut offset = 0;
    while let Some(start) = source[offset..].find("tools: [") {
        let start = offset + start + "tools: [".len();
        let end = start + source[start..].find(']').unwrap();
        let catalog = "\"final.answer\", \"rule.fetch\"";
        source.replace_range(start..end, catalog);
        offset = start + catalog.len();
    }
    source.replace("retry: 12", "retry: 0")
}

fn program(root: &std::path::Path, main: bool) -> atman_runtime::source_program::LinkedProgram {
    let path = root.join("commands/empty-response-test.at");
    std::fs::write(&path, source(main)).unwrap();
    atman_runtime::source_program::load_program(
        path,
        &atman_runtime::source_program::SourceRoots {
            config_dir: Some(root.to_path_buf()),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn registry() -> common::ModelRegistryGuard {
    common::ModelRegistryGuard::acquire(common::config(
        ["smart", "probe"].map(|model| common::model_for_provider(model, "probe", 131_072, None)),
    ))
    .await
}

#[tokio::test]
async fn empty_success_reaches_flow_without_transport_retry() {
    let _registry = registry().await;
    let executor = Executor::new();
    let provider = Arc::new(Scripted {
        replies: vec![Reply::Empty],
        requests: Mutex::new(vec![]),
        cancel: None,
    });
    executor.providers.register(provider.clone());
    let file = parse_file("flow probe() -> any { return llm.call(model: \"probe\", prompt: \"hi\", record: false, output: \"response\", retry: 0) }").unwrap();
    let output = executor.run(&file, "probe", vec![]).await.unwrap();
    assert!(
        matches!(output.field("message"), Some(Value::Host(AtmanPayload::Message(message))) if message.parts.is_empty())
    );
    assert!(matches!(output.field("stop_reason"), Some(Value::Str(reason)) if reason == "end"));
    assert!(matches!(output.field("has_text"), Some(Value::Bool(false))));
    assert_eq!(provider.requests.lock().unwrap().len(), 1);
    assert!(executor.events.snapshot().iter().any(|event| matches!(
        event,
        atman_runtime::Event::LlmCall {
            status: LlmCallStatus::Ok,
            ..
        }
    )));
}

#[tokio::test(start_paused = true)]
async fn managed_flows_continue_empty_responses_without_replaying_tools() {
    assert_managed(false).await;
}

#[tokio::test(start_paused = true)]
async fn managed_empty_continuation_is_cancellable() {
    assert_managed(true).await;
}

async fn assert_managed(cancel: bool) {
    let _registry = registry().await;
    for entry in [
        "probe",
        "research_loop",
        "verify_loop",
        "implement_loop",
        "review_loop",
    ] {
        let dir = tempfile::tempdir().unwrap();
        atman_runtime::templates::ensure_managed_agent_at(dir.path()).unwrap();
        let mut executor = Executor::new();
        executor.source_dir = Some(dir.path().join("commands"));
        atman_runtime::tools::register_tier_zero(&executor.tools);
        atman_runtime::tools::register_watch(&executor.tools);
        let session = Arc::new(Session::open_ephemeral());
        session.goal_control.set("Unfinished work", None).unwrap();
        let turn = TurnId::now();
        session.begin_turn(Message::user_text(turn.clone(), "hi"));
        let provider = Arc::new(Scripted {
            replies: if cancel {
                vec![Reply::Empty; 3]
            } else {
                vec![
                    Reply::Empty,
                    Reply::Thinking,
                    Reply::Whitespace,
                    Reply::Tool,
                    Reply::Empty,
                    Reply::Text,
                    Reply::Final,
                ]
            },
            requests: Mutex::new(vec![]),
            cancel: cancel.then(|| session.flow_cancel_token()),
        });
        executor.providers.register(provider.clone());
        let args = if entry == "probe" {
            vec![]
        } else {
            vec![
                ("goal".into(), Value::Str("work".into())),
                ("model".into(), Value::Str("probe".into())),
                ("max_iter".into(), Value::Int(200)),
            ]
        };
        let program = program(dir.path(), entry == "probe");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            executor.run_linked_in_turn_with_env(
                &program,
                entry,
                args,
                Some(turn),
                Some(session.clone()),
                Default::default(),
            ),
        )
        .await
        .expect("flow must progress or honour cancellation");
        let requests = provider.requests.lock().unwrap();
        if cancel {
            assert!(
                matches!(result, Err(RuntimeError::Cancelled(_))),
                "{entry}: {result:?}"
            );
            assert_eq!(requests.len(), 3, "{entry}");
            assert!(
                !session
                    .messages()
                    .iter()
                    .any(|message| message.origin == MessageOrigin::FinalAnswer)
            );
        } else {
            assert!(
                matches!(result, Ok(Value::Str(ref answer)) if answer == "Done."),
                "{entry}: {result:?}"
            );
            assert_eq!(requests.len(), 7, "{entry}");
            let messages = session.messages();
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message.origin == MessageOrigin::FinalAnswer)
                    .count(),
                1
            );
            let results = messages.iter().flat_map(|message| &message.parts).filter(|part| matches!(part, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call-3")).count();
            assert_eq!(results, 1, "{entry}: a completed tool must not run again");
            assert!(requests[6].messages.iter().flat_map(|message| &message.parts).any(|part| matches!(part, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == "call-3")));
            for request in &requests[1..4] {
                assert_eq!(
                    request
                        .messages
                        .iter()
                        .filter(|message| message
                            .text_concat()
                            .contains("previous model response contained no usable answer"))
                        .count(),
                    1,
                    "continuation instructions must not accumulate during one empty sequence"
                );
            }
            for request in &*requests {
                assert!(
                    !request.messages.iter().any(|message| message.role
                        == atman_runtime::message::MessageRole::Assistant
                        && message.parts.is_empty()),
                    "empty assistant history must not be sent to providers"
                );
            }
        }
        assert_eq!(
            session.goal_control.record().unwrap().objective,
            "Unfinished work"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn empty_response_wait_respects_pause_resume_and_cancel() {
    use atman_runtime::memory::goal::{GoalActor, GoalPauseReason};
    let _registry = registry().await;
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        atman_runtime::templates::ensure_managed_agent_at(dir.path()).unwrap();
        let mut executor = Executor::new();
        executor.source_dir = Some(dir.path().join("commands"));
        atman_runtime::tools::register_tier_zero(&executor.tools);
        atman_runtime::tools::register_watch(&executor.tools);
        let session = Arc::new(Session::open_ephemeral());
        session.goal_control.set("Unfinished work", None).unwrap();
        let turn = TurnId::now();
        session.begin_turn(Message::user_text(turn.clone(), "hi"));
        let provider = Arc::new(Scripted {
            replies: vec![Reply::Empty, Reply::Final],
            requests: Mutex::new(vec![]),
            cancel: None,
        });
        executor.providers.register(provider.clone());
        let runtime_session = session.clone();
        let program = program(dir.path(), true);
        let task = tokio::spawn(async move {
            executor
                .run_linked_in_turn_with_env(
                    &program,
                    "probe",
                    vec![],
                    Some(turn),
                    Some(runtime_session),
                    Default::default(),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(provider.requests.lock().unwrap().len(), 1);
        if cancel {
            session.cancel_flow();
        } else {
            session
                .goal_control
                .pause(
                    GoalActor::User,
                    GoalPauseReason::UserRequested,
                    "pause".into(),
                    None,
                )
                .unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "a paused or cancelled flow must not make another request"
        );
        if !cancel {
            session.goal_control.resume(GoalActor::User, None).unwrap();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        if cancel {
            assert!(matches!(result, Err(RuntimeError::Cancelled(_))));
        } else {
            assert!(matches!(result, Ok(Value::Str(answer)) if answer == "Done."));
            assert_eq!(provider.requests.lock().unwrap().len(), 2);
        }
    }
}
